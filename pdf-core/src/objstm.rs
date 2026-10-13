use alloc::collections::BTreeSet;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::Range;

use crate::error::{malformed, DocError};
use crate::limits::{check, LimitKind, Limits};
use crate::object::{parse_value, unsigned, Cursor, Object};
use crate::work::ObjectRef;

#[derive(Clone)]
pub(crate) struct ObjectStream(Arc<ObjectStreamData>);

struct ObjectStreamData {
    buf: Arc<[u8]>,
    members: Vec<(u32, Range<usize>)>,
}

impl ObjectStream {
    pub(crate) fn parse(
        object: &Object,
        buf: Arc<[u8]>,
        limits: &Limits,
    ) -> Result<Self, DocError> {
        let wrong = || DocError::WrongType {
            id: object.id(),
            expected: "object stream",
        };
        let dict = object.dict().ok_or_else(wrong)?;
        if !dict
            .get(b"Type")
            .and_then(|value| value.as_name())
            .map(|name| name.eq_bytes(b"ObjStm"))
            .unwrap_or(false)
        {
            return Err(wrong());
        }
        let n = dict
            .get(b"N")
            .and_then(|value| value.as_i64())
            .and_then(|n| u64::try_from(n).ok())
            .ok_or_else(wrong)?;
        check(
            LimitKind::ObjStmMembers,
            n,
            u64::from(limits.max_objstm_members),
        )?;
        let first = dict
            .get(b"First")
            .and_then(|value| value.as_i64())
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(wrong)?;
        if first > buf.len() {
            return Err(malformed(first, "object stream First"));
        }
        let count = usize::try_from(n).map_err(|_| malformed(first, "object stream N"))?;
        let mut cursor = Cursor::at(&buf, 0..first, limits)?;
        let mut pairs = Vec::new();
        let mut seen = BTreeSet::new();
        for _ in 0..count {
            let number = unsigned(cursor.required()?.bytes())
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| malformed(cursor.position(), "object stream header"))?;
            let offset = unsigned(cursor.required()?.bytes())
                .and_then(|n| usize::try_from(n).ok())
                .and_then(|n| first.checked_add(n))
                .filter(|n| *n <= buf.len())
                .ok_or_else(|| malformed(cursor.position(), "object stream offset"))?;
            if !seen.insert(number) {
                return Err(malformed(cursor.position(), "object stream duplicate"));
            }
            if pairs
                .last()
                .map(|pair: &(u32, usize)| pair.1 > offset)
                .unwrap_or(false)
            {
                return Err(malformed(cursor.position(), "object stream order"));
            }
            pairs.push((number, offset));
        }
        if cursor.next()?.is_some() {
            return Err(malformed(cursor.position(), "object stream header"));
        }
        let mut members = Vec::new();
        for (index, (number, start)) in pairs.iter().enumerate() {
            let end = pairs
                .get(index.saturating_add(1))
                .map(|pair| pair.1)
                .unwrap_or(buf.len());
            members.push((*number, *start..end));
        }
        Ok(Self(Arc::new(ObjectStreamData { buf, members })))
    }

    /// Repair keeps header slots stable. Sequential lexing supplies missing or
    /// damaged offsets without renumbering the surviving members.
    pub(crate) fn parse_repair(
        object: &Object,
        buf: Arc<[u8]>,
        limits: &Limits,
    ) -> Result<(Self, bool), DocError> {
        let strict = match Self::parse(object, buf.clone(), limits) {
            Ok(stream) => Ok(stream),
            Err(error) if crate::repair::is_hard_error(&error) => return Err(error),
            Err(error) => Err(error),
        };
        let wrong = || DocError::WrongType {
            id: object.id(),
            expected: "object stream",
        };
        let dict = object.dict().ok_or_else(wrong)?;
        if !dict
            .get(b"Type")
            .and_then(|value| value.as_name())
            .map(|name| name.eq_bytes(b"ObjStm"))
            .unwrap_or(false)
            && !(object.is_stream() && dict.get(b"N").is_some() && dict.get(b"First").is_some())
        {
            return Err(wrong());
        }
        let n = dict
            .get(b"N")
            .and_then(|value| value.as_i64())
            .and_then(|n| u64::try_from(n).ok())
            .ok_or_else(wrong)?;
        check(
            LimitKind::ObjStmMembers,
            n,
            u64::from(limits.max_objstm_members),
        )?;
        let declared_first = dict
            .get(b"First")
            .and_then(|value| value.as_i64())
            .and_then(|n| usize::try_from(n).ok());
        let first = repair_first(&buf, n, declared_first, limits)?
            .ok_or_else(|| malformed(declared_first.unwrap_or(0), "object stream First"))?;
        check(
            LimitKind::ObjectBytes,
            first as u64,
            limits.max_object_bytes,
        )?;

        let mut pos = 0usize;
        let mut tokens = 0u64;
        let mut pairs = Vec::new();
        while let Some(number) = header_word(&buf, &mut pos, first) {
            tokens = tokens.saturating_add(1);
            check(
                LimitKind::Tokens,
                tokens,
                u64::from(limits.max_tokens_per_object),
            )?;
            let offset = header_word(&buf, &mut pos, first);
            if offset.is_some() {
                tokens = tokens.saturating_add(1);
                check(
                    LimitKind::Tokens,
                    tokens,
                    u64::from(limits.max_tokens_per_object),
                )?;
            }
            check(
                LimitKind::ObjStmMembers,
                (pairs.len() as u64).saturating_add(1),
                u64::from(limits.max_objstm_members),
            )?;
            let number = unsigned(number).and_then(|n| u32::try_from(n).ok());
            let start = offset
                .and_then(unsigned)
                .and_then(|n| usize::try_from(n).ok())
                .and_then(|n| first.checked_add(n))
                .filter(|n| *n < buf.len());
            pairs.push((number, start));
        }

        let mut members = Vec::new();
        let mut seen = BTreeSet::new();
        let mut starts = BTreeSet::new();
        let mut recovered =
            strict.is_err() || declared_first != Some(first) || pairs.len() as u64 != n;
        let mut sequential = Some(first);
        let mut usable = 0usize;
        for (number, declared) in pairs {
            let index =
                u32::try_from(members.len()).map_err(|_| malformed(pos, "object stream index"))?;
            // Even an unreadable number owns a body value and an xref slot.
            members.push((0, 0..0));
            let id = number
                .filter(|number| *number != 0)
                .map(|number| ObjectRef::new(number, 0));
            let mut member = None;
            let mut from_sequential = false;
            if let Some(start) = sequential {
                match parse_value(&buf, start..buf.len(), id, limits) {
                    Ok(value) => {
                        from_sequential = true;
                        member = Some(value);
                    }
                    Err(error) if crate::repair::is_hard_error(&error) => return Err(error),
                    Err(_) => recovered = true,
                }
            }
            if member.is_none() {
                if let Some(start) = declared {
                    match parse_value(&buf, start..buf.len(), id, limits) {
                        Ok(value) => member = Some(value),
                        Err(error) if crate::repair::is_hard_error(&error) => return Err(error),
                        Err(_) => recovered = true,
                    }
                }
            }
            let Some(member) = member else {
                sequential = None;
                recovered = true;
                continue;
            };
            let range = member.root().span();
            sequential = if from_sequential {
                Some(range.end)
            } else {
                None
            };
            let Some(id) = id else {
                recovered = true;
                continue;
            };
            if seen.contains(&id.number()) || starts.contains(&range.start) {
                recovered = true;
                continue;
            }
            // Whitespace before a value is permitted in an offset. A value at
            // another location was recovered by walking the body, not the table.
            if !declared.is_some_and(|start| {
                start <= range.start
                    && buf
                        .get(start..range.start)
                        .is_some_and(|bytes| bytes.iter().copied().all(crate::object::whitespace))
            }) {
                recovered = true;
            }
            if let Ok(stream) = &strict {
                match stream.member(index, id, limits) {
                    Ok(value) if value.root().span() == range => {}
                    Ok(_) => recovered = true,
                    Err(error) if crate::repair::is_hard_error(&error) => return Err(error),
                    Err(_) => recovered = true,
                }
            }
            if let Some(slot) = members.get_mut(index as usize) {
                *slot = (id.number(), range.clone());
            }
            seen.insert(id.number());
            starts.insert(range.start);
            usable = usable.saturating_add(1);
        }
        if usable == 0 && (n != 0 || !members.is_empty()) {
            return Err(match strict {
                Err(error) => error,
                Ok(_) => malformed(first, "object stream members"),
            });
        }
        Ok((Self(Arc::new(ObjectStreamData { buf, members })), recovered))
    }

    pub(crate) fn entries(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        self.0
            .members
            .iter()
            .enumerate()
            .filter_map(|(index, (number, _))| {
                if *number == 0 {
                    None
                } else {
                    u32::try_from(index).ok().map(|index| (index, *number))
                }
            })
    }

    pub(crate) fn member(
        &self,
        index: u32,
        expected: ObjectRef,
        limits: &Limits,
    ) -> Result<Object, DocError> {
        let (number, range) = self
            .0
            .members
            .get(index as usize)
            .ok_or_else(|| malformed(0, "object stream index"))?;
        if *number != expected.number() {
            return Err(malformed(range.start, "object stream number"));
        }
        let object = parse_value(
            &self.0.buf,
            range.clone(),
            Some(ObjectRef::new(*number, 0)),
            limits,
        )?;
        let end = object.root().span().end;
        let mut tail = Cursor::at(&self.0.buf, end..range.end, limits)?;
        if tail.next()?.is_some() {
            return Err(malformed(end, "object stream member"));
        }
        Ok(object)
    }

    pub(crate) fn cost(&self) -> u64 {
        (self.0.buf.len() as u64).saturating_add(
            (self.0.members.len() as u64)
                .saturating_mul(core::mem::size_of::<(u32, Range<usize>)>() as u64),
        )
    }
}

fn header_word<'a>(buf: &'a [u8], pos: &mut usize, end: usize) -> Option<&'a [u8]> {
    loop {
        while *pos < end
            && buf
                .get(*pos)
                .copied()
                .is_some_and(crate::object::whitespace)
        {
            *pos = (*pos).saturating_add(1);
        }
        if *pos >= end {
            return None;
        }
        if buf
            .get(*pos)
            .is_some_and(|byte| b"()<>[]{}/".contains(byte))
        {
            return None;
        }
        if buf.get(*pos) != Some(&b'%') {
            break;
        }
        while *pos < end && !matches!(buf.get(*pos), Some(b'\r' | b'\n')) {
            *pos = (*pos).saturating_add(1);
        }
    }
    let start = *pos;
    while *pos < end
        && !buf
            .get(*pos)
            .copied()
            .is_some_and(crate::object::whitespace)
        && !buf
            .get(*pos)
            .is_some_and(|byte| b"()<>[]{}/%".contains(byte))
    {
        *pos = (*pos).saturating_add(1);
    }
    buf.get(start..*pos)
}

fn repair_first(
    buf: &[u8],
    n: u64,
    declared: Option<usize>,
    limits: &Limits,
) -> Result<Option<usize>, DocError> {
    let bound = usize::try_from(limits.max_object_bytes).unwrap_or(usize::MAX);
    let cap = buf.len().min(bound);
    let mut pos = 0usize;
    let mut tokens = 0u64;
    for _ in 0..n.saturating_mul(2) {
        if header_word(buf, &mut pos, cap).is_none() {
            break;
        }
        tokens = tokens.saturating_add(1);
        check(
            LimitKind::Tokens,
            tokens,
            u64::from(limits.max_tokens_per_object),
        )?;
    }
    // Consume only trivia here. A delimiter starts the first body value, even
    // when /N overstates the number of pairs or /First points into that value.
    let mut body = pos;
    let _ = header_word(buf, &mut body, cap);
    let delimiter = buf
        .get(body)
        .is_some_and(|byte| b"()<>[]{}/".contains(byte));
    if delimiter && tokens != 0 && tokens % 2 == 0 {
        if let Some(first) = declared.filter(|first| *first >= pos && *first <= body) {
            return Ok(Some(first));
        }
        return Ok(Some(body));
    }
    if let Some(first) = declared.filter(|first| *first <= buf.len()) {
        return Ok(Some(first));
    }
    if body == cap && cap < buf.len() {
        check(
            LimitKind::ObjectBytes,
            limits.max_object_bytes.saturating_add(1),
            limits.max_object_bytes,
        )?;
    }
    Ok(None)
}
