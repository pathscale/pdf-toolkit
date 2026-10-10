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
        let wrong = || DocError::WrongType { id: object.id(), expected: "object stream" };
        let dict = object.dict().ok_or_else(wrong)?;
        if !dict.get(b"Type").and_then(|value| value.as_name()).map(|name| name.eq_bytes(b"ObjStm")).unwrap_or(false) {
            return Err(wrong());
        }
        let n = dict.get(b"N").and_then(|value| value.as_i64())
            .and_then(|n| u64::try_from(n).ok()).ok_or_else(wrong)?;
        check(LimitKind::ObjStmMembers, n, u64::from(limits.max_objstm_members))?;
        let first = dict.get(b"First").and_then(|value| value.as_i64())
            .and_then(|n| usize::try_from(n).ok()).ok_or_else(wrong)?;
        if first > buf.len() { return Err(malformed(first, "object stream First")); }
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
            if !seen.insert(number) { return Err(malformed(cursor.position(), "object stream duplicate")); }
            if pairs.last().map(|pair: &(u32, usize)| pair.1 > offset).unwrap_or(false) {
                return Err(malformed(cursor.position(), "object stream order"));
            }
            pairs.push((number, offset));
        }
        if cursor.next()?.is_some() { return Err(malformed(cursor.position(), "object stream header")); }
        let mut members = Vec::new();
        for (index, (number, start)) in pairs.iter().enumerate() {
            let end = pairs.get(index.saturating_add(1)).map(|pair| pair.1).unwrap_or(buf.len());
            members.push((*number, *start..end));
        }
        Ok(Self(Arc::new(ObjectStreamData { buf, members })))
    }

    pub(crate) fn member(
        &self,
        index: u32,
        expected: ObjectRef,
        limits: &Limits,
    ) -> Result<Object, DocError> {
        let (number, range) = self.0.members.get(index as usize)
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
        if tail.next()?.is_some() { return Err(malformed(end, "object stream member")); }
        Ok(object)
    }

    pub(crate) fn cost(&self) -> u64 {
        (self.0.buf.len() as u64).saturating_add(
            (self.0.members.len() as u64)
                .saturating_mul(core::mem::size_of::<(u32, Range<usize>)>() as u64),
        )
    }
}

