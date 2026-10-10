use alloc::vec::Vec;
use core::fmt;
use core::ops::Range;

use crate::lex::{LexError, Lexer, LexerLimits, Token, TokenKind};
use crate::source::{PdfSource, RawSpan};
use crate::work::{ObjectRef, ObjectWork, WorkBudget, WorkError};

/// A half-open byte range in the original PDF source.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ObjectSpan {
    start: usize,
    end: usize,
}

impl ObjectSpan {
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

/// A borrowed byte sequence and its range in the original source.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RawObjectValue<'source> {
    bytes: &'source [u8],
    span: ObjectSpan,
}

impl<'source> RawObjectValue<'source> {
    pub const fn as_bytes(self) -> &'source [u8] {
        self.bytes
    }

    pub const fn span(self) -> ObjectSpan {
        self.span
    }

    pub fn range(self) -> Range<usize> {
        self.span.range()
    }
}

/// An index into the flat value graph owned by an indirect object.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ObjectValueIndex(usize);

impl ObjectValueIndex {
    pub const fn index(self) -> usize {
        self.0
    }
}

/// A dictionary key and the graph index of its value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DictionaryEntry<'source> {
    key: RawObjectValue<'source>,
    value: ObjectValueIndex,
}

impl<'source> DictionaryEntry<'source> {
    pub const fn key(self) -> RawObjectValue<'source> {
        self.key
    }

    pub const fn value(self) -> ObjectValueIndex {
        self.value
    }
}

/// The kind of a direct PDF value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ObjectKind<'source> {
    Array(Vec<ObjectValueIndex>),
    Dictionary(Vec<DictionaryEntry<'source>>),
    String,
    Name,
    Number,
    Boolean(bool),
    Null,
    Reference(ObjectRef),
}

/// One node in an indirect object's direct value graph.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectValue<'source> {
    raw: RawObjectValue<'source>,
    kind: ObjectKind<'source>,
}

impl<'source> ObjectValue<'source> {
    pub const fn raw(&self) -> RawObjectValue<'source> {
        self.raw
    }

    pub const fn kind(&self) -> &ObjectKind<'source> {
        &self.kind
    }
}

/// A validated indirect object and its parsed, flat direct-value graph.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Object<'source> {
    id: ObjectRef,
    root: ObjectValueIndex,
    values: Vec<ObjectValue<'source>>,
    raw: RawObjectValue<'source>,
    token_count: usize,
}

impl<'source> Object<'source> {
    pub const fn id(&self) -> ObjectRef {
        self.id
    }

    pub const fn root_index(&self) -> ObjectValueIndex {
        self.root
    }

    pub fn root_value(&self) -> Option<&ObjectValue<'source>> {
        self.value(self.root)
    }

    pub fn value(&self, index: ObjectValueIndex) -> Option<&ObjectValue<'source>> {
        self.values.get(index.0)
    }

    /// Returns every node in source order without following indirect references.
    pub fn values(&self) -> &[ObjectValue<'source>] {
        &self.values
    }

    pub const fn raw(&self) -> RawObjectValue<'source> {
        self.raw
    }

    pub const fn token_count(&self) -> usize {
        self.token_count
    }
}

/// Errors returned while parsing one bounded indirect object.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PdfError {
    Work(WorkError),
    Lexical {
        error: LexError,
        base_offset: usize,
    },
    InvalidObjectHeader {
        offset: usize,
    },
    ObjectIdMismatch {
        expected: ObjectRef,
        found: ObjectRef,
    },
    InvalidValue {
        offset: usize,
    },
    InvalidReference {
        offset: usize,
    },
    DictionaryKeyNotName {
        offset: usize,
    },
    UnexpectedToken {
        offset: usize,
    },
    UnexpectedEnd {
        offset: usize,
    },
    UnclosedContainer {
        offset: usize,
    },
    InvalidSpan {
        start: usize,
        end: usize,
    },
    OffsetOverflow {
        offset: usize,
    },
    TokenCountOverflow {
        offset: usize,
    },
}

impl fmt::Display for PdfError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Work(error) => fmt::Display::fmt(error, formatter),
            Self::Lexical { error, base_offset } => {
                write!(formatter, "PDF lexical error at base byte {base_offset}: {error:?}")
            }
            Self::InvalidObjectHeader { offset } => {
                write!(formatter, "invalid indirect object header at byte {offset}")
            }
            Self::ObjectIdMismatch { expected, found } => write!(
                formatter,
                "xref identifies object {} {}, but offset contains object {} {}",
                expected.number(),
                expected.generation(),
                found.number(),
                found.generation()
            ),
            Self::InvalidValue { offset } => {
                write!(formatter, "invalid PDF object value at byte {offset}")
            }
            Self::InvalidReference { offset } => {
                write!(formatter, "invalid indirect reference at byte {offset}")
            }
            Self::DictionaryKeyNotName { offset } => {
                write!(formatter, "dictionary key is not a name at byte {offset}")
            }
            Self::UnexpectedToken { offset } => {
                write!(formatter, "unexpected PDF token at byte {offset}")
            }
            Self::UnexpectedEnd { offset } => {
                write!(formatter, "unexpected end of indirect object at byte {offset}")
            }
            Self::UnclosedContainer { offset } => {
                write!(formatter, "unclosed PDF container at byte {offset}")
            }
            Self::InvalidSpan { start, end } => {
                write!(formatter, "invalid parsed span {start}..{end}")
            }
            Self::OffsetOverflow { offset } => {
                write!(formatter, "PDF source offset overflows at byte {offset}")
            }
            Self::TokenCountOverflow { offset } => {
                write!(formatter, "PDF token count overflows at byte {offset}")
            }
        }
    }
}

impl From<WorkError> for PdfError {
    fn from(error: WorkError) -> Self {
        Self::Work(error)
    }
}

#[cfg(feature = "std")]
impl std::error::Error for PdfError {}

/// Parses one indirect object at the exact offset recorded by `work`.
///
/// The returned value graph owns only node metadata. Every raw value borrows
/// directly from `source`; indirect references are recorded but not followed.
pub fn parse_object<'source>(
    source: &'source PdfSource,
    work: ObjectWork,
    budget: &WorkBudget,
) -> Result<Object<'source>, PdfError> {
    let candidate: RawSpan<'source> = work.span(source, budget)?;
    let input = candidate.as_bytes();
    let base_offset = candidate.range().start;
    let mut cursor = TokenCursor::new(input, base_offset, budget);

    let first = required_token(&mut cursor, base_offset)?;
    if first.kind() != TokenKind::Number || first.span().start() != 0 {
        return Err(PdfError::InvalidObjectHeader {
            offset: absolute_offset(base_offset, first.span().start())?,
        });
    }
    let second = required_token(&mut cursor, base_offset)?;
    let object_keyword = required_token(&mut cursor, base_offset)?;
    if second.kind() != TokenKind::Number {
        return Err(PdfError::InvalidObjectHeader {
            offset: absolute_offset(base_offset, second.span().start())?,
        });
    }
    if object_keyword.kind() != TokenKind::Keyword || object_keyword.bytes() != b"obj" {
        return Err(PdfError::InvalidObjectHeader {
            offset: absolute_offset(base_offset, object_keyword.span().start())?,
        });
    }

    let number = parse_unsigned(first.bytes()).ok_or(PdfError::InvalidObjectHeader {
        offset: absolute_offset(base_offset, first.span().start())?,
    })?;
    let generation = parse_unsigned(second.bytes()).ok_or(PdfError::InvalidObjectHeader {
        offset: absolute_offset(base_offset, second.span().start())?,
    })?;
    let found = match (u32::try_from(number), u16::try_from(generation)) {
        (Ok(number), Ok(generation)) => ObjectRef::new(number, generation),
        _ => {
            return Err(PdfError::InvalidObjectHeader {
                offset: absolute_offset(base_offset, first.span().start())?,
            });
        }
    };
    let expected = work.id();
    if found != expected {
        return Err(PdfError::ObjectIdMismatch { expected, found });
    }

    let (root, values) = parse_value_graph(&mut cursor, input, base_offset, budget)?;
    let end_object = required_token(&mut cursor, base_offset)?;
    if end_object.kind() != TokenKind::Keyword || end_object.bytes() != b"endobj" {
        return Err(PdfError::UnexpectedToken {
            offset: absolute_offset(base_offset, end_object.span().start())?,
        });
    }

    let raw = raw_value(input, base_offset, 0, end_object.span().end())?;
    Ok(Object {
        id: found,
        root,
        values,
        raw,
        token_count: cursor.consumed(),
    })
}

#[derive(Debug)]
enum ContainerFrame<'source> {
    Array {
        start: usize,
        values: Vec<ObjectValueIndex>,
    },
    Dictionary {
        start: usize,
        entries: Vec<DictionaryEntry<'source>>,
        pending_key: Option<RawObjectValue<'source>>,
    },
}

fn parse_value_graph<'source>(
    cursor: &mut TokenCursor<'source>,
    input: &'source [u8],
    base_offset: usize,
    budget: &WorkBudget,
) -> Result<(ObjectValueIndex, Vec<ObjectValue<'source>>), PdfError> {
    let mut values = Vec::new();
    let mut frames = Vec::<ContainerFrame<'source>>::new();
    let mut root = None;

    loop {
        if frames.is_empty() && root.is_some() {
            break;
        }

        let dictionary_needs_key = matches!(
            frames.last(),
            Some(ContainerFrame::Dictionary {
                pending_key: None,
                ..
            })
        );
        let token = required_token(cursor, base_offset)?;

        if dictionary_needs_key {
            if token.kind() == TokenKind::DictionaryEnd {
                let frame = frames.pop().ok_or(PdfError::UnclosedContainer {
                    offset: absolute_offset(base_offset, token.span().start())?,
                })?;
                let ContainerFrame::Dictionary { start, entries, .. } = frame else {
                    return Err(PdfError::UnexpectedToken {
                        offset: absolute_offset(base_offset, token.span().start())?,
                    });
                };
                let node = ObjectValue {
                    raw: raw_value(input, base_offset, start, token.span().end())?,
                    kind: ObjectKind::Dictionary(entries),
                };
                attach_value(node, &mut values, &mut frames, &mut root)?;
            } else if token.kind() == TokenKind::Name {
                let key = raw_token(input, base_offset, token)?;
                match frames.last_mut() {
                    Some(ContainerFrame::Dictionary { pending_key, .. }) => {
                        *pending_key = Some(key);
                    }
                    _ => {
                        return Err(PdfError::UnexpectedToken {
                            offset: absolute_offset(base_offset, token.span().start())?,
                        });
                    }
                }
            } else {
                return Err(PdfError::DictionaryKeyNotName {
                    offset: absolute_offset(base_offset, token.span().start())?,
                });
            }
            continue;
        }

        match token.kind() {
            TokenKind::ArrayEnd => {
                let frame = frames.pop().ok_or(PdfError::UnexpectedToken {
                    offset: absolute_offset(base_offset, token.span().start())?,
                })?;
                let ContainerFrame::Array {
                    start,
                    values: child_values,
                } = frame
                else {
                    return Err(PdfError::UnexpectedToken {
                        offset: absolute_offset(base_offset, token.span().start())?,
                    });
                };
                let node = ObjectValue {
                    raw: raw_value(input, base_offset, start, token.span().end())?,
                    kind: ObjectKind::Array(child_values),
                };
                attach_value(node, &mut values, &mut frames, &mut root)?;
            }
            TokenKind::DictionaryEnd => {
                return Err(PdfError::UnexpectedToken {
                    offset: absolute_offset(base_offset, token.span().start())?,
                });
            }
            TokenKind::ArrayStart => {
                check_container_depth(base_offset, token, frames.len(), budget)?;
                frames.push(ContainerFrame::Array {
                    start: token.span().start(),
                    values: Vec::new(),
                });
            }
            TokenKind::DictionaryStart => {
                check_container_depth(base_offset, token, frames.len(), budget)?;
                frames.push(ContainerFrame::Dictionary {
                    start: token.span().start(),
                    entries: Vec::new(),
                    pending_key: None,
                });
            }
            TokenKind::Name => {
                let node = ObjectValue {
                    raw: raw_token(input, base_offset, token)?,
                    kind: ObjectKind::Name,
                };
                attach_value(node, &mut values, &mut frames, &mut root)?;
            }
            TokenKind::LiteralString | TokenKind::HexString => {
                let node = ObjectValue {
                    raw: raw_token(input, base_offset, token)?,
                    kind: ObjectKind::String,
                };
                attach_value(node, &mut values, &mut frames, &mut root)?;
            }
            TokenKind::Number => {
                let node = parse_number_value(cursor, input, base_offset, token)?;
                attach_value(node, &mut values, &mut frames, &mut root)?;
            }
            TokenKind::Keyword if token.bytes() == b"true" => {
                let node = ObjectValue {
                    raw: raw_token(input, base_offset, token)?,
                    kind: ObjectKind::Boolean(true),
                };
                attach_value(node, &mut values, &mut frames, &mut root)?;
            }
            TokenKind::Keyword if token.bytes() == b"false" => {
                let node = ObjectValue {
                    raw: raw_token(input, base_offset, token)?,
                    kind: ObjectKind::Boolean(false),
                };
                attach_value(node, &mut values, &mut frames, &mut root)?;
            }
            TokenKind::Keyword if token.bytes() == b"null" => {
                let node = ObjectValue {
                    raw: raw_token(input, base_offset, token)?,
                    kind: ObjectKind::Null,
                };
                attach_value(node, &mut values, &mut frames, &mut root)?;
            }
            TokenKind::Keyword => {
                return Err(PdfError::InvalidValue {
                    offset: absolute_offset(base_offset, token.span().start())?,
                });
            }
        }
    }

    if !frames.is_empty() {
        return Err(PdfError::UnclosedContainer {
            offset: absolute_offset(base_offset, cursor.offset())?,
        });
    }
    let root = root.ok_or(PdfError::UnexpectedEnd {
        offset: absolute_offset(base_offset, cursor.offset())?,
    })?;
    Ok((root, values))
}

fn check_container_depth(
    base_offset: usize,
    token: Token<'_>,
    open_containers: usize,
    budget: &WorkBudget,
) -> Result<(), PdfError> {
    let depth = open_containers
        .checked_add(1)
        .ok_or(PdfError::OffsetOverflow {
            offset: absolute_offset(base_offset, token.span().start())?,
        })?;
    budget.check_depth(depth)?;
    Ok(())
}

fn attach_value<'source>(
    node: ObjectValue<'source>,
    values: &mut Vec<ObjectValue<'source>>,
    frames: &mut [ContainerFrame<'source>],
    root: &mut Option<ObjectValueIndex>,
) -> Result<(), PdfError> {
    let offset = node.raw.span.start();
    let index = ObjectValueIndex(values.len());
    values.push(node);
    match frames.last_mut() {
        Some(ContainerFrame::Array { values, .. }) => values.push(index),
        Some(ContainerFrame::Dictionary {
            entries,
            pending_key,
            ..
        }) => {
            let key = pending_key
                .take()
                .ok_or(PdfError::UnexpectedToken { offset })?;
            entries.push(DictionaryEntry { key, value: index });
        }
        None => {
            if root.replace(index).is_some() {
                return Err(PdfError::UnexpectedToken { offset });
            }
        }
    }
    Ok(())
}

fn parse_number_value<'source>(
    cursor: &mut TokenCursor<'source>,
    input: &'source [u8],
    base_offset: usize,
    first: Token<'source>,
) -> Result<ObjectValue<'source>, PdfError> {
    let Some(second) = cursor.peek(0)? else {
        return Ok(ObjectValue {
            raw: raw_token(input, base_offset, first)?,
            kind: ObjectKind::Number,
        });
    };
    if second.kind() != TokenKind::Number {
        return Ok(ObjectValue {
            raw: raw_token(input, base_offset, first)?,
            kind: ObjectKind::Number,
        });
    }
    let Some(reference_marker) = cursor.peek(1)? else {
        return Ok(ObjectValue {
            raw: raw_token(input, base_offset, first)?,
            kind: ObjectKind::Number,
        });
    };
    if reference_marker.kind() != TokenKind::Keyword || reference_marker.bytes() != b"R" {
        return Ok(ObjectValue {
            raw: raw_token(input, base_offset, first)?,
            kind: ObjectKind::Number,
        });
    }

    let object_number = parse_unsigned(first.bytes());
    let generation = parse_unsigned(second.bytes());
    let (Some(object_number), Some(generation)) = (object_number, generation) else {
        return Err(PdfError::InvalidReference {
            offset: absolute_offset(base_offset, first.span().start())?,
        });
    };
    let (Ok(object_number), Ok(generation)) =
        (u32::try_from(object_number), u16::try_from(generation))
    else {
        return Err(PdfError::InvalidReference {
            offset: absolute_offset(base_offset, first.span().start())?,
        });
    };

    let consumed_number = required_token(cursor, base_offset)?;
    let consumed_marker = required_token(cursor, base_offset)?;
    if consumed_number.span() != second.span() || consumed_marker.span() != reference_marker.span() {
        return Err(PdfError::UnexpectedToken {
            offset: absolute_offset(base_offset, first.span().start())?,
        });
    }
    Ok(ObjectValue {
        raw: raw_value(
            input,
            base_offset,
            first.span().start(),
            reference_marker.span().end(),
        )?,
        kind: ObjectKind::Reference(ObjectRef::new(object_number, generation)),
    })
}

fn parse_unsigned(bytes: &[u8]) -> Option<u64> {
    let digits = match bytes.first() {
        Some(b'+') => bytes.get(1..)?,
        Some(b'-') | None => return None,
        Some(_) => bytes,
    };
    if digits.is_empty() {
        return None;
    }
    let mut value = 0u64;
    for byte in digits {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value
            .checked_mul(10)?
            .checked_add(u64::from(*byte - b'0'))?;
    }
    Some(value)
}

fn required_token<'source>(
    cursor: &mut TokenCursor<'source>,
    base_offset: usize,
) -> Result<Token<'source>, PdfError> {
    cursor.next()?.ok_or(PdfError::UnexpectedEnd {
        offset: absolute_offset(base_offset, cursor.offset())?,
    })
}

fn raw_token<'source>(
    input: &'source [u8],
    base_offset: usize,
    token: Token<'source>,
) -> Result<RawObjectValue<'source>, PdfError> {
    raw_value(
        input,
        base_offset,
        token.span().start(),
        token.span().end(),
    )
}

fn raw_value<'source>(
    input: &'source [u8],
    base_offset: usize,
    start: usize,
    end: usize,
) -> Result<RawObjectValue<'source>, PdfError> {
    let bytes = input
        .get(start..end)
        .ok_or(PdfError::InvalidSpan { start, end })?;
    let span = ObjectSpan {
        start: absolute_offset(base_offset, start)?,
        end: absolute_offset(base_offset, end)?,
    };
    Ok(RawObjectValue { bytes, span })
}

fn absolute_offset(base_offset: usize, relative: usize) -> Result<usize, PdfError> {
    base_offset
        .checked_add(relative)
        .ok_or(PdfError::OffsetOverflow { offset: base_offset })
}

struct TokenCursor<'input> {
    lexer: Lexer<'input>,
    base_offset: usize,
    buffered: [Option<Result<Token<'input>, LexError>>; 3],
    buffered_len: usize,
    finished: bool,
    consumed: usize,
}

impl<'input> TokenCursor<'input> {
    fn new(input: &'input [u8], base_offset: usize, budget: &WorkBudget) -> Self {
        Self {
            lexer: Lexer::with_limits(
                input,
                LexerLimits::new(
                    budget.max_tokens(),
                    budget.max_input_bytes(),
                    budget.max_depth(),
                ),
            ),
            base_offset,
            buffered: [None, None, None],
            buffered_len: 0,
            finished: false,
            consumed: 0,
        }
    }

    fn peek(&mut self, index: usize) -> Result<Option<Token<'input>>, PdfError> {
        if index >= self.buffered.len() {
            return Err(PdfError::UnexpectedToken {
                offset: absolute_offset(self.base_offset, self.lexer.offset())?,
            });
        }
        while self.buffered_len <= index && !self.finished {
            match self.lexer.next() {
                Some(Ok(token)) => {
                    let Some(slot) = self.buffered.get_mut(self.buffered_len) else {
                        return Err(PdfError::UnexpectedToken {
                            offset: absolute_offset(self.base_offset, self.lexer.offset())?,
                        });
                    };
                    *slot = Some(Ok(token));
                    self.buffered_len += 1;
                }
                Some(Err(error)) => {
                    self.finished = true;
                    return Err(PdfError::Lexical {
                        error,
                        base_offset: self.base_offset,
                    });
                }
                None => self.finished = true,
            }
        }
        let Some(buffered) = self.buffered.get(index).copied().flatten() else {
            return Ok(None);
        };
        match buffered {
            Ok(token) => Ok(Some(token)),
            Err(error) => Err(PdfError::Lexical {
                error,
                base_offset: self.base_offset,
            }),
        }
    }

    fn next(&mut self) -> Result<Option<Token<'input>>, PdfError> {
        let token = self.peek(0)?;
        if token.is_some() {
            for index in 1..self.buffered_len {
                let next = self.buffered.get(index).copied().flatten();
                if let Some(slot) = self.buffered.get_mut(index - 1) {
                    *slot = next;
                }
            }
            self.buffered_len -= 1;
            if let Some(slot) = self.buffered.get_mut(self.buffered_len) {
                *slot = None;
            }
            self.consumed = self
                .consumed
                .checked_add(1)
                .ok_or(PdfError::TokenCountOverflow {
                    offset: absolute_offset(self.base_offset, self.lexer.offset())?,
                })?;
        }
        Ok(token)
    }

    const fn offset(&self) -> usize {
        self.lexer.offset()
    }

    const fn consumed(&self) -> usize {
        self.consumed
    }
}
