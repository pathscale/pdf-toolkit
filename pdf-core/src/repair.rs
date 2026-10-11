use alloc::format;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::document::{Document, Resolver};
use crate::error::{malformed, DocError, Warning};
use crate::limits::{check, LimitKind, Limits};
use crate::object::{keyword_at, parse_indirect_repair, parse_value, whitespace, Object};
use crate::objstm::ObjectStream;
use crate::source::PdfSource;
use crate::work::ObjectRef;
use crate::xref::{header_at, Xref, XrefEntry, XrefKind};

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
    warnings: Vec<Warning>,
}

pub(crate) fn needs_rebuild(document: &Document) -> Result<bool, DocError> {
    for number in 1..document.xref().size() {
        match document.xref().get(number) {
            Some(XrefEntry::InUse { offset, generation }) => {
                let found = usize::try_from(offset).ok()
                    .and_then(|offset| header_at(document.source().bytes(), offset));
                if found != Some(ObjectRef::new(number, generation)) {
                    return Ok(true);
                }
            }
            Some(XrefEntry::Compressed { stream, .. }) => {
                if !matches!(document.xref().get(stream), Some(XrefEntry::InUse { .. })) {
                    return Ok(true);
                }
            }
            _ => {}
        }
    }
    // Crypt initialization happens after this probe. Do not try to decode
    // encrypted members before a key has been installed.
    if document.encrypted() {
        return Ok(false);
    }
    let Some(root) = document.root_ref() else { return Ok(true) };
    let mut resolver = document.resolver();
    match catalog_status(&mut resolver, root) {
        Ok(status) => Ok(status != 2),
        Err(error) if is_hard_error(&error) => Err(error),
        Err(_) => Ok(true),
    }
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
        mut catalogs,
        object_streams,
        mut warnings,
    } = scan(&source, &limits)?;
    let (trailer, kind, has_trailer) = match trailer_candidate {
        Some((_, trailer, kind)) => (trailer, kind, true),
        None => (empty_trailer(&limits)?, XrefKind::Table, false),
    };
    let trailer_root = root_from_trailer(&trailer);
    let probe = Document::from_parts(
        source.clone(),
        Xref::from_repaired(entries.clone(), kind),
        trailer.clone(),
        limits,
        Vec::new(),
        trailer_root,
        true,
    );
    let (stream_warnings, stream_error) = index_object_streams(
        &probe,
        &mut entries,
        &object_streams,
        &mut catalogs,
        &limits,
    )?;
    warnings.extend(stream_warnings);
    let probe = Document::from_parts(
        source.clone(),
        Xref::from_repaired(entries.clone(), kind),
        trailer.clone(),
        limits,
        Vec::new(),
        trailer_root,
        true,
    );
    catalogs.sort_unstable_by_key(|candidate| (candidate.offset, candidate.order));
    let mut resolver = probe.resolver();
    let mut root = None;
    let mut fallback = None;
    let mut root_error = stream_error;
    if probe.encrypted() {
        root = trailer_root.or_else(|| catalogs.iter().rev()
            .find(|candidate| live_catalog(candidate, &entries))
            .map(|candidate| candidate.id));
    } else {
        if let Some(id) = trailer_root {
            match catalog_status(&mut resolver, id) {
                Ok(2) => root = Some(id),
                Ok(1) => fallback = Some(id),
                Ok(_) => {}
                Err(error) if is_hard_error(&error) => return Err(error),
                Err(error) => root_error = Some(error),
            }
        }
        let mut scanned_fallback = None;
        if root.is_none() {
            for candidate in catalogs.iter().rev() {
                if !live_catalog(candidate, &entries) { continue; }
                match catalog_status(&mut resolver, candidate.id) {
                    Ok(2) => {
                        root = Some(candidate.id);
                        break;
                    }
                    Ok(1) if scanned_fallback.is_none() => {
                        scanned_fallback = Some(candidate.id);
                    }
                    Ok(_) => {}
                    Err(error) if is_hard_error(&error) => return Err(error),
                    Err(error) if root_error.is_none() => root_error = Some(error),
                    Err(_) => {}
                }
            }
        }
        fallback = scanned_fallback.or(fallback);
    }
    warnings.extend(resolver.take_warnings());
    if root.is_none() && fallback.is_none() && trailer_root.is_some() {
        warnings.push(Warning {
            offset: None,
            object: trailer_root,
            what: "repair_root_unresolved",
        });
    }
    // A syntactically intact trailer root remains the last resort used by the
    // previous repair path. Later resolution still reports its actual error.
    let root = match root.or(fallback).or(trailer_root) {
        Some(root) => root,
        None => return Err(root_error.unwrap_or_else(|| malformed(0, "repair root"))),
    };
    if !has_trailer || trailer_root != Some(root) {
        warnings.push(Warning {
            offset: None,
            object: Some(root),
            what: "trailer_rebuilt",
        });
    }
    let trailer = if has_trailer && trailer_root == Some(root) {
        trailer
    } else {
        trailer_with_root(&trailer, root, &limits)?
    };
    warnings.insert(0, Warning {
        offset: None,
        object: None,
        what: "xref_repaired",
    });
    Ok(Document::from_parts(
        source,
        Xref::from_repaired(entries, kind),
        trailer,
        limits,
        warnings,
        Some(root),
        true,
    ))
}

// 0: not a catalog; 1: catalog with an unusable Pages link; 2: usable spine.
// Retain a parsed catalog as a last resort, but prefer one with live Pages.
fn catalog_status(resolver: &mut Resolver<'_>, id: ObjectRef) -> Result<u8, DocError> {
    let catalog = resolver.get(id)?;
    let Some(dict) = catalog.dict() else { return Ok(0) };
    if !catalog_candidate(&catalog) { return Ok(0); }
    let Some(pages) = dict.get(b"Pages").and_then(|value| value.as_ref()) else {
        return Ok(1);
    };
    let pages = match resolver.get(pages) {
        Ok(pages) => pages,
        Err(error) if is_hard_error(&error) => return Err(error),
        Err(_) => {
            resolver.warn(Warning {
                offset: None,
                object: Some(id),
                what: "repair_pages_unresolved",
            });
            return Ok(1);
        }
    };
    let Some(dict) = pages.dict() else { return Ok(1) };
    if let Some(kind) = dict.get(b"Type") {
        match resolver.resolve(kind) {
            Ok(kind) => {
                let Some(name) = kind.get().as_name() else { return Ok(1) };
                if name.eq_bytes(b"Page") { return Ok(2); }
                if !name.eq_bytes(b"Pages") { return Ok(1); }
            }
            Err(error) if is_hard_error(&error) => return Err(error),
            Err(_) => return Ok(1),
        }
    }
    let Some(kids) = dict.get(b"Kids") else { return Ok(1) };
    match resolver.resolve(kids) {
        Ok(kids) if kids.get().as_array().is_some() => Ok(2),
        Err(error) if is_hard_error(&error) => Err(error),
        _ => Ok(1),
    }
}

fn scan(source: &PdfSource, limits: &Limits) -> Result<Scan, DocError> {
    let bytes = source.bytes();
    let shared = source.shared();
    let mut entries = Vec::new();
    let mut trailer = None;
    let mut catalogs = Vec::new();
    let mut object_streams = Vec::new();
    let mut warnings = Vec::new();
    let mut header_count = 0u64;
    let mut offset = 0usize;

    while offset < bytes.len() {
        if bytes.get(offset) == Some(&b'%') {
            while offset < bytes.len() && !matches!(bytes.get(offset), Some(b'\r' | b'\n')) {
                offset = offset.saturating_add(1);
            }
            continue;
        }
        let boundary = offset == 0
            || bytes.get(offset.saturating_sub(1)).copied().is_some_and(whitespace);
        if !boundary {
            offset = offset.saturating_add(1);
            continue;
        }
        if keyword_at(bytes, offset, b"trailer") {
            let start = offset.saturating_add(7);
            match parse_value(&shared, start..bytes.len(), None, limits) {
                Ok(object) if object.dict().is_some() => {
                    let kind = if object.dict().and_then(|dict| dict.get(b"XRefStm")).is_some() {
                        XrefKind::Hybrid
                    } else {
                        XrefKind::Table
                    };
                    let end = object.root().span().end;
                    keep_trailer(&mut trailer, offset, object, kind);
                    offset = end;
                    continue;
                }
                Err(error) if is_hard_error(&error) => return Err(error),
                _ => {}
            }
        }
        if !matches!(bytes.get(offset), Some(b'+' | b'0'..=b'9')) {
            offset = offset.saturating_add(1);
            continue;
        }
        let Some(id) = header_at(bytes, offset) else {
            offset = offset.saturating_add(1);
            continue;
        };
        header_count = header_count.saturating_add(1);
        check(LimitKind::Objects, header_count, u64::from(limits.max_objects))?;
        // Dense xref storage must not be sized by an arbitrary damaged number.
        if u64::from(id.number()) >= bytes.len() as u64 {
            warnings.push(Warning {
                offset: Some(offset as u64),
                object: Some(id),
                what: "repair_object_number_skipped",
            });
            offset = offset.saturating_add(1);
            continue;
        }
        insert_in_use(&mut entries, id, offset, limits)?;
        match parse_indirect_repair(&shared, offset, id, limits) {
            Ok(object) => {
                if has_type(&object, b"XRef")
                    || (object.is_stream()
                        && object.dict().and_then(|dict| dict.get(b"W")).is_some()
                        && root_from_trailer(&object).is_some())
                {
                    let kind = if object.dict().and_then(|dict| dict.get(b"XRefStm")).is_some() {
                        XrefKind::Hybrid
                    } else {
                        XrefKind::Stream
                    };
                    keep_trailer(&mut trailer, offset, object.clone(), kind);
                }
                if object_stream_candidate(&object) {
                    object_streams.push((offset, id));
                }
                if catalog_candidate(&object) {
                    catalogs.push(CatalogCandidate { offset, order: 0, id });
                }
                if object.tail_recovered() {
                    warnings.push(Warning {
                        offset: Some(object.root().span().end as u64),
                        object: Some(id),
                        what: "object_tail_recovered",
                    });
                }
                if object.stream_recovered() {
                    warnings.push(Warning {
                        offset: object.stream_range().map(|range| range.start as u64),
                        object: Some(id),
                        what: "stream_length_recovered",
                    });
                }
                // Skip parsed strings, dictionaries and raw stream payloads.
                // Failed objects do not stop discovery of subsequent headers.
                offset = object.stream_range().map(|range| range.end)
                    .unwrap_or_else(|| object.root().span().end)
                    .max(offset.saturating_add(1));
            }
            Err(error) if is_hard_error(&error) => return Err(error),
            Err(_) => {
                warnings.push(Warning {
                    offset: Some(offset as u64),
                    object: Some(id),
                    what: "repair_object_unparsed",
                });
                offset = offset.saturating_add(1);
            }
        }
    }
    Ok(Scan { entries, trailer, catalogs, object_streams, warnings })
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

fn live_catalog(candidate: &CatalogCandidate, entries: &[XrefEntry]) -> bool {
    let offset = candidate.offset as u64;
    match entries.get(candidate.id.number() as usize) {
        Some(XrefEntry::InUse { offset: current, generation }) => {
            candidate.order == 0 && *current == offset && *generation == candidate.id.generation()
        }
        Some(XrefEntry::Compressed { stream, index }) => {
            candidate.id.generation() == 0
                && index.checked_add(1) == Some(candidate.order)
                && matches!(entries.get(*stream as usize),
                    Some(XrefEntry::InUse { offset: current, .. }) if *current == offset)
        }
        _ => false,
    }
}

fn catalog_candidate(object: &Object) -> bool {
    has_type(object, b"Catalog")
        || object.dict().and_then(|dict| dict.get(b"Pages"))
            .and_then(|value| value.as_ref()).is_some()
}

fn object_stream_candidate(object: &Object) -> bool {
    has_type(object, b"ObjStm")
        || (object.is_stream()
            && object.dict().and_then(|dict| dict.get(b"N")).is_some()
            && object.dict().and_then(|dict| dict.get(b"First")).is_some())
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
    catalogs: &mut Vec<CatalogCandidate>,
    limits: &Limits,
) -> Result<(Vec<Warning>, Option<DocError>), DocError> {
    if probe.encrypted() {
        return Ok((Vec::new(), None));
    }
    let mut resolver = probe.resolver();
    let mut first_error = None;
    let mut decoded_total = 0u64;
    for (offset, id) in candidates {
        if !matches!(entries.get(id.number() as usize),
            Some(XrefEntry::InUse { offset: current, generation })
                if *current == *offset as u64 && *generation == id.generation())
        {
            continue;
        }
        let result = (|| {
            let container = resolver.get(*id)?;
            if !object_stream_candidate(&container) {
                return Err(malformed(*offset, "repair object stream"));
            }
            let decoded = resolver.decode_stream(&container)?;
            if decoded.image.is_some() {
                return Err(malformed(*offset, "object stream decode"));
            }
            decoded_total = decoded_total.saturating_add(decoded.bytes.len() as u64);
            check(LimitKind::TotalDecodedBytes, decoded_total, limits.max_total_decoded_bytes)?;
            let truncated = decoded.truncated;
            let (stream, recovered) = ObjectStream::parse_repair(
                &container, Arc::<[u8]>::from(decoded.bytes), limits,
            )?;
            if recovered || truncated {
                resolver.warn(Warning {
                    offset: Some(*offset as u64),
                    object: Some(*id),
                    what: "object_stream_recovered",
                });
            }
            for (index, number) in stream.entries() {
                if u64::from(number) >= probe.source().bytes().len() as u64 {
                    resolver.warn(Warning {
                        offset: Some(*offset as u64),
                        object: Some(ObjectRef::new(number, 0)),
                        what: "repair_object_number_skipped",
                    });
                    continue;
                }
                let member_id = ObjectRef::new(number, 0);
                // parse_repair has already validated this member's own range.
                let member = stream.member(index, member_id, limits)?;
                insert_compressed(
                    entries,
                    number,
                    XrefEntry::Compressed { stream: id.number(), index },
                    limits,
                    *offset,
                )?;
                if catalog_candidate(&member) {
                    catalogs.push(CatalogCandidate {
                        offset: *offset,
                        order: index.saturating_add(1),
                        id: member_id,
                    });
                }
            }
            Ok::<(), DocError>(())
        })();
        if let Err(error) = result {
            if is_hard_error(&error) { return Err(error); }
            resolver.warn(Warning {
                offset: Some(*offset as u64),
                object: Some(*id),
                what: error.code(),
            });
            if first_error.is_none() { first_error = Some(error); }
        }
    }
    Ok((resolver.take_warnings(), first_error))
}

fn insert_compressed(
    entries: &mut Vec<XrefEntry>,
    number: u32,
    entry: XrefEntry,
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
        XrefEntry::InUse { .. } => false,
        XrefEntry::Compressed { .. } => true,
    };
    if replace {
        *slot = entry;
    }
    Ok(())
}

fn empty_trailer(limits: &Limits) -> Result<Object, DocError> {
    let bytes = Arc::<[u8]>::from(&b"<<>>"[..]);
    parse_value(&bytes, 0..bytes.len(), None, limits)
}

fn trailer_with_root(trailer: &Object, root: ObjectRef, limits: &Limits) -> Result<Object, DocError> {
    let mut bytes = format!("<< /Root {} {} R", root.number(), root.generation()).into_bytes();
    if let Some(dict) = trailer.dict() {
        for (name, value) in dict.iter() {
            if name.eq_bytes(b"Root") { continue; }
            let needed = (bytes.len() as u64).saturating_add(name.encoded().len() as u64)
                .saturating_add(value.raw().len() as u64).saturating_add(6);
            check(LimitKind::ObjectBytes, needed, limits.max_object_bytes)?;
            bytes.extend_from_slice(b" /");
            bytes.extend_from_slice(name.encoded());
            bytes.push(b' ');
            bytes.extend_from_slice(value.raw());
        }
    }
    check(LimitKind::ObjectBytes, (bytes.len() as u64).saturating_add(3), limits.max_object_bytes)?;
    bytes.extend_from_slice(b" >>");
    let bytes = Arc::<[u8]>::from(bytes);
    parse_value(&bytes, 0..bytes.len(), None, limits)
}

pub(crate) fn is_hard_error(error: &DocError) -> bool {
    matches!(error,
        DocError::Refused(_) | DocError::Encrypted | DocError::Source(_)
        | DocError::Filter(crate::filter::FilterError::Refused(_))
        | DocError::Filter(crate::filter::FilterError::OutputLimit { .. }))
}

