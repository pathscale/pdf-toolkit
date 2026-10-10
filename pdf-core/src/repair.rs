use alloc::collections::BTreeSet;
use alloc::format;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::Range;

use crate::document::Document;
use crate::error::{malformed, DocError, Warning};
use crate::lex::{Lexer, LexerLimits};
use crate::limits::{check, LimitKind, Limits};
use crate::object::{keyword_at, parse_indirect, parse_value, unsigned, whitespace, Cursor, Object};
use crate::source::PdfSource;
use crate::work::ObjectRef;
use crate::xref::{Xref, XrefEntry, XrefKind};

#[derive(Clone, Copy)]
struct CatalogCandidate {
    offset: usize,
    order: u32,
    id: ObjectRef,
}

struct Scan {
    entries: Vec<XrefEntry>,
    trailer: Option<(usize, Object, XrefKind)>,
    catalogs: Vec<CatalogCandidate>,
    object_streams: Vec<(usize, ObjectRef)>,
}

struct ObjectStreamMember {
    number: u32,
    range: Range<usize>,
}

pub(crate) fn rebuild(source: PdfSource, limits: Limits) -> Result<Document, DocError> {
    let file_bytes = u64::try_from(source.bytes().len()).unwrap_or(u64::MAX);
    check(
        LimitKind::FileBytes,
        file_bytes,
        limits.max_file_bytes.min(u64::from(u32::MAX)),
    )?;

    let Scan {
        mut entries,
        trailer: trailer_candidate,
        catalogs,
        object_streams,
    } = scan(&source, &limits)?;

    let mut catalog = first_live_catalog(&catalogs, &entries);
    let (trailer, kind, has_trailer) = match trailer_candidate {
        Some((_, trailer, kind)) => (trailer, kind, true),
        None => (empty_trailer(&limits)?, XrefKind::Table, false),
    };
    let mut root = root_from_trailer(&trailer).or_else(|| catalog.map(|candidate| candidate.id));

    let probe_xref = Xref::from_repaired(entries.clone(), kind);
    let probe = Document::from_parts(
        source.clone(),
        probe_xref,
        trailer.clone(),
        limits,
        Vec::new(),
        root,
        false,
    );
    let stream_warnings = index_object_streams(
        &probe,
        &mut entries,
        &object_streams,
        &mut catalog,
        root.is_none(),
        &limits,
    )?;

    if root.is_none() {
        root = catalog.map(|candidate| candidate.id);
    }
    let root = root.ok_or_else(|| malformed(0, "repair root"))?;
    let trailer = if has_trailer {
        trailer
    } else {
        trailer_with_root(root, &limits)?
    };

    let xref = Xref::from_repaired(entries, kind);
    let mut warnings = Vec::new();
    warnings.push(Warning {
        offset: None,
        object: None,
        what: "xref_repaired",
    });
    warnings.extend(stream_warnings);

    Ok(Document::from_parts(
        source,
        xref,
        trailer,
        limits,
        warnings,
        Some(root),
        true,
    ))
}

fn scan(source: &PdfSource, limits: &Limits) -> Result<Scan, DocError> {
    let bytes = source.bytes();
    let shared = source.shared();
    let mut entries = Vec::new();
    let mut trailer = None;
    let mut catalogs = Vec::new();
    let mut object_streams = Vec::new();
    let mut header_count = 0u64;
    let mut line_start = 0usize;

    while line_start < bytes.len() {
        let mut line_end = line_start;
        while line_end < bytes.len()
            && !matches!(bytes.get(line_end), Some(b'\r' | b'\n'))
        {
            line_end = line_end.saturating_add(1);
        }
        let line = bytes
            .get(line_start..line_end)
            .ok_or_else(|| malformed(line_start, "repair scan"))?;
        let mut leading = 0usize;
        while line.get(leading).copied().is_some_and(whitespace) {
            leading = leading.saturating_add(1);
        }
        let content = line.get(leading..).unwrap_or(&[]);

        if keyword_at(content, 0, b"trailer") {
            let value_start = line_start
                .checked_add(leading)
                .and_then(|offset| offset.checked_add(7))
                .ok_or_else(|| malformed(line_start, "repair trailer offset"))?;
            match parse_value(&shared, value_start..bytes.len(), None, limits) {
                Ok(object) if object.dict().is_some() => {
                    let kind = if object
                        .dict()
                        .and_then(|dict| dict.get(b"XRefStm"))
                        .is_some()
                    {
                        XrefKind::Hybrid
                    } else {
                        XrefKind::Table
                    };
                    keep_trailer(&mut trailer, line_start, object, kind);
                }
                Ok(_) => {}
                Err(error) if is_hard_error(&error) => return Err(error),
                Err(_) => {}
            }
        }

        if let Some(id) = line_header(content) {
            header_count = header_count
                .checked_add(1)
                .ok_or_else(|| malformed(line_start, "repair object count"))?;
            check(
                LimitKind::Objects,
                header_count,
                u64::from(limits.max_objects),
            )?;
            let offset = line_start
                .checked_add(leading)
                .ok_or_else(|| malformed(line_start, "repair object offset"))?;
            insert_in_use(&mut entries, id, offset, limits)?;

            match parse_indirect(&shared, offset, id, limits) {
                Ok(object) => {
                    if has_type(&object, b"XRef") {
                        let kind = if object
                            .dict()
                            .and_then(|dict| dict.get(b"XRefStm"))
                            .is_some()
                        {
                            XrefKind::Hybrid
                        } else {
                            XrefKind::Stream
                        };
                        keep_trailer(&mut trailer, offset, object.clone(), kind);
                    }
                    if has_type(&object, b"ObjStm") {
                        object_streams.push((offset, id));
                    }
                    if has_type(&object, b"Catalog") {
                        catalogs.push(CatalogCandidate {
                            offset,
                            order: 0,
                            id,
                        });
                    }
                }
                Err(error) if is_hard_error(&error) => return Err(error),
                Err(_) => {}
            }
        }

        if line_end >= bytes.len() {
            break;
        }
        line_start = line_end;
        if bytes.get(line_start) == Some(&b'\r') {
            line_start = line_start.saturating_add(1);
            if bytes.get(line_start) == Some(&b'\n') {
                line_start = line_start.saturating_add(1);
            }
        } else {
            line_start = line_start.saturating_add(1);
        }
    }

    Ok(Scan {
        entries,
        trailer,
        catalogs,
        object_streams,
    })
}

fn line_header(line: &[u8]) -> Option<ObjectRef> {
    let mut lexer = Lexer::with_limits(line, LexerLimits::new(3, 64, 0));
    let first = lexer.next()?.ok()?;
    let second = lexer.next()?.ok()?;
    let marker = lexer.next()?.ok()?;
    if first.span().start() != 0 || marker.bytes() != b"obj" {
        return None;
    }
    let number = u32::try_from(unsigned(first.bytes())?).ok()?;
    let generation = u16::try_from(unsigned(second.bytes())?).ok()?;
    Some(ObjectRef::new(number, generation))
}

fn keep_trailer(
    slot: &mut Option<(usize, Object, XrefKind)>,
    offset: usize,
    object: Object,
    kind: XrefKind,
) {
    let replace = match slot.as_ref() {
        Some((old_offset, _, _)) => offset >= *old_offset,
        None => true,
    };
    if replace {
        *slot = Some((offset, object, kind));
    }
}

fn insert_in_use(
    entries: &mut Vec<XrefEntry>,
    id: ObjectRef,
    offset: usize,
    limits: &Limits,
) -> Result<(), DocError> {
    if id.number() == 0 {
        return Ok(());
    }
    let index =
        usize::try_from(id.number()).map_err(|_| malformed(offset, "repair object number"))?;
    let needed = index
        .checked_add(1)
        .ok_or_else(|| malformed(offset, "repair object number"))?;
    check(
        LimitKind::Objects,
        u64::try_from(needed).unwrap_or(u64::MAX),
        u64::from(limits.max_objects),
    )?;
    if entries.len() < needed {
        entries.resize(needed, XrefEntry::Free);
    }
    let object_offset =
        u64::try_from(offset).map_err(|_| malformed(offset, "repair object offset"))?;
    if let Some(slot) = entries.get_mut(index) {
        let replace = match slot {
            XrefEntry::InUse {
                offset: current, ..
            } => object_offset >= *current,
            _ => true,
        };
        if replace {
            *slot = XrefEntry::InUse {
                offset: object_offset,
                generation: id.generation(),
            };
        }
    }
    Ok(())
}

fn first_live_catalog(
    catalogs: &[CatalogCandidate],
    entries: &[XrefEntry],
) -> Option<CatalogCandidate> {
    let mut first: Option<CatalogCandidate> = None;
    for candidate in catalogs {
        let index = usize::try_from(candidate.id.number()).ok()?;
        let offset = u64::try_from(candidate.offset).ok()?;
        let live = matches!(
            entries.get(index),
            Some(XrefEntry::InUse {
                offset: current,
                generation,
            }) if *current == offset && *generation == candidate.id.generation()
        );
        if !live {
            continue;
        }
        let replace = match first {
            Some(current) => {
                (candidate.offset, candidate.order) < (current.offset, current.order)
            }
            None => true,
        };
        if replace {
            first = Some(*candidate);
        }
    }
    first
}

fn root_from_trailer(trailer: &Object) -> Option<ObjectRef> {
    trailer
        .dict()
        .and_then(|dict| dict.get(b"Root"))
        .and_then(|value| value.as_ref())
}

fn has_type(object: &Object, expected: &[u8]) -> bool {
    object
        .dict()
        .and_then(|dict| dict.get(b"Type"))
        .and_then(|value| value.as_name())
        .map(|name| name.eq_bytes(expected))
        .unwrap_or(false)
}

fn index_object_streams(
    probe: &Document,
    entries: &mut Vec<XrefEntry>,
    candidates: &[(usize, ObjectRef)],
    catalog: &mut Option<CatalogCandidate>,
    find_catalog: bool,
    limits: &Limits,
) -> Result<Vec<Warning>, DocError> {
    if probe.encrypted() {
        return Ok(Vec::new());
    }

    let mut streams = Vec::new();
    for (offset, id) in candidates {
        let index = usize::try_from(id.number())
            .map_err(|_| malformed(*offset, "repair object stream number"))?;
        let offset_u64 =
            u64::try_from(*offset).map_err(|_| malformed(*offset, "repair object stream offset"))?;
        let live = matches!(
            entries.get(index),
            Some(XrefEntry::InUse {
                offset: current,
                generation,
            }) if *current == offset_u64 && *generation == id.generation()
        );
        if live {
            streams.push((*offset, *id));
        }
    }
    streams.sort_unstable_by_key(|(offset, id)| (*offset, id.number()));

    let mut resolver = probe.resolver();
    let mut need_catalog = find_catalog;
    for (offset, id) in streams {
        let container = match resolver.get(id) {
            Ok(object) => object,
            Err(error) if is_hard_error(&error) => return Err(error),
            Err(_) => continue,
        };
        if !has_type(&container, b"ObjStm") {
            continue;
        }
        let decoded = match resolver.decode_stream(&container) {
            Ok(decoded) if decoded.image.is_none() && !decoded.truncated => decoded,
            Ok(_) => continue,
            Err(error) if is_hard_error(&error) => return Err(error),
            Err(_) => continue,
        };
        let data = Arc::<[u8]>::from(decoded.bytes);
        let members = match object_stream_members(&container, &data, limits) {
            Ok(members) => members,
            Err(error) if is_hard_error(&error) => return Err(error),
            Err(_) => continue,
        };
        let stream_offset =
            u64::try_from(offset).map_err(|_| malformed(offset, "repair object stream offset"))?;

        for (member_index, member) in members.into_iter().enumerate() {
            let index = u32::try_from(member_index)
                .map_err(|_| malformed(offset, "repair object stream index"))?;
            if member.number != 0 {
                insert_compressed(
                    entries,
                    member.number,
                    XrefEntry::Compressed {
                        stream: id.number(),
                        index,
                    },
                    stream_offset,
                    limits,
                    offset,
                )?;
            }

            if need_catalog {
                let member_id = ObjectRef::new(member.number, 0);
                match parse_value(&data, member.range, Some(member_id), limits) {
                    Ok(object) if has_type(&object, b"Catalog") => {
                        let order = index.saturating_add(1);
                        let candidate = CatalogCandidate {
                            offset,
                            order,
                            id: member_id,
                        };
                        let replace = match catalog.as_ref() {
                            Some(current) => {
                                (candidate.offset, candidate.order)
                                    < (current.offset, current.order)
                            }
                            None => true,
                        };
                        if replace {
                            *catalog = Some(candidate);
                        }
                        need_catalog = false;
                    }
                    Err(error) if is_hard_error(&error) => return Err(error),
                    _ => {}
                }
            }
        }
    }

    Ok(resolver.take_warnings())
}

fn insert_compressed(
    entries: &mut Vec<XrefEntry>,
    number: u32,
    entry: XrefEntry,
    stream_offset: u64,
    limits: &Limits,
    error_offset: usize,
) -> Result<(), DocError> {
    let index =
        usize::try_from(number).map_err(|_| malformed(error_offset, "repair object number"))?;
    let needed = index
        .checked_add(1)
        .ok_or_else(|| malformed(error_offset, "repair object number"))?;
    check(
        LimitKind::Objects,
        u64::try_from(needed).unwrap_or(u64::MAX),
        u64::from(limits.max_objects),
    )?;
    if entries.len() < needed {
        entries.resize(needed, XrefEntry::Free);
    }
    let Some(slot) = entries.get_mut(index) else {
        return Err(malformed(error_offset, "repair object number"));
    };
    let replace = match slot {
        XrefEntry::Free => true,
        XrefEntry::InUse { offset, .. } => *offset < stream_offset,
        XrefEntry::Compressed { .. } => true,
    };
    if replace {
        *slot = entry;
    }
    Ok(())
}

fn object_stream_members(
    container: &Object,
    data: &Arc<[u8]>,
    limits: &Limits,
) -> Result<Vec<ObjectStreamMember>, DocError> {
    let wrong = || DocError::WrongType {
        id: container.id(),
        expected: "object stream",
    };
    let dict = container.dict().ok_or_else(wrong)?;
    if !dict
        .get(b"Type")
        .and_then(|value| value.as_name())
        .map(|name| name.eq_bytes(b"ObjStm"))
        .unwrap_or(false)
    {
        return Err(wrong());
    }
    let count = dict
        .get(b"N")
        .and_then(|value| value.as_i64())
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(wrong)?;
    check(
        LimitKind::ObjStmMembers,
        count,
        u64::from(limits.max_objstm_members),
    )?;
    let first = dict
        .get(b"First")
        .and_then(|value| value.as_i64())
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(wrong)?;
    if first > data.len() {
        return Err(malformed(first, "object stream First"));
    }
    let count =
        usize::try_from(count).map_err(|_| malformed(first, "object stream N"))?;
    let mut cursor = Cursor::at(data, 0..first, limits)?;
    let mut pairs = Vec::new();
    let mut seen = BTreeSet::new();
    for _ in 0..count {
        let number = unsigned(cursor.required()?.bytes())
            .and_then(|number| u32::try_from(number).ok())
            .ok_or_else(|| malformed(cursor.position(), "object stream header"))?;
        let start = unsigned(cursor.required()?.bytes())
            .and_then(|offset| usize::try_from(offset).ok())
            .and_then(|offset| first.checked_add(offset))
            .filter(|offset| *offset <= data.len())
            .ok_or_else(|| malformed(cursor.position(), "object stream offset"))?;
        if !seen.insert(number) {
            return Err(malformed(cursor.position(), "object stream duplicate"));
        }
        if pairs
            .last()
            .map(|pair: &(u32, usize)| pair.1 > start)
            .unwrap_or(false)
        {
            return Err(malformed(cursor.position(), "object stream order"));
        }
        pairs.push((number, start));
    }
    if cursor.next()?.is_some() {
        return Err(malformed(cursor.position(), "object stream header"));
    }

    let mut members = Vec::new();
    for (index, (number, start)) in pairs.iter().copied().enumerate() {
        let end = pairs
            .get(index.saturating_add(1))
            .map(|pair| pair.1)
            .unwrap_or(data.len());
        members.push(ObjectStreamMember {
            number,
            range: start..end,
        });
    }
    Ok(members)
}

fn empty_trailer(limits: &Limits) -> Result<Object, DocError> {
    let bytes = Arc::<[u8]>::from(&b"<<>>"[..]);
    parse_value(&bytes, 0..bytes.len(), None, limits)
}

fn trailer_with_root(root: ObjectRef, limits: &Limits) -> Result<Object, DocError> {
    let text = format!("<< /Root {} {} R >>", root.number(), root.generation());
    let bytes = Arc::<[u8]>::from(text.into_bytes());
    parse_value(&bytes, 0..bytes.len(), None, limits)
}

fn is_hard_error(error: &DocError) -> bool {
    matches!(error, DocError::Refused(_) | DocError::Encrypted)
}

