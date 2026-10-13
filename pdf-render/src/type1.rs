// Clean-room implementation of the Adobe Type 1 Font Format specification.
// This reads the font data grammar, never executes its PostScript procedures.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use pdf_core::{DocError, LimitKind, Refusal};
use pdf_text::encoding::BaseEncoding;

use crate::display::{Path, PathEl};

const MAX_FONT_BYTES: usize = 64 << 20;
const MAX_PROGRAM_BYTES: usize = 65_535;
const MAX_TOKEN_BYTES: usize = 65_535;
const MAX_NAME_BYTES: usize = 4096;
const MAX_GLYPHS: usize = 65_536;
const MAX_SUBRS: usize = 65_536;
const MAX_TOKENS: usize = 1_000_000;
const MAX_OPS: usize = 1_000_000;
const MAX_PATH_ELEMENTS: usize = 1_000_000;
const MAX_PS_DEPTH: usize = 32;
const MAX_DEPTH: usize = 10;
const MAX_OPERANDS: usize = 24;

type Result<T> = core::result::Result<T, DocError>;
type Point = [f64; 2];
type Encoding = [Option<EncodingName>; 256];

#[derive(Clone, Debug)]
pub struct Type1Font {
    program: Arc<FontProgram>,
}

#[derive(Debug)]
struct FontProgram {
    bytes: Vec<u8>,
    subrs: Vec<Option<Span>>,
    glyphs: Vec<GlyphDef>,
    matrix: [f64; 6],
    encoding: Option<Encoding>,
}

#[derive(Clone, Copy, Debug, Default)]
struct Span {
    start: usize,
    end: usize,
}

impl Span {
    fn bytes(self, source: &[u8]) -> Result<&[u8]> {
        source.get(self.start..self.end).ok_or_else(malformed)
    }

    fn len(self) -> Result<usize> {
        self.end.checked_sub(self.start).ok_or_else(malformed)
    }
}

#[derive(Debug)]
struct GlyphDef {
    name: String,
    code: Span,
}

#[derive(Debug)]
enum EncodingName {
    Static(&'static str),
    Owned(String),
}

impl EncodingName {
    fn as_str(&self) -> &str {
        match self {
            Self::Static(name) => name,
            Self::Owned(name) => name.as_str(),
        }
    }
}

/// Parses the decoded PDF FontFile stream. Length1 and Length2 delimit the
/// cleartext and eexec portions, excluding any PFB record headers.
///
/// A zero Length1 discovers eexec lexically. A zero Length2 uses the remaining
/// input. Unencrypted programs are accepted when Length2 is zero.
///
/// The contract's Option return maps malformed programs and named internal
/// limit refusals to None. The caller is responsible for font_program_failed.
pub fn parse_type1(data: &[u8], length1: usize, length2: usize) -> Option<Type1Font> {
    parse_checked(data, length1, length2).ok()
}

impl Type1Font {
    /// Returns an uncached outline in units of 1/1000 em, with y pointing up.
    /// FontMatrix, including its translation, is applied exactly once.
    pub fn outline(&self, glyph_name: &str) -> Option<Path> {
        self.program.outline(glyph_name).ok()
    }

    pub fn encoding(&self) -> Option<[Option<&str>; 256]> {
        self.program.encoding.as_ref().map(|encoding| {
            core::array::from_fn(|index| {
                encoding
                    .get(index)
                    .and_then(Option::as_ref)
                    .map(EncodingName::as_str)
            })
        })
    }
}

fn malformed() -> DocError {
    DocError::Malformed {
        offset: 0,
        what: "type1_font_program",
    }
}

fn refusal(kind: LimitKind, requested: usize, maximum: usize) -> DocError {
    DocError::Refused(Refusal {
        limit: kind,
        requested: requested as u64,
        maximum: maximum as u64,
    })
}

fn check(kind: LimitKind, requested: usize, maximum: usize) -> Result<()> {
    if requested > maximum {
        Err(refusal(kind, requested, maximum))
    } else {
        Ok(())
    }
}

fn allocation(kind: LimitKind, maximum: usize) -> DocError {
    refusal(kind, maximum.saturating_add(1), maximum)
}

fn owned_name(bytes: &[u8]) -> Result<String> {
    check(LimitKind::ObjectBytes, bytes.len(), MAX_NAME_BYTES)?;
    let text = core::str::from_utf8(bytes).map_err(|_| malformed())?;
    if text.is_empty() {
        return Err(malformed());
    }
    let mut name = String::new();
    name.try_reserve_exact(text.len())
        .map_err(|_| allocation(LimitKind::ObjectBytes, MAX_NAME_BYTES))?;
    name.push_str(text);
    Ok(name)
}

fn finite_point(point: Point) -> Result<Point> {
    if point.iter().all(|value| value.is_finite()) {
        Ok(point)
    } else {
        Err(malformed())
    }
}

fn add_point(left: Point, right: Point) -> Result<Point> {
    let [x, y] = left;
    let [dx, dy] = right;
    finite_point([x + dx, y + dy])
}

fn integer(value: f64) -> Result<usize> {
    if !value.is_finite() || value < 0.0 || value >= usize::MAX as f64 {
        return Err(malformed());
    }
    let integer = value as usize;
    if integer as f64 == value {
        Ok(integer)
    } else {
        Err(malformed())
    }
}

fn space(byte: u8) -> bool {
    matches!(byte, 0 | 9 | 10 | 12 | 13 | 32)
}

fn delimiter(byte: u8) -> bool {
    space(byte) || b"()<>[]{}/%".contains(&byte)
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte.saturating_sub(b'0')),
        b'a'..=b'f' => Some(byte.saturating_sub(b'a').saturating_add(10)),
        b'A'..=b'F' => Some(byte.saturating_sub(b'A').saturating_add(10)),
        _ => None,
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum TokenKind {
    Name,
    Word,
    Open(u8),
    Close(u8),
    String,
}

#[derive(Clone, Copy)]
struct Token<'a> {
    kind: TokenKind,
    bytes: &'a [u8],
}

impl Token<'_> {
    fn word(self, expected: &[u8]) -> bool {
        self.kind == TokenKind::Word && self.bytes == expected
    }

    fn number(self) -> Result<f64> {
        if self.kind != TokenKind::Word {
            return Err(malformed());
        }
        let text = core::str::from_utf8(self.bytes).map_err(|_| malformed())?;
        let number = text.parse::<f64>().map_err(|_| malformed())?;
        if number.is_finite() {
            Ok(number)
        } else {
            Err(malformed())
        }
    }

    fn unsigned(self) -> Result<usize> {
        if self.kind != TokenKind::Word {
            return Err(malformed());
        }
        let text = core::str::from_utf8(self.bytes).map_err(|_| malformed())?;
        text.parse::<usize>().map_err(|_| malformed())
    }
}

struct Reader<'a> {
    data: &'a [u8],
    position: usize,
    tokens: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8], tokens: usize) -> Self {
        Self {
            data,
            position: 0,
            tokens,
        }
    }

    fn advance(&mut self) -> Result<u8> {
        let byte = self
            .data
            .get(self.position)
            .copied()
            .ok_or_else(malformed)?;
        self.position = self.position.checked_add(1).ok_or_else(malformed)?;
        Ok(byte)
    }

    fn next(&mut self) -> Result<Option<Token<'a>>> {
        while let Some(byte) = self.data.get(self.position).copied() {
            if space(byte) {
                self.advance()?;
            } else if byte == b'%' {
                while let Some(byte) = self.data.get(self.position).copied() {
                    if matches!(byte, b'\r' | b'\n') {
                        break;
                    }
                    self.advance()?;
                }
            } else {
                break;
            }
        }

        if self.position == self.data.len() {
            return Ok(None);
        }
        self.tokens = self
            .tokens
            .checked_add(1)
            .ok_or_else(|| refusal(LimitKind::Tokens, usize::MAX, MAX_TOKENS))?;
        check(LimitKind::Tokens, self.tokens, MAX_TOKENS)?;

        let start = self.position;
        let first = self.advance()?;
        let kind = match first {
            b'/' => {
                while let Some(byte) = self.data.get(self.position).copied() {
                    if delimiter(byte) {
                        break;
                    }
                    self.advance()?;
                }
                TokenKind::Name
            }
            b'[' | b'{' => TokenKind::Open(first),
            b']' | b'}' => TokenKind::Close(first),
            b'<' if self.data.get(self.position) == Some(&b'<') => {
                self.advance()?;
                TokenKind::Open(b'<')
            }
            b'>' if self.data.get(self.position) == Some(&b'>') => {
                self.advance()?;
                TokenKind::Close(b'>')
            }
            b'<' => {
                loop {
                    let byte = self.advance()?;
                    if byte == b'>' {
                        break;
                    }
                    if !space(byte) && hex(byte).is_none() {
                        return Err(malformed());
                    }
                }
                TokenKind::String
            }
            b'(' => {
                let mut depth = 1usize;
                while depth != 0 {
                    match self.advance()? {
                        b'\\' => {
                            self.advance()?;
                        }
                        b'(' => {
                            depth = depth.checked_add(1).ok_or_else(malformed)?;
                            check(LimitKind::Depth, depth, MAX_PS_DEPTH)?;
                        }
                        b')' => {
                            depth = depth.checked_sub(1).ok_or_else(malformed)?;
                        }
                        _ => {}
                    }
                }
                TokenKind::String
            }
            b')' | b'>' => return Err(malformed()),
            _ => {
                while let Some(byte) = self.data.get(self.position).copied() {
                    if delimiter(byte) {
                        break;
                    }
                    self.advance()?;
                }
                TokenKind::Word
            }
        };

        let length = self.position.checked_sub(start).ok_or_else(malformed)?;
        check(LimitKind::ObjectBytes, length, MAX_TOKEN_BYTES)?;
        let start = if kind == TokenKind::Name {
            start.checked_add(1).ok_or_else(malformed)?
        } else {
            start
        };
        let bytes = self.data.get(start..self.position).ok_or_else(malformed)?;
        Ok(Some(Token { kind, bytes }))
    }

    fn required(&mut self) -> Result<Token<'a>> {
        self.next()?.ok_or_else(malformed)
    }

    fn unsigned(&mut self) -> Result<usize> {
        self.required()?.unsigned()
    }

    fn require_word(&mut self, expected: &[u8]) -> Result<()> {
        if self.required()?.word(expected) {
            Ok(())
        } else {
            Err(malformed())
        }
    }

    fn group(&mut self, opening: u8) -> Result<()> {
        let mut stack = FixedStack::<u8, MAX_PS_DEPTH>::new(LimitKind::Depth);
        stack.push(closing(opening)?)?;
        while let Some(expected) = stack.last().copied() {
            let token = self.required()?;
            match token.kind {
                TokenKind::Open(opening) => stack.push(closing(opening)?)?,
                TokenKind::Close(found) if found == expected => {
                    stack.pop();
                }
                TokenKind::Close(_) => return Err(malformed()),
                _ => {}
            }
        }
        Ok(())
    }

    fn binary(&mut self, length: usize) -> Result<Span> {
        check(LimitKind::ObjectBytes, length, MAX_PROGRAM_BYTES)?;
        // The Type 1 data grammar permits a procedure name for RD. Its name
        // does not matter here: the length and single separator delimit bytes.
        let marker = self.required()?;
        if marker.kind != TokenKind::Word || marker.bytes.is_empty() {
            return Err(malformed());
        }
        if !space(self.advance()?) {
            return Err(malformed());
        }
        let start = self.position;
        let end = start.checked_add(length).ok_or_else(malformed)?;
        self.data.get(start..end).ok_or_else(malformed)?;
        self.position = end;
        Ok(Span { start, end })
    }
}

fn closing(opening: u8) -> Result<u8> {
    match opening {
        b'[' => Ok(b']'),
        b'{' => Ok(b'}'),
        b'<' => Ok(b'>'),
        _ => Err(malformed()),
    }
}

enum ScanEnd {
    Eexec(usize),
    Complete,
    Eof,
}

struct Parsed {
    matrix: [f64; 6],
    encoding: Option<Encoding>,
    len_iv: i64,
    subrs: Vec<Option<Span>>,
    glyphs: Vec<GlyphDef>,
    subrs_seen: bool,
    tokens: usize,
}

impl Parsed {
    fn new() -> Self {
        Self {
            matrix: [0.001, 0.0, 0.0, 0.001, 0.0, 0.0],
            encoding: None,
            len_iv: 4,
            subrs: Vec::new(),
            glyphs: Vec::new(),
            subrs_seen: false,
            tokens: 0,
        }
    }

    fn scan(&mut self, data: &[u8], allow_eexec: bool) -> Result<ScanEnd> {
        let mut reader = Reader::new(data, self.tokens);
        let mut in_subrs = false;
        while let Some(token) = reader.next()? {
            match token.kind {
                TokenKind::Open(opening) => reader.group(opening)?,
                TokenKind::Close(_) => return Err(malformed()),
                TokenKind::Name => {
                    in_subrs = false;
                    match token.bytes {
                        b"FontType" => {
                            if reader.unsigned()? != 1 {
                                return Err(malformed());
                            }
                        }
                        b"FontMatrix" => {
                            let opening = reader.required()?;
                            let end = match opening.kind {
                                TokenKind::Open(b'[') => b']',
                                TokenKind::Open(b'{') => b'}',
                                _ => return Err(malformed()),
                            };
                            for slot in &mut self.matrix {
                                *slot = reader.required()?.number()?;
                            }
                            if reader.required()?.kind != TokenKind::Close(end) {
                                return Err(malformed());
                            }
                        }
                        b"Encoding" => {
                            self.encoding = read_encoding(&mut reader)?;
                        }
                        b"lenIV" => {
                            let value = reader.required()?;
                            if value.kind != TokenKind::Word {
                                return Err(malformed());
                            }
                            let text =
                                core::str::from_utf8(value.bytes).map_err(|_| malformed())?;
                            let len_iv = text.parse::<i64>().map_err(|_| malformed())?;
                            if len_iv < -1 {
                                return Err(malformed());
                            }
                            if len_iv >= 0 {
                                let requested = usize::try_from(len_iv).map_err(|_| malformed())?;
                                check(LimitKind::ObjectBytes, requested, MAX_PROGRAM_BYTES)?;
                            }
                            self.len_iv = len_iv;
                        }
                        b"Subrs" => {
                            if self.subrs_seen {
                                return Err(malformed());
                            }
                            self.subrs_seen = true;
                            let count = reader.unsigned()?;
                            check(LimitKind::Objects, count, MAX_SUBRS)?;
                            reader.require_word(b"array")?;
                            self.subrs
                                .try_reserve_exact(count)
                                .map_err(|_| allocation(LimitKind::Objects, MAX_SUBRS))?;
                            self.subrs.resize(count, None);
                            in_subrs = true;
                        }
                        b"CharStrings" => {
                            self.read_glyphs(&mut reader)?;
                            self.tokens = reader.tokens;
                            return Ok(ScanEnd::Complete);
                        }
                        _ => {}
                    }
                }
                TokenKind::Word if token.word(b"eexec") => {
                    if !allow_eexec {
                        return Err(malformed());
                    }
                    self.tokens = reader.tokens;
                    return Ok(ScanEnd::Eexec(reader.position));
                }
                TokenKind::Word if in_subrs && token.word(b"dup") => {
                    let index = reader.unsigned()?;
                    let length = reader.unsigned()?;
                    let code = reader.binary(length)?;
                    let slot = self.subrs.get_mut(index).ok_or_else(malformed)?;
                    if slot.is_some() {
                        return Err(malformed());
                    }
                    *slot = Some(code);
                }
                TokenKind::Word
                    if in_subrs
                        && (token.word(b"def") || token.word(b"ND") || token.word(b"|-")) =>
                {
                    in_subrs = false;
                }
                // Synthetic and hybrid programs require PostScript execution.
                TokenKind::Word if token.word(b"FontDirectory") || token.word(b"hires") => {
                    return Err(DocError::Unsupported {
                        feature: "Type1 synthetic or hybrid font",
                    });
                }
                _ => {}
            }
        }
        self.tokens = reader.tokens;
        Ok(ScanEnd::Eof)
    }

    fn read_glyphs(&mut self, reader: &mut Reader<'_>) -> Result<()> {
        let count = reader.unsigned()?;
        check(LimitKind::Objects, count, MAX_GLYPHS)?;
        reader.require_word(b"dict")?;
        self.glyphs
            .try_reserve_exact(count)
            .map_err(|_| allocation(LimitKind::Objects, MAX_GLYPHS))?;

        // Normal programs use "dup begin" or "begin" after the dictionary.
        loop {
            let token = reader.required()?;
            if token.word(b"begin") {
                break;
            }
            if !token.word(b"dup") {
                return Err(malformed());
            }
        }

        loop {
            let token = reader.required()?;
            if token.word(b"end") {
                return if self.glyphs.is_empty() {
                    Err(malformed())
                } else {
                    Ok(())
                };
            }
            match token.kind {
                TokenKind::Name => {
                    let requested = self.glyphs.len().checked_add(1).ok_or_else(malformed)?;
                    check(LimitKind::Objects, requested, MAX_GLYPHS)?;
                    if requested > count {
                        return Err(malformed());
                    }
                    let name = owned_name(token.bytes)?;
                    let length = reader.unsigned()?;
                    let code = reader.binary(length)?;
                    self.glyphs.push(GlyphDef { name, code });
                }
                // ND is a procedure alias. Also accept the unabbreviated
                // "noaccess def", without running either form.
                TokenKind::Word => {}
                _ => return Err(malformed()),
            }
        }
    }

    fn finish(mut self, source: &[u8]) -> Result<Type1Font> {
        if self.glyphs.is_empty() {
            return Err(malformed());
        }
        let skip = if self.len_iv == -1 {
            0
        } else {
            usize::try_from(self.len_iv).map_err(|_| malformed())?
        };

        let mut total = 0usize;
        for span in self
            .subrs
            .iter()
            .flatten()
            .chain(self.glyphs.iter().map(|glyph| &glyph.code))
        {
            let length = span.len()?.checked_sub(skip).ok_or_else(malformed)?;
            total = total
                .checked_add(length)
                .ok_or_else(|| refusal(LimitKind::DecodedBytes, usize::MAX, MAX_FONT_BYTES))?;
            check(LimitKind::DecodedBytes, total, MAX_FONT_BYTES)?;
        }

        // One arena for all decoded charstrings. Subroutine calls and outline
        // construction borrow it and never allocate temporary program buffers.
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(total)
            .map_err(|_| allocation(LimitKind::DecodedBytes, MAX_FONT_BYTES))?;
        for span in self.subrs.iter_mut().flatten() {
            *span = decode_program(source, *span, self.len_iv, &mut bytes)?;
        }
        for glyph in &mut self.glyphs {
            glyph.code = decode_program(source, glyph.code, self.len_iv, &mut bytes)?;
        }

        self.glyphs
            .sort_unstable_by(|left, right| left.name.cmp(&right.name));
        for pair in self.glyphs.windows(2) {
            let left = pair.first().ok_or_else(malformed)?;
            let right = pair.get(1).ok_or_else(malformed)?;
            if left.name == right.name {
                return Err(malformed());
            }
        }

        Ok(Type1Font {
            program: Arc::new(FontProgram {
                bytes,
                subrs: self.subrs,
                glyphs: self.glyphs,
                matrix: self.matrix,
                encoding: self.encoding,
            }),
        })
    }
}

fn read_encoding(reader: &mut Reader<'_>) -> Result<Option<Encoding>> {
    let first = reader.required()?;
    let mut encoding: Encoding = core::array::from_fn(|_| None);
    let known = if first.word(b"StandardEncoding") {
        for (code, slot) in encoding.iter_mut().enumerate() {
            let byte = u8::try_from(code).map_err(|_| malformed())?;
            *slot = BaseEncoding::Standard
                .glyph_name(byte)
                .map(EncodingName::Static);
        }
        true
    } else if first.kind == TokenKind::Open(b'[') {
        let mut code = 0usize;
        loop {
            let token = reader.required()?;
            if token.kind == TokenKind::Close(b']') {
                return Ok(Some(encoding));
            }
            if token.kind != TokenKind::Name {
                return Err(malformed());
            }
            let name = owned_name(token.bytes)?;
            let slot = encoding.get_mut(code).ok_or_else(malformed)?;
            *slot = Some(EncodingName::Owned(name));
            code = code.checked_add(1).ok_or_else(malformed)?;
        }
    } else if first.kind == TokenKind::Word && first.bytes.first().is_some_and(u8::is_ascii_digit) {
        if first.unsigned()? != 256 {
            return Err(malformed());
        }
        reader.require_word(b"array")?;
        // The standard custom-encoding initializer fills unused entries with
        // .notdef. Explicit dup/code/name/put assignments override these.
        for slot in &mut encoding {
            *slot = Some(EncodingName::Static(".notdef"));
        }
        true
    } else {
        false
    };

    loop {
        let token = reader.required()?;
        if token.word(b"def") || token.word(b"readonly") {
            return Ok(if known { Some(encoding) } else { None });
        }
        match token.kind {
            TokenKind::Open(opening) => reader.group(opening)?,
            TokenKind::Close(_) => return Err(malformed()),
            TokenKind::Word if known && token.word(b"dup") => {
                let code = reader.unsigned()?;
                let name = reader.required()?;
                if name.kind != TokenKind::Name {
                    return Err(malformed());
                }
                let name = owned_name(name.bytes)?;
                reader.require_word(b"put")?;
                let slot = encoding.get_mut(code).ok_or_else(malformed)?;
                *slot = Some(EncodingName::Owned(name));
            }
            TokenKind::Name => return Err(malformed()),
            _ => {}
        }
    }
}

fn parse_checked(data: &[u8], length1: usize, length2: usize) -> Result<Type1Font> {
    check(LimitKind::DecodedBytes, data.len(), MAX_FONT_BYTES)?;
    let clear = if length1 == 0 {
        data
    } else {
        data.get(..length1).ok_or_else(malformed)?
    };
    let mut parsed = Parsed::new();
    match parsed.scan(clear, true)? {
        ScanEnd::Complete if length2 == 0 => parsed.finish(clear),
        ScanEnd::Eexec(position) => {
            let start = if length1 == 0 {
                let mut start = position;
                while data.get(start).copied().is_some_and(space) {
                    start = start.checked_add(1).ok_or_else(malformed)?;
                }
                start
            } else {
                // Length1 includes the cleartext separator. Never strip bytes
                // from a length-delimited binary ciphertext.
                if !clear
                    .get(position..)
                    .ok_or_else(malformed)?
                    .iter()
                    .copied()
                    .all(space)
                {
                    return Err(malformed());
                }
                length1
            };
            let end = if length2 == 0 {
                data.len()
            } else {
                start.checked_add(length2).ok_or_else(malformed)?
            };
            let cipher = data.get(start..end).ok_or_else(malformed)?;
            let plain = decode_eexec(cipher, length2 == 0)?;
            match parsed.scan(&plain, false)? {
                ScanEnd::Complete => parsed.finish(&plain),
                _ => Err(malformed()),
            }
        }
        _ => Err(malformed()),
    }
}

struct Cipher {
    state: u16,
}

impl Cipher {
    fn byte(&mut self, cipher: u8) -> u8 {
        let plain = cipher ^ (self.state >> 8) as u8;
        // The cipher's recurrence is explicitly arithmetic modulo 65536.
        self.state = self
            .state
            .wrapping_add(u16::from(cipher))
            .wrapping_mul(52_845)
            .wrapping_add(22_719);
        plain
    }
}

fn decode_eexec(input: &[u8], infer_length: bool) -> Result<Vec<u8>> {
    let mut probe = input.iter().copied().filter(|byte| !space(*byte));
    let mut hexadecimal = true;
    for _ in 0..4 {
        let byte = probe.next().ok_or_else(malformed)?;
        hexadecimal &= hex(byte).is_some();
    }

    let mut output = Vec::new();
    output
        .try_reserve_exact(input.len())
        .map_err(|_| allocation(LimitKind::DecodedBytes, MAX_FONT_BYTES))?;
    let mut cipher = Cipher { state: 55_665 };
    let mut count = 0usize;
    let mut high = None;

    for (position, byte) in input.iter().copied().enumerate() {
        let encrypted = if hexadecimal {
            if infer_length && high.is_none() {
                let end = position.checked_add(11).ok_or_else(malformed)?;
                if input.get(position..end) == Some(b"cleartomark")
                    && input.get(end).copied().map(delimiter).unwrap_or(true)
                {
                    break;
                }
            }
            if space(byte) {
                continue;
            }
            let digit = hex(byte).ok_or_else(malformed)?;
            match high.take() {
                Some(first) => (first << 4) | digit,
                None => {
                    high = Some(digit);
                    continue;
                }
            }
        } else {
            byte
        };
        let plain = cipher.byte(encrypted);
        if count >= 4 {
            output.push(plain);
        }
        count = count.checked_add(1).ok_or_else(malformed)?;
    }
    if count < 4 || high.is_some() {
        return Err(malformed());
    }
    Ok(output)
}

fn decode_program(source: &[u8], span: Span, len_iv: i64, output: &mut Vec<u8>) -> Result<Span> {
    let input = span.bytes(source)?;
    let start = output.len();
    if len_iv == -1 {
        output.extend_from_slice(input);
    } else {
        let skip = usize::try_from(len_iv).map_err(|_| malformed())?;
        if input.len() < skip {
            return Err(malformed());
        }
        let mut cipher = Cipher { state: 4330 };
        for (index, byte) in input.iter().copied().enumerate() {
            let plain = cipher.byte(byte);
            if index >= skip {
                output.push(plain);
            }
        }
    }
    Ok(Span {
        start,
        end: output.len(),
    })
}

struct FixedStack<T: Copy + Default, const N: usize> {
    values: [T; N],
    len: usize,
    limit: LimitKind,
}

impl<T: Copy + Default, const N: usize> FixedStack<T, N> {
    fn new(limit: LimitKind) -> Self {
        Self {
            values: [T::default(); N],
            len: 0,
            limit,
        }
    }

    fn push(&mut self, value: T) -> Result<()> {
        let requested = self
            .len
            .checked_add(1)
            .ok_or_else(|| refusal(self.limit, usize::MAX, N))?;
        check(self.limit, requested, N)?;
        let slot = self.values.get_mut(self.len).ok_or_else(malformed)?;
        *slot = value;
        self.len = requested;
        Ok(())
    }

    fn pop(&mut self) -> Option<T> {
        let index = self.len.checked_sub(1)?;
        let value = self.values.get(index).copied()?;
        self.len = index;
        Some(value)
    }

    fn last(&self) -> Option<&T> {
        self.values.get(self.len.checked_sub(1)?)
    }

    fn last_mut(&mut self) -> Option<&mut T> {
        let index = self.len.checked_sub(1)?;
        self.values.get_mut(index)
    }
}

impl FixedStack<f64, MAX_OPERANDS> {
    fn number(&mut self, value: f64) -> Result<()> {
        if !value.is_finite() {
            return Err(malformed());
        }
        self.push(value)
    }

    fn take<const N: usize>(&mut self) -> Result<[f64; N]> {
        if self.len != N {
            return Err(malformed());
        }
        let mut result = [0.0; N];
        result.copy_from_slice(self.values.get(..N).ok_or_else(malformed)?);
        self.len = 0;
        Ok(result)
    }

    fn arguments(&mut self, count: usize) -> Result<[f64; MAX_OPERANDS]> {
        check(LimitKind::Tokens, count, MAX_OPERANDS)?;
        let start = self.len.checked_sub(count).ok_or_else(malformed)?;
        let mut result = [0.0; MAX_OPERANDS];
        result
            .get_mut(..count)
            .ok_or_else(malformed)?
            .copy_from_slice(self.values.get(start..self.len).ok_or_else(malformed)?);
        self.len = start;
        Ok(result)
    }
}

#[derive(Clone, Copy, Default)]
struct Frame {
    code: Span,
    position: usize,
}

impl Frame {
    fn new(code: Span) -> Self {
        Self {
            code,
            position: code.start,
        }
    }

    fn byte(&mut self, source: &[u8]) -> Result<u8> {
        if self.position >= self.code.end {
            return Err(malformed());
        }
        let byte = source.get(self.position).copied().ok_or_else(malformed)?;
        self.position = self.position.checked_add(1).ok_or_else(malformed)?;
        Ok(byte)
    }

    fn number(&mut self, first: u8, source: &[u8]) -> Result<f64> {
        let value = match first {
            32..=246 => i32::from(first).saturating_sub(139),
            247..=250 => {
                let second = self.byte(source)?;
                i32::from(first)
                    .saturating_sub(247)
                    .saturating_mul(256)
                    .saturating_add(i32::from(second))
                    .saturating_add(108)
            }
            251..=254 => {
                let second = self.byte(source)?;
                let positive = i32::from(first)
                    .saturating_sub(251)
                    .saturating_mul(256)
                    .saturating_add(i32::from(second))
                    .saturating_add(108);
                positive.checked_neg().ok_or_else(malformed)?
            }
            255 => {
                let a = self.byte(source)?;
                let b = self.byte(source)?;
                let c = self.byte(source)?;
                let d = self.byte(source)?;
                // Type 1 uses a signed integer here, not Type 2's 16.16 number.
                i32::from_be_bytes([a, b, c, d])
            }
            _ => return Err(malformed()),
        };
        Ok(f64::from(value))
    }
}

#[derive(Clone, Copy, Default)]
struct Component {
    code: Span,
    offset: Point,
    depth: usize,
}

struct Accent {
    base: Span,
    accent: Span,
    offset: Point,
}

struct Flex {
    points: [Point; 7],
    count: usize,
}

struct Builder<'a> {
    path: &'a mut Path,
    matrix: [f64; 6],
    offset: Point,
    point: Point,
    bearing: Point,
    width_set: bool,
    open: bool,
    flex: Option<Flex>,
}

impl Builder<'_> {
    fn metrics(&self) -> Result<()> {
        if self.width_set {
            Ok(())
        } else {
            Err(malformed())
        }
    }

    fn set_width(&mut self, bearing: Point) -> Result<()> {
        if self.width_set {
            return Err(malformed());
        }
        self.bearing = finite_point(bearing)?;
        self.point = bearing;
        self.width_set = true;
        Ok(())
    }

    fn mapped(&self, point: Point) -> Result<(f32, f32)> {
        let [x, y] = add_point(point, self.offset)?;
        let [a, b, c, d, e, f] = self.matrix;
        let x = (a * x + c * y + e) * 1000.0;
        let y = (b * x_coordinate(point, self.offset)? + d * y + f) * 1000.0;
        if !x.is_finite()
            || !y.is_finite()
            || x.abs() > f64::from(f32::MAX)
            || y.abs() > f64::from(f32::MAX)
        {
            return Err(malformed());
        }
        Ok((x as f32, y as f32))
    }

    fn emit(&mut self, element: PathEl) -> Result<()> {
        let requested = self
            .path
            .els
            .len()
            .checked_add(1)
            .ok_or_else(|| refusal(LimitKind::ContentOps, usize::MAX, MAX_PATH_ELEMENTS))?;
        check(LimitKind::ContentOps, requested, MAX_PATH_ELEMENTS)?;
        if self.path.els.len() == self.path.els.capacity() {
            let remaining = MAX_PATH_ELEMENTS
                .checked_sub(self.path.els.len())
                .ok_or_else(malformed)?;
            let growth = self.path.els.capacity().max(256).min(remaining);
            self.path
                .els
                .try_reserve_exact(growth)
                .map_err(|_| allocation(LimitKind::ContentOps, MAX_PATH_ELEMENTS))?;
        }
        self.path.els.push(element);
        Ok(())
    }

    fn ensure_open(&mut self) -> Result<()> {
        self.metrics()?;
        if !self.open {
            let (x, y) = self.mapped(self.point)?;
            self.emit(PathEl::MoveTo(x, y))?;
            self.open = true;
        }
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        if self.flex.is_some() {
            return Err(malformed());
        }
        if self.open {
            self.emit(PathEl::Close)?;
            self.open = false;
        }
        // Type 1 closepath preserves the charstring's current point.
        Ok(())
    }

    fn move_by(&mut self, delta: Point) -> Result<()> {
        self.metrics()?;
        let point = add_point(self.point, delta)?;
        if self.flex.is_none() {
            self.close()?;
            let (x, y) = self.mapped(point)?;
            self.emit(PathEl::MoveTo(x, y))?;
            self.open = true;
        }
        self.point = point;
        Ok(())
    }

    fn line_by(&mut self, delta: Point) -> Result<()> {
        if self.flex.is_some() {
            return Err(malformed());
        }
        self.ensure_open()?;
        let point = add_point(self.point, delta)?;
        let (x, y) = self.mapped(point)?;
        self.emit(PathEl::LineTo(x, y))?;
        self.point = point;
        Ok(())
    }

    fn curve_by(&mut self, deltas: [f64; 6]) -> Result<()> {
        let [dx1, dy1, dx2, dy2, dx3, dy3] = deltas;
        let first = add_point(self.point, [dx1, dy1])?;
        let second = add_point(first, [dx2, dy2])?;
        let last = add_point(second, [dx3, dy3])?;
        self.curve(first, second, last)
    }

    fn curve(&mut self, first: Point, second: Point, last: Point) -> Result<()> {
        if self.flex.is_some() {
            return Err(malformed());
        }
        self.ensure_open()?;
        let (x1, y1) = self.mapped(first)?;
        let (x2, y2) = self.mapped(second)?;
        let (x3, y3) = self.mapped(last)?;
        self.emit(PathEl::CubicTo(x1, y1, x2, y2, x3, y3))?;
        self.point = finite_point(last)?;
        Ok(())
    }

    fn start_flex(&mut self) -> Result<()> {
        if self.flex.is_some() {
            return Err(malformed());
        }
        self.ensure_open()?;
        self.flex = Some(Flex {
            points: [[0.0; 2]; 7],
            count: 0,
        });
        Ok(())
    }

    fn flex_point(&mut self) -> Result<()> {
        let flex = self.flex.as_mut().ok_or_else(malformed)?;
        let requested = flex.count.checked_add(1).ok_or_else(malformed)?;
        check(LimitKind::Tokens, requested, 7)?;
        let slot = flex.points.get_mut(flex.count).ok_or_else(malformed)?;
        *slot = self.point;
        flex.count = requested;
        Ok(())
    }

    fn end_flex(&mut self) -> Result<()> {
        let flex = self.flex.take().ok_or_else(malformed)?;
        if flex.count != 7 {
            return Err(malformed());
        }
        let [_, first, second, middle, fourth, fifth, last] = flex.points;
        // The first recorded point is the flex reference point. The remaining
        // six points describe two cubics. Device-dependent hinting is ignored.
        self.curve(first, second, middle)?;
        self.curve(fourth, fifth, last)
    }
}

fn x_coordinate(point: Point, offset: Point) -> Result<f64> {
    let [x, _] = add_point(point, offset)?;
    Ok(x)
}

impl FontProgram {
    fn glyph(&self, name: &str) -> Result<Span> {
        let index = self
            .glyphs
            .binary_search_by(|glyph| glyph.name.as_str().cmp(name))
            .map_err(|_| malformed())?;
        self.glyphs
            .get(index)
            .map(|glyph| glyph.code)
            .ok_or_else(malformed)
    }

    fn standard_glyph(&self, code: f64) -> Result<Span> {
        let code = u8::try_from(integer(code)?).map_err(|_| malformed())?;
        let name = BaseEncoding::Standard
            .glyph_name(code)
            .ok_or_else(malformed)?;
        self.glyph(name)
    }

    fn outline(&self, name: &str) -> Result<Path> {
        let code = self.glyph(name)?;
        let mut path = Path::default();
        let mut pending = FixedStack::<Component, { MAX_DEPTH + 1 }>::new(LimitKind::Depth);
        pending.push(Component {
            code,
            offset: [0.0, 0.0],
            depth: 0,
        })?;
        let mut operations = 0usize;

        while let Some(component) = pending.pop() {
            check(LimitKind::Depth, component.depth, MAX_DEPTH)?;
            let mut builder = Builder {
                path: &mut path,
                matrix: self.matrix,
                offset: component.offset,
                point: [0.0, 0.0],
                bearing: [0.0, 0.0],
                width_set: false,
                open: false,
                flex: None,
            };
            if let Some(accent) = self.run(component, &mut builder, &mut operations)? {
                let depth = component.depth.checked_add(1).ok_or_else(malformed)?;
                check(LimitKind::Depth, depth, MAX_DEPTH)?;
                // LIFO order paints the base before the accent.
                pending.push(Component {
                    code: accent.accent,
                    offset: add_point(component.offset, accent.offset)?,
                    depth,
                })?;
                pending.push(Component {
                    code: accent.base,
                    offset: component.offset,
                    depth,
                })?;
            }
        }
        Ok(path)
    }

    fn run(
        &self,
        component: Component,
        builder: &mut Builder<'_>,
        operations: &mut usize,
    ) -> Result<Option<Accent>> {
        let mut frames = FixedStack::<Frame, { MAX_DEPTH + 1 }>::new(LimitKind::Depth);
        frames.push(Frame::new(component.code))?;
        let mut stack = FixedStack::<f64, MAX_OPERANDS>::new(LimitKind::Tokens);
        let mut results = FixedStack::<f64, 2>::new(LimitKind::Tokens);

        loop {
            *operations = operations
                .checked_add(1)
                .ok_or_else(|| refusal(LimitKind::ContentOps, usize::MAX, MAX_OPS))?;
            check(LimitKind::ContentOps, *operations, MAX_OPS)?;
            let frame = frames.last_mut().ok_or_else(malformed)?;
            let first = frame.byte(&self.bytes)?;
            if first >= 32 {
                stack.number(frame.number(first, &self.bytes)?)?;
                continue;
            }

            match first {
                1 | 3 => {
                    builder.metrics()?;
                    stack.take::<2>()?;
                }
                4 => {
                    let [dy] = stack.take::<1>()?;
                    builder.move_by([0.0, dy])?;
                }
                5 => {
                    let [dx, dy] = stack.take::<2>()?;
                    builder.line_by([dx, dy])?;
                }
                6 => {
                    let [dx] = stack.take::<1>()?;
                    builder.line_by([dx, 0.0])?;
                }
                7 => {
                    let [dy] = stack.take::<1>()?;
                    builder.line_by([0.0, dy])?;
                }
                8 => builder.curve_by(stack.take::<6>()?)?,
                9 => {
                    builder.metrics()?;
                    stack.take::<0>()?;
                    builder.close()?;
                }
                10 => {
                    let index = integer(stack.pop().ok_or_else(malformed)?)?;
                    let code = self
                        .subrs
                        .get(index)
                        .copied()
                        .flatten()
                        .ok_or_else(malformed)?;
                    let depth = component
                        .depth
                        .checked_add(frames.len)
                        .ok_or_else(malformed)?;
                    check(LimitKind::Depth, depth, MAX_DEPTH)?;
                    frames.push(Frame::new(code))?;
                }
                11 => {
                    if frames.len <= 1 {
                        return Err(malformed());
                    }
                    frames.pop();
                }
                12 => {
                    let escape = frames.last_mut().ok_or_else(malformed)?.byte(&self.bytes)?;
                    match escape {
                        0 => {
                            builder.metrics()?;
                            stack.take::<0>()?;
                        }
                        1 | 2 => {
                            builder.metrics()?;
                            stack.take::<6>()?;
                        }
                        6 => {
                            builder.metrics()?;
                            if builder.flex.is_some() || results.len != 0 {
                                return Err(malformed());
                            }
                            let [asb, adx, ady, base, accent] = stack.take::<5>()?;
                            let base = self.standard_glyph(base)?;
                            let accent = self.standard_glyph(accent)?;
                            let [sbx, sby] = builder.bearing;
                            let offset = finite_point([sbx + adx - asb, sby + ady])?;
                            builder.close()?;
                            return Ok(Some(Accent {
                                base,
                                accent,
                                offset,
                            }));
                        }
                        7 => {
                            let [sbx, sby, _, _] = stack.take::<4>()?;
                            builder.set_width([sbx, sby])?;
                        }
                        12 => {
                            let denominator = stack.pop().ok_or_else(malformed)?;
                            let numerator = stack.pop().ok_or_else(malformed)?;
                            if denominator == 0.0 {
                                return Err(malformed());
                            }
                            stack.number(numerator / denominator)?;
                        }
                        16 => {
                            othersubr(builder, &mut stack, &mut results)?;
                        }
                        17 => {
                            stack.number(results.pop().ok_or_else(malformed)?)?;
                        }
                        33 => {
                            builder.metrics()?;
                            if builder.flex.is_some() {
                                return Err(malformed());
                            }
                            let [x, y] = stack.take::<2>()?;
                            builder.point = finite_point([x, y])?;
                        }
                        _ => return Err(malformed()),
                    }
                }
                13 => {
                    let [sbx, _] = stack.take::<2>()?;
                    builder.set_width([sbx, 0.0])?;
                }
                14 => {
                    builder.metrics()?;
                    stack.take::<0>()?;
                    if results.len != 0 {
                        return Err(malformed());
                    }
                    builder.close()?;
                    return Ok(None);
                }
                21 => {
                    let [dx, dy] = stack.take::<2>()?;
                    builder.move_by([dx, dy])?;
                }
                22 => {
                    let [dx] = stack.take::<1>()?;
                    builder.move_by([dx, 0.0])?;
                }
                30 => {
                    let [dy1, dx2, dy2, dx3] = stack.take::<4>()?;
                    builder.curve_by([0.0, dy1, dx2, dy2, dx3, 0.0])?;
                }
                31 => {
                    let [dx1, dx2, dy2, dy3] = stack.take::<4>()?;
                    builder.curve_by([dx1, 0.0, dx2, dy2, 0.0, dy3])?;
                }
                _ => return Err(malformed()),
            }
        }
    }
}

fn othersubr(
    builder: &mut Builder<'_>,
    stack: &mut FixedStack<f64, MAX_OPERANDS>,
    results: &mut FixedStack<f64, 2>,
) -> Result<()> {
    let subroutine = integer(stack.pop().ok_or_else(malformed)?)?;
    let count = integer(stack.pop().ok_or_else(malformed)?)?;
    let arguments = stack.arguments(count)?;
    if results.len != 0 {
        return Err(malformed());
    }
    match subroutine {
        0 if count == 3 => {
            let x = arguments.get(1).copied().ok_or_else(malformed)?;
            let y = arguments.get(2).copied().ok_or_else(malformed)?;
            builder.end_flex()?;
            // pop transfers x first, then y, to the charstring stack.
            results.push(y)?;
            results.push(x)?;
        }
        1 if count == 0 => builder.start_flex()?,
        2 if count == 0 => builder.flex_point()?,
        3 if count == 1 => {
            // Hint replacement returns the subroutine index for the following
            // pop/callsubr sequence. Its hint definitions are otherwise ignored.
            results.push(arguments.first().copied().ok_or_else(malformed)?)?;
        }
        0..=3 => return Err(malformed()),
        _ => {
            return Err(DocError::Unsupported {
                feature: "Type1 OtherSubrs",
            });
        }
    }
    Ok(())
}
