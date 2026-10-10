use alloc::vec::Vec;
use core::cmp::Ordering;

use pdf_core::lex::{LexError, Lexer, LexerLimits, Token, TokenKind};

use crate::font::GlyphText;

const MAX_ENTRIES: u32 = 100_000;
const MAX_CODESPACES: usize = 256;

#[derive(Clone, Copy, Debug)]
struct CodeSpace {
    start: u32,
    end: u32,
    len: u8,
}

impl CodeSpace {
    fn contains(self, code: u32, len: u8) -> bool {
        if len != self.len || code < self.start || code > self.end {
            return false;
        }
        // PDF codespaces bound each byte independently.
        for index in 0..self.len {
            let shift = u32::from(self.len.saturating_sub(index).saturating_sub(1))
                .saturating_mul(8);
            let byte = code.checked_shr(shift).unwrap_or(0) & 255;
            let low = self.start.checked_shr(shift).unwrap_or(0) & 255;
            let high = self.end.checked_shr(shift).unwrap_or(0) & 255;
            if byte < low || byte > high {
                return false;
            }
        }
        true
    }
}

#[derive(Clone, Copy, Debug)]
struct HexText {
    bytes: [u8; 16],
    len: u8,
}

impl HexText {
    fn unicode(self, offset: u32) -> Option<GlyphText> {
        let mut bytes = self.bytes;
        let bytes = bytes.get_mut(..usize::from(self.len))?;
        let mut carry = u64::from(offset);
        for byte in bytes.iter_mut().rev() {
            let sum = u64::from(*byte).checked_add(carry)?;
            *byte = (sum & 255) as u8;
            carry = sum >> 8;
        }
        if carry != 0 || bytes.is_empty() || bytes.len() % 2 != 0 {
            return None;
        }
        let units = bytes.chunks_exact(2).map(|pair| {
            let high = pair.first().copied().unwrap_or(0);
            let low = pair.get(1).copied().unwrap_or(0);
            (u16::from(high) << 8) | u16::from(low)
        });
        let mut text = GlyphText::new();
        for scalar in core::char::decode_utf16(units) {
            if !text.push(scalar.ok()?) {
                return None;
            }
        }
        Some(text)
    }
}

#[derive(Clone, Copy, Debug)]
struct UnicodeRange {
    start: u32,
    end: u32,
    destination: HexText,
}

#[derive(Clone, Copy, Debug)]
struct CidRange {
    start: u32,
    end: u32,
    destination: u32,
}

#[derive(Clone, Debug, Default)]
pub struct CMap {
    spaces: Vec<CodeSpace>,
    unicode_ranges: Vec<UnicodeRange>,
    cid_ranges: Vec<CidRange>,
    identity_cid: bool,
    vertical: bool,
}

impl CMap {
    pub fn parse(bytes: &[u8], max_entries: u32) -> (Self, Option<&'static str>) {
        parse(bytes, max_entries)
    }

    pub fn identity(two_byte: bool, vertical: bool) -> Self {
        let mut map = Self::default();
        map.spaces.push(CodeSpace {
            start: 0,
            end: if two_byte { 65_535 } else { 255 },
            len: if two_byte { 2 } else { 1 },
        });
        map.identity_cid = true;
        map.vertical = vertical;
        map
    }

    pub fn is_vertical(&self) -> bool {
        self.vertical
    }

    pub fn next_code(&self, bytes: &[u8]) -> (u32, u8) {
        if bytes.is_empty() {
            return (0, 0);
        }
        let mut code = 0u32;
        for len in 1u8..=4 {
            let Some(byte) = bytes.get(usize::from(len.saturating_sub(1))) else {
                break;
            };
            code = (code << 8) | u32::from(*byte);
            if self.accepts(code, len) {
                return (code, len);
            }
        }

        let shortest = self.spaces.first().map(|space| space.len).unwrap_or(1);
        let consumed = usize::from(shortest).min(bytes.len());
        let mut code = 0u32;
        for byte in bytes.iter().take(consumed) {
            code = (code << 8) | u32::from(*byte);
        }
        (code, consumed as u8)
    }

    pub fn unicode(&self, code: u32) -> Option<GlyphText> {
        let index = self.unicode_ranges.binary_search_by(|range| {
            compare_range(range.start, range.end, code)
        }).ok()?;
        let range = self.unicode_ranges.get(index)?;
        range.destination.unicode(code.checked_sub(range.start)?)
    }

    pub fn cid(&self, code: u32) -> u32 {
        if let Ok(index) = self.cid_ranges.binary_search_by(|range| {
            compare_range(range.start, range.end, code)
        }) {
            if let Some(range) = self.cid_ranges.get(index) {
                return code.checked_sub(range.start)
                    .and_then(|offset| range.destination.checked_add(offset))
                    .unwrap_or(0);
            }
        }
        if self.identity_cid { code } else { 0 }
    }

    pub(crate) fn accepts(&self, code: u32, len: u8) -> bool {
        let start = self.spaces.partition_point(|space| space.len < len);
        let end = self.spaces.partition_point(|space| space.len <= len);
        self.spaces.get(start..end)
            .map(|spaces| spaces.iter().any(|space| space.contains(code, len)))
            .unwrap_or(false)
    }
}

pub fn identity(two_byte: bool, vertical: bool) -> CMap {
    CMap::identity(two_byte, vertical)
}

/// Parses the supported CMap operators and preserves completed entries on error.
/// Sequential bfrange and cidrange mappings remain compact.
pub fn parse(bytes: &[u8], max_entries: u32) -> (CMap, Option<&'static str>) {
    let maximum = max_entries.min(MAX_ENTRIES);
    let token_limit = maximum.saturating_mul(12).saturating_add(4096) as usize;
    let mut parser = Parser {
        reader: Reader {
            lexer: Lexer::with_limits(bytes, LexerLimits::new(token_limit, 4096, 16)),
        },
        map: CMap::default(),
        maximum,
        entries: 0,
        inferred_lengths: 0,
        warning: None,
    };
    if let Err(warning) = parser.read() {
        parser.warn(warning);
    }
    parser.finish()
}

struct Reader<'a> {
    lexer: Lexer<'a>,
}

impl<'a> Reader<'a> {
    fn next(&mut self) -> Result<Option<Token<'a>>, &'static str> {
        match self.lexer.next() {
            Some(Ok(token)) => Ok(Some(token)),
            Some(Err(
                LexError::TokenBudgetExceeded { .. }
                | LexError::TokenTooLong { .. }
                | LexError::LiteralNestingTooDeep { .. },
            )) => Err("cmap_limit"),
            Some(Err(_)) => Err("cmap_malformed"),
            None => Ok(None),
        }
    }

    fn required(&mut self) -> Result<Token<'a>, &'static str> {
        self.next()?.ok_or("cmap_malformed")
    }
}

struct Parser<'a> {
    reader: Reader<'a>,
    map: CMap,
    maximum: u32,
    entries: u32,
    inferred_lengths: u8,
    warning: Option<&'static str>,
}

impl Parser<'_> {
    fn warn(&mut self, warning: &'static str) {
        if self.warning.is_none() {
            self.warning = Some(warning);
        }
    }

    fn charge(&mut self) -> Result<(), &'static str> {
        if self.entries >= self.maximum {
            return Err("cmap_limit");
        }
        self.entries = self.entries.saturating_add(1);
        Ok(())
    }

    fn read(&mut self) -> Result<(), &'static str> {
        let mut previous = None;
        while let Some(token) = self.reader.next()? {
            if previous.is_some_and(|name| name_eq(name, b"WMode")) {
                match integer(token) {
                    Some(0) => self.map.vertical = false,
                    Some(1) => self.map.vertical = true,
                    _ => self.warn("cmap_malformed"),
                }
            }

            if token.kind() == TokenKind::Keyword {
                let section = match token.bytes() {
                    b"begincodespacerange" => Some(Section::CodeSpace),
                    b"beginbfchar" => Some(Section::BfChar),
                    b"beginbfrange" => Some(Section::BfRange),
                    b"begincidchar" => Some(Section::CidChar),
                    b"begincidrange" => Some(Section::CidRange),
                    _ => None,
                };
                if let Some(section) = section {
                    let count = previous.and_then(integer).ok_or("cmap_malformed")?;
                    self.section(section, count)?;
                    previous = None;
                    continue;
                }
                if token.bytes() == b"usecmap" {
                    let name = previous.ok_or("cmap_malformed")?;
                    if name.kind() != TokenKind::Name {
                        return Err("cmap_malformed");
                    }
                    self.map.identity_cid = true;
                    if name_eq(name, b"Identity-H") {
                        self.map.vertical = false;
                    } else if name_eq(name, b"Identity-V") {
                        self.map.vertical = true;
                    } else {
                        self.warn("predefined_cmap_unsupported");
                        self.map.vertical = name.bytes().ends_with(b"-V");
                    }
                }
            }
            previous = Some(token);
        }
        Ok(())
    }

    fn source(&mut self) -> Result<(u32, u8), &'static str> {
        let token = self.reader.required()?;
        let value = hex_string(token)?;
        if !(1..=4).contains(&value.len) {
            return Err("cmap_malformed");
        }
        let mut code = 0u32;
        for byte in value.bytes.get(..usize::from(value.len)).ok_or("cmap_malformed")? {
            code = (code << 8) | u32::from(*byte);
        }
        self.inferred_lengths |= 1u8 << value.len.saturating_sub(1);
        Ok((code, value.len))
    }

    fn endpoints(&mut self) -> Result<(u32, u32, u8), &'static str> {
        let (start, len) = self.source()?;
        let (end, end_len) = self.source()?;
        if end_len != len || end < start {
            return Err("cmap_malformed");
        }
        Ok((start, end, len))
    }

    fn destination(&mut self) -> Result<HexText, &'static str> {
        let destination = hex_string(self.reader.required()?)?;
        if destination.unicode(0).is_none() {
            return Err("cmap_malformed");
        }
        Ok(destination)
    }

    fn section(&mut self, section: Section, count: u32) -> Result<(), &'static str> {
        for _ in 0..count {
            match section {
                Section::CodeSpace => {
                    self.charge()?;
                    if self.map.spaces.len() >= MAX_CODESPACES {
                        return Err("cmap_limit");
                    }
                    let (start, end, len) = self.endpoints()?;
                    let space = CodeSpace { start, end, len };
                    if !space.contains(start, len) || !space.contains(end, len) {
                        return Err("cmap_malformed");
                    }
                    self.map.spaces.push(space);
                }
                Section::BfChar => {
                    self.charge()?;
                    let (code, _) = self.source()?;
                    let destination = self.destination()?;
                    self.map.unicode_ranges.push(UnicodeRange {
                        start: code,
                        end: code,
                        destination,
                    });
                }
                Section::BfRange => {
                    let (start, end, _) = self.endpoints()?;
                    let destination = self.reader.required()?;
                    if destination.kind() == TokenKind::ArrayStart {
                        let count = u64::from(end)
                            .saturating_sub(u64::from(start))
                            .saturating_add(1);
                        for offset in 0..count {
                            self.charge()?;
                            let destination = self.destination()?;
                            let code = u64::from(start).checked_add(offset)
                                .and_then(|code| u32::try_from(code).ok())
                                .ok_or("cmap_malformed")?;
                            self.map.unicode_ranges.push(UnicodeRange {
                                start: code,
                                end: code,
                                destination,
                            });
                        }
                        if self.reader.required()?.kind() != TokenKind::ArrayEnd {
                            return Err("cmap_malformed");
                        }
                    } else {
                        self.charge()?;
                        let destination = hex_string(destination)?;
                        if destination.unicode(0).is_none() {
                            return Err("cmap_malformed");
                        }
                        if destination.unicode(end.saturating_sub(start)).is_none() {
                            self.warn("cmap_malformed");
                        }
                        self.map.unicode_ranges.push(UnicodeRange {
                            start,
                            end,
                            destination,
                        });
                    }
                }
                Section::CidChar => {
                    self.charge()?;
                    let (code, _) = self.source()?;
                    let destination = integer(self.reader.required()?)
                        .ok_or("cmap_malformed")?;
                    self.map.cid_ranges.push(CidRange {
                        start: code,
                        end: code,
                        destination,
                    });
                }
                Section::CidRange => {
                    self.charge()?;
                    let (start, end, _) = self.endpoints()?;
                    let destination = integer(self.reader.required()?)
                        .ok_or("cmap_malformed")?;
                    if destination.checked_add(end.saturating_sub(start)).is_none() {
                        return Err("cmap_malformed");
                    }
                    self.map.cid_ranges.push(CidRange { start, end, destination });
                }
            }
        }
        if self.reader.required()?.bytes() != section.end() {
            return Err("cmap_malformed");
        }
        Ok(())
    }

    fn finish(mut self) -> (CMap, Option<&'static str>) {
        if self.map.spaces.is_empty() {
            if self.map.identity_cid {
                self.map.spaces.push(CodeSpace { start: 0, end: 65_535, len: 2 });
            } else {
                self.warn("cmap_codespace_missing");
                for len in 1u8..=4 {
                    if self.inferred_lengths & (1u8 << len.saturating_sub(1)) != 0 {
                        let end = if len == 4 {
                            u32::MAX
                        } else {
                            1u32.checked_shl(u32::from(len).saturating_mul(8))
                                .unwrap_or(0).saturating_sub(1)
                        };
                        self.map.spaces.push(CodeSpace { start: 0, end, len });
                    }
                }
                if self.map.spaces.is_empty() {
                    self.map.spaces.push(CodeSpace { start: 0, end: 255, len: 1 });
                }
            }
        }
        self.map.spaces.sort_unstable_by_key(|space| (space.len, space.start, space.end));
        self.map.spaces.dedup_by_key(|space| (space.len, space.start, space.end));

        // Overlapping mappings are malformed. Keep the first sorted range,
        // dropping overlaps so every lookup remains a binary search.
        self.map.unicode_ranges.sort_by_key(|range| (range.start, range.end));
        let mut last = None;
        let mut overlap = false;
        self.map.unicode_ranges.retain(|range| {
            if last.is_some_and(|end| range.start <= end) {
                overlap = true;
                false
            } else {
                last = Some(range.end);
                true
            }
        });
        self.map.cid_ranges.sort_by_key(|range| (range.start, range.end));
        last = None;
        self.map.cid_ranges.retain(|range| {
            if last.is_some_and(|end| range.start <= end) {
                overlap = true;
                false
            } else {
                last = Some(range.end);
                true
            }
        });
        if overlap {
            self.warn("cmap_overlap");
        }
        (self.map, self.warning)
    }
}

#[derive(Clone, Copy)]
enum Section {
    CodeSpace,
    BfChar,
    BfRange,
    CidChar,
    CidRange,
}

impl Section {
    fn end(self) -> &'static [u8] {
        match self {
            Self::CodeSpace => b"endcodespacerange",
            Self::BfChar => b"endbfchar",
            Self::BfRange => b"endbfrange",
            Self::CidChar => b"endcidchar",
            Self::CidRange => b"endcidrange",
        }
    }
}

fn compare_range(start: u32, end: u32, code: u32) -> Ordering {
    if end < code {
        Ordering::Less
    } else if start > code {
        Ordering::Greater
    } else {
        Ordering::Equal
    }
}

fn integer(token: Token<'_>) -> Option<u32> {
    if token.kind() != TokenKind::Number {
        return None;
    }
    core::str::from_utf8(token.bytes()).ok()?.parse().ok()
}

fn hex_string(token: Token<'_>) -> Result<HexText, &'static str> {
    if token.kind() != TokenKind::HexString {
        return Err("cmap_malformed");
    }
    let input = token.bytes()
        .get(1..token.bytes().len().saturating_sub(1))
        .ok_or("cmap_malformed")?;
    let mut value = HexText { bytes: [0; 16], len: 0 };
    let mut high = None;
    for byte in input {
        if matches!(byte, 0 | 9 | 10 | 12 | 13 | 32) {
            continue;
        }
        let digit = hex(*byte).ok_or("cmap_malformed")?;
        if let Some(first) = high.take() {
            let slot = value.bytes.get_mut(usize::from(value.len))
                .ok_or("cmap_text_too_long")?;
            *slot = (first << 4) | digit;
            value.len = value.len.saturating_add(1);
        } else {
            high = Some(digit);
        }
    }
    if let Some(first) = high {
        let slot = value.bytes.get_mut(usize::from(value.len))
            .ok_or("cmap_text_too_long")?;
        *slot = first << 4;
        value.len = value.len.saturating_add(1);
    }
    Ok(value)
}

fn name_eq(token: Token<'_>, expected: &[u8]) -> bool {
    if token.kind() != TokenKind::Name {
        return false;
    }
    let mut bytes = token.bytes().get(1..).unwrap_or(&[]);
    let mut expected = expected.iter();
    while let Some((&first, rest)) = bytes.split_first() {
        let byte;
        if first == b'#' {
            let Some(high) = rest.first().copied().and_then(hex) else { return false };
            let Some(low) = rest.get(1).copied().and_then(hex) else { return false };
            byte = (high << 4) | low;
            bytes = rest.get(2..).unwrap_or(&[]);
        } else {
            byte = first;
            bytes = rest;
        }
        if expected.next().copied() != Some(byte) {
            return false;
        }
    }
    expected.next().is_none()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

