use std::collections::BTreeMap;
use std::io;
use std::ops::Range;
use std::sync::Arc;

use super::bounds::*;
use super::emit::{dict, name, reference, Output};
use crate::Value;

pub(super) fn slice(bytes: &[u8], at: usize, length: usize) -> io::Result<&[u8]> {
    bytes
        .get(at..add(at, length)?)
        .ok_or_else(|| invalid("font or image data is truncated"))
}

pub(super) fn read16(bytes: &[u8], at: usize) -> io::Result<u16> {
    let bytes = slice(bytes, at, 2)?;
    let a = *bytes
        .first()
        .ok_or_else(|| invalid("missing integer byte"))?;
    let b = *bytes
        .get(1)
        .ok_or_else(|| invalid("missing integer byte"))?;
    Ok(u16::from_be_bytes([a, b]))
}

pub(super) fn signed16(bytes: &[u8], at: usize) -> io::Result<i16> {
    Ok(read16(bytes, at)? as i16)
}

pub(super) fn read32(bytes: &[u8], at: usize) -> io::Result<u32> {
    let bytes = slice(bytes, at, 4)?;
    let mut word = [0u8; 4];
    word.copy_from_slice(bytes);
    Ok(u32::from_be_bytes(word))
}

pub(super) fn patch(out: &mut [u8], at: usize, bytes: &[u8]) -> io::Result<()> {
    out.get_mut(at..add(at, bytes.len())?)
        .ok_or_else(|| invalid("font patch outside table"))?
        .copy_from_slice(bytes);
    Ok(())
}

pub(super) fn set16(out: &mut [u8], at: usize, value: u16) -> io::Result<()> {
    patch(out, at, &value.to_be_bytes())
}

pub(super) fn set32(out: &mut [u8], at: usize, value: u32) -> io::Result<()> {
    patch(out, at, &value.to_be_bytes())
}

pub(super) fn size32(value: usize) -> io::Result<u32> {
    u32::try_from(value).map_err(|_| invalid("font offset exceeds u32"))
}

struct Segment {
    start: u16,
    end: u16,
    delta: u16,
    offset: Option<usize>,
}

struct Group {
    start: u32,
    end: u32,
    glyph: u32,
}

enum Cmap {
    Four(Vec<Segment>),
    Twelve(Vec<Group>),
    Six { start: u16, glyphs: Vec<u16> },
    Zero(Vec<u8>),
}

pub(super) struct TrueType {
    pub data: Arc<[u8]>,
    pub tables: BTreeMap<[u8; 4], Range<usize>>,
    pub offsets: Vec<usize>,
    pub glyph_count: u16,
    pub units: u16,
    pub metrics: u16,
    pub used: BTreeMap<u16, char>,
    cmap: Cmap,
    /// Absolute offset and pair count of a horizontal format 0 `kern` subtable.
    kern: Option<(usize, usize)>,
    pub(super) base_name: String,
}

impl TrueType {
    pub fn table(&self, tag: &[u8; 4]) -> io::Result<&[u8]> {
        let range = self
            .tables
            .get(tag)
            .ok_or_else(|| invalid("required TrueType table is missing"))?;
        self.data
            .get(range.clone())
            .ok_or_else(|| invalid("TrueType table outside font"))
    }

    pub fn glyph_bytes(&self, glyph: u16) -> io::Result<&[u8]> {
        if glyph >= self.glyph_count {
            return Err(invalid("glyph identifier exceeds font glyph count"));
        }
        let index = usize::from(glyph);
        let start = *self
            .offsets
            .get(index)
            .ok_or_else(|| invalid("missing loca entry"))?;
        let end = *self
            .offsets
            .get(add(index, 1)?)
            .ok_or_else(|| invalid("missing loca entry"))?;
        self.table(b"glyf")?
            .get(start..end)
            .ok_or_else(|| invalid("glyph outside glyf table"))
    }

    pub fn new(data: Arc<[u8]>) -> io::Result<Self> {
        check("font_bytes", data.len(), MAX_FONT_BYTES)?;
        if read32(&data, 0)? != 0x0001_0000 && slice(&data, 0, 4)? != b"true" {
            return Err(invalid("only standalone glyf TrueType fonts are supported"));
        }
        let count = usize::from(read16(&data, 4)?);
        check("font_tables", count, 256)?;
        slice(&data, 12, mul(count, 16)?)?;
        let directory_end = add(12, mul(count, 16)?)?;
        let mut tables = BTreeMap::new();
        for index in 0..count {
            let at = add(12, mul(index, 16)?)?;
            let mut tag = [0u8; 4];
            tag.copy_from_slice(slice(&data, at, 4)?);
            let start = usize::try_from(read32(&data, add(at, 8)?)?)
                .map_err(|_| invalid("font table offset overflow"))?;
            let length = usize::try_from(read32(&data, add(at, 12)?)?)
                .map_err(|_| invalid("font table length overflow"))?;
            let end = add(start, length)?;
            slice(&data, start, length)?;
            if length != 0 && start < directory_end {
                return Err(invalid("font table overlaps directory"));
            }
            if tables.insert(tag, start..end).is_some() {
                return Err(invalid("duplicate TrueType table"));
            }
        }
        let mut ranges: Vec<_> = tables
            .values()
            .filter(|range| !range.is_empty())
            .cloned()
            .collect();
        ranges.sort_by_key(|range| range.start);
        for pair in ranges.windows(2) {
            let first = pair
                .first()
                .ok_or_else(|| invalid("missing font table range"))?;
            let second = pair
                .get(1)
                .ok_or_else(|| invalid("missing font table range"))?;
            if first.end > second.start {
                return Err(invalid("overlapping TrueType tables"));
            }
        }
        let get = |tag: &[u8; 4]| -> io::Result<&[u8]> {
            let range = tables
                .get(tag)
                .ok_or_else(|| invalid("required TrueType table is missing"))?;
            data.get(range.clone())
                .ok_or_else(|| invalid("TrueType table outside font"))
        };
        let head = get(b"head")?;
        slice(head, 0, 54)?;
        if read32(head, 12)? != 0x5f0f_3cf5 {
            return Err(invalid("TrueType head magic is invalid"));
        }
        let units = read16(head, 18)?;
        if !(16..=16384).contains(&units) {
            return Err(invalid("TrueType units per em are invalid"));
        }
        let loca_format = signed16(head, 50)?;
        if !matches!(loca_format, 0 | 1) {
            return Err(invalid("TrueType loca format is invalid"));
        }
        let maxp = get(b"maxp")?;
        slice(maxp, 0, 32)?;
        if read32(maxp, 0)? != 0x0001_0000 {
            return Err(invalid("glyf font requires maxp version one"));
        }
        let glyph_count = read16(maxp, 4)?;
        if glyph_count == 0 {
            return Err(invalid("TrueType font has no glyphs"));
        }
        let hhea = get(b"hhea")?;
        slice(hhea, 0, 36)?;
        let metrics = read16(hhea, 34)?;
        if metrics == 0 || metrics > glyph_count {
            return Err(invalid("TrueType horizontal metric count is invalid"));
        }
        let metric_bytes = add(
            mul(usize::from(metrics), 4)?,
            mul(usize::from(glyph_count - metrics), 2)?,
        )?;
        slice(get(b"hmtx")?, 0, metric_bytes)?;
        if let Some(range) = tables.get(b"OS/2") {
            let os2 = data
                .get(range.clone())
                .ok_or_else(|| invalid("OS/2 outside font"))?;
            slice(os2, 0, 78)?;
            if read16(os2, 8)? & (0x0002 | 0x0100 | 0x0200) != 0 {
                return Err(invalid("font embedding or subsetting is restricted"));
            }
        }
        let loca = get(b"loca")?;
        let glyf_length = get(b"glyf")?.len();
        let entries = add(usize::from(glyph_count), 1)?;
        let stride = if loca_format == 0 { 2 } else { 4 };
        slice(loca, 0, mul(entries, stride)?)?;
        let mut offsets = Vec::new();
        let mut previous = 0usize;
        for index in 0..entries {
            let at = mul(index, stride)?;
            let offset = if loca_format == 0 {
                mul(usize::from(read16(loca, at)?), 2)?
            } else {
                usize::try_from(read32(loca, at)?).map_err(|_| invalid("loca offset overflow"))?
            };
            if !(previous..=glyf_length).contains(&offset) {
                return Err(invalid("loca offsets are invalid"));
            }
            push(&mut offsets, offset, "loca_entries", 65536)?;
            previous = offset;
        }
        let cmap_range = tables
            .get(b"cmap")
            .ok_or_else(|| invalid("TrueType cmap is missing"))?;
        let cmap = parse_cmap(get(b"cmap")?, cmap_range.start)?;
        let base_name = font_name(
            tables
                .get(b"name")
                .and_then(|range| data.get(range.clone())),
        );
        let kern = match tables.get(b"kern") {
            Some(range) => parse_kern(&data, range.clone())?,
            None => None,
        };
        Ok(Self {
            data,
            tables,
            offsets,
            glyph_count,
            units,
            metrics,
            used: BTreeMap::new(),
            cmap,
            kern,
            base_name,
        })
    }

    pub fn glyph(&self, character: char) -> io::Result<u16> {
        let code = u32::from(character);
        let glyph = match &self.cmap {
            Cmap::Four(segments) => {
                if code > 0xffff {
                    0
                } else {
                    let code = code as u16;
                    let index = segments.partition_point(|segment| segment.end < code);
                    match segments.get(index).filter(|segment| segment.start <= code) {
                        Some(segment) => match segment.offset {
                            None => code.wrapping_add(segment.delta),
                            Some(offset) => {
                                let at = add(offset, mul(usize::from(code - segment.start), 2)?)?;
                                let glyph = read16(&self.data, at)?;
                                if glyph == 0 {
                                    0
                                } else {
                                    glyph.wrapping_add(segment.delta)
                                }
                            }
                        },
                        None => 0,
                    }
                }
            }
            Cmap::Twelve(groups) => {
                let index = groups.partition_point(|group| group.end < code);
                match groups.get(index).filter(|group| group.start <= code) {
                    Some(group) => {
                        let glyph = group
                            .glyph
                            .checked_add(code - group.start)
                            .ok_or_else(|| invalid("cmap glyph overflow"))?;
                        u16::try_from(glyph).map_err(|_| invalid("cmap glyph exceeds u16"))?
                    }
                    None => 0,
                }
            }
            Cmap::Six { start, glyphs } => code
                .checked_sub(u32::from(*start))
                .and_then(|index| usize::try_from(index).ok())
                .and_then(|index| glyphs.get(index))
                .copied()
                .unwrap_or(0),
            Cmap::Zero(glyphs) => usize::try_from(code)
                .ok()
                .and_then(|index| glyphs.get(index))
                .copied()
                .map(u16::from)
                .unwrap_or(0),
        };
        if glyph >= self.glyph_count {
            return Err(invalid("cmap refers to a nonexistent glyph"));
        }
        Ok(glyph)
    }

    pub fn metric(&self, glyph: u16) -> io::Result<(u16, i16)> {
        if glyph >= self.glyph_count {
            return Err(invalid("metric glyph exceeds font glyph count"));
        }
        let bytes = self.table(b"hmtx")?;
        let metric_index = glyph.min(self.metrics - 1);
        let advance = read16(bytes, mul(usize::from(metric_index), 4)?)?;
        let bearing_at = if glyph < self.metrics {
            add(mul(usize::from(glyph), 4)?, 2)?
        } else {
            add(
                mul(usize::from(self.metrics), 4)?,
                mul(usize::from(glyph - self.metrics), 2)?,
            )?
        };
        Ok((advance, signed16(bytes, bearing_at)?))
    }

    /// Pair adjustment in font units from the `kern` table, zero when absent.
    pub fn kerning(&self, left: u16, right: u16) -> io::Result<i16> {
        let Some((start, count)) = self.kern else {
            return Ok(0);
        };
        let key = (u32::from(left) << 16) | u32::from(right);
        let (mut low, mut high) = (0usize, count);
        while low < high {
            let middle = low + (high - low) / 2;
            let at = add(start, mul(middle, 6)?)?;
            let pair = read32(&self.data, at)?;
            if pair == key {
                return signed16(&self.data, add(at, 4)?);
            }
            if pair < key {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        Ok(0)
    }

    /// The hhea ascender in 1/1000 em.
    pub fn ascent(&self) -> io::Result<f64> {
        Ok(self.scale(i32::from(signed16(self.table(b"hhea")?, 4)?)))
    }

    pub fn scale(&self, number: i32) -> f64 {
        f64::from(number) * 1000.0 / f64::from(self.units)
    }

    pub fn write(&self, out: &mut Output) -> io::Result<u32> {
        let (program, tag) = super::subset::build(self)?;
        let base_name = format!("{tag}+{}", self.base_name);
        let file_id = out.alloc()?;
        let descriptor_id = out.alloc()?;
        let cid_id = out.alloc()?;
        let unicode_id = out.alloc()?;
        let type0_id = out.alloc()?;
        out.stream(
            file_id,
            vec![("Length1".to_owned(), Value::Int(program.len() as i64))],
            &program,
            out.compress,
        )?;
        let head = self.table(b"head")?;
        let hhea = self.table(b"hhea")?;
        let post = self
            .tables
            .get(b"post")
            .and_then(|range| self.data.get(range.clone()));
        let italic = match post {
            Some(bytes) if bytes.len() >= 32 => f64::from(read32(bytes, 4)? as i32) / 65536.0,
            _ => 0.0,
        };
        let fixed = match post {
            Some(bytes) if bytes.len() >= 32 => read32(bytes, 12)? != 0,
            _ => false,
        };
        let ascent = self.scale(i32::from(signed16(hhea, 4)?));
        let descent = self.scale(i32::from(signed16(hhea, 6)?));
        let cap_height = match self
            .tables
            .get(b"OS/2")
            .and_then(|range| self.data.get(range.clone()))
        {
            Some(bytes) if bytes.len() >= 90 && read16(bytes, 0)? >= 2 => {
                self.scale(i32::from(signed16(bytes, 88)?))
            }
            _ => ascent,
        };
        let mut bbox = Vec::new();
        for at in [36, 38, 40, 42] {
            bbox.push(Value::Real(self.scale(i32::from(signed16(head, at)?))));
        }
        let flags = 32 | if fixed { 1 } else { 0 } | if italic != 0.0 { 64 } else { 0 };
        out.object(
            descriptor_id,
            dict(vec![
                ("Type", name("FontDescriptor")),
                ("FontName", name(&base_name)),
                ("Flags", Value::Int(flags)),
                ("FontBBox", Value::Array(bbox)),
                ("ItalicAngle", Value::Real(italic)),
                ("Ascent", Value::Real(ascent)),
                ("Descent", Value::Real(descent)),
                ("CapHeight", Value::Real(cap_height)),
                ("StemV", Value::Int(80)),
                ("FontFile2", reference(file_id)),
            ]),
        )?;
        let mut widths = Vec::new();
        for glyph in self.used.keys() {
            push(
                &mut widths,
                Value::Int(i64::from(*glyph)),
                "font_width_values",
                131070,
            )?;
            let width = self.scale(i32::from(self.metric(*glyph)?.0));
            push(
                &mut widths,
                Value::Array(vec![Value::Real(width)]),
                "font_width_values",
                131070,
            )?;
        }
        out.object(
            cid_id,
            dict(vec![
                ("Type", name("Font")),
                ("Subtype", name("CIDFontType2")),
                ("BaseFont", name(&base_name)),
                (
                    "CIDSystemInfo",
                    dict(vec![
                        ("Registry", Value::Str(b"Adobe".to_vec())),
                        ("Ordering", Value::Str(b"Identity".to_vec())),
                        ("Supplement", Value::Int(0)),
                    ]),
                ),
                ("FontDescriptor", reference(descriptor_id)),
                ("CIDToGIDMap", name("Identity")),
                ("DW", Value::Int(1000)),
                ("W", Value::Array(widths)),
            ]),
        )?;
        let cmap = self.to_unicode()?;
        out.stream(unicode_id, Vec::new(), &cmap, out.compress)?;
        out.object(
            type0_id,
            dict(vec![
                ("Type", name("Font")),
                ("Subtype", name("Type0")),
                ("BaseFont", name(&base_name)),
                ("Encoding", name("Identity-H")),
                ("DescendantFonts", Value::Array(vec![reference(cid_id)])),
                ("ToUnicode", reference(unicode_id)),
            ]),
        )?;
        Ok(type0_id)
    }

    fn to_unicode(&self) -> io::Result<Vec<u8>> {
        let mut out = copied(
            b"/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n\
/CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
/CMapName /HouseToUnicode def\n/CMapType 2 def\n\
1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n",
            MAX_VALUE_BYTES,
        )?;
        let entries: Vec<_> = self.used.iter().collect();
        for chunk in entries.chunks(100) {
            append(
                &mut out,
                format!("{} beginbfchar\n", chunk.len()).as_bytes(),
                MAX_VALUE_BYTES,
            )?;
            for (glyph, character) in chunk {
                append(
                    &mut out,
                    format!("<{glyph:04X}> <").as_bytes(),
                    MAX_VALUE_BYTES,
                )?;
                let mut units = [0u16; 2];
                for unit in character.encode_utf16(&mut units).iter() {
                    append(&mut out, format!("{unit:04X}").as_bytes(), MAX_VALUE_BYTES)?;
                }
                append(&mut out, b">\n", MAX_VALUE_BYTES)?;
            }
            append(&mut out, b"endbfchar\n", MAX_VALUE_BYTES)?;
        }
        append(
            &mut out,
            b"endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n",
            MAX_VALUE_BYTES,
        )?;
        Ok(out)
    }
}

fn parse_cmap(bytes: &[u8], base: usize) -> io::Result<Cmap> {
    if read16(bytes, 0)? != 0 {
        return Err(invalid("cmap version is invalid"));
    }
    let count = usize::from(read16(bytes, 2)?);
    check("cmap_records", count, 256)?;
    slice(bytes, 4, mul(count, 8)?)?;
    // Prefer the full-repertoire (3,10) subtable, then the BMP (3,1) one.
    let mut order = Vec::new();
    for wanted in [10u16, 1] {
        for index in 0..count {
            let record = add(4, mul(index, 8)?)?;
            if read16(bytes, record)? == 3 && read16(bytes, add(record, 2)?)? == wanted {
                push(&mut order, record, "cmap_records", 512)?;
            }
        }
    }
    for record in order {
        let offset = usize::try_from(read32(bytes, add(record, 4)?)?)
            .map_err(|_| invalid("cmap offset overflow"))?;
        if offset < add(4, mul(count, 8)?)? {
            return Err(invalid("cmap subtable overlaps records"));
        }
        let format = read16(bytes, offset)?;
        let length = if format == 12 {
            usize::try_from(read32(bytes, add(offset, 4)?)?)
                .map_err(|_| invalid("cmap length overflow"))?
        } else {
            usize::from(read16(bytes, add(offset, 2)?)?)
        };
        let table = slice(bytes, offset, length)?;
        match format {
            4 => {
                let twice = usize::from(read16(table, 6)?);
                if twice == 0 || !twice.is_multiple_of(2) {
                    return Err(invalid("cmap segment count is invalid"));
                }
                let count = twice / 2;
                slice(table, 0, add(16, mul(count, 8)?)?)?;
                let mut segments = Vec::new();
                let mut previous = None;
                for index in 0..count {
                    let delta = mul(index, 2)?;
                    let end = read16(table, add(14, delta)?)?;
                    let start = read16(table, add(add(16, mul(count, 2)?)?, delta)?)?;
                    let adjustment = read16(table, add(add(16, mul(count, 4)?)?, delta)?)?;
                    let word = add(add(16, mul(count, 6)?)?, delta)?;
                    let range = usize::from(read16(table, word)?);
                    if start > end
                        || previous.is_some_and(|value| start <= value)
                        || !range.is_multiple_of(2)
                    {
                        return Err(invalid("cmap segments are invalid"));
                    }
                    let address = if range == 0 {
                        None
                    } else {
                        let local = add(word, range)?;
                        if local < add(16, mul(count, 8)?)? {
                            return Err(invalid("cmap glyph array overlaps segment arrays"));
                        }
                        slice(table, local, mul(add(usize::from(end - start), 1)?, 2)?)?;
                        Some(add(add(base, offset)?, local)?)
                    };
                    push(
                        &mut segments,
                        Segment {
                            start,
                            end,
                            delta: adjustment,
                            offset: address,
                        },
                        "cmap_segments",
                        8191,
                    )?;
                    previous = Some(end);
                }
                if !segments.last().is_some_and(|segment| {
                    segment.start == 0xffff
                        && segment.end == 0xffff
                        && segment.delta == 1
                        && segment.offset.is_none()
                }) {
                    return Err(invalid("cmap sentinel is missing"));
                }
                return Ok(Cmap::Four(segments));
            }
            12 => {
                if read16(table, 2)? != 0 {
                    return Err(invalid("cmap format twelve reserved field is invalid"));
                }
                let count = usize::try_from(read32(table, 12)?)
                    .map_err(|_| invalid("cmap group count overflow"))?;
                check("cmap_groups", count, 65536)?;
                slice(table, 16, mul(count, 12)?)?;
                let mut groups = Vec::new();
                let mut previous = None;
                for index in 0..count {
                    let at = add(16, mul(index, 12)?)?;
                    let start = read32(table, at)?;
                    let end = read32(table, add(at, 4)?)?;
                    let glyph = read32(table, add(at, 8)?)?;
                    if start > end || end > 0x10ffff || previous.is_some_and(|value| start <= value)
                    {
                        return Err(invalid("cmap groups are invalid"));
                    }
                    if glyph
                        .checked_add(end - start)
                        .is_none_or(|last| last > 65535)
                    {
                        return Err(invalid("cmap group glyphs exceed u16"));
                    }
                    push(
                        &mut groups,
                        Group { start, end, glyph },
                        "cmap_groups",
                        65536,
                    )?;
                    previous = Some(end);
                }
                return Ok(Cmap::Twelve(groups));
            }
            6 => {
                let start = read16(table, 6)?;
                let count = usize::from(read16(table, 8)?);
                if add(usize::from(start), count)? > 65536 {
                    return Err(invalid("cmap trimmed range exceeds BMP"));
                }
                slice(table, 10, mul(count, 2)?)?;
                let mut glyphs = Vec::new();
                for index in 0..count {
                    push(
                        &mut glyphs,
                        read16(table, add(10, mul(index, 2)?)?)?,
                        "cmap_glyphs",
                        65536,
                    )?;
                }
                return Ok(Cmap::Six { start, glyphs });
            }
            0 => return Ok(Cmap::Zero(copied(slice(table, 6, 256)?, 256)?)),
            _ => continue,
        }
    }
    Err(invalid("a supported Windows (3,1) cmap is required"))
}

/// Finds the first horizontal, non-minimum, non-cross-stream format 0
/// subtable of a version 0 `kern` table. Pairs must be sorted, as the
/// format requires; unsorted or Apple-format tables are ignored.
fn parse_kern(data: &[u8], range: Range<usize>) -> io::Result<Option<(usize, usize)>> {
    let Some(table) = data.get(range.clone()) else {
        return Err(invalid("kern table outside font"));
    };
    if table.len() < 4 || read16(table, 0)? != 0 {
        return Ok(None);
    }
    let subtables = usize::from(read16(table, 2)?);
    let mut at = 4usize;
    for _ in 0..subtables.min(64) {
        if add(at, 14)? > table.len() {
            return Ok(None);
        }
        let length = usize::from(read16(table, add(at, 2)?)?);
        let coverage = read16(table, add(at, 4)?)?;
        if coverage >> 8 == 0 && coverage & 0x0007 == 0x0001 {
            let count = usize::from(read16(table, add(at, 6)?)?);
            let pairs = add(at, 14)?;
            if add(pairs, mul(count, 6)?)? > table.len() {
                return Ok(None);
            }
            let mut previous = None;
            for index in 0..count {
                let key = read32(table, add(pairs, mul(index, 6)?)?)?;
                if previous.is_some_and(|value| key <= value) {
                    return Ok(None);
                }
                previous = Some(key);
            }
            return Ok(Some((add(range.start, pairs)?, count)));
        }
        if length < 14 {
            return Ok(None);
        }
        at = add(at, length)?;
    }
    Ok(None)
}

fn font_name(bytes: Option<&[u8]>) -> String {
    let parsed = (|| -> io::Result<Option<String>> {
        let Some(bytes) = bytes else { return Ok(None) };
        let count = usize::from(read16(bytes, 2)?);
        check("font_name_records", count, 1024)?;
        let strings = usize::from(read16(bytes, 4)?);
        slice(bytes, 6, mul(count, 12)?)?;
        for index in 0..count {
            let at = add(6, mul(index, 12)?)?;
            if read16(bytes, at)? != 3 || read16(bytes, add(at, 6)?)? != 6 {
                continue;
            }
            let length = usize::from(read16(bytes, add(at, 8)?)?);
            check("font_name_bytes", length, 254)?;
            let start = add(strings, usize::from(read16(bytes, add(at, 10)?)?))?;
            let raw = slice(bytes, start, length)?;
            if !length.is_multiple_of(2) {
                continue;
            }
            let mut name = String::new();
            for pair in raw.as_chunks::<2>().0 {
                let unit = read16(pair, 0)?;
                if let Some(character) = char::from_u32(u32::from(unit)).filter(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
                }) {
                    name.push(character);
                }
            }
            if !name.is_empty() {
                return Ok(Some(name));
            }
        }
        Ok(None)
    })();
    match parsed {
        Ok(Some(name)) => name,
        _ => "EmbeddedFont".to_owned(),
    }
}
