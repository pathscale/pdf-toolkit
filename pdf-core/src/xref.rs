use alloc::collections::BTreeSet;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::document::filter_steps;
use crate::error::{malformed, DocError, Warning};
use crate::lex::{Lexer, LexerLimits, Token};
use crate::limits::{check, LimitKind, Limits};
use crate::object::{keyword_at, parse_indirect, parse_value, unsigned, DictRef, Object};
use crate::source::PdfSource;
use crate::work::ObjectRef;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XrefKind {
    Table,
    Stream,
    Hybrid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XrefEntry {
    Free,
    InUse { offset: u64, generation: u16 },
    Compressed { stream: u32, index: u32 },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct XrefCounts {
    pub free: u32,
    pub in_use: u32,
    pub compressed: u32,
    pub object_streams: u32,
}

pub struct Xref {
    entries: Vec<XrefEntry>,
    kind: XrefKind,
    sections: u32,
}

impl Xref {
    pub fn get(&self, number: u32) -> Option<XrefEntry> {
        self.entries.get(number as usize).copied()
    }
    pub fn size(&self) -> u32 {
        u32::try_from(self.entries.len()).unwrap_or(u32::MAX)
    }
    pub fn kind(&self) -> XrefKind {
        self.kind
    }
    pub fn sections(&self) -> u32 {
        self.sections
    }
    pub fn counts(&self) -> XrefCounts {
        let mut counts = XrefCounts::default();
        let mut streams = BTreeSet::new();
        for entry in &self.entries {
            match entry {
                XrefEntry::Free => counts.free = counts.free.saturating_add(1),
                XrefEntry::InUse { .. } => counts.in_use = counts.in_use.saturating_add(1),
                XrefEntry::Compressed { stream, .. } => {
                    counts.compressed = counts.compressed.saturating_add(1);
                    streams.insert(*stream);
                }
            }
        }
        counts.object_streams = u32::try_from(streams.len()).unwrap_or(u32::MAX);
        counts
    }

    pub(crate) fn from_repaired(entries: Vec<XrefEntry>, kind: XrefKind) -> Xref {
        Self {
            entries,
            kind,
            sections: 1,
        }
    }
}

struct Section {
    entries: Vec<Option<XrefEntry>>,
    trailer: Object,
    kind: XrefKind,
}

pub(crate) fn read(
    source: &PdfSource,
    limits: &Limits,
) -> Result<(Xref, Object, Vec<Warning>), DocError> {
    let bytes = source.bytes();
    check(
        LimitKind::FileBytes,
        bytes.len() as u64,
        limits.max_file_bytes.min(u64::from(u32::MAX)),
    )?;
    let buf = source.shared();
    let original = startxref(bytes)?;
    let header = source.header().offset();
    let mut warnings = Vec::new();
    let (mut offset, shift) = locate(bytes, original, header, &mut warnings)?;
    let mut visited = BTreeSet::new();
    let mut entries: Vec<Option<XrefEntry>> = Vec::new();
    let mut newest = None;
    let mut kind = XrefKind::Table;
    let mut sections = 0u32;
    loop {
        if !visited.insert(offset) {
            warnings.push(Warning {
                offset: Some(offset as u64),
                object: None,
                what: "prev_cycle",
            });
            break;
        }
        count_section(&mut sections, limits)?;
        let mut section = section(&buf, offset, shift, limits, &mut warnings)?;
        let table = section.kind == XrefKind::Table;
        let dict = section
            .trailer
            .dict()
            .ok_or_else(|| malformed(offset, "trailer"))?;
        let prev = integer(dict, b"Prev")?;
        let hybrid = if table {
            integer(dict, b"XRefStm")?
        } else {
            None
        };
        if let Some(hybrid) = hybrid {
            let hybrid = shifted(hybrid, shift, offset)?;
            if !visited.insert(hybrid) {
                warnings.push(Warning {
                    offset: Some(hybrid as u64),
                    object: None,
                    what: "prev_cycle",
                });
            } else {
                // The /XRefStm stream belongs to the same revision as its table:
                // `sections` counts revisions, so it is not counted again.
                let supplemental = stream(&buf, hybrid, shift, limits, &mut warnings)?;
                if supplemental.entries.len() > section.entries.len() {
                    section.entries.resize(supplemental.entries.len(), None);
                }
                for (number, entry) in supplemental.entries.into_iter().enumerate() {
                    if let Some(slot) = section.entries.get_mut(number) {
                        if slot.is_none() || *slot == Some(XrefEntry::Free) {
                            if entry.is_some() {
                                *slot = entry;
                            }
                        }
                    }
                }
                section.kind = XrefKind::Hybrid;
            }
        }
        if newest.is_none() {
            kind = section.kind;
            newest = Some(section.trailer);
        }
        if section.entries.len() > entries.len() {
            entries.resize(section.entries.len(), None);
        }
        for (number, entry) in section.entries.into_iter().enumerate() {
            if let Some(slot) = entries.get_mut(number) {
                if slot.is_none() {
                    *slot = entry;
                }
            }
        }
        let Some(prev) = prev else {
            break;
        };
        offset = shifted(prev, shift, offset)?;
    }
    let trailer = newest.ok_or_else(|| malformed(original, "xref"))?;
    let entries = entries
        .into_iter()
        .map(|entry| entry.unwrap_or(XrefEntry::Free))
        .collect();
    Ok((
        Xref {
            entries,
            kind,
            sections,
        },
        trailer,
        warnings,
    ))
}

fn count_section(sections: &mut u32, limits: &Limits) -> Result<(), DocError> {
    let requested = u64::from(*sections).saturating_add(1);
    check(
        LimitKind::PrevChain,
        requested,
        u64::from(limits.max_prev_chain),
    )?;
    *sections = u32::try_from(requested).map_err(|_| malformed(0, "xref section count"))?;
    Ok(())
}

fn startxref(buf: &[u8]) -> Result<usize, DocError> {
    let base = buf.len().saturating_sub(1024);
    let tail = buf
        .get(base..)
        .ok_or_else(|| malformed(base, "startxref"))?;
    for (relative, window) in tail.windows(9).enumerate().rev() {
        if window != b"startxref" {
            continue;
        }
        let start = base.saturating_add(relative).saturating_add(9);
        let Some(rest) = buf.get(start..) else {
            continue;
        };
        let mut lexer = Lexer::with_limits(rest, LexerLimits::new(1, 32, 0));
        if let Some(Ok(token)) = lexer.next() {
            if let Some(offset) = unsigned(token.bytes()).and_then(|n| usize::try_from(n).ok()) {
                return Ok(offset);
            }
        }
    }
    Err(malformed(base, "startxref"))
}

fn locate(
    buf: &[u8],
    original: usize,
    header: usize,
    warnings: &mut Vec<Warning>,
) -> Result<(usize, u64), DocError> {
    if looks_like_section(buf, original) {
        return Ok((original, 0));
    }
    let shifted = original.checked_add(header);
    if header != 0 {
        if let Some(offset) = shifted.filter(|offset| looks_like_section(buf, *offset)) {
            warnings.push(Warning {
                offset: Some(original as u64),
                object: None,
                what: "offsets_shifted",
            });
            return Ok((offset, header as u64));
        }
    }
    for (center, shift) in [(Some(original), 0usize), (shifted, header)] {
        let Some(center) = center else {
            continue;
        };
        if shift != 0 && header == 0 {
            continue;
        }
        for distance in 1..=64usize {
            for candidate in [center.checked_sub(distance), center.checked_add(distance)] {
                if let Some(offset) = candidate.filter(|offset| looks_like_section(buf, *offset)) {
                    if shift != 0 {
                        warnings.push(Warning {
                            offset: Some(original as u64),
                            object: None,
                            what: "offsets_shifted",
                        });
                    }
                    warnings.push(Warning {
                        offset: Some(original as u64),
                        object: None,
                        what: "startxref_adjusted",
                    });
                    return Ok((offset, shift as u64));
                }
            }
        }
    }
    Err(malformed(original, "startxref"))
}

fn looks_like_section(buf: &[u8], offset: usize) -> bool {
    keyword_at(buf, offset, b"xref") || header_at(buf, offset).is_some()
}

pub(crate) fn header_at(buf: &[u8], offset: usize) -> Option<ObjectRef> {
    let input = buf.get(offset..offset.saturating_add(80).min(buf.len()))?;
    let mut lexer = Lexer::with_limits(input, LexerLimits::new(3, 32, 0));
    let first = lexer.next()?.ok()?;
    let second = lexer.next()?.ok()?;
    let marker = lexer.next()?.ok()?;
    if first.span().start() != 0 || marker.bytes() != b"obj" {
        return None;
    }
    Some(ObjectRef::new(
        u32::try_from(unsigned(first.bytes())?).ok()?,
        u16::try_from(unsigned(second.bytes())?).ok()?,
    ))
}

fn shifted(offset: u64, shift: u64, at: usize) -> Result<usize, DocError> {
    offset
        .checked_add(shift)
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| malformed(at, "xref offset"))
}

fn section(
    buf: &Arc<[u8]>,
    offset: usize,
    shift: u64,
    limits: &Limits,
    warnings: &mut Vec<Warning>,
) -> Result<Section, DocError> {
    if keyword_at(buf, offset, b"xref") {
        table(buf, offset, shift, limits, warnings)
    } else {
        stream(buf, offset, shift, limits, warnings)
    }
}

fn required<'a>(lexer: &mut Lexer<'a>, base: usize) -> Result<Token<'a>, DocError> {
    match lexer.next() {
        Some(Ok(token)) => Ok(token),
        _ => Err(malformed(
            base.saturating_add(lexer.offset()),
            "xref",
        )),
    }
}

fn table(
    buf: &Arc<[u8]>,
    offset: usize,
    shift: u64,
    limits: &Limits,
    warnings: &mut Vec<Warning>,
) -> Result<Section, DocError> {
    let input = buf.get(offset..).ok_or_else(|| malformed(offset, "xref"))?;
    let mut lexer = Lexer::with_limits(input, LexerLimits::new(usize::MAX, 64, 0));
    if required(&mut lexer, offset)?.bytes() != b"xref" {
        return Err(malformed(offset, "xref"));
    }
    let mut pairs = Vec::new();
    let mut first_subsection = true;
    let trailer;
    loop {
        let first = required(&mut lexer, offset)?;
        if first.bytes() == b"trailer" {
            let start = offset.saturating_add(lexer.offset());
            trailer = parse_value(buf, start..buf.len(), None, limits)?;
            break;
        }
        let start =
            unsigned(first.bytes()).ok_or_else(|| malformed(offset, "xref subsection"))?;
        let count =
            unsigned(required(&mut lexer, offset)?.bytes()).ok_or_else(|| {
                malformed(offset, "xref subsection")
            })?;
        check(LimitKind::Objects, count, u64::from(limits.max_objects))?;
        let end = start
            .checked_add(count)
            .ok_or_else(|| malformed(offset, "xref subsection"))?;
        let allowance =
            u64::from(limits.max_objects).saturating_add(u64::from(first_subsection && start == 1));
        check(LimitKind::Objects, end, allowance)?;
        let first_pair = pairs.len();
        for number in start..end {
            let field =
                unsigned(required(&mut lexer, offset)?.bytes()).ok_or_else(|| {
                    malformed(offset, "xref entry")
                })?;
            let generation = unsigned(required(&mut lexer, offset)?.bytes())
                .and_then(|n| u16::try_from(n).ok())
                .ok_or_else(|| malformed(offset, "xref generation"))?;
            let state = required(&mut lexer, offset)?;
            let entry = match state.bytes() {
                b"f" => XrefEntry::Free,
                b"n" => XrefEntry::InUse {
                    offset: field
                        .checked_add(shift)
                        .ok_or_else(|| malformed(offset, "object offset"))?,
                    generation,
                },
                _ => return Err(malformed(offset, "xref entry")),
            };
            let number =
                u32::try_from(number).map_err(|_| malformed(offset, "xref number"))?;
            pairs.push((number, entry));
        }
        if first_subsection && start == 1 && count >= 2 {
            let first_free = pairs
                .get(first_pair)
                .map(|pair| pair.1 == XrefEntry::Free)
                .unwrap_or(false);
            let points_to_one = pairs
                .get(first_pair.saturating_add(1))
                .and_then(|pair| match pair.1 {
                    XrefEntry::InUse { offset, .. } => usize::try_from(offset)
                        .ok()
                        .and_then(|offset| header_at(buf, offset)),
                    _ => None,
                })
                == Some(ObjectRef::new(1, 0));
            if first_free && points_to_one {
                for (number, _) in pairs.iter_mut().skip(first_pair) {
                    *number = number.saturating_sub(1);
                }
                warnings.push(Warning {
                    offset: Some(offset as u64),
                    object: None,
                    what: "xref_subsection_adjusted",
                });
            }
        }
        first_subsection = false;
        check(
            LimitKind::Objects,
            pairs.len() as u64,
            u64::from(limits.max_objects),
        )?;
    }
    let dict = trailer
        .dict()
        .ok_or_else(|| malformed(offset, "trailer"))?;
    let size = size(dict, limits, offset)?;
    let mut entries = alloc::vec![None; size];
    for (number, entry) in pairs {
        let slot = entries
            .get_mut(number as usize)
            .ok_or_else(|| malformed(offset, "xref Size"))?;
        *slot = Some(entry);
    }
    Ok(Section {
        entries,
        trailer,
        kind: XrefKind::Table,
    })
}

fn stream(
    buf: &Arc<[u8]>,
    offset: usize,
    shift: u64,
    limits: &Limits,
    warnings: &mut Vec<Warning>,
) -> Result<Section, DocError> {
    let id = header_at(buf, offset).ok_or_else(|| malformed(offset, "xref stream header"))?;
    let trailer = parse_indirect(buf, offset, id, limits)?;
    let dict = trailer
        .dict()
        .ok_or_else(|| malformed(offset, "xref stream dictionary"))?;
    if !dict
        .get(b"Type")
        .and_then(|value| value.as_name())
        .map(|name| name.eq_bytes(b"XRef"))
        .unwrap_or(false)
    {
        return Err(malformed(offset, "xref stream Type"));
    }
    let size = size(dict, limits, offset)?;
    let widths = dict
        .get(b"W")
        .and_then(|value| value.as_array())
        .ok_or_else(|| malformed(offset, "xref W"))?;
    if widths.len() != 3 {
        return Err(malformed(offset, "xref W"));
    }
    let mut w = [0usize; 3];
    for (slot, value) in w.iter_mut().zip(widths.iter()) {
        *slot = value
            .as_i64()
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| *n <= 8)
            .ok_or_else(|| malformed(offset, "xref W"))?;
    }
    let stride = w
        .iter()
        .try_fold(0usize, |sum, width| sum.checked_add(*width))
        .ok_or_else(|| malformed(offset, "xref W"))?;
    let mut ranges = Vec::new();
    if let Some(index) = dict.get(b"Index") {
        let index = index
            .as_array()
            .ok_or_else(|| malformed(offset, "xref Index"))?;
        if index.len() % 2 != 0 {
            return Err(malformed(offset, "xref Index"));
        }
        let mut values = index.iter();
        while let Some(first) = values.next() {
            let start = first
                .as_i64()
                .and_then(|n| usize::try_from(n).ok())
                .ok_or_else(|| malformed(offset, "xref Index"))?;
            let count = values
                .next()
                .and_then(|value| value.as_i64())
                .and_then(|n| usize::try_from(n).ok())
                .ok_or_else(|| malformed(offset, "xref Index"))?;
            let end = start
                .checked_add(count)
                .filter(|end| *end <= size)
                .ok_or_else(|| malformed(offset, "xref Index"))?;
            ranges.push(start..end);
        }
    } else {
        ranges.push(0..size);
    }
    let rows = ranges
        .iter()
        .try_fold(0usize, |sum, range| {
            sum.checked_add(range.end.saturating_sub(range.start))
        })
        .ok_or_else(|| malformed(offset, "xref Index"))?;
    check(
        LimitKind::Objects,
        rows as u64,
        u64::from(limits.max_objects),
    )?;
    let needed = rows
        .checked_mul(stride)
        .ok_or_else(|| malformed(offset, "xref data size"))?;
    check(
        LimitKind::DecodedBytes,
        needed as u64,
        limits.max_decoded_bytes,
    )?;
    let steps = filter_steps(&trailer, |value| {
        if value.as_ref().is_some() {
            Err(DocError::Unsupported {
                feature: "xref:indirect_filter",
            })
        } else {
            Ok(value.to_owned())
        }
    })?;
    let raw = trailer
        .stream_raw()
        .ok_or_else(|| malformed(offset, "xref stream data"))?;
    let decoded = crate::filter::decode(raw, &steps, limits.max_decoded_bytes)
        .map_err(DocError::from)?;
    if decoded.image.is_some() || decoded.truncated || decoded.bytes.len() < needed {
        return Err(malformed(offset, "xref stream data"));
    }
    if trailer.stream_recovered() {
        warnings.push(Warning {
            offset: Some(offset as u64),
            object: Some(id),
            what: "stream_length_recovered",
        });
    }
    let mut entries = alloc::vec![None; size];
    let mut position = 0usize;
    for range in ranges {
        for number in range {
            let mut fields = [0u64; 3];
            for (field, width) in fields.iter_mut().zip(w) {
                let end = position
                    .checked_add(width)
                    .ok_or_else(|| malformed(offset, "xref field"))?;
                let bytes = decoded
                    .bytes
                    .get(position..end)
                    .ok_or_else(|| malformed(offset, "xref field"))?;
                for byte in bytes {
                    *field = field
                        .checked_mul(256)
                        .and_then(|n| n.checked_add(u64::from(*byte)))
                        .ok_or_else(|| malformed(offset, "xref field"))?;
                }
                position = end;
            }
            let [mut kind, field, generation] = fields;
            if w.first() == Some(&0) {
                kind = 1;
            }
            let entry = match kind {
                0 => Some(XrefEntry::Free),
                1 => Some(XrefEntry::InUse {
                    offset: field
                        .checked_add(shift)
                        .ok_or_else(|| malformed(offset, "object offset"))?,
                    generation: u16::try_from(generation)
                        .map_err(|_| malformed(offset, "xref generation"))?,
                }),
                2 => Some(XrefEntry::Compressed {
                    stream: u32::try_from(field)
                        .map_err(|_| malformed(offset, "object stream number"))?,
                    index: u32::try_from(generation)
                        .map_err(|_| malformed(offset, "object stream index"))?,
                }),
                _ => None,
            };
            let slot = entries
                .get_mut(number)
                .ok_or_else(|| malformed(offset, "xref Index"))?;
            *slot = entry;
        }
    }
    Ok(Section {
        entries,
        trailer,
        kind: XrefKind::Stream,
    })
}

fn size(dict: DictRef<'_>, limits: &Limits, offset: usize) -> Result<usize, DocError> {
    let size = integer(dict, b"Size")?.ok_or_else(|| malformed(offset, "xref Size"))?;
    check(LimitKind::Objects, size, u64::from(limits.max_objects))?;
    usize::try_from(size).map_err(|_| malformed(offset, "xref Size"))
}

fn integer(dict: DictRef<'_>, key: &[u8]) -> Result<Option<u64>, DocError> {
    let Some(value) = dict.get(key) else {
        return Ok(None);
    };
    value
        .as_i64()
        .and_then(|n| u64::try_from(n).ok())
        .map(Some)
        .ok_or_else(|| malformed(value.span().start, "xref integer"))
}

