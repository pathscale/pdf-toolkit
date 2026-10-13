//! A deterministic, house-authored TrueType program with unhinted outlines.

use std::io;

const MAX_FONT_BYTES: usize = 4096;
const MAX_CONTOURS: usize = 2;
const MAX_POINTS: usize = 8;
const MAX_NAME_BYTES: usize = 64;

pub(super) const SQUARE: [[i16; 2]; 4] = [[100, 100], [100, 800], [800, 800], [800, 100]];
pub(super) const TRIANGLE: [[i16; 2]; 3] = [[100, 100], [450, 800], [800, 100]];
// Opposite winding to the outer square makes a hole under non-zero filling.
pub(super) const HOLE: [[i16; 2]; 4] = [[300, 300], [600, 300], [600, 600], [300, 600]];

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn bounded(requested: usize, maximum: usize, kind: &str) -> io::Result<()> {
    if requested > maximum {
        return Err(invalid(&format!(
            "{kind} requested {requested}, maximum {maximum}"
        )));
    }
    Ok(())
}

fn at(base: usize, offset: usize) -> io::Result<usize> {
    base.checked_add(offset)
        .ok_or_else(|| invalid("font offset overflow"))
}

fn size16(size: usize) -> io::Result<u16> {
    u16::try_from(size).map_err(|_| invalid("font size exceeds u16"))
}

fn size32(size: usize) -> io::Result<u32> {
    u32::try_from(size).map_err(|_| invalid("font size exceeds u32"))
}

fn append(out: &mut Vec<u8>, bytes: &[u8]) -> io::Result<()> {
    let length = at(out.len(), bytes.len())?;
    bounded(length, MAX_FONT_BYTES, "font_bytes")?;
    out.try_reserve(bytes.len())
        .map_err(|_| io::Error::new(io::ErrorKind::Other, "font allocation failed"))?;
    out.extend_from_slice(bytes);
    Ok(())
}

fn patch(out: &mut [u8], offset: usize, bytes: &[u8]) -> io::Result<()> {
    let end = at(offset, bytes.len())?;
    let target = out
        .get_mut(offset..end)
        .ok_or_else(|| invalid("font patch outside table"))?;
    target.copy_from_slice(bytes);
    Ok(())
}

fn put16(out: &mut Vec<u8>, value: u16) -> io::Result<()> {
    append(out, &value.to_be_bytes())
}

fn put_signed(out: &mut Vec<u8>, value: i16) -> io::Result<()> {
    append(out, &value.to_be_bytes())
}

fn put32(out: &mut Vec<u8>, value: u32) -> io::Result<()> {
    append(out, &value.to_be_bytes())
}

fn set16(out: &mut [u8], offset: usize, value: u16) -> io::Result<()> {
    patch(out, offset, &value.to_be_bytes())
}

fn set_signed(out: &mut [u8], offset: usize, value: i16) -> io::Result<()> {
    patch(out, offset, &value.to_be_bytes())
}

fn set32(out: &mut [u8], offset: usize, value: u32) -> io::Result<()> {
    patch(out, offset, &value.to_be_bytes())
}

fn align(out: &mut Vec<u8>) -> io::Result<()> {
    let padding = (4 - out.len() % 4) % 4;
    let zeros = [0u8; 3];
    append(
        out,
        zeros
            .get(..padding)
            .ok_or_else(|| invalid("font alignment"))?,
    )
}

fn checksum(bytes: &[u8]) -> u32 {
    let mut sum = 0u32;
    for chunk in bytes.chunks(4) {
        let mut word = [0u8; 4];
        for (slot, byte) in word.iter_mut().zip(chunk) {
            *slot = *byte;
        }
        sum = sum.wrapping_add(u32::from_be_bytes(word));
    }
    sum
}

/// Flags use full signed 16-bit deltas, avoiding compressed-coordinate state.
fn simple_glyph(out: &mut Vec<u8>, contours: &[&[[i16; 2]]]) -> io::Result<()> {
    bounded(contours.len(), MAX_CONTOURS, "font_contours")?;
    let points = contours
        .iter()
        .try_fold(0usize, |count, contour| at(count, contour.len()))?;
    bounded(points, MAX_POINTS, "font_points")?;
    if contours.is_empty() || contours.iter().any(|contour| contour.is_empty()) {
        return Err(invalid("empty simple glyph contour"));
    }
    let mut bounds = [i16::MAX, i16::MAX, i16::MIN, i16::MIN];
    for [x, y] in contours.iter().flat_map(|contour| contour.iter()).copied() {
        bounds[0] = bounds[0].min(x);
        bounds[1] = bounds[1].min(y);
        bounds[2] = bounds[2].max(x);
        bounds[3] = bounds[3].max(y);
    }
    let count = i16::try_from(contours.len()).map_err(|_| invalid("font contour count"))?;
    put_signed(out, count)?;
    for value in bounds {
        put_signed(out, value)?;
    }
    let mut end = 0usize;
    for contour in contours {
        end = at(end, contour.len())?;
        let last = end
            .checked_sub(1)
            .ok_or_else(|| invalid("font contour endpoint"))?;
        put16(out, size16(last)?)?;
    }
    put16(out, 0)?; // instructionLength
    for _ in 0..points {
        append(out, &[1])?;
    } // on-curve, full x/y deltas
    for axis in 0..2 {
        let mut previous = 0i16;
        for point in contours.iter().flat_map(|contour| contour.iter()) {
            let coordinate = point
                .get(axis)
                .copied()
                .ok_or_else(|| invalid("font axis"))?;
            let delta = coordinate
                .checked_sub(previous)
                .ok_or_else(|| invalid("font coordinate delta overflow"))?;
            put_signed(out, delta)?;
            previous = coordinate;
        }
    }
    align(out)
}

fn head() -> io::Result<Vec<u8>> {
    let mut out = vec![0; 54];
    set32(&mut out, 0, 0x0001_0000)?;
    set32(&mut out, 4, 0x0001_0000)?;
    set32(&mut out, 12, 0x5f0f_3cf5)?;
    set16(&mut out, 16, 3)?;
    set16(&mut out, 18, 1000)?;
    // Created and modified timestamps are zero for reproducibility.
    for (offset, value) in [(36, 100), (38, 100), (40, 800), (42, 800), (48, 2), (50, 1)] {
        set_signed(&mut out, offset, value)?;
    }
    set16(&mut out, 46, 8)?;
    Ok(out)
}

fn hhea() -> io::Result<Vec<u8>> {
    let mut out = vec![0; 36];
    set32(&mut out, 0, 0x0001_0000)?;
    set_signed(&mut out, 4, 800)?;
    set16(&mut out, 10, 1000)?;
    set_signed(&mut out, 14, 200)?;
    set_signed(&mut out, 16, 800)?;
    set_signed(&mut out, 18, 1)?;
    set16(&mut out, 34, 4)?;
    Ok(out)
}

fn maxp() -> io::Result<Vec<u8>> {
    let mut out = vec![0; 32];
    set32(&mut out, 0, 0x0001_0000)?;
    set16(&mut out, 4, 4)?;
    set16(&mut out, 6, 8)?;
    set16(&mut out, 8, 2)?;
    set16(&mut out, 14, 1)?;
    Ok(out)
}

fn hmtx() -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    for bearing in [0, 100, 100, 100] {
        put16(&mut out, 1000)?;
        put_signed(&mut out, bearing)?;
    }
    Ok(out)
}

fn cmap() -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    for value in [0, 1, 3, 1] {
        put16(&mut out, value)?;
    }
    put32(&mut out, 12)?;
    // Format 4, four segments, including the mandatory 0xffff sentinel.
    for value in [4, 48, 0, 8, 8, 2, 0] {
        put16(&mut out, value)?;
    }
    for value in [65, 66, 79, 0xffff] {
        put16(&mut out, value)?;
    }
    put16(&mut out, 0)?;
    for value in [65, 66, 79, 0xffff] {
        put16(&mut out, value)?;
    }
    for value in [-64, -64, -76, 1] {
        put_signed(&mut out, value)?;
    }
    for _ in 0..4 {
        put16(&mut out, 0)?;
    }
    Ok(out)
}

fn post() -> io::Result<Vec<u8>> {
    let mut out = vec![0; 32];
    set32(&mut out, 0, 0x0003_0000)?;
    set_signed(&mut out, 8, -100)?;
    set_signed(&mut out, 10, 50)?;
    set32(&mut out, 12, 1)?;
    Ok(out)
}

fn names() -> io::Result<Vec<u8>> {
    const STRINGS: [(u16, &str); 6] = [
        (1, "House Shapes"),
        (2, "Regular"),
        (3, "HouseShapes-Regular-1"),
        (4, "House Shapes Regular"),
        (5, "Version 1.000"),
        (6, "HouseShapes-Regular"),
    ];
    let mut out = vec![0; 78];
    set16(&mut out, 2, 6)?;
    set16(&mut out, 4, 78)?;
    for (index, (id, text)) in STRINGS.into_iter().enumerate() {
        bounded(text.len(), MAX_NAME_BYTES, "font_name_bytes")?;
        let offset = out
            .len()
            .checked_sub(78)
            .ok_or_else(|| invalid("font name offset"))?;
        let length = text
            .encode_utf16()
            .count()
            .checked_mul(2)
            .ok_or_else(|| invalid("font name length overflow"))?;
        let record = index
            .checked_mul(12)
            .and_then(|value| value.checked_add(6))
            .ok_or_else(|| invalid("font name record overflow"))?;
        for (field, value) in [3, 1, 0x0409, id, size16(length)?, size16(offset)?]
            .into_iter()
            .enumerate()
        {
            let delta = field
                .checked_mul(2)
                .ok_or_else(|| invalid("font name field overflow"))?;
            set16(&mut out, at(record, delta)?, value)?;
        }
        for unit in text.encode_utf16() {
            put16(&mut out, unit)?;
        }
    }
    Ok(out)
}

fn os2() -> io::Result<Vec<u8>> {
    let mut out = vec![0; 78]; // OS/2 version 0
    set_signed(&mut out, 2, 1000)?;
    for (offset, value) in [
        (4, 400),
        (6, 5),
        (10, 650),
        (12, 600),
        (16, 75),
        (18, 650),
        (20, 600),
        (24, 350),
        (26, 50),
        (28, 300),
        (62, 64),
        (64, 65),
        (66, 79),
        (74, 800),
    ] {
        set16(&mut out, offset, value)?;
    }
    set32(&mut out, 42, 1)?; // Basic Latin
    patch(&mut out, 58, b"HOME")?;
    set_signed(&mut out, 68, 800)?;
    Ok(out)
}

pub(super) fn program() -> io::Result<Vec<u8>> {
    let mut glyf = Vec::new();
    let mut loca = Vec::new();
    put32(&mut loca, 0)?; // Empty .notdef at glyph zero.
    let a: &[&[[i16; 2]]] = &[&SQUARE];
    let b: &[&[[i16; 2]]] = &[&TRIANGLE];
    let o: &[&[[i16; 2]]] = &[&SQUARE, &HOLE];
    for contours in [a, b, o] {
        put32(&mut loca, size32(glyf.len())?)?;
        simple_glyph(&mut glyf, contours)?;
    }
    put32(&mut loca, size32(glyf.len())?)?;

    // Tags are in ascending byte order. Directory lengths exclude table padding.
    let tables: [([u8; 4], Vec<u8>); 10] = [
        (*b"OS/2", os2()?),
        (*b"cmap", cmap()?),
        (*b"glyf", glyf),
        (*b"head", head()?),
        (*b"hhea", hhea()?),
        (*b"hmtx", hmtx()?),
        (*b"loca", loca),
        (*b"maxp", maxp()?),
        (*b"name", names()?),
        (*b"post", post()?),
    ];
    let mut out = vec![0; 172]; // 12-byte header and ten 16-byte directory records.
    set32(&mut out, 0, 0x0001_0000)?;
    set16(&mut out, 4, 10)?;
    set16(&mut out, 6, 128)?;
    set16(&mut out, 8, 3)?;
    set16(&mut out, 10, 32)?;
    let mut head_offset = None;
    for (index, (tag, table)) in tables.into_iter().enumerate() {
        align(&mut out)?;
        let offset = out.len();
        let record = index
            .checked_mul(16)
            .and_then(|value| value.checked_add(12))
            .ok_or_else(|| invalid("font directory overflow"))?;
        patch(&mut out, record, &tag)?;
        set32(&mut out, at(record, 4)?, checksum(&table))?;
        set32(&mut out, at(record, 8)?, size32(offset)?)?;
        set32(&mut out, at(record, 12)?, size32(table.len())?)?;
        if tag == *b"head" {
            head_offset = Some(offset);
        }
        append(&mut out, &table)?;
        align(&mut out)?;
    }
    let offset = head_offset.ok_or_else(|| invalid("font head table missing"))?;
    // The head directory checksum uses a zero checkSumAdjustment, as required.
    let adjustment = 0xb1b0_afba_u32.wrapping_sub(checksum(&out));
    set32(&mut out, at(offset, 8)?, adjustment)?;
    Ok(out)
}
