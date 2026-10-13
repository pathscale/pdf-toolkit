//! Clean-room glyf subsetting. Original glyph IDs are retained; unused slots
//! have empty outlines. Composite references consequently require no remapping.

use std::collections::{BTreeMap, BTreeSet};
use std::io;

use super::bounds::*;
use super::font::{patch, read16, set16, set32, signed16, size32, slice, TrueType};

const MAX_COMPONENTS: usize = 1024;
const MAX_WORK: usize = 65536;

fn align(out: &mut Vec<u8>) -> io::Result<()> {
    let padding = (4 - out.len() % 4) % 4;
    append(out, slice(&[0u8; 3], 0, padding)?, MAX_FONT_BYTES)
}

fn checksum(bytes: &[u8]) -> u32 {
    let mut sum = 0u32;
    for chunk in bytes.chunks(4) {
        let mut word = [0u8; 4];
        for (target, source) in word.iter_mut().zip(chunk) {
            *target = *source;
        }
        sum = sum.wrapping_add(u32::from_be_bytes(word));
    }
    sum
}

/// Returns unhinted bytes and the immediate component glyph IDs.
fn unhint(bytes: &[u8]) -> io::Result<(Vec<u8>, Vec<u16>)> {
    if bytes.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    slice(bytes, 0, 10)?;
    let contours = signed16(bytes, 0)?;
    if contours >= 0 {
        let count = usize::try_from(contours).map_err(|_| invalid("contour count overflow"))?;
        let instructions_at = add(10, mul(count, 2)?)?;
        let mut previous = None;
        for index in 0..count {
            let end = read16(bytes, add(10, mul(index, 2)?)?)?;
            if previous.is_some_and(|value| end <= value) {
                return Err(invalid("simple glyph contour endpoints are invalid"));
            }
            previous = Some(end);
        }
        let points = previous.map_or(0, |last| usize::from(last).saturating_add(1));
        let instructions = usize::from(read16(bytes, instructions_at)?);
        let flags_start = add(add(instructions_at, 2)?, instructions)?;
        slice(bytes, add(instructions_at, 2)?, instructions)?;
        let mut cursor = flags_start;
        let mut decoded = 0usize;
        let mut coordinate_bytes = 0usize;
        for _ in 0..points {
            if decoded == points {
                break;
            }
            let flag = *bytes
                .get(cursor)
                .ok_or_else(|| invalid("simple glyph flags are truncated"))?;
            cursor = add(cursor, 1)?;
            let repeat = if flag & 8 != 0 {
                let repeat = *bytes
                    .get(cursor)
                    .ok_or_else(|| invalid("glyph repeat is truncated"))?;
                cursor = add(cursor, 1)?;
                add(usize::from(repeat), 1)?
            } else {
                1
            };
            decoded = add(decoded, repeat)?;
            if decoded > points {
                return Err(invalid("glyph flag repeat exceeds point count"));
            }
            let x = if flag & 2 != 0 {
                1
            } else if flag & 16 != 0 {
                0
            } else {
                2
            };
            let y = if flag & 4 != 0 {
                1
            } else if flag & 32 != 0 {
                0
            } else {
                2
            };
            coordinate_bytes = add(coordinate_bytes, mul(repeat, add(x, y)?)?)?;
        }
        let end = add(cursor, coordinate_bytes)?;
        slice(bytes, cursor, coordinate_bytes)?;
        let mut out = copied(slice(bytes, 0, instructions_at)?, MAX_FONT_BYTES)?;
        append(&mut out, &[0, 0], MAX_FONT_BYTES)?;
        append(
            &mut out,
            bytes
                .get(flags_start..end)
                .ok_or_else(|| invalid("glyph data range"))?,
            MAX_FONT_BYTES,
        )?;
        return Ok((out, Vec::new()));
    }
    if contours != -1 {
        return Err(invalid("composite glyph contour count is invalid"));
    }
    let mut out = copied(slice(bytes, 0, 10)?, MAX_FONT_BYTES)?;
    let mut components = Vec::new();
    let mut cursor = 10usize;
    let mut instructions = false;
    let mut complete = false;
    for _ in 0..MAX_COMPONENTS {
        let flags = read16(bytes, cursor)?;
        let glyph = read16(bytes, add(cursor, 2)?)?;
        if flags & 0xe010 != 0 {
            return Err(invalid("composite glyph has unsupported flags"));
        }
        let transforms = [0x0008, 0x0040, 0x0080]
            .iter()
            .filter(|flag| flags & **flag != 0)
            .count();
        if transforms > 1 || flags & 0x1800 == 0x1800 {
            return Err(invalid("composite glyph transform flags conflict"));
        }
        instructions |= flags & 0x0100 != 0;
        let arguments = if flags & 1 != 0 { 4 } else { 2 };
        let transform = if flags & 0x0080 != 0 {
            8
        } else if flags & 0x0040 != 0 {
            4
        } else if flags & 0x0008 != 0 {
            2
        } else {
            0
        };
        let length = add(4, add(arguments, transform)?)?;
        let start = out.len();
        append(&mut out, slice(bytes, cursor, length)?, MAX_FONT_BYTES)?;
        set16(&mut out, start, flags & !0x0100)?;
        push(&mut components, glyph, "glyph_components", MAX_COMPONENTS)?;
        cursor = add(cursor, length)?;
        if flags & 0x0020 == 0 {
            complete = true;
            break;
        }
    }
    if !complete {
        return Err(refusal(
            "glyph_components",
            MAX_COMPONENTS.saturating_add(1),
            MAX_COMPONENTS,
        ));
    }
    if instructions {
        let count = usize::from(read16(bytes, cursor)?);
        slice(bytes, add(cursor, 2)?, count)?;
    }
    Ok((out, components))
}

fn retained(font: &TrueType) -> io::Result<BTreeSet<u16>> {
    let mut roots = BTreeSet::new();
    roots.insert(0);
    roots.extend(font.used.keys().copied());
    let mut states = BTreeMap::<u16, u8>::new();
    let mut heights = BTreeMap::<u16, usize>::new();
    let mut kept = BTreeSet::new();
    for root in roots {
        let mut stack = Vec::new();
        push(
            &mut stack,
            (root, 1usize, false),
            "composite_work",
            MAX_WORK,
        )?;
        while let Some((glyph, depth, exit)) = stack.pop() {
            if exit {
                let (_, components) = unhint(font.glyph_bytes(glyph)?)?;
                let mut height = 1usize;
                for component in components {
                    let child = *heights
                        .get(&component)
                        .ok_or_else(|| invalid("composite dependency is unfinished"))?;
                    height = height.max(add(child, 1)?);
                }
                check("composite_depth", height, MAX_DEPTH)?;
                heights.insert(glyph, height);
                states.insert(glyph, 2);
                continue;
            }
            match states.get(&glyph).copied() {
                Some(1) => return Err(invalid("composite glyph cycle")),
                Some(2) => continue,
                _ => {}
            }
            check("composite_depth", depth, MAX_DEPTH)?;
            let (_, components) = unhint(font.glyph_bytes(glyph)?)?;
            states.insert(glyph, 1);
            kept.insert(glyph);
            push(&mut stack, (glyph, depth, true), "composite_work", MAX_WORK)?;
            for component in components.into_iter().rev() {
                push(
                    &mut stack,
                    (component, add(depth, 1)?, false),
                    "composite_work",
                    MAX_WORK,
                )?;
            }
        }
    }
    Ok(kept)
}

fn minimal_cmap(font: &TrueType) -> io::Result<Vec<u8>> {
    let mut mapping = BTreeMap::new();
    for (glyph, character) in &font.used {
        mapping.insert(u32::from(*character), *glyph);
    }
    let bmp: Vec<_> = mapping.iter().filter(|(code, _)| **code < 0xffff).collect();
    let count = add(bmp.len(), 1)?;
    check("subset_cmap_segments", count, 8189)?;
    let length = add(16, mul(count, 8)?)?;
    let mut four = zeroed(length, MAX_FONT_BYTES)?;
    set16(&mut four, 0, 4)?;
    set16(
        &mut four,
        2,
        u16::try_from(length).map_err(|_| invalid("cmap length exceeds u16"))?,
    )?;
    set16(
        &mut four,
        6,
        u16::try_from(mul(count, 2)?).map_err(|_| invalid("cmap segments exceed u16"))?,
    )?;
    let power = 1usize << (usize::BITS - 1 - count.leading_zeros());
    set16(
        &mut four,
        8,
        u16::try_from(mul(power, 2)?).map_err(|_| invalid("cmap search range"))?,
    )?;
    set16(&mut four, 10, power.trailing_zeros() as u16)?;
    set16(
        &mut four,
        12,
        u16::try_from(mul(count - power, 2)?).map_err(|_| invalid("cmap range shift"))?,
    )?;
    for (index, (code, glyph)) in bmp.into_iter().enumerate() {
        let code = u16::try_from(*code).map_err(|_| invalid("BMP cmap code overflow"))?;
        let offset = mul(index, 2)?;
        set16(&mut four, add(14, offset)?, code)?;
        set16(&mut four, add(add(16, mul(count, 2)?)?, offset)?, code)?;
        set16(
            &mut four,
            add(add(16, mul(count, 4)?)?, offset)?,
            glyph.wrapping_sub(code),
        )?;
    }
    let sentinel = mul(count - 1, 2)?;
    set16(&mut four, add(14, sentinel)?, 0xffff)?;
    set16(&mut four, add(add(16, mul(count, 2)?)?, sentinel)?, 0xffff)?;
    set16(&mut four, add(add(16, mul(count, 4)?)?, sentinel)?, 1)?;

    let mut twelve = zeroed(add(16, mul(mapping.len(), 12)?)?, MAX_FONT_BYTES)?;
    set16(&mut twelve, 0, 12)?;
    let twelve_length = size32(twelve.len())?;
    set32(&mut twelve, 4, twelve_length)?;
    set32(&mut twelve, 12, size32(mapping.len())?)?;
    for (index, (code, glyph)) in mapping.into_iter().enumerate() {
        let at = add(16, mul(index, 12)?)?;
        set32(&mut twelve, at, code)?;
        set32(&mut twelve, add(at, 4)?, code)?;
        set32(&mut twelve, add(at, 8)?, u32::from(glyph))?;
    }
    let mut out = zeroed(20, MAX_FONT_BYTES)?;
    set16(&mut out, 2, 2)?;
    set16(&mut out, 4, 3)?;
    set16(&mut out, 6, 1)?;
    set32(&mut out, 8, 20)?;
    set16(&mut out, 12, 3)?;
    set16(&mut out, 14, 10)?;
    set32(&mut out, 16, size32(add(20, four.len())?)?)?;
    append(&mut out, &four, MAX_FONT_BYTES)?;
    append(&mut out, &twelve, MAX_FONT_BYTES)?;
    Ok(out)
}

fn names(name: &str) -> io::Result<Vec<u8>> {
    let entries = [
        (1u16, name),
        (2, "Regular"),
        (3, name),
        (4, name),
        (5, "Version 1.000"),
        (6, name),
    ];
    let header = add(6, mul(entries.len(), 12)?)?;
    let mut out = zeroed(header, MAX_FONT_BYTES)?;
    set16(&mut out, 2, entries.len() as u16)?;
    set16(&mut out, 4, header as u16)?;
    for (index, (id, text)) in entries.into_iter().enumerate() {
        let offset = out
            .len()
            .checked_sub(header)
            .ok_or_else(|| invalid("name string offset"))?;
        let length = mul(text.encode_utf16().count(), 2)?;
        let record = add(6, mul(index, 12)?)?;
        for (field, value) in [
            3,
            1,
            0x0409,
            id,
            u16::try_from(length).map_err(|_| invalid("name length exceeds u16"))?,
            u16::try_from(offset).map_err(|_| invalid("name offset exceeds u16"))?,
        ]
        .into_iter()
        .enumerate()
        {
            set16(&mut out, add(record, mul(field, 2)?)?, value)?;
        }
        for unit in text.encode_utf16() {
            append(&mut out, &unit.to_be_bytes(), MAX_FONT_BYTES)?;
        }
    }
    Ok(out)
}

pub(super) fn build(font: &TrueType) -> io::Result<(Vec<u8>, String)> {
    let kept = retained(font)?;
    let mut used = BTreeSet::new();
    used.insert(0);
    used.extend(font.used.keys().copied());
    let mut hash_input = Vec::new();
    for glyph in used {
        append(&mut hash_input, &glyph.to_be_bytes(), 131070)?;
    }
    let mut tag = String::new();
    for byte in crate::sha256::digest(&hash_input).iter().take(6) {
        tag.push(char::from(b'A' + byte % 26));
    }

    let mut glyf = Vec::new();
    let mut loca = Vec::new();
    let mut hmtx = Vec::new();
    for glyph in 0..font.glyph_count {
        append(
            &mut loca,
            &size32(glyf.len())?.to_be_bytes(),
            MAX_FONT_BYTES,
        )?;
        if kept.contains(&glyph) {
            let (bytes, _) = unhint(font.glyph_bytes(glyph)?)?;
            append(&mut glyf, &bytes, MAX_FONT_BYTES)?;
            align(&mut glyf)?;
        }
        let (advance, bearing) = font.metric(glyph)?;
        append(&mut hmtx, &advance.to_be_bytes(), MAX_FONT_BYTES)?;
        append(&mut hmtx, &bearing.to_be_bytes(), MAX_FONT_BYTES)?;
    }
    append(
        &mut loca,
        &size32(glyf.len())?.to_be_bytes(),
        MAX_FONT_BYTES,
    )?;
    let mut head = copied(font.table(b"head")?, MAX_FONT_BYTES)?;
    set32(&mut head, 8, 0)?;
    let flags = read16(&head, 16)?;
    set16(&mut head, 16, flags & !0x001c)?;
    set16(&mut head, 50, 1)?;
    let mut hhea = copied(font.table(b"hhea")?, MAX_FONT_BYTES)?;
    set16(&mut hhea, 34, font.glyph_count)?;
    let mut maxp = copied(font.table(b"maxp")?, MAX_FONT_BYTES)?;
    set16(&mut maxp, 14, 1)?;
    for at in [18, 20, 22, 24, 26] {
        set16(&mut maxp, at, 0)?;
    }
    let mut post = zeroed(32, MAX_FONT_BYTES)?;
    if let Some(range) = font.tables.get(b"post") {
        if let Some(bytes) = font
            .data
            .get(range.clone())
            .filter(|bytes| bytes.len() >= 32)
        {
            patch(&mut post, 4, slice(bytes, 4, 28)?)?;
        }
    }
    set32(&mut post, 0, 0x0003_0000)?;
    for at in [16, 20, 24, 28] {
        set32(&mut post, at, 0)?;
    }
    let mut os2 = match font.tables.get(b"OS/2") {
        Some(_) => copied(font.table(b"OS/2")?, MAX_FONT_BYTES)?,
        None => {
            let mut bytes = zeroed(78, MAX_FONT_BYTES)?;
            set16(&mut bytes, 4, 400)?;
            set16(&mut bytes, 6, 5)?;
            patch(&mut bytes, 58, b"HOME")?;
            let hhea = font.table(b"hhea")?;
            patch(&mut bytes, 68, slice(hhea, 4, 4)?)?;
            bytes
        }
    };
    let first = font
        .used
        .values()
        .map(|character| u32::from(*character).min(65535) as u16)
        .min()
        .unwrap_or(0);
    let last = font
        .used
        .values()
        .map(|character| u32::from(*character).min(65535) as u16)
        .max()
        .unwrap_or(0);
    set16(&mut os2, 64, first)?;
    set16(&mut os2, 66, last)?;
    let base_name = font.tables.get(b"name");
    let _ = base_name;
    // The PDF name and font program use the same deterministic subset tag.
    let pdf_name = font.subset_name(&tag);
    let tables: [([u8; 4], Vec<u8>); 10] = [
        (*b"OS/2", os2),
        (*b"cmap", minimal_cmap(font)?),
        (*b"glyf", glyf),
        (*b"head", head),
        (*b"hhea", hhea),
        (*b"hmtx", hmtx),
        (*b"loca", loca),
        (*b"maxp", maxp),
        (*b"name", names(&pdf_name)?),
        (*b"post", post),
    ];
    let mut out = zeroed(172, MAX_FONT_BYTES)?;
    set32(&mut out, 0, 0x0001_0000)?;
    set16(&mut out, 4, 10)?;
    set16(&mut out, 6, 128)?;
    set16(&mut out, 8, 3)?;
    set16(&mut out, 10, 32)?;
    let mut head_at = None;
    for (index, (table_tag, bytes)) in tables.into_iter().enumerate() {
        align(&mut out)?;
        let offset = out.len();
        let record = add(12, mul(index, 16)?)?;
        patch(&mut out, record, &table_tag)?;
        set32(&mut out, add(record, 4)?, checksum(&bytes))?;
        set32(&mut out, add(record, 8)?, size32(offset)?)?;
        set32(&mut out, add(record, 12)?, size32(bytes.len())?)?;
        if table_tag == *b"head" {
            head_at = Some(offset);
        }
        append(&mut out, &bytes, MAX_FONT_BYTES)?;
        align(&mut out)?;
    }
    let head_at = head_at.ok_or_else(|| invalid("subset head table is missing"))?;
    let adjustment = 0xb1b0_afba_u32.wrapping_sub(checksum(&out));
    set32(&mut out, add(head_at, 8)?, adjustment)?;
    Ok((out, tag))
}

impl TrueType {
    pub(super) fn subset_name(&self, tag: &str) -> String {
        format!("{tag}+{}", self.base_name())
    }

    fn base_name(&self) -> &str {
        &self.base_name
    }
}
