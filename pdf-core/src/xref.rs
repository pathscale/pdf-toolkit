use alloc::collections::BTreeSet;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::document::filter_steps;
use crate::error::{malformed, DocError, Warning};
use crate::lex::{Lexer, LexerLimits, Token};
use crate::limits::{check, LimitKind, Limits};
use crate::object::{
    keyword_at, parse_indirect_with_scratch, parse_value_with_scratch, unsigned, DictRef, Object,
    ParseScratch,
};
use crate::source::PdfSource;
use crate::work::ObjectRef;

#[derive(Default)]
struct DecodeBudget {
    decoded_bytes: u64,
    work_bytes: u64,
}

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
    exact_primary_offset: Option<usize>,
    parsed_streams: BTreeSet<(u32, u16, usize)>,
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
    /// Absolute source byte offset of the primary xref section when the
    /// reader's decoded `startxref` pointer resolved without location recovery
    /// or a header shift. This belongs to the primary trailer of this parse;
    /// “exact” describes pointer resolution only, not the pointer's lexical
    /// occurrence or the validity of the surrounding tail.
    pub fn exact_primary_offset(&self) -> Option<usize> {
        self.exact_primary_offset
    }
    pub(crate) fn parsed_stream(&self, object: &Object) -> bool {
        object.is_stream()
            && object.id().is_some_and(|id| {
                self.parsed_streams.contains(&(
                    id.number(),
                    id.generation(),
                    object.root().span().start,
                ))
            })
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
            exact_primary_offset: None,
            parsed_streams: BTreeSet::new(),
        }
    }
}

struct Section {
    entries: Vec<Option<XrefEntry>>,
    trailer: Object,
    kind: XrefKind,
    parsed_stream: Option<(u32, u16, usize)>,
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
    let (located_primary_offset, shift) = locate(bytes, original, header, &mut warnings)?;
    let exact_primary_offset = (shift == 0 && located_primary_offset == original)
        .then_some(located_primary_offset);
    let mut offset = located_primary_offset;
    let mut visited = BTreeSet::new();
    let mut parsed_streams = BTreeSet::new();
    let mut entries: Vec<Option<XrefEntry>> = Vec::new();
    let mut newest = None;
    let mut kind = XrefKind::Table;
    let mut sections = 0u32;
    let mut decode_budget = DecodeBudget::default();
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
        let mut section = section(&buf, offset, shift, limits, &mut decode_budget, &mut warnings)?;
        if let Some(parsed_stream) = section.parsed_stream {
            parsed_streams.insert(parsed_stream);
        }
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
                let supplemental = stream(&buf, hybrid, shift, limits, &mut decode_budget, &mut warnings)?;
                if let Some(parsed_stream) = supplemental.parsed_stream {
                    parsed_streams.insert(parsed_stream);
                }
                if supplemental.entries.len() > section.entries.len() {
                    grow_entries(
                        &mut section.entries,
                        supplemental.entries.len(),
                        allocation_maximum(limits, buf.len()),
                    )?;
                }
                for (number, entry) in supplemental.entries.into_iter().enumerate() {
                    if let Some(slot) = section.entries.get_mut(number) {
                        if (slot.is_none() || *slot == Some(XrefEntry::Free)) && entry.is_some() {
                            *slot = entry;
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
        grow_entries(
            &mut entries,
            section.entries.len(),
            allocation_maximum(limits, buf.len()),
        )?;
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
    let maximum = allocation_maximum(limits, buf.len());
    let mut final_entries = Vec::new();
    final_entries
        .try_reserve_exact(entries.len())
        .map_err(|_| allocation_failure(maximum))?;
    for entry in entries {
        final_entries.push(entry.unwrap_or(XrefEntry::Free));
    }
    Ok((
        Xref {
            entries: final_entries,
            kind,
            sections,
            exact_primary_offset,
            parsed_streams,
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
    budget: &mut DecodeBudget,
    warnings: &mut Vec<Warning>,
) -> Result<Section, DocError> {
    if keyword_at(buf, offset, b"xref") {
        table(buf, offset, shift, limits, warnings)
    } else {
        stream(buf, offset, shift, limits, budget, warnings)
    }
}

fn required<'a>(lexer: &mut Lexer<'a>, base: usize) -> Result<Token<'a>, DocError> {
    match lexer.next() {
        Some(Ok(token)) => Ok(token),
        _ => Err(malformed(base.saturating_add(lexer.offset()), "xref")),
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
    let mut scratch = ParseScratch::default();
    let mut first_subsection = true;
    let trailer;
    loop {
        let first = required(&mut lexer, offset)?;
        if first.bytes() == b"trailer" {
            let start = offset.saturating_add(lexer.offset());
            trailer = parse_value_with_scratch(buf, start..buf.len(), None, limits, &mut scratch)?;
            break;
        }
        let start = unsigned(first.bytes()).ok_or_else(|| malformed(offset, "xref subsection"))?;
        let count = unsigned(required(&mut lexer, offset)?.bytes())
            .ok_or_else(|| malformed(offset, "xref subsection"))?;
        check(LimitKind::Objects, count, u64::from(limits.max_objects))?;
        let end = start
            .checked_add(count)
            .ok_or_else(|| malformed(offset, "xref subsection"))?;
        let allowance =
            u64::from(limits.max_objects).saturating_add(u64::from(first_subsection && start == 1));
        check(LimitKind::Objects, end, allowance)?;
        let maximum = allocation_maximum(limits, buf.len());
        let count_usize =
            usize::try_from(count).map_err(|_| malformed(offset, "xref subsection"))?;
        let pair_count = pairs
            .len()
            .checked_add(count_usize)
            .ok_or_else(|| allocation_failure(maximum))?;
        check(LimitKind::Objects, pair_count as u64, maximum)?;
        pairs
            .try_reserve_exact(count_usize)
            .map_err(|_| allocation_failure(maximum))?;
        let first_pair = pairs.len();
        for number in start..end {
            let field = unsigned(required(&mut lexer, offset)?.bytes())
                .ok_or_else(|| malformed(offset, "xref entry"))?;
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
            let number = u32::try_from(number).map_err(|_| malformed(offset, "xref number"))?;
            pairs.push((number, entry));
        }
        if first_subsection && start == 1 && count >= 2 {
            let first_free = pairs
                .get(first_pair)
                .map(|pair| pair.1 == XrefEntry::Free)
                .unwrap_or(false);
            let points_to_one =
                pairs
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
    let dict = trailer.dict().ok_or_else(|| malformed(offset, "trailer"))?;
    let size = size(dict, limits, offset, buf.len())?;
    let mut entries = Vec::new();
    grow_entries(&mut entries, size, allocation_maximum(limits, buf.len()))?;
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
        parsed_stream: None,
    })
}

fn stream(
    buf: &Arc<[u8]>,
    offset: usize,
    shift: u64,
    limits: &Limits,
    budget: &mut DecodeBudget,
    warnings: &mut Vec<Warning>,
) -> Result<Section, DocError> {
    let id = header_at(buf, offset).ok_or_else(|| malformed(offset, "xref stream header"))?;
    let mut scratch = ParseScratch::default();
    let mut trailer = parse_indirect_with_scratch(buf, offset, id, limits, &mut scratch)?;
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
    let size = size(dict, limits, offset, buf.len())?;
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
    // Preserve the baseline local row-size refusal before filter metadata lookup.
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
    let encoded = raw.len() as u64;
    let work_maximum = limits.max_total_decode_work_bytes;
    let Some(encoded_requested) = budget.work_bytes.checked_add(encoded) else {
        return Err(refusal(LimitKind::DecodeWorkBytes, u64::MAX, work_maximum));
    };
    if encoded_requested > work_maximum {
        return Err(refusal(
            LimitKind::DecodeWorkBytes,
            encoded_requested,
            work_maximum,
        ));
    }
    budget.work_bytes = encoded_requested;

    let (output_maximum, output_limit) = output_limit(limits, budget);
    if needed as u64 > output_maximum {
        let (limit, requested, maximum) = match output_limit {
            LimitKind::TotalDecodedBytes => (
                output_limit,
                budget.decoded_bytes.checked_add(needed as u64).unwrap_or(u64::MAX),
                limits.max_total_decoded_bytes,
            ),
            LimitKind::DecodeWorkBytes => (
                output_limit,
                budget.work_bytes.checked_add(needed as u64).unwrap_or(u64::MAX),
                work_maximum,
            ),
            _ => (LimitKind::DecodedBytes, needed as u64, limits.max_decoded_bytes),
        };
        return Err(refusal(limit, requested, maximum));
    }
    let decoded = match crate::filter::decode(raw, &steps, output_maximum) {
        Ok(decoded) => decoded,
        Err(crate::filter::FilterError::OutputLimit { .. })
            if matches!(output_limit, LimitKind::TotalDecodedBytes | LimitKind::DecodeWorkBytes) =>
        {
            let maximum = match output_limit {
                LimitKind::TotalDecodedBytes => limits.max_total_decoded_bytes,
                _ => work_maximum,
            };
            return Err(refusal(output_limit, maximum.saturating_add(1), maximum));
        }
        Err(error @ crate::filter::FilterError::OutputLimit { .. }) => {
            // Preserve the established local decoder-cap error contract.
            return Err(DocError::from(error));
        }
        Err(error) => return Err(DocError::from(error)),
    };
    let decoded_len = decoded.bytes.len() as u64;
    let Some(decoded_requested) = budget.decoded_bytes.checked_add(decoded_len) else {
        return Err(refusal(
            LimitKind::TotalDecodedBytes,
            u64::MAX,
            limits.max_total_decoded_bytes,
        ));
    };
    if decoded_requested > limits.max_total_decoded_bytes {
        return Err(refusal(
            LimitKind::TotalDecodedBytes,
            decoded_requested,
            limits.max_total_decoded_bytes,
        ));
    }
    let Some(work_requested) = budget.work_bytes.checked_add(decoded_len) else {
        return Err(refusal(LimitKind::DecodeWorkBytes, u64::MAX, work_maximum));
    };
    if work_requested > work_maximum {
        return Err(refusal(LimitKind::DecodeWorkBytes, work_requested, work_maximum));
    }
    budget.decoded_bytes = decoded_requested;
    budget.work_bytes = work_requested;
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
    let mut entries = Vec::new();
    grow_entries(&mut entries, size, allocation_maximum(limits, buf.len()))?;
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
    let parsed_stream = (id.number(), id.generation(), trailer.root().span().start);
    trailer.mark_xref_stream();
    Ok(Section {
        entries,
        trailer,
        kind: XrefKind::Stream,
        parsed_stream: Some(parsed_stream),
    })
}

fn refusal(limit: LimitKind, requested: u64, maximum: u64) -> DocError {
    DocError::Refused(crate::limits::Refusal { limit, requested, maximum })
}

fn output_limit(limits: &Limits, budget: &DecodeBudget) -> (u64, LimitKind) {
    let total_remaining = limits.max_total_decoded_bytes.saturating_sub(budget.decoded_bytes);
    let work_remaining = limits.max_total_decode_work_bytes.saturating_sub(budget.work_bytes);
    let candidates = [
        (limits.max_decoded_bytes, 2u8, LimitKind::DecodedBytes),
        (total_remaining, 0u8, LimitKind::TotalDecodedBytes),
        (work_remaining, 1u8, LimitKind::DecodeWorkBytes),
    ];
    candidates
        .into_iter()
        .min_by_key(|(maximum, priority, _)| (*maximum, *priority))
        .map(|(maximum, _, limit)| (maximum, limit))
        .unwrap_or((limits.max_decoded_bytes, LimitKind::DecodedBytes))
}

#[cfg(test)]
mod decode_budget_tests {
    use alloc::format;
    use super::*;

    fn stream_bytes(filter: Option<&str>, data: &[u8]) -> Arc<[u8]> {
        let mut bytes = format!(
            "1 0 obj\n<< /Type /XRef /Size 4 /W [1 1 0] /Length {}{} >>\nstream\n",
            data.len(),
            filter.map(|name| format!(" /Filter /{name}")).unwrap_or_default(),
        )
        .into_bytes();
        bytes.extend_from_slice(data);
        bytes.extend_from_slice(b"\nendstream\nendobj\n");
        Arc::from(bytes)
    }

    fn run_stream(
        bytes: Arc<[u8]>,
        limits: &Limits,
        budget: &mut DecodeBudget,
    ) -> Result<Section, DocError> {
        stream(&bytes, 0, 0, limits, budget, &mut Vec::new())
    }

    fn append_xref_stream(bytes: &mut Vec<u8>, object: u32, prev: Option<usize>) -> usize {
        let offset = bytes.len();
        let previous = prev.map(|value| format!(" /Prev {value}")).unwrap_or_default();
        bytes.extend_from_slice(
            format!(
                "{object} 0 obj\n<< /Type /XRef /Size 4 /W [1 1 0] /Length 8{previous} >>\nstream\n"
            )
            .as_bytes(),
        );
        bytes.extend_from_slice(b"00000000\nendstream\nendobj\n");
        offset
    }

    fn two_stream_pdf(hybrid: bool) -> Arc<[u8]> {
        let mut bytes = b"%PDF-1.7\n".to_vec();
        let older = append_xref_stream(&mut bytes, 1, None);
        let supplement = append_xref_stream(&mut bytes, 2, None);
        if hybrid {
            let table = bytes.len();
            bytes.extend_from_slice(
                format!(
                    "xref\n0 1\n0000000000 65535 f\ntrailer\n<< /Size 4 /XRefStm {supplement} /Prev {older} >>\n"
                )
                .as_bytes(),
            );
            bytes.extend_from_slice(format!("startxref\n{table}\n%%EOF\n").as_bytes());
        } else {
            let newest = append_xref_stream(&mut bytes, 3, Some(older));
            bytes.extend_from_slice(format!("startxref\n{newest}\n%%EOF\n").as_bytes());
        }
        Arc::from(bytes)
    }

    fn read_pdf(bytes: Arc<[u8]>, limits: &Limits) -> Result<(Xref, Object, Vec<Warning>), DocError> {
        let source = PdfSource::new(bytes).expect("synthetic PDF header should parse");
        read(&source, limits)
    }

    fn table_pdf(pointer_delta: isize) -> (Arc<[u8]>, usize) {
        let mut bytes = b"%PDF-1.7\n".to_vec();
        let offset = bytes.len();
        bytes.extend_from_slice(
            b"xref\n0 1\n0000000000 65535 f\ntrailer\n<< /Size 1 >>\n",
        );
        let pointer = offset.checked_add_signed(pointer_delta).unwrap();
        bytes.extend_from_slice(format!("startxref\n{pointer}\n%%EOF\n").as_bytes());
        (Arc::from(bytes), offset)
    }

    #[test]
    fn exact_primary_offset_tracks_unadjusted_pointer_only() {
        let (bytes, offset) = table_pdf(0);
        let (xref, _, warnings) = read_pdf(bytes, &Limits::default()).unwrap();
        assert_eq!(warnings.len(), 0);
        assert_eq!(xref.exact_primary_offset(), Some(offset));

        let (bytes, _) = table_pdf(1);
        let (xref, _, warnings) = read_pdf(bytes, &Limits::default()).unwrap();
        assert!(warnings.iter().any(|warning| warning.what == "startxref_adjusted"));
        assert_eq!(xref.exact_primary_offset(), None);

        let repaired = Xref::from_repaired(alloc::vec![XrefEntry::Free], XrefKind::Table);
        assert_eq!(repaired.exact_primary_offset(), None);
    }

    #[test]
    fn local_decoder_limit_keeps_decoded_bytes_refusal() {
        let bytes = stream_bytes(None, b"000000000");
        let limits = Limits { max_decoded_bytes: 8, ..Limits::default() };
        let error = run_stream(bytes, &limits, &mut DecodeBudget::default())
            .err().expect("xref stream should refuse");
        assert!(matches!(error, DocError::Refused(crate::limits::Refusal {
            limit: LimitKind::DecodedBytes, requested: 9, maximum: 8,
        })));
    }

    #[test]
    fn required_row_preflight_precedes_filter_metadata() {
        let bytes = stream_bytes(Some("UnsupportedFilter"), b"000000000");
        let limits = Limits { max_decoded_bytes: 7, ..Limits::default() };
        let error = run_stream(bytes, &limits, &mut DecodeBudget::default())
            .err().expect("xref stream should refuse");
        assert!(matches!(error, DocError::Refused(crate::limits::Refusal {
            limit: LimitKind::DecodedBytes, requested: 8, maximum: 7,
        })));
    }

    #[test]
    fn aggregate_equality_succeeds_and_charges_both_counters() {
        let bytes = stream_bytes(None, b"00000000");
        let limits = Limits {
            max_decoded_bytes: 8,
            max_total_decoded_bytes: 8,
            max_total_decode_work_bytes: 16,
            ..Limits::default()
        };
        let mut budget = DecodeBudget::default();
        run_stream(bytes, &limits, &mut budget).expect("exact aggregate limits should pass");
        assert_eq!(budget.decoded_bytes, 8);
        assert_eq!(budget.work_bytes, 16);
    }

    #[test]
    fn aggregate_row_preflight_reports_binding_budget() {
        let bytes = stream_bytes(None, b"00000000");
        let limits = Limits { max_total_decoded_bytes: 7, ..Limits::default() };
        let error = run_stream(bytes, &limits, &mut DecodeBudget::default())
            .err().expect("aggregate output budget should refuse");
        assert!(matches!(error, DocError::Refused(crate::limits::Refusal {
            limit: LimitKind::TotalDecodedBytes, requested: 8, maximum: 7,
        })));
    }

    #[test]
    fn prev_chain_shares_output_and_work_budgets() {
        let bytes = two_stream_pdf(false);
        let output_limited = Limits {
            max_total_decoded_bytes: 15,
            ..Limits::default()
        };
        let error = read_pdf(Arc::clone(&bytes), &output_limited)
            .err().expect("two xref streams exceed output budget");
        assert!(matches!(error, DocError::Refused(crate::limits::Refusal {
            limit: LimitKind::TotalDecodedBytes, requested: 16, maximum: 15,
        })));

        let work_limited = Limits {
            max_total_decode_work_bytes: 31,
            ..Limits::default()
        };
        let error = read_pdf(Arc::clone(&bytes), &work_limited)
            .err().expect("two xref streams exceed work budget");
        assert!(matches!(error, DocError::Refused(crate::limits::Refusal {
            limit: LimitKind::DecodeWorkBytes, requested: 32, maximum: 31,
        })));

        let exact = Limits {
            max_total_decoded_bytes: 16,
            max_total_decode_work_bytes: 32,
            ..Limits::default()
        };
        let (xref, _, _) = read_pdf(bytes, &exact).expect("exact aggregate limits should pass");
        assert_eq!(xref.sections(), 2);
    }

    #[test]
    fn hybrid_supplement_and_prev_share_budgets() {
        let bytes = two_stream_pdf(true);
        let output_limited = Limits { max_total_decoded_bytes: 15, ..Limits::default() };
        let error = read_pdf(Arc::clone(&bytes), &output_limited)
            .err().expect("hybrid and prior streams exceed output budget");
        assert!(matches!(error, DocError::Refused(crate::limits::Refusal {
            limit: LimitKind::TotalDecodedBytes, requested: 16, maximum: 15,
        })));

        let work_limited = Limits { max_total_decode_work_bytes: 31, ..Limits::default() };
        let error = read_pdf(Arc::clone(&bytes), &work_limited)
            .err().expect("hybrid and prior streams exceed work budget");
        assert!(matches!(error, DocError::Refused(crate::limits::Refusal {
            limit: LimitKind::DecodeWorkBytes, requested: 32, maximum: 31,
        })));

        let exact = Limits {
            max_total_decoded_bytes: 16,
            max_total_decode_work_bytes: 32,
            ..Limits::default()
        };
        let (xref, _, _) = read_pdf(bytes, &exact).expect("exact hybrid budgets should pass");
        assert_eq!(xref.kind(), XrefKind::Hybrid);
        assert_eq!(xref.sections(), 2);
    }

    #[test]
    fn table_only_open_does_not_consume_decode_budgets() {
        let mut bytes = b"%PDF-1.7\n".to_vec();
        let offset = bytes.len();
        bytes.extend_from_slice(
            format!(
                "xref\n0 1\n0000000000 65535 f\ntrailer\n<< /Size 1 >>\nstartxref\n{offset}\n%%EOF\n"
            )
            .as_bytes(),
        );
        let limits = Limits {
            max_total_decoded_bytes: 0,
            max_total_decode_work_bytes: 0,
            ..Limits::default()
        };
        let (xref, _, _) = read_pdf(Arc::from(bytes), &limits)
            .expect("table-only xref should not charge decode budgets");
        assert_eq!(xref.kind(), XrefKind::Table);
        assert_eq!(xref.sections(), 1);
    }

    #[test]
    fn encoded_work_overflow_refuses_before_filter_decode() {
        let bytes = stream_bytes(Some("FlateDecode"), b"00000000");
        let limits = Limits { max_total_decode_work_bytes: u64::MAX, ..Limits::default() };
        let mut budget = DecodeBudget { decoded_bytes: 0, work_bytes: u64::MAX };
        let error = run_stream(bytes, &limits, &mut budget)
            .err().expect("encoded work overflow should refuse");
        assert!(matches!(error, DocError::Refused(crate::limits::Refusal {
            limit: LimitKind::DecodeWorkBytes, requested: u64::MAX, maximum: u64::MAX,
        })));
        assert_eq!(budget.decoded_bytes, 0);
        assert_eq!(budget.work_bytes, u64::MAX);
    }
}

fn allocation_maximum(limits: &Limits, file_len: usize) -> u64 {
    u64::from(limits.max_objects).min((file_len as u64).saturating_mul(8).saturating_add(1024))
}

fn allocation_failure(maximum: u64) -> DocError {
    DocError::Refused(crate::limits::Refusal {
        limit: LimitKind::Objects,
        requested: maximum.saturating_add(1),
        maximum,
    })
}

fn grow_entries(
    entries: &mut Vec<Option<XrefEntry>>,
    length: usize,
    maximum: u64,
) -> Result<(), DocError> {
    check(LimitKind::Objects, length as u64, maximum)?;
    if length > entries.len() {
        entries
            .try_reserve_exact(length - entries.len())
            .map_err(|_| allocation_failure(maximum))?;
        entries.resize(length, None);
    }
    Ok(())
}

fn size(
    dict: DictRef<'_>,
    limits: &Limits,
    offset: usize,
    file_len: usize,
) -> Result<usize, DocError> {
    let size = integer(dict, b"Size")?.ok_or_else(|| malformed(offset, "xref Size"))?;
    check(
        LimitKind::Objects,
        size,
        allocation_maximum(limits, file_len),
    )?;
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
