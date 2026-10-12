use alloc::vec::Vec;

use pdf_core::lex::{LexError, Lexer, LexerLimits, Token, TokenKind};
use pdf_core::{DocError, LimitKind, Limits, Refusal};

const MAX_OPERANDS: usize = 64;
const INLINE_SCAN_BYTES: u64 = 16 << 20;

/// Names exclude the slash but retain PDF name escapes. Strings and containers
/// retain their delimiters and borrow the decoded content stream.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Operand<'a> {
    Number(f64),
    Name(&'a [u8]),
    String(&'a [u8]),
    Array(&'a [u8]),
    Dict(&'a [u8]),
    Bool(bool),
    Null,
}

/// A lending operator: the operand slice is reused by the next call.
#[derive(Clone, Copy, Debug)]
pub struct Op<'input, 'operands> {
    pub name: &'input [u8],
    pub operands: &'operands [Operand<'input>],
}

#[derive(Clone, Copy, Debug)]
struct InlineData {
    start: usize,
    length: Option<usize>,
}

/// Owned continuation state, suitable for an explicit Form invocation stack.
#[derive(Clone, Copy, Debug, Default)]
pub struct ContentCursor {
    offset: usize,
    tokens: u64,
    inline_header: bool,
    inline_data: Option<InlineData>,
}

enum Item<'a> {
    Value(Operand<'a>),
    Operator(&'a [u8]),
}

struct Scan<'a> {
    input: &'a [u8],
    lexer: Lexer<'a>,
    base: usize,
    tokens: u64,
    limits: Limits,
    containers: Vec<TokenKind>,
    junk: u64,
}

impl<'a> Scan<'a> {
    fn new(input: &'a [u8], limits: Limits, cursor: ContentCursor) -> Self {
        let base = cursor.offset.min(input.len());
        let lexer = Lexer::with_limits(
            input.get(base..).unwrap_or(&[]),
            lexer_limits(limits, cursor.tokens),
        );
        Self {
            input,
            lexer,
            base,
            tokens: cursor.tokens,
            limits,
            containers: Vec::new(),
            junk: 0,
        }
    }

    fn position(&self) -> usize {
        self.base.saturating_add(self.lexer.offset())
    }

    fn seek(&mut self, offset: usize) {
        self.base = offset.min(self.input.len());
        self.lexer = Lexer::with_limits(
            self.input.get(self.base..).unwrap_or(&[]),
            lexer_limits(self.limits, self.tokens),
        );
    }

    fn token(&mut self) -> Result<Option<(Token<'a>, usize, usize)>, DocError> {
        loop {
            match self.lexer.next() {
                Some(Ok(token)) => {
                    self.tokens = self.tokens.saturating_add(1);
                    let maximum = token_limit(self.limits);
                    check(LimitKind::Tokens, self.tokens, maximum)?;
                    let start = self.base.saturating_add(token.span().start());
                    let end = self.base.saturating_add(token.span().end());
                    return Ok(Some((token, start, end)));
                }
                Some(Err(error)) => {
                    self.tokens = self.tokens.saturating_add(1);
                    check(LimitKind::Tokens, self.tokens, token_limit(self.limits))?;
                    let offset = match error {
                        LexError::TokenBudgetExceeded { .. } => {
                            return Err(refusal(
                                LimitKind::Tokens,
                                token_limit(self.limits).saturating_add(1),
                                token_limit(self.limits),
                            ));
                        }
                        LexError::TokenTooLong { .. } => {
                            return Err(refusal(
                                LimitKind::ObjectBytes,
                                self.limits.max_object_bytes.saturating_add(1),
                                self.limits.max_object_bytes,
                            ));
                        }
                        LexError::LiteralNestingTooDeep { .. } => {
                            let maximum = u64::from(self.limits.max_depth);
                            return Err(refusal(
                                LimitKind::Depth,
                                maximum.saturating_add(1),
                                maximum,
                            ));
                        }
                        LexError::InvalidDelimiter { offset }
                        | LexError::InvalidNameEscape { offset }
                        | LexError::InvalidHexDigit { offset } => offset,
                        _ => {
                            return Err(DocError::Malformed {
                                offset: self.position() as u64,
                                what: "content_token",
                            });
                        }
                    };
                    self.junk = self.junk.saturating_add(1);
                    let next = self.base.saturating_add(offset).saturating_add(1);
                    self.seek(next);
                }
                None => return Ok(None),
            }
        }
    }

    fn item(&mut self) -> Result<Option<Item<'a>>, DocError> {
        loop {
            let Some((token, start, _)) = self.token()? else {
                return Ok(None);
            };
            let value = match token.kind() {
                TokenKind::Number => {
                    let number = parse_number(token.bytes()).filter(|number| number.is_finite());
                    let Some(number) = number else {
                        self.junk = self.junk.saturating_add(1);
                        continue;
                    };
                    Operand::Number(number)
                }
                TokenKind::Name => Operand::Name(token.bytes().get(1..).unwrap_or(&[])),
                TokenKind::LiteralString | TokenKind::HexString => {
                    Operand::String(token.bytes())
                }
                TokenKind::ArrayStart | TokenKind::DictionaryStart => {
                    self.containers.clear();
                    self.open(token.kind())?;
                    let end = loop {
                        let Some((next, _, end)) = self.token()? else {
                            return Err(DocError::Malformed {
                                offset: start as u64,
                                what: "content_container",
                            });
                        };
                        match next.kind() {
                            TokenKind::ArrayStart | TokenKind::DictionaryStart => {
                                self.open(next.kind())?;
                            }
                            TokenKind::ArrayEnd | TokenKind::DictionaryEnd => {
                                let matching = matches!(
                                    (self.containers.pop(), next.kind()),
                                    (Some(TokenKind::ArrayStart), TokenKind::ArrayEnd)
                                        | (
                                            Some(TokenKind::DictionaryStart),
                                            TokenKind::DictionaryEnd
                                        )
                                );
                                if !matching {
                                    return Err(DocError::Malformed {
                                        offset: start as u64,
                                        what: "content_container",
                                    });
                                }
                                if self.containers.is_empty() {
                                    break end;
                                }
                            }
                            _ => {}
                        }
                    };
                    let raw = self.input.get(start..end).unwrap_or(&[]);
                    if token.kind() == TokenKind::ArrayStart {
                        Operand::Array(raw)
                    } else {
                        Operand::Dict(raw)
                    }
                }
                TokenKind::ArrayEnd | TokenKind::DictionaryEnd => {
                    self.junk = self.junk.saturating_add(1);
                    continue;
                }
                TokenKind::Keyword => match token.bytes() {
                    b"true" => Operand::Bool(true),
                    b"false" => Operand::Bool(false),
                    b"null" => Operand::Null,
                    name => return Ok(Some(Item::Operator(name))),
                },
            };
            return Ok(Some(Item::Value(value)));
        }
    }

    fn open(&mut self, kind: TokenKind) -> Result<(), DocError> {
        let requested = self.containers.len().saturating_add(1) as u64;
        let maximum = u64::from(self.limits.max_depth);
        check(LimitKind::Depth, requested, maximum)?;
        self.containers.try_reserve(1).map_err(|_| {
            refusal(LimitKind::Depth, maximum.saturating_add(1), maximum)
        })?;
        self.containers.push(kind);
        Ok(())
    }
}

/// A bounded content stream operator reader. This uses a lending `next_op`
/// instead of Iterator so its small operand Vec can be reused.
pub struct Content<'a> {
    scan: Scan<'a>,
    operands: Vec<Operand<'a>>,
    inline_header: bool,
    inline_data: Option<InlineData>,
    dropped: u64,
}

impl<'a> Content<'a> {
    pub fn new(input: &'a [u8], limits: &Limits) -> Result<Self, DocError> {
        Self::resume(input, limits, ContentCursor::default())
    }

    pub fn resume(
        input: &'a [u8],
        limits: &Limits,
        cursor: ContentCursor,
    ) -> Result<Self, DocError> {
        let mut operands = Vec::new();
        operands.try_reserve_exact(MAX_OPERANDS).map_err(|_| {
            refusal(LimitKind::Tokens, MAX_OPERANDS as u64, token_limit(*limits))
        })?;
        Ok(Self {
            scan: Scan::new(input, *limits, cursor),
            operands,
            inline_header: cursor.inline_header,
            inline_data: cursor.inline_data,
            dropped: 0,
        })
    }

    pub fn cursor(&self) -> ContentCursor {
        ContentCursor {
            offset: self.scan.position(),
            tokens: self.scan.tokens,
            inline_header: self.inline_header,
            inline_data: self.inline_data,
        }
    }

    /// Reads an unfiltered inline payload with a caller-computed sample length.
    /// The exact length also prevents binary sample bytes from being mistaken
    /// for an EI marker by the following next_op call.
    pub fn inline_bytes(&mut self, length: usize) -> Result<&'a [u8], DocError> {
        check(
            LimitKind::DecodedBytes,
            length as u64,
            self.scan.limits.max_decoded_bytes,
        )?;
        let data = self.inline_data.ok_or(DocError::Malformed {
            offset: self.scan.position() as u64,
            what: "inline_image_state",
        })?;
        if data.length.is_some_and(|declared| declared != length) {
            return Err(DocError::Malformed {
                offset: data.start as u64,
                what: "inline_image_length",
            });
        }
        let end = data.start.checked_add(length).ok_or_else(|| {
            refusal(
                LimitKind::DecodedBytes,
                u64::MAX,
                self.scan.limits.max_decoded_bytes,
            )
        })?;
        let bytes = self.scan.input.get(data.start..end).ok_or(
            DocError::Malformed {
                offset: data.start as u64,
                what: "inline_image_length",
            },
        )?;
        let mut marker = end;
        let cap = end.saturating_add(1024).min(self.scan.input.len());
        while marker < cap
            && self.scan.input.get(marker).copied().map(whitespace).unwrap_or(false)
        {
            marker = marker.saturating_add(1);
        }
        let after = marker.checked_add(2).ok_or(DocError::Malformed {
            offset: marker as u64,
            what: "inline_image_end",
        })?;
        if self.scan.input.get(marker..after) != Some(b"EI")
            || !self.scan.input.get(after).copied().map(whitespace).unwrap_or(true)
        {
            return Err(DocError::Malformed {
                offset: marker as u64,
                what: "inline_image_end",
            });
        }
        self.inline_data = Some(InlineData { start: data.start, length: Some(length) });
        Ok(bytes)
    }

    pub fn dropped_operands(&self) -> u64 {
        self.dropped
    }

    pub fn junk_tokens(&self) -> u64 {
        self.scan.junk
    }

    pub fn next_op(&mut self) -> Result<Option<Op<'a, '_>>, DocError> {
        self.operands.clear();
        if let Some(data) = self.inline_data.take() {
            self.skip_inline(data)?;
            return Ok(Some(Op { name: b"EI", operands: &self.operands }));
        }

        loop {
            let Some(item) = self.scan.item()? else {
                if self.inline_header {
                    return Err(DocError::Malformed {
                        offset: self.scan.position() as u64,
                        what: "inline_image_header",
                    });
                }
                return Ok(None);
            };
            match item {
                Item::Value(value) => {
                    if self.operands.len() < MAX_OPERANDS {
                        self.operands.push(value);
                    } else {
                        self.dropped = self.dropped.saturating_add(1);
                    }
                }
                Item::Operator(name) => {
                    if self.inline_header {
                        if name != b"ID" {
                            self.scan.junk = self.scan.junk.saturating_add(1);
                            continue;
                        }
                        self.inline_header = false;
                        let mut start = self.scan.position();
                        match self.scan.input.get(start).copied() {
                            Some(b'\r') => {
                                start = start.saturating_add(1);
                                if self.scan.input.get(start) == Some(&b'\n') {
                                    start = start.saturating_add(1);
                                }
                            }
                            Some(byte) if whitespace(byte) => {
                                start = start.saturating_add(1);
                            }
                            _ => {
                                return Err(DocError::Malformed {
                                    offset: start as u64,
                                    what: "inline_image_separator",
                                });
                            }
                        }
                        let length = inline_number(&self.operands, b"L", b"Length")
                            .filter(|number| *number >= 0.0 && *number < usize::MAX as f64)
                            .map(|number| number as usize);
                        self.inline_data = Some(InlineData { start, length });
                    } else if name == b"BI" {
                        self.inline_header = true;
                    }
                    return Ok(Some(Op { name, operands: &self.operands }));
                }
            }
        }
    }

    fn skip_inline(&mut self, data: InlineData) -> Result<(), DocError> {
        let input = self.scan.input;
        if let Some(length) = data.length {
            check(
                LimitKind::DecodedBytes,
                length as u64,
                self.scan.limits.max_decoded_bytes,
            )?;
            let Some(mut end) = data.start.checked_add(length) else {
                return Err(refusal(
                    LimitKind::DecodedBytes,
                    u64::MAX,
                    self.scan.limits.max_decoded_bytes,
                ));
            };
            if end > input.len() {
                return Err(DocError::Malformed {
                    offset: data.start as u64,
                    what: "inline_image_length",
                });
            }
            // The separator after a length-delimited payload is small and bounded.
            let cap = end.saturating_add(1024).min(input.len());
            while end < cap && input.get(end).copied().map(whitespace).unwrap_or(false) {
                end = end.saturating_add(1);
            }
            let after = end.saturating_add(2);
            if input.get(end..after) == Some(b"EI")
                && input.get(after).copied().map(whitespace).unwrap_or(true)
            {
                self.scan.seek(after);
                return Ok(());
            }
            return Err(DocError::Malformed {
                offset: end as u64,
                what: "inline_image_end",
            });
        }

        let maximum = INLINE_SCAN_BYTES.min(self.scan.limits.max_decoded_bytes);
        let bound = usize::try_from(maximum).unwrap_or(usize::MAX);
        let cap = data.start.saturating_add(bound).min(input.len());
        let bytes = input.get(data.start..cap).unwrap_or(&[]);
        for (relative, window) in bytes.windows(4).enumerate() {
            if whitespace(window[0])
                && window.get(1..3) == Some(b"EI")
                && whitespace(window[3])
            {
                self.scan.seek(data.start.saturating_add(relative).saturating_add(3));
                return Ok(());
            }
        }
        if cap < input.len() {
            return Err(refusal(
                LimitKind::DecodedBytes,
                maximum.saturating_add(1),
                maximum,
            ));
        }
        Err(DocError::Malformed {
            offset: data.start as u64,
            what: "inline_image_end",
        })
    }
}

/// Iterates borrowed container members without building an object graph.
/// Nested arrays and dictionaries remain borrowed ranges.
pub struct Values<'a> {
    scan: Scan<'a>,
}

impl<'a> Values<'a> {
    pub fn new(bytes: &'a [u8], limits: &Limits) -> Self {
        Self::resume(bytes, limits, ContentCursor::default())
    }

    pub fn resume(bytes: &'a [u8], limits: &Limits, cursor: ContentCursor) -> Self {
        Self { scan: Scan::new(bytes, *limits, cursor) }
    }

    pub fn cursor(&self) -> ContentCursor {
        ContentCursor {
            offset: self.scan.position(),
            tokens: self.scan.tokens,
            inline_header: false,
            inline_data: None,
        }
    }

    pub fn next_value(&mut self) -> Result<Option<Operand<'a>>, DocError> {
        loop {
            match self.scan.item()? {
                Some(Item::Value(value)) => return Ok(Some(value)),
                Some(Item::Operator(_)) => {}
                None => return Ok(None),
            }
        }
    }
}

pub fn container_body(raw: &[u8]) -> &[u8] {
    let delimiter = if raw.starts_with(b"<<") { 2 } else { 1 };
    raw.get(delimiter..raw.len().saturating_sub(delimiter)).unwrap_or(&[])
}

pub fn inline_number(operands: &[Operand<'_>], short: &[u8], long: &[u8]) -> Option<f64> {
    operands.windows(2).rev().find_map(|pair| {
        match (pair.first(), pair.get(1)) {
            (Some(Operand::Name(name)), Some(Operand::Number(number)))
                if name_eq(name, short) || name_eq(name, long) => Some(*number),
            _ => None,
        }
    })
}

pub fn name_eq(encoded: &[u8], decoded: &[u8]) -> bool {
    NameBytes(encoded).eq(decoded.iter().copied())
}

struct NameBytes<'a>(&'a [u8]);

impl Iterator for NameBytes<'_> {
    type Item = u8;

    fn next(&mut self) -> Option<u8> {
        let (&first, rest) = self.0.split_first()?;
        if first == b'#' {
            if let (Some(high), Some(low)) = (
                rest.first().copied().and_then(hex),
                rest.get(1).copied().and_then(hex),
            ) {
                self.0 = rest.get(2..).unwrap_or(&[]);
                return Some((high << 4) | low);
            }
        }
        self.0 = rest;
        Some(first)
    }
}

pub fn decode_name<'a>(
    raw: &'a [u8],
    buffer: &'a mut Vec<u8>,
    maximum: u64,
) -> Result<&'a [u8], DocError> {
    if !raw.contains(&b'#') {
        return Ok(raw);
    }
    prepare_bytes(buffer, raw.len(), maximum)?;
    buffer.extend(NameBytes(raw));
    Ok(buffer)
}

/// Decode an object-level string while retaining its owner and provenance.
pub(crate) fn decode_object_string(
    resolver: &mut pdf_core::Resolver<'_>,
    value: &pdf_core::Value,
    maximum: u64,
) -> Result<Vec<u8>, DocError> {
    if !resolver.document().decryptable() {
        return Err(DocError::Encrypted);
    }
    let text = value.get().as_str().ok_or(DocError::WrongType {
        id: value.object().id(), expected: "PDF string",
    })?;
    check(LimitKind::ObjectBytes, text.raw().len() as u64, maximum)?;
    let owner = value.object().id().unwrap_or(pdf_core::ObjectRef::new(0, 0));
    let bytes = resolver.decode_string(owner, text);
    check(LimitKind::DecodedBytes, bytes.len() as u64, maximum)?;
    Ok(bytes)
}

/// Decodes a string in an already-decoded content stream. Object-level strings
/// use decode_object_string so their encryption key is selected by their owner.
pub fn decode_string<'a>(
    raw: &'a [u8],
    buffer: &'a mut Vec<u8>,
    maximum: u64,
) -> Result<&'a [u8], DocError> {
    let input = raw.get(1..raw.len().saturating_sub(1)).unwrap_or(&[]);
    if raw.first() == Some(&b'(') && !input.contains(&b'\\') && !input.contains(&b'\r') {
        return Ok(input);
    }
    prepare_bytes(buffer, input.len(), maximum)?;
    if raw.first() == Some(&b'<') {
        let mut high = None;
        for digit in input.iter().copied().filter_map(hex) {
            if let Some(first) = high.take() {
                buffer.push((first << 4) | digit);
            } else {
                high = Some(digit);
            }
        }
        if let Some(first) = high {
            buffer.push(first << 4);
        }
        return Ok(buffer);
    }

    let mut pos = 0usize;
    while let Some(byte) = input.get(pos).copied() {
        pos = pos.saturating_add(1);
        if byte == b'\\' {
            let Some(escaped) = input.get(pos).copied() else {
                break;
            };
            pos = pos.saturating_add(1);
            match escaped {
                b'n' => buffer.push(b'\n'),
                b'r' => buffer.push(b'\r'),
                b't' => buffer.push(b'\t'),
                b'b' => buffer.push(8),
                b'f' => buffer.push(12),
                b'\n' => {}
                b'\r' => {
                    if input.get(pos) == Some(&b'\n') {
                        pos = pos.saturating_add(1);
                    }
                }
                b'0'..=b'7' => {
                    let mut value = u16::from(escaped - b'0');
                    for _ in 0..2 {
                        match input.get(pos).copied() {
                            Some(next @ b'0'..=b'7') => {
                                value = value.saturating_mul(8)
                                    .saturating_add(u16::from(next - b'0'));
                                pos = pos.saturating_add(1);
                            }
                            _ => break,
                        }
                    }
                    buffer.push(value as u8);
                }
                other => buffer.push(other),
            }
        } else if byte == b'\r' {
            if input.get(pos) == Some(&b'\n') {
                pos = pos.saturating_add(1);
            }
            buffer.push(b'\n');
        } else {
            buffer.push(byte);
        }
    }
    Ok(buffer)
}

fn prepare_bytes(buffer: &mut Vec<u8>, length: usize, maximum: u64) -> Result<(), DocError> {
    check(LimitKind::DecodedBytes, length as u64, maximum)?;
    buffer.clear();
    buffer.try_reserve(length).map_err(|_| {
        refusal(LimitKind::DecodedBytes, maximum.saturating_add(1), maximum)
    })
}

fn token_limit(limits: Limits) -> u64 {
    limits.max_content_ops.saturating_mul(8).min(usize::MAX as u64)
}

fn lexer_limits(limits: Limits, used: u64) -> LexerLimits {
    LexerLimits::new(
        usize::try_from(token_limit(limits).saturating_sub(used)).unwrap_or(usize::MAX),
        usize::try_from(limits.max_object_bytes).unwrap_or(usize::MAX),
        limits.max_depth as usize,
    )
}

pub(crate) fn check(kind: LimitKind, requested: u64, maximum: u64) -> Result<(), DocError> {
    if requested > maximum {
        Err(refusal(kind, requested, maximum))
    } else {
        Ok(())
    }
}

pub(crate) fn refusal(kind: LimitKind, requested: u64, maximum: u64) -> DocError {
    DocError::Refused(Refusal { limit: kind, requested, maximum })
}

fn whitespace(byte: u8) -> bool {
    matches!(byte, 0 | 9 | 10 | 12 | 13 | 32)
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}


/// PDF numbers are `[+-]?digits[.digits]`. A mantissa below 2^53 with at most
/// 22 fraction digits divides exactly once, which is the correctly rounded value
/// `str::parse` gives, so the fast path matches it bit for bit.
fn parse_number(bytes: &[u8]) -> Option<f64> {
    const POW10: [f64; 23] = [
        1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14,
        1e15, 1e16, 1e17, 1e18, 1e19, 1e20, 1e21, 1e22,
    ];
    let (negative, digits) = match bytes.first() {
        Some(b'-') => (true, &bytes[1..]),
        Some(b'+') => (false, &bytes[1..]),
        _ => (false, bytes),
    };
    let mut mantissa = 0u64;
    let mut fraction = 0usize;
    let mut seen_dot = false;
    let mut seen_digit = false;
    for &byte in digits {
        match byte {
            b'0'..=b'9' => {
                mantissa = mantissa.wrapping_mul(10).wrapping_add(u64::from(byte - b'0'));
                if mantissa >= (1u64 << 53) {
                    return slow_number(bytes);
                }
                seen_digit = true;
                if seen_dot {
                    fraction += 1;
                }
            }
            b'.' if !seen_dot => seen_dot = true,
            _ => return slow_number(bytes),
        }
    }
    if !seen_digit || fraction >= POW10.len() {
        return slow_number(bytes);
    }
    let value = mantissa as f64 / POW10[fraction];
    Some(if negative { -value } else { value })
}

fn slow_number(bytes: &[u8]) -> Option<f64> {
    core::str::from_utf8(bytes).ok().and_then(|text| text.parse::<f64>().ok())
}
