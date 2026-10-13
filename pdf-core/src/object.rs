use alloc::borrow::Cow;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::Range;

use crate::error::{malformed, DocError};
use crate::lex::{LexError, Lexer, LexerLimits, Token, TokenKind};
use crate::limits::{check, LimitKind, Limits, Refusal};
use crate::work::ObjectRef;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ValueKind {
    Null,
    Bool,
    Int,
    Real,
    Name,
    String,
    Array,
    Dict,
    Ref,
}

#[derive(Clone, Debug)]
enum Kind {
    Null,
    Bool(bool),
    Int(i64),
    Real(f64),
    Name,
    String,
    DecryptedString(Arc<[u8]>),
    Array(Vec<usize>),
    Dict(Vec<(Range<usize>, usize)>),
    Ref(ObjectRef),
}

#[derive(Clone, Debug)]
struct Node {
    span: Range<usize>,
    kind: Kind,
}

#[derive(Clone, Debug)]
struct ObjectData {
    buf: Arc<[u8]>,
    nodes: Vec<Node>,
    root: usize,
    id: Option<ObjectRef>,
    stream: Option<Range<usize>>,
    stream_start: Option<usize>,
    syntax_bytes: u64,
    token_count: u32,
    recovered: bool,
    syntax_recovered: bool,
    strings_plain: bool,
}

#[derive(Clone, Debug)]
pub struct Object(Arc<ObjectData>);

#[derive(Clone, Copy)]
pub struct ValueRef<'o> {
    object: &'o Object,
    index: usize,
}

#[derive(Clone, Copy)]
pub struct ArrayRef<'o>(ValueRef<'o>);

#[derive(Clone, Copy)]
pub struct DictRef<'o>(ValueRef<'o>);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Name<'o> {
    bytes: &'o [u8],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PdfStr<'o> {
    bytes: &'o [u8],
    owner: Option<ObjectRef>,
    plaintext: bool,
    decoded: Option<&'o [u8]>,
}

#[derive(Clone, Debug)]
pub struct Value {
    object: Object,
    index: usize,
}

impl Object {
    pub fn id(&self) -> Option<ObjectRef> {
        self.0.id
    }
    pub fn root(&self) -> ValueRef<'_> {
        ValueRef {
            object: self,
            index: self.0.root,
        }
    }
    pub fn dict(&self) -> Option<DictRef<'_>> {
        self.root().as_dict()
    }
    pub fn is_stream(&self) -> bool {
        self.0.stream_start.is_some()
    }
    pub fn stream_raw(&self) -> Option<&[u8]> {
        self.0
            .stream
            .as_ref()
            .and_then(|range| self.0.buf.get(range.clone()))
    }
    pub fn stream_range(&self) -> Option<Range<usize>> {
        self.0.stream.clone()
    }
    pub fn buffer(&self) -> &Arc<[u8]> {
        &self.0.buf
    }
    pub fn token_count(&self) -> u32 {
        self.0.token_count
    }
    pub(crate) fn stream_recovered(&self) -> bool {
        self.0.recovered && self.is_stream()
    }
    pub(crate) fn tail_recovered(&self) -> bool {
        self.0.recovered && !self.is_stream()
    }
    pub(crate) fn syntax_recovered(&self) -> bool {
        self.0.syntax_recovered
    }

    pub(crate) fn decrypt_private_strings(
        &mut self,
        key: &crate::encryption::ChaChaKey,
        limits: &Limits,
    ) -> Result<(), DocError> {
        if self.0.strings_plain {
            return Ok(());
        }
        let id = self
            .id()
            .ok_or_else(|| malformed(0, "PSChaCha object ID"))?;
        let data = Arc::make_mut(&mut self.0);
        let mut total = 0u64;
        for node in &mut data.nodes {
            if !matches!(&node.kind, Kind::String) {
                continue;
            }
            let raw = data
                .buf
                .get(node.span.clone())
                .ok_or_else(|| malformed(0, "PSChaCha string span"))?;
            let encoded = PdfStr {
                bytes: raw,
                owner: Some(id),
                plaintext: false,
                decoded: None,
            }
            .decode();
            let plain = key.open(id, &encoded, limits.max_decoded_bytes)?;
            total = total.saturating_add(plain.len() as u64);
            check(LimitKind::ObjectBytes, total, limits.max_object_bytes)?;
            node.kind = Kind::DecryptedString(Arc::from(plain));
        }
        data.strings_plain = true;
        Ok(())
    }

    pub(crate) fn decrypted_string_bytes(&self) -> u64 {
        self.0
            .nodes
            .iter()
            .fold(0u64, |total, node| match &node.kind {
                Kind::DecryptedString(bytes) => total.saturating_add(bytes.len() as u64),
                _ => total,
            })
    }

    pub(crate) fn finish_stream(
        &mut self,
        length: Option<u64>,
        limits: &Limits,
    ) -> Result<(), DocError> {
        self.finish_stream_with_mode(length, limits, false)
    }

    pub(crate) fn finish_stream_repair(
        &mut self,
        length: Option<u64>,
        limits: &Limits,
    ) -> Result<(), DocError> {
        self.finish_stream_with_mode(length, limits, true)
    }

    fn finish_stream_with_mode(
        &mut self,
        length: Option<u64>,
        limits: &Limits,
        repair: bool,
    ) -> Result<(), DocError> {
        if self.0.stream.is_some() {
            return Ok(());
        }
        let start = self.0.stream_start.ok_or_else(|| malformed(0, "stream"))?;
        let remaining = limits.max_object_bytes.saturating_sub(self.0.syntax_bytes);
        let exact = length
            .and_then(|length| usize::try_from(length).ok())
            .and_then(|length| start.checked_add(length))
            .filter(|end| *end <= self.0.buf.len())
            .and_then(|end| stream_tail(&self.0.buf, end, remaining).map(|tail| (end, tail)));
        let (end, tail, recovered) = if let Some((end, tail)) = exact {
            (end, tail, false)
        } else {
            let bound = limits.max_object_bytes.min(limits.max_decoded_bytes);
            let scan_len = usize::try_from(bound).unwrap_or(usize::MAX);
            let cap = start.saturating_add(scan_len).min(self.0.buf.len());
            let input = self
                .0
                .buf
                .get(start..cap)
                .ok_or_else(|| malformed(start, "stream"))?;
            let mut found = if repair {
                repaired_stream_bounds(&self.0.buf, start, length, cap)
            } else {
                None
            };
            for (relative, window) in input.windows(9).enumerate() {
                if found.is_some() {
                    break;
                }
                if window != b"endstream" {
                    continue;
                }
                let marker = start.saturating_add(relative);
                if marker > start
                    && !self
                        .0
                        .buf
                        .get(marker.saturating_sub(1))
                        .copied()
                        .map(whitespace)
                        .unwrap_or(false)
                {
                    continue;
                }
                if let Some(tail) = stream_tail(&self.0.buf, marker, remaining) {
                    let mut end = marker;
                    if end > start && self.0.buf.get(end.saturating_sub(1)) == Some(&b'\n') {
                        end = end.saturating_sub(1);
                        if end > start && self.0.buf.get(end.saturating_sub(1)) == Some(&b'\r') {
                            end = end.saturating_sub(1);
                        }
                    } else if end > start && self.0.buf.get(end.saturating_sub(1)) == Some(&b'\r') {
                        end = end.saturating_sub(1);
                    }
                    found = Some((end, tail));
                    break;
                }
            }
            match found {
                Some((end, tail)) => (end, tail, true),
                None if cap < self.0.buf.len() => {
                    return Err(DocError::Refused(Refusal {
                        limit: if limits.max_object_bytes <= limits.max_decoded_bytes {
                            LimitKind::ObjectBytes
                        } else {
                            LimitKind::DecodedBytes
                        },
                        requested: bound.saturating_add(1),
                        maximum: bound,
                    }));
                }
                None => return Err(malformed(start, "endstream")),
            }
        };
        let syntax = self
            .0
            .syntax_bytes
            .saturating_add(tail.saturating_sub(end) as u64);
        check(LimitKind::ObjectBytes, syntax, limits.max_object_bytes)?;
        let tokens = u64::from(self.0.token_count).saturating_add(2);
        check(
            LimitKind::Tokens,
            tokens,
            u64::from(limits.max_tokens_per_object),
        )?;
        let data = Arc::make_mut(&mut self.0);
        data.stream = Some(start..end);
        data.syntax_bytes = syntax;
        data.token_count = u32::try_from(tokens).map_err(|_| malformed(start, "token count"))?;
        data.recovered = recovered;
        Ok(())
    }
}

impl<'o> ValueRef<'o> {
    fn node(&self) -> Option<&'o Node> {
        self.object.0.nodes.get(self.index)
    }
    pub fn kind(&self) -> ValueKind {
        match self.node().map(|node| &node.kind) {
            Some(Kind::Bool(_)) => ValueKind::Bool,
            Some(Kind::Int(_)) => ValueKind::Int,
            Some(Kind::Real(_)) => ValueKind::Real,
            Some(Kind::Name) => ValueKind::Name,
            Some(Kind::String | Kind::DecryptedString(_)) => ValueKind::String,
            Some(Kind::Array(_)) => ValueKind::Array,
            Some(Kind::Dict(_)) => ValueKind::Dict,
            Some(Kind::Ref(_)) => ValueKind::Ref,
            _ => ValueKind::Null,
        }
    }
    pub fn raw(&self) -> &'o [u8] {
        self.node()
            .and_then(|node| self.object.0.buf.get(node.span.clone()))
            .unwrap_or(&[])
    }
    pub fn span(&self) -> Range<usize> {
        self.node().map(|node| node.span.clone()).unwrap_or(0..0)
    }
    pub fn as_bool(&self) -> Option<bool> {
        match self.node().map(|node| &node.kind) {
            Some(Kind::Bool(value)) => Some(*value),
            _ => None,
        }
    }
    pub fn as_i64(&self) -> Option<i64> {
        match self.node().map(|node| &node.kind) {
            Some(Kind::Int(value)) => Some(*value),
            _ => None,
        }
    }
    pub fn as_f64(&self) -> Option<f64> {
        match self.node().map(|node| &node.kind) {
            Some(Kind::Int(value)) => Some(*value as f64),
            Some(Kind::Real(value)) => Some(*value),
            _ => None,
        }
    }
    pub fn as_name(&self) -> Option<Name<'o>> {
        (self.kind() == ValueKind::Name).then(|| Name {
            bytes: self.raw().get(1..).unwrap_or(&[]),
        })
    }
    pub fn as_str(&self) -> Option<PdfStr<'o>> {
        (self.kind() == ValueKind::String).then(|| PdfStr {
            bytes: self.raw(),
            owner: self.object.id(),
            plaintext: self.object.0.strings_plain,
            decoded: match self.node().map(|node| &node.kind) {
                Some(Kind::DecryptedString(bytes)) => Some(bytes.as_ref()),
                _ => None,
            },
        })
    }
    pub fn as_array(&self) -> Option<ArrayRef<'o>> {
        (self.kind() == ValueKind::Array).then_some(ArrayRef(*self))
    }
    pub fn as_dict(&self) -> Option<DictRef<'o>> {
        (self.kind() == ValueKind::Dict).then_some(DictRef(*self))
    }
    pub fn as_ref(&self) -> Option<ObjectRef> {
        match self.node().map(|node| &node.kind) {
            Some(Kind::Ref(value)) => Some(*value),
            _ => None,
        }
    }
    pub fn is_null(&self) -> bool {
        self.kind() == ValueKind::Null
    }
    pub fn to_owned(&self) -> Value {
        Value {
            object: self.object.clone(),
            index: self.index,
        }
    }
}

impl<'o> ArrayRef<'o> {
    fn items(&self) -> &'o [usize] {
        match self.0.node().map(|node| &node.kind) {
            Some(Kind::Array(items)) => items,
            _ => &[],
        }
    }
    pub fn len(&self) -> usize {
        self.items().len()
    }
    pub fn is_empty(&self) -> bool {
        self.items().is_empty()
    }
    pub fn get(&self, i: usize) -> Option<ValueRef<'o>> {
        self.items().get(i).map(|index| ValueRef {
            object: self.0.object,
            index: *index,
        })
    }
    pub fn iter(&self) -> impl Iterator<Item = ValueRef<'o>> + 'o {
        let object = self.0.object;
        self.items().iter().map(move |index| ValueRef {
            object,
            index: *index,
        })
    }
}

impl<'o> DictRef<'o> {
    fn entries(&self) -> &'o [(Range<usize>, usize)] {
        match self.0.node().map(|node| &node.kind) {
            Some(Kind::Dict(entries)) => entries,
            _ => &[],
        }
    }
    pub fn len(&self) -> usize {
        self.entries().len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries().is_empty()
    }
    pub fn get(&self, key: &[u8]) -> Option<ValueRef<'o>> {
        self.entries().iter().rev().find_map(|(range, index)| {
            let name = Name {
                bytes: self.0.object.0.buf.get(range.clone()).unwrap_or(&[]),
            };
            name.eq_bytes(key).then_some(ValueRef {
                object: self.0.object,
                index: *index,
            })
        })
    }
    pub fn iter(&self) -> impl Iterator<Item = (Name<'o>, ValueRef<'o>)> + 'o {
        let object = self.0.object;
        self.entries().iter().map(move |(range, index)| {
            (
                Name {
                    bytes: object.0.buf.get(range.clone()).unwrap_or(&[]),
                },
                ValueRef {
                    object,
                    index: *index,
                },
            )
        })
    }
}

impl<'o> Name<'o> {
    pub fn encoded(&self) -> &'o [u8] {
        self.bytes
    }
    pub fn eq_bytes(&self, decoded: &[u8]) -> bool {
        NameBytes(self.bytes).eq(decoded.iter().copied())
    }
    pub fn decoded(&self) -> Cow<'o, [u8]> {
        if self.bytes.contains(&b'#') {
            Cow::Owned(NameBytes(self.bytes).collect())
        } else {
            Cow::Borrowed(self.bytes)
        }
    }
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

impl<'o> PdfStr<'o> {
    pub fn raw(&self) -> &'o [u8] {
        self.bytes
    }
    pub fn owner(&self) -> Option<ObjectRef> {
        self.owner
    }
    pub(crate) fn plaintext(&self) -> bool {
        self.plaintext
    }

    /// Apply a caller-supplied decryption step after PDF escape decoding.
    pub fn decode_with<F>(&self, decrypt: F) -> Vec<u8>
    where
        F: FnOnce(Vec<u8>) -> Vec<u8>,
    {
        decrypt(self.decode())
    }
    pub fn is_hex(&self) -> bool {
        self.bytes.first() == Some(&b'<')
    }
    pub fn decode(&self) -> Vec<u8> {
        if let Some(bytes) = self.decoded {
            return bytes.to_vec();
        }
        let input = self
            .bytes
            .get(1..self.bytes.len().saturating_sub(1))
            .unwrap_or(&[]);
        let mut out = Vec::new();
        if self.is_hex() {
            let mut high = None;
            for digit in input.iter().copied().filter_map(hex) {
                if let Some(first) = high.take() {
                    out.push((first << 4) | digit);
                } else {
                    high = Some(digit);
                }
            }
            if let Some(first) = high {
                out.push(first << 4);
            }
            return out;
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
                    b'n' => out.push(b'\n'),
                    b'r' => out.push(b'\r'),
                    b't' => out.push(b'\t'),
                    b'b' => out.push(8),
                    b'f' => out.push(12),
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
                                    value = value
                                        .saturating_mul(8)
                                        .saturating_add(u16::from(next - b'0'));
                                    pos = pos.saturating_add(1);
                                }
                                _ => break,
                            }
                        }
                        out.push(value as u8);
                    }
                    other => out.push(other),
                }
            } else if byte == b'\r' {
                if input.get(pos) == Some(&b'\n') {
                    pos = pos.saturating_add(1);
                }
                out.push(b'\n');
            } else {
                out.push(byte);
            }
        }
        out
    }
}

impl Value {
    pub fn get(&self) -> ValueRef<'_> {
        ValueRef {
            object: &self.object,
            index: self.index,
        }
    }
    pub fn object(&self) -> &Object {
        &self.object
    }
}

pub fn parse_value(
    buf: &Arc<[u8]>,
    range: Range<usize>,
    id: Option<ObjectRef>,
    limits: &Limits,
) -> Result<Object, DocError> {
    let mut cursor = Cursor::at(buf, range, limits)?;
    let (nodes, root) = graph(&mut cursor, limits)?;
    Ok(Object(Arc::new(ObjectData {
        buf: buf.clone(),
        nodes,
        root,
        id,
        stream: None,
        stream_start: None,
        syntax_bytes: cursor.used() as u64,
        token_count: cursor.count,
        recovered: false,
        syntax_recovered: cursor.recovered,
        strings_plain: true,
    })))
}

pub fn parse_indirect(
    buf: &Arc<[u8]>,
    offset: usize,
    expected: ObjectRef,
    limits: &Limits,
) -> Result<Object, DocError> {
    let mut object = parse_indirect_pending(buf, offset, expected, limits)?;
    if object.is_stream() && object.stream_raw().is_none() {
        object.finish_stream(None, limits)?;
    }
    Ok(object)
}

pub(crate) fn parse_indirect_repair(
    buf: &Arc<[u8]>,
    offset: usize,
    expected: ObjectRef,
    limits: &Limits,
) -> Result<Object, DocError> {
    let mut object = parse_indirect_pending_repair(buf, offset, expected, limits)?;
    if object.is_stream() && object.stream_raw().is_none() {
        object.finish_stream_repair(None, limits)?;
    }
    Ok(object)
}

pub(crate) fn parse_indirect_pending_repair(
    buf: &Arc<[u8]>,
    offset: usize,
    expected: ObjectRef,
    limits: &Limits,
) -> Result<Object, DocError> {
    parse_indirect_pending_with_mode(buf, offset, expected, limits, true)
}

pub(crate) fn parse_indirect_pending(
    buf: &Arc<[u8]>,
    offset: usize,
    expected: ObjectRef,
    limits: &Limits,
) -> Result<Object, DocError> {
    parse_indirect_pending_with_mode(buf, offset, expected, limits, false)
}

fn parse_indirect_pending_with_mode(
    buf: &Arc<[u8]>,
    offset: usize,
    expected: ObjectRef,
    limits: &Limits,
    repair: bool,
) -> Result<Object, DocError> {
    let mut cursor = Cursor::at(buf, offset..buf.len(), limits)?;
    cursor.repair = repair;
    let first = cursor.required()?;
    let second = cursor.required()?;
    let marker = cursor.required()?;
    let number = unsigned(first.bytes()).and_then(|n| u32::try_from(n).ok());
    let generation = unsigned(second.bytes()).and_then(|n| u16::try_from(n).ok());
    if first.span().start() != 0 || marker.bytes() != b"obj" {
        return Err(malformed(offset, "object header"));
    }
    let (Some(number), Some(generation)) = (number, generation) else {
        return Err(malformed(offset, "object header"));
    };
    if number != expected.number() {
        return Err(malformed(offset, "object number"));
    }
    let (nodes, root) = graph(&mut cursor, limits)?;
    let end = match cursor.required() {
        Ok(end) => Some(end),
        Err(error) if repair && !crate::repair::is_hard_error(&error) => None,
        Err(error) => return Err(error),
    };
    let mut stream_start = None;
    let mut repaired_tail = None;
    if end.as_ref().is_some_and(|end| end.bytes() == b"stream") {
        if !matches!(nodes.get(root).map(|node| &node.kind), Some(Kind::Dict(_))) {
            return Err(malformed(cursor.position(), "stream dictionary"));
        }
        let mut start = cursor.position();
        match buf.get(start).copied() {
            Some(b'\r') => {
                start = start.saturating_add(1);
                if buf.get(start) == Some(&b'\n') {
                    start = start.saturating_add(1);
                }
            }
            Some(b'\n') => start = start.saturating_add(1),
            _ => return Err(malformed(start, "stream EOL")),
        }
        check(
            LimitKind::ObjectBytes,
            start.saturating_sub(offset) as u64,
            limits.max_object_bytes,
        )?;
        stream_start = Some(start);
    } else if !end.as_ref().is_some_and(|end| end.bytes() == b"endobj") {
        if !repair {
            return Err(malformed(cursor.position(), "endobj"));
        }
        let start = nodes
            .get(root)
            .map(|node| node.span.end)
            .ok_or_else(|| malformed(offset, "object root"))?;
        repaired_tail = Some(repaired_object_tail(buf, start, offset, limits)?);
    }
    let syntax_bytes = stream_start
        .or(repaired_tail)
        .unwrap_or(cursor.position())
        .saturating_sub(offset) as u64;
    let mut object = Object(Arc::new(ObjectData {
        buf: buf.clone(),
        nodes,
        root,
        id: Some(ObjectRef::new(number, generation)),
        stream: None,
        stream_start,
        syntax_bytes,
        token_count: cursor.count,
        recovered: repaired_tail.is_some(),
        syntax_recovered: cursor.recovered,
        strings_plain: false,
    }));
    if object
        .dict()
        .and_then(|dict| dict.get(b"Type"))
        .and_then(|value| value.as_name())
        .is_some_and(|name| name.eq_bytes(b"XRef"))
    {
        Arc::make_mut(&mut object.0).strings_plain = true;
    }
    if object.is_stream() {
        let length = object.dict().and_then(|dict| dict.get(b"Length"));
        let indirect = length.and_then(|value| value.as_ref()).is_some();
        let direct = length
            .and_then(|value| value.as_i64())
            .and_then(|n| u64::try_from(n).ok());
        if !indirect {
            if repair {
                object.finish_stream_repair(direct, limits)?;
            } else {
                object.finish_stream(direct, limits)?;
            }
        }
    }
    Ok(object)
}

enum Frame {
    Array {
        start: usize,
        items: Vec<usize>,
    },
    Dict {
        start: usize,
        entries: Vec<(Range<usize>, usize)>,
        key: Option<Range<usize>>,
    },
}

fn graph(cursor: &mut Cursor<'_>, limits: &Limits) -> Result<(Vec<Node>, usize), DocError> {
    let mut nodes = Vec::new();
    let mut frames = Vec::new();
    let mut root = None;
    loop {
        if frames.is_empty() {
            if let Some(root) = root {
                return Ok((nodes, root));
            }
        }
        let token = cursor.required()?;
        let span = cursor.span(token);
        if matches!(frames.last(), Some(Frame::Dict { key: None, .. }))
            && token.kind() != TokenKind::DictionaryEnd
        {
            if token.kind() != TokenKind::Name {
                if !cursor.repair || object_marker(token.bytes()) {
                    return Err(malformed(span.start, "dictionary key"));
                }
                if matches!(
                    token.kind(),
                    TokenKind::ArrayStart | TokenKind::DictionaryStart
                ) {
                    skip_container(cursor, token.kind(), frames.len(), limits)?;
                }
                cursor.recovered = true;
                continue;
            }
            if let Some(Frame::Dict { key, .. }) = frames.last_mut() {
                *key = Some(span.start.saturating_add(1)..span.end);
            }
            continue;
        }
        let kind = match token.kind() {
            TokenKind::ArrayStart | TokenKind::DictionaryStart => {
                check(
                    LimitKind::Depth,
                    frames.len().saturating_add(1) as u64,
                    u64::from(limits.max_depth),
                )?;
                if token.kind() == TokenKind::ArrayStart {
                    frames.push(Frame::Array {
                        start: span.start,
                        items: Vec::new(),
                    });
                } else {
                    frames.push(Frame::Dict {
                        start: span.start,
                        entries: Vec::new(),
                        key: None,
                    });
                }
                continue;
            }
            TokenKind::ArrayEnd | TokenKind::DictionaryEnd => {
                let frame = frames
                    .pop()
                    .ok_or_else(|| malformed(span.start, "container end"))?;
                let (start, kind) = match frame {
                    Frame::Array { start, items } if token.kind() == TokenKind::ArrayEnd => {
                        (start, Kind::Array(items))
                    }
                    Frame::Dict {
                        start,
                        entries,
                        key: None,
                    } if token.kind() == TokenKind::DictionaryEnd => (start, Kind::Dict(entries)),
                    Frame::Dict {
                        start,
                        mut entries,
                        key: Some(key),
                    } if cursor.repair && token.kind() == TokenKind::DictionaryEnd => {
                        let index = nodes.len();
                        nodes.push(Node {
                            span: span.start..span.start,
                            kind: Kind::Null,
                        });
                        entries.push((key, index));
                        cursor.recovered = true;
                        (start, Kind::Dict(entries))
                    }
                    _ => return Err(malformed(span.start, "container end")),
                };
                attach(
                    Node {
                        span: start..span.end,
                        kind,
                    },
                    &mut nodes,
                    &mut frames,
                    &mut root,
                )?;
                continue;
            }
            TokenKind::Name => Kind::Name,
            TokenKind::LiteralString | TokenKind::HexString => Kind::String,
            TokenKind::Number => {
                let mut probe = cursor.clone();
                let reference = match probe.next() {
                    Ok(Some(second)) if second.kind() == TokenKind::Number => match probe.next() {
                        Ok(Some(marker)) if marker.bytes() == b"R" => Some((second, marker)),
                        _ => None,
                    },
                    _ => None,
                };
                if let Some((second, marker)) = reference {
                    let number = unsigned(token.bytes()).and_then(|n| u32::try_from(n).ok());
                    let generation = unsigned(second.bytes()).and_then(|n| u16::try_from(n).ok());
                    let (Some(number), Some(generation)) = (number, generation) else {
                        return Err(malformed(span.start, "reference"));
                    };
                    let end = probe.span(marker).end;
                    *cursor = probe;
                    attach(
                        Node {
                            span: span.start..end,
                            kind: Kind::Ref(ObjectRef::new(number, generation)),
                        },
                        &mut nodes,
                        &mut frames,
                        &mut root,
                    )?;
                    continue;
                }
                if cursor.repair && matches!(frames.last(), Some(Frame::Dict { key: Some(_), .. }))
                {
                    let number = unsigned(token.bytes()).and_then(|n| u32::try_from(n).ok());
                    if let Some(number) = number.filter(|number| *number != 0) {
                        let mut probe = cursor.clone();
                        if let Some(second) = probe.next()? {
                            let candidate = if second.bytes() == b"R" {
                                Some((ObjectRef::new(number, 0), probe))
                            } else if second.kind() == TokenKind::Number {
                                let generation =
                                    unsigned(second.bytes()).and_then(|n| u16::try_from(n).ok());
                                let after_second = probe.clone();
                                match probe.next()? {
                                    Some(marker)
                                        if matches!(
                                            marker.kind(),
                                            TokenKind::Name | TokenKind::DictionaryEnd
                                        ) =>
                                    {
                                        generation.map(|generation| {
                                            (ObjectRef::new(number, generation), after_second)
                                        })
                                    }
                                    _ => None,
                                }
                            } else {
                                None
                            };
                            if let Some((id, mut probe)) = candidate {
                                let end = probe.position();
                                probe.recovered = true;
                                *cursor = probe;
                                attach(
                                    Node {
                                        span: span.start..end,
                                        kind: Kind::Ref(id),
                                    },
                                    &mut nodes,
                                    &mut frames,
                                    &mut root,
                                )?;
                                continue;
                            }
                        }
                    }
                }
                number_kind(token.bytes())
            }
            TokenKind::Keyword => match token.bytes() {
                b"true" => Kind::Bool(true),
                b"false" => Kind::Bool(false),
                b"null" => Kind::Null,
                bytes if matches!(bytes.first(), Some(b'+' | b'-' | b'.' | b'0'..=b'9')) => {
                    Kind::Null
                }
                bytes if cursor.repair && !object_marker(bytes) => {
                    cursor.recovered = true;
                    Kind::Null
                }
                _ => return Err(malformed(span.start, "value")),
            },
        };
        attach(Node { span, kind }, &mut nodes, &mut frames, &mut root)?;
    }
}

fn object_marker(bytes: &[u8]) -> bool {
    matches!(
        bytes,
        b"obj" | b"endobj" | b"stream" | b"endstream" | b"xref" | b"trailer" | b"startxref"
    )
}

fn skip_container(
    cursor: &mut Cursor<'_>,
    first: TokenKind,
    outer_depth: usize,
    limits: &Limits,
) -> Result<(), DocError> {
    let mut stack = Vec::new();
    stack.push(first);
    check(
        LimitKind::Depth,
        outer_depth.saturating_add(1) as u64,
        u64::from(limits.max_depth),
    )?;
    while let Some(open) = stack.last().copied() {
        let token = cursor.required()?;
        match token.kind() {
            TokenKind::ArrayStart | TokenKind::DictionaryStart => {
                check(
                    LimitKind::Depth,
                    outer_depth.saturating_add(stack.len()).saturating_add(1) as u64,
                    u64::from(limits.max_depth),
                )?;
                stack.push(token.kind());
            }
            TokenKind::ArrayEnd if open == TokenKind::ArrayStart => {
                stack.pop();
            }
            TokenKind::DictionaryEnd if open == TokenKind::DictionaryStart => {
                stack.pop();
            }
            TokenKind::ArrayEnd | TokenKind::DictionaryEnd => {
                return Err(malformed(cursor.span(token).start, "container end"));
            }
            _ if object_marker(token.bytes()) => {
                return Err(malformed(cursor.span(token).start, "container end"));
            }
            _ => {}
        }
    }
    Ok(())
}

fn attach(
    node: Node,
    nodes: &mut Vec<Node>,
    frames: &mut [Frame],
    root: &mut Option<usize>,
) -> Result<(), DocError> {
    let offset = node.span.start;
    let index = nodes.len();
    nodes.push(node);
    match frames.last_mut() {
        Some(Frame::Array { items, .. }) => items.push(index),
        Some(Frame::Dict { entries, key, .. }) => {
            entries.push((
                key.take()
                    .ok_or_else(|| malformed(offset, "dictionary key"))?,
                index,
            ));
        }
        None => {
            if root.replace(index).is_some() {
                return Err(malformed(offset, "multiple roots"));
            }
        }
    }
    Ok(())
}

fn number_kind(bytes: &[u8]) -> Kind {
    let Ok(text) = core::str::from_utf8(bytes) else {
        return Kind::Null;
    };
    if !bytes.contains(&b'.') {
        if let Ok(value) = text.parse::<i64>() {
            return Kind::Int(value);
        }
    }
    match text.parse::<f64>() {
        Ok(value) if value.is_finite() => Kind::Real(value),
        _ => Kind::Null,
    }
}

pub(crate) fn unsigned(bytes: &[u8]) -> Option<u64> {
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

#[derive(Clone)]
pub(crate) struct Cursor<'a> {
    lexer: Lexer<'a>,
    base: usize,
    count: u32,
    limits: Limits,
    cut: bool,
    repair: bool,
    recovered: bool,
}

impl<'a> Cursor<'a> {
    pub(crate) fn at(
        buf: &'a [u8],
        range: Range<usize>,
        limits: &Limits,
    ) -> Result<Self, DocError> {
        if range.start > range.end || range.end > buf.len() {
            return Err(malformed(range.start, "value range"));
        }
        let bound = usize::try_from(limits.max_object_bytes).unwrap_or(usize::MAX);
        let cap = range
            .start
            .saturating_add(bound.saturating_add(1))
            .min(range.end);
        let input = buf
            .get(range.start..cap)
            .ok_or_else(|| malformed(range.start, "value range"))?;
        Ok(Self {
            lexer: Lexer::with_limits(
                input,
                LexerLimits::new(
                    limits.max_tokens_per_object as usize,
                    bound,
                    limits.max_depth as usize,
                ),
            ),
            base: range.start,
            count: 0,
            limits: *limits,
            cut: cap < range.end,
            repair: false,
            recovered: false,
        })
    }
    pub(crate) fn position(&self) -> usize {
        self.base.saturating_add(self.lexer.offset())
    }
    fn used(&self) -> usize {
        self.lexer.offset()
    }
    fn span(&self, token: Token<'_>) -> Range<usize> {
        self.base.saturating_add(token.span().start())..self.base.saturating_add(token.span().end())
    }
    pub(crate) fn next(&mut self) -> Result<Option<Token<'a>>, DocError> {
        match self.lexer.next() {
            Some(Ok(token)) => {
                check(
                    LimitKind::ObjectBytes,
                    token.span().end() as u64,
                    self.limits.max_object_bytes,
                )?;
                self.count = self.count.saturating_add(1);
                Ok(Some(token))
            }
            Some(Err(error)) => {
                let (kind, maximum) = match error {
                    LexError::TokenBudgetExceeded { .. } => (
                        Some(LimitKind::Tokens),
                        u64::from(self.limits.max_tokens_per_object),
                    ),
                    LexError::TokenTooLong { .. } => {
                        (Some(LimitKind::ObjectBytes), self.limits.max_object_bytes)
                    }
                    LexError::LiteralNestingTooDeep { .. } => {
                        (Some(LimitKind::Depth), u64::from(self.limits.max_depth))
                    }
                    LexError::UnterminatedLiteralString { .. }
                    | LexError::UnterminatedHexString { .. }
                        if self.cut =>
                    {
                        (Some(LimitKind::ObjectBytes), self.limits.max_object_bytes)
                    }
                    _ => (None, 0),
                };
                if let Some(limit) = kind {
                    Err(DocError::Refused(Refusal {
                        limit,
                        requested: maximum.saturating_add(1),
                        maximum,
                    }))
                } else {
                    Err(malformed(self.position(), "token"))
                }
            }
            None if self.cut => Err(DocError::Refused(Refusal {
                limit: LimitKind::ObjectBytes,
                requested: self.limits.max_object_bytes.saturating_add(1),
                maximum: self.limits.max_object_bytes,
            })),
            None => Ok(None),
        }
    }
    pub(crate) fn required(&mut self) -> Result<Token<'a>, DocError> {
        self.next()?
            .ok_or_else(|| malformed(self.position(), "unexpected EOF"))
    }
}

pub(crate) fn whitespace(byte: u8) -> bool {
    matches!(byte, 0 | 9 | 10 | 12 | 13 | 32)
}

pub(crate) fn keyword_at(buf: &[u8], offset: usize, keyword: &[u8]) -> bool {
    let Some(end) = offset.checked_add(keyword.len()) else {
        return false;
    };
    if buf.get(offset..end) != Some(keyword) {
        return false;
    }
    buf.get(end)
        .copied()
        .map(|byte| whitespace(byte) || b"()<>[]{}/%".contains(&byte))
        .unwrap_or(true)
}

fn skip_trivia(buf: &[u8], mut pos: usize, cap: usize) -> usize {
    while pos < cap {
        match buf.get(pos).copied() {
            Some(byte) if whitespace(byte) => pos = pos.saturating_add(1),
            Some(b'%') => {
                while pos < cap && !matches!(buf.get(pos), Some(b'\r' | b'\n')) {
                    pos = pos.saturating_add(1);
                }
            }
            _ => break,
        }
    }
    pos
}

fn stream_tail(buf: &[u8], end: usize, maximum: u64) -> Option<usize> {
    if end > buf.len() {
        return None;
    }
    let bound = usize::try_from(maximum).unwrap_or(usize::MAX);
    let cap = end.saturating_add(bound).min(buf.len());
    let marker = skip_trivia(buf, end, cap);
    if !keyword_at(buf, marker, b"endstream") {
        return None;
    }
    let after = marker.checked_add(9)?;
    if after > cap {
        return None;
    }
    let endobj = skip_trivia(buf, after, cap);
    if !keyword_at(buf, endobj, b"endobj") {
        return None;
    }
    let tail = endobj.checked_add(6)?;
    (tail <= cap).then_some(tail)
}

// Repair scans stop at the next header rather than borrowing a later stream's
// terminator. Payloads remain slices of the original input.
fn repaired_stream_bounds(
    buf: &[u8],
    start: usize,
    length: Option<u64>,
    cap: usize,
) -> Option<(usize, usize)> {
    let mut pos = start;
    while pos < cap {
        let boundary = pos == start
            || buf
                .get(pos.saturating_sub(1))
                .copied()
                .is_some_and(whitespace);
        if boundary {
            if keyword_at(buf, pos, b"endstream")
                && pos.checked_add(9).is_some_and(|end| end <= cap)
            {
                if let Some(tail) = stream_tail(buf, pos, cap.saturating_sub(pos) as u64) {
                    return Some((repaired_payload_end(buf, start, pos, length), tail));
                }
                let after = pos + 9;
                let next = skip_trivia(buf, after, cap);
                if next == buf.len() || object_boundary_at(buf, next, cap) {
                    return Some((repaired_payload_end(buf, start, pos, length), after));
                }
                if next.checked_add(6).is_some_and(|end| end <= cap)
                    && damaged_keyword_at(buf, next, b"endobj")
                {
                    return Some((repaired_payload_end(buf, start, pos, length), next + 6));
                }
            }
            if keyword_at(buf, pos, b"endobj")
                && pos.checked_add(6).is_some_and(|end| end <= cap)
                && strip_tail_keyword(buf, start, pos, b"endstream").is_some()
            {
                return Some((repaired_payload_end(buf, start, pos, length), pos + 6));
            }
            if object_boundary_at(buf, pos, cap) {
                return Some((repaired_payload_end(buf, start, pos, length), pos));
            }
        }
        pos = pos.saturating_add(1);
    }
    if cap == buf.len() {
        Some((repaired_payload_end(buf, start, cap, length), cap))
    } else {
        None
    }
}

fn repaired_payload_end(buf: &[u8], start: usize, boundary: usize, length: Option<u64>) -> usize {
    let mut end = strip_tail_keyword(buf, start, boundary, b"endobj").unwrap_or(boundary);
    end = strip_tail_keyword(buf, start, end, b"endstream").unwrap_or(end);
    if let Some(declared) = length
        .and_then(|n| usize::try_from(n).ok())
        .and_then(|n| start.checked_add(n))
        .filter(|n| *n <= end)
    {
        if buf
            .get(declared..end)
            .is_some_and(|bytes| bytes.iter().copied().all(whitespace))
        {
            return declared;
        }
    }
    if end > start && buf.get(end - 1) == Some(&b'\n') {
        end -= 1;
        if end > start && buf.get(end - 1) == Some(&b'\r') {
            end -= 1;
        }
    } else if end > start && buf.get(end - 1) == Some(&b'\r') {
        end -= 1;
    }
    end
}

fn strip_tail_keyword(buf: &[u8], start: usize, mut end: usize, keyword: &[u8]) -> Option<usize> {
    while end > start && buf.get(end - 1).copied().is_some_and(whitespace) {
        end -= 1;
    }
    let marker = end
        .checked_sub(keyword.len())
        .filter(|marker| *marker >= start)?;
    if marker > start && !buf.get(marker - 1).copied().is_some_and(whitespace) {
        return None;
    }
    let word = buf.get(marker..end)?;
    // A single damaged byte in either closing keyword is enough to lose the
    // strict tail. Do not strip arbitrary trailing payload words.
    (word.iter().zip(keyword).filter(|(a, b)| a != b).count() <= 1).then_some(marker)
}

fn damaged_keyword_at(buf: &[u8], pos: usize, keyword: &[u8]) -> bool {
    let Some(end) = pos.checked_add(keyword.len()) else {
        return false;
    };
    let Some(word) = buf.get(pos..end) else {
        return false;
    };
    word.iter().zip(keyword).filter(|(a, b)| a != b).count() <= 1
        && buf
            .get(end)
            .copied()
            .map(|byte| whitespace(byte) || b"()<>[]{}/%".contains(&byte))
            .unwrap_or(true)
}

fn object_boundary_at(buf: &[u8], pos: usize, cap: usize) -> bool {
    if matches!(buf.get(pos), Some(b'+' | b'0'..=b'9'))
        && crate::xref::header_at(buf.get(..cap).unwrap_or(&[]), pos).is_some()
    {
        return true;
    }
    [
        b"xref".as_slice(),
        b"trailer".as_slice(),
        b"startxref".as_slice(),
    ]
    .iter()
    .any(|keyword| pos.saturating_add(keyword.len()) <= cap && keyword_at(buf, pos, keyword))
}

fn repaired_object_tail(
    buf: &[u8],
    start: usize,
    offset: usize,
    limits: &Limits,
) -> Result<usize, DocError> {
    let bound = usize::try_from(limits.max_object_bytes).unwrap_or(usize::MAX);
    let cap = offset.saturating_add(bound).min(buf.len());
    let mut pos = start;
    while pos < cap {
        if (pos == start
            || buf
                .get(pos.saturating_sub(1))
                .copied()
                .is_some_and(whitespace))
            && object_boundary_at(buf, pos, cap)
        {
            return Ok(pos);
        }
        pos = pos.saturating_add(1);
    }
    if cap < buf.len() {
        return Err(DocError::Refused(Refusal {
            limit: LimitKind::ObjectBytes,
            requested: limits.max_object_bytes.saturating_add(1),
            maximum: limits.max_object_bytes,
        }));
    }
    Ok(cap)
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
