use core::iter::FusedIterator;
use core::ops::Range;

/// The default maximum number of tokens emitted by a lexer.
pub const DEFAULT_MAX_TOKEN_COUNT: usize = 1_000_000;
/// The default maximum byte length of one token, including its delimiters.
pub const DEFAULT_MAX_TOKEN_BYTES: usize = 1_048_576;
/// The default maximum nesting depth inside a literal string.
pub const DEFAULT_MAX_LITERAL_NESTING: usize = 128;

/// Bounds applied while scanning a PDF object's lexical representation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LexerLimits {
    /// Maximum number of tokens the iterator may emit.
    pub max_tokens: usize,
    /// Maximum token length in bytes, including string and delimiter markers.
    pub max_token_bytes: usize,
    /// Maximum nested opening parentheses in a literal string.
    pub max_literal_nesting: usize,
}

impl LexerLimits {
    pub const fn new(
        max_tokens: usize,
        max_token_bytes: usize,
        max_literal_nesting: usize,
    ) -> Self {
        Self {
            max_tokens,
            max_token_bytes,
            max_literal_nesting,
        }
    }
}

impl Default for LexerLimits {
    fn default() -> Self {
        Self::new(
            DEFAULT_MAX_TOKEN_COUNT,
            DEFAULT_MAX_TOKEN_BYTES,
            DEFAULT_MAX_LITERAL_NESTING,
        )
    }
}

/// The lexical category of one PDF token.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TokenKind {
    Name,
    Number,
    Keyword,
    ArrayStart,
    ArrayEnd,
    DictionaryStart,
    DictionaryEnd,
    LiteralString,
    HexString,
}

/// A half-open byte range in the original input.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TokenSpan {
    start: usize,
    end: usize,
}

impl TokenSpan {
    pub const fn start(self) -> usize {
        self.start
    }

    pub const fn end(self) -> usize {
        self.end
    }

    pub fn range(self) -> Range<usize> {
        self.start..self.end
    }
}

/// A token's kind, borrowed source bytes, and original byte offsets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Token<'input> {
    kind: TokenKind,
    span: TokenSpan,
    bytes: &'input [u8],
}

impl<'input> Token<'input> {
    pub const fn kind(self) -> TokenKind {
        self.kind
    }

    pub const fn span(self) -> TokenSpan {
        self.span
    }

    /// Returns the token's original encoded bytes without allocating or decoding names.
    pub const fn bytes(self) -> &'input [u8] {
        self.bytes
    }
}

/// A lexical error with the byte offset where scanning could not continue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LexError {
    InvalidDelimiter { offset: usize },
    InvalidNameEscape { offset: usize },
    InvalidHexDigit { offset: usize },
    UnterminatedLiteralString { offset: usize },
    UnterminatedHexString { offset: usize },
    TokenTooLong { offset: usize, limit: usize },
    LiteralNestingTooDeep { offset: usize, limit: usize },
    TokenBudgetExceeded { offset: usize, limit: usize },
    OffsetOverflow { offset: usize },
    InvalidSpan { start: usize, end: usize },
}

/// Iterates borrowed tokens over a PDF byte slice.
///
/// Whitespace and percent comments are skipped. Names retain their encoded form,
/// including any valid `#xx` escapes. The iterator does not build PDF objects or
/// validate array and dictionary structure.
#[derive(Clone, Debug)]
pub struct Lexer<'input> {
    input: &'input [u8],
    offset: usize,
    emitted: usize,
    limits: LexerLimits,
    number: Option<f64>,
    terminated: bool,
}

impl<'input> Lexer<'input> {
    pub const fn new(input: &'input [u8]) -> Self {
        Self::with_limits(
            input,
            LexerLimits::new(
                DEFAULT_MAX_TOKEN_COUNT,
                DEFAULT_MAX_TOKEN_BYTES,
                DEFAULT_MAX_LITERAL_NESTING,
            ),
        )
    }

    pub const fn with_limits(input: &'input [u8], limits: LexerLimits) -> Self {
        Self {
            input,
            offset: 0,
            emitted: 0,
            limits,
            number: None,
            terminated: false,
        }
    }

    pub const fn input(&self) -> &'input [u8] {
        self.input
    }

    pub const fn offset(&self) -> usize {
        self.offset
    }

    pub const fn limits(&self) -> LexerLimits {
        self.limits
    }

    /// Returns the fast parsed value when the last token was a number.
    pub fn number(&self) -> Option<f64> {
        self.number
    }

    fn skip_trivia(&mut self) -> Result<(), LexError> {
        loop {
            let Some(byte) = byte_at(self.input, self.offset) else {
                return Ok(());
            };

            if is_pdf_whitespace(byte) {
                self.offset = next_offset(self.offset, self.offset)?;
                continue;
            }

            if byte == b'%' {
                while let Some(comment_byte) = byte_at(self.input, self.offset) {
                    if comment_byte == b'\r' || comment_byte == b'\n' {
                        break;
                    }
                    self.offset = next_offset(self.offset, self.offset)?;
                }
                continue;
            }

            return Ok(());
        }
    }

    fn scan_token(&mut self, start: usize) -> Result<(TokenKind, usize), LexError> {
        let byte = byte_at(self.input, start).ok_or(LexError::InvalidSpan { start, end: start })?;

        match byte {
            b'/' => self.scan_name(start),
            b'(' => self.scan_literal_string(start),
            b'<' if has_pair(self.input, start, b'<') => {
                let end = next_offset(next_offset(start, start)?, start)?;
                check_token_length(start, end, self.limits)?;
                Ok((TokenKind::DictionaryStart, end))
            }
            b'<' => self.scan_hex_string(start),
            b'>' if has_pair(self.input, start, b'>') => {
                let end = next_offset(next_offset(start, start)?, start)?;
                check_token_length(start, end, self.limits)?;
                Ok((TokenKind::DictionaryEnd, end))
            }
            b'[' => {
                let end = next_offset(start, start)?;
                check_token_length(start, end, self.limits)?;
                Ok((TokenKind::ArrayStart, end))
            }
            b']' => {
                let end = next_offset(start, start)?;
                check_token_length(start, end, self.limits)?;
                Ok((TokenKind::ArrayEnd, end))
            }
            b')' | b'>' | b'{' | b'}' => Err(LexError::InvalidDelimiter { offset: start }),
            _ if is_pdf_whitespace(byte) || is_delimiter(byte) => {
                Err(LexError::InvalidDelimiter { offset: start })
            }
            _ => self.scan_regular(start),
        }
    }

    fn scan_name(&self, start: usize) -> Result<(TokenKind, usize), LexError> {
        let mut end = next_offset(start, start)?;
        check_token_length(start, end, self.limits)?;

        loop {
            let Some(byte) = byte_at(self.input, end) else {
                return Ok((TokenKind::Name, end));
            };
            if is_pdf_whitespace(byte) || is_delimiter(byte) {
                return Ok((TokenKind::Name, end));
            }

            if byte == b'#' {
                let high_offset = next_offset(end, start)?;
                let low_offset = next_offset(high_offset, start)?;
                let high = byte_at(self.input, high_offset)
                    .filter(|digit| is_hex_digit(*digit))
                    .ok_or(LexError::InvalidNameEscape { offset: end })?;
                let low = byte_at(self.input, low_offset)
                    .filter(|digit| is_hex_digit(*digit))
                    .ok_or(LexError::InvalidNameEscape { offset: end })?;
                let _ = (high, low);
                end = next_offset(low_offset, start)?;
            } else {
                end = next_offset(end, start)?;
            }
            check_token_length(start, end, self.limits)?;
        }
    }

    fn scan_regular(&mut self, start: usize) -> Result<(TokenKind, usize), LexError> {
        const POW10: [f64; 23] = [
            1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15,
            1e16, 1e17, 1e18, 1e19, 1e20, 1e21, 1e22,
        ];
        let mut end = start;
        let mut is_number = true;
        let mut negative = false;
        let mut mantissa = 0u64;
        let mut fraction = 0usize;
        let mut seen_dot = false;
        let mut seen_digit = false;
        let mut needs_slow = false;

        loop {
            let Some(byte) = byte_at(self.input, end) else {
                break;
            };
            if is_pdf_whitespace(byte) || is_delimiter(byte) {
                break;
            }

            if is_number {
                match byte {
                    b'+' if end == start => {}
                    b'-' if end == start => negative = true,
                    b'0'..=b'9' => {
                        mantissa = mantissa
                            .wrapping_mul(10)
                            .wrapping_add(u64::from(byte - b'0'));
                        if mantissa >= (1u64 << 53) {
                            needs_slow = true;
                        }
                        seen_digit = true;
                        if seen_dot {
                            fraction += 1;
                        }
                    }
                    b'.' if !seen_dot => seen_dot = true,
                    _ => is_number = false,
                }
            }

            end = next_offset(end, start)?;
            check_token_length(start, end, self.limits)?;
        }

        if end == start {
            return Err(LexError::InvalidDelimiter { offset: start });
        }

        let kind = if is_number && seen_digit {
            TokenKind::Number
        } else {
            TokenKind::Keyword
        };
        if kind == TokenKind::Number && !needs_slow && fraction < POW10.len() {
            let value = mantissa as f64 / POW10[fraction];
            self.number = Some(if negative { -value } else { value });
        }
        Ok((kind, end))
    }

    fn scan_literal_string(&self, start: usize) -> Result<(TokenKind, usize), LexError> {
        let mut end = next_offset(start, start)?;
        let mut depth = 1usize;
        if depth > self.limits.max_literal_nesting {
            return Err(LexError::LiteralNestingTooDeep {
                offset: start,
                limit: self.limits.max_literal_nesting,
            });
        }
        check_token_length(start, end, self.limits)?;
        let limit_end = start.saturating_add(self.limits.max_token_bytes);
        let window_end = limit_end.min(self.input.len());

        loop {
            let run = self.input.get(end..window_end).unwrap_or(&[]);
            let stop = run
                .iter()
                .position(|byte| matches!(*byte, b'\\' | b'(' | b')'))
                .unwrap_or(run.len());
            end = end.saturating_add(stop);

            let Some(byte) = byte_at(self.input, end) else {
                return Err(LexError::UnterminatedLiteralString { offset: start });
            };

            match byte {
                b'\\' => {
                    let escaped_offset = next_offset(end, start)?;
                    let Some(escaped_byte) = byte_at(self.input, escaped_offset) else {
                        return Err(LexError::UnterminatedLiteralString { offset: start });
                    };
                    if escaped_byte == b'\r' {
                        let after_cr = next_offset(escaped_offset, start)?;
                        end = if byte_at(self.input, after_cr) == Some(b'\n') {
                            next_offset(after_cr, start)?
                        } else {
                            after_cr
                        };
                    } else {
                        end = next_offset(escaped_offset, start)?;
                    }
                }
                b'(' => {
                    depth = depth
                        .checked_add(1)
                        .ok_or(LexError::LiteralNestingTooDeep {
                            offset: end,
                            limit: self.limits.max_literal_nesting,
                        })?;
                    if depth > self.limits.max_literal_nesting {
                        return Err(LexError::LiteralNestingTooDeep {
                            offset: end,
                            limit: self.limits.max_literal_nesting,
                        });
                    }
                    end = next_offset(end, start)?;
                }
                b')' if depth == 1 => {
                    end = next_offset(end, start)?;
                    check_token_length(start, end, self.limits)?;
                    return Ok((TokenKind::LiteralString, end));
                }
                b')' => {
                    depth -= 1;
                    end = next_offset(end, start)?;
                }
                _ => end = next_offset(end, start)?,
            }
            check_token_length(start, end, self.limits)?;
        }
    }

    fn scan_hex_string(&self, start: usize) -> Result<(TokenKind, usize), LexError> {
        let mut end = next_offset(start, start)?;
        check_token_length(start, end, self.limits)?;
        let limit_end = start.saturating_add(self.limits.max_token_bytes);
        let window_end = limit_end.min(self.input.len());

        loop {
            let run = self.input.get(end..window_end).unwrap_or(&[]);
            let stop = run
                .iter()
                .position(|byte| !matches!(*byte, b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F'))
                .unwrap_or(run.len());
            end = end.saturating_add(stop);

            let Some(byte) = byte_at(self.input, end) else {
                return Err(LexError::UnterminatedHexString { offset: start });
            };

            if byte == b'>' {
                end = next_offset(end, start)?;
                check_token_length(start, end, self.limits)?;
                return Ok((TokenKind::HexString, end));
            }
            if !is_pdf_whitespace(byte) && !is_hex_digit(byte) {
                return Err(LexError::InvalidHexDigit { offset: end });
            }

            end = next_offset(end, start)?;
            check_token_length(start, end, self.limits)?;
        }
    }
}

impl<'input> Iterator for Lexer<'input> {
    type Item = Result<Token<'input>, LexError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.number = None;
        if self.terminated {
            return None;
        }

        if let Err(error) = self.skip_trivia() {
            self.terminated = true;
            return Some(Err(error));
        }

        let start = self.offset;
        if byte_at(self.input, start).is_none() {
            self.terminated = true;
            return None;
        }
        if self.emitted >= self.limits.max_tokens {
            self.terminated = true;
            return Some(Err(LexError::TokenBudgetExceeded {
                offset: start,
                limit: self.limits.max_tokens,
            }));
        }

        let (kind, end) = match self.scan_token(start) {
            Ok(scanned) => scanned,
            Err(error) => {
                self.terminated = true;
                return Some(Err(error));
            }
        };
        let Some(bytes) = self.input.get(start..end) else {
            self.terminated = true;
            return Some(Err(LexError::InvalidSpan { start, end }));
        };
        let Some(emitted) = self.emitted.checked_add(1) else {
            self.terminated = true;
            return Some(Err(LexError::TokenBudgetExceeded {
                offset: start,
                limit: self.limits.max_tokens,
            }));
        };

        self.offset = end;
        self.emitted = emitted;
        Some(Ok(Token {
            kind,
            span: TokenSpan { start, end },
            bytes,
        }))
    }
}

impl FusedIterator for Lexer<'_> {}

fn byte_at(input: &[u8], offset: usize) -> Option<u8> {
    input.get(offset).copied()
}

fn has_pair(input: &[u8], offset: usize, byte: u8) -> bool {
    let Some(second_offset) = offset.checked_add(1) else {
        return false;
    };
    byte_at(input, offset) == Some(byte) && byte_at(input, second_offset) == Some(byte)
}

fn next_offset(offset: usize, error_offset: usize) -> Result<usize, LexError> {
    offset.checked_add(1).ok_or(LexError::OffsetOverflow {
        offset: error_offset,
    })
}

fn check_token_length(start: usize, end: usize, limits: LexerLimits) -> Result<(), LexError> {
    if end - start > limits.max_token_bytes {
        return Err(LexError::TokenTooLong {
            offset: start,
            limit: limits.max_token_bytes,
        });
    }
    Ok(())
}

fn is_pdf_whitespace(byte: u8) -> bool {
    matches!(byte, 0 | 9 | 10 | 12 | 13 | 32)
}

fn is_delimiter(byte: u8) -> bool {
    matches!(
        byte,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

fn is_hex_digit(byte: u8) -> bool {
    byte.is_ascii_hexdigit()
}
