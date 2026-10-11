//! A deterministic, house-authored TrueType font for the authoring fixtures.
//!
//! Every glyph is one filled rectangle, so pixel probes can tell where a glyph
//! landed. Widths follow simple classes (see `advance`) so expectations for
//! wrapping and kerning can be computed by hand. The cmap covers ASCII,
//! Latin-1, Greek, Cyrillic, four CJK ideographs and one astral emoji, through
//! a (3,1) format 4 subtable and a (3,10) format 12 subtable. A format 0
//! `kern` table holds four pairs.

use std::io;

pub(crate) const UNITS: u16 = 1000;
pub(crate) const ASCENT: i16 = 800;
pub(crate) const DESCENT: i16 = -200;
/// Horizontal inset of each rectangle inside its advance.
pub(crate) const INSET: u16 = 50;
pub(crate) const KERN_PAIRS: [(char, char, i16); 4] = [
    ('A', 'V', -120),
    ('A', 'W', -60),
    ('T', 'o', -80),
    ('V', 'A', -120),
];
const NAME: &str = "HouseSans";

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

/// Code points in the font, ascending. Glyph IDs follow this order from 1.
pub(crate) fn repertoire() -> Vec<u32> {
    let mut codes: Vec<u32> = (0x20..=0x7e).collect();
    codes.extend(0xa0..=0xff);
    codes.extend((0x386..=0x3ce).filter(|code| ![0x387, 0x38b, 0x38d, 0x3a2].contains(code)));
    codes.extend(0x410..=0x44f);
    codes.extend([0x2013, 0x2019, 0x201c, 0x201d, 0x2026, 0x20ac]);
    codes.extend([0x4e16, 0x4f60, 0x597d, 0x754c, 0x1f642]);
    codes
}

/// Advance width in font units (1000 per em).
pub(crate) fn advance(character: char) -> u16 {
    match character {
        ' ' | '\u{a0}' => 250,
        'i' | 'l' | 'j' | 'I' | '.' | ',' | ':' | ';' | '!' | '\'' | '|' => 300,
        'M' | 'W' | 'm' | 'w' => 800,
        'A'..='Z' => 650,
        'a'..='z' => 500,
        '0'..='9' => 550,
        '\u{4e00}'..='\u{9fff}' | '\u{1f600}'..='\u{1f64f}' => 1000,
        '\u{21}'..='\u{7e}' => 400,
        _ => 600,
    }
}

/// Rectangle top in font units: lowercase Latin letters are short.
pub(crate) fn top(character: char) -> i16 {
    if character.is_ascii_lowercase() {
        500
    } else {
        700
    }
}

pub(crate) fn glyph_id(character: char) -> Option<u16> {
    let codes = repertoire();
    let index = codes.binary_search(&u32::from(character)).ok()?;
    u16::try_from(index.checked_add(1)?).ok()
}

fn put16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn puti(out: &mut Vec<u8>, value: i16) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn put32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn size16(value: usize) -> io::Result<u16> {
    u16::try_from(value).map_err(|_| invalid("font value exceeds u16"))
}

fn size32(value: usize) -> io::Result<u32> {
    u32::try_from(value).map_err(|_| invalid("font value exceeds u32"))
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

/// Binary-search header fields for `count` entries of `size` bytes.
fn search(count: usize, size: usize) -> io::Result<(u16, u16, u16)> {
    let mut power = 1usize;
    let mut selector = 0u16;
    while power * 2 <= count {
        power *= 2;
        selector += 1;
    }
    let range = power * size;
    Ok((size16(range)?, selector, size16(count * size - range)?))
}

fn rectangle(out: &mut Vec<u8>, x0: i16, x1: i16, height: i16) {
    puti(out, 1);
    for value in [x0, 0, x1, height] {
        puti(out, value);
    }
    put16(out, 3);
    put16(out, 0);
    out.extend_from_slice(&[1, 1, 1, 1]);
    // Clockwise: up the left edge, across the top, down the right edge.
    for value in [x0, 0, x1 - x0, 0] {
        puti(out, value);
    }
    for value in [0, height, 0, -height] {
        puti(out, value);
    }
}

fn cmap(codes: &[u32]) -> io::Result<Vec<u8>> {
    // Consecutive codes have consecutive glyph IDs, so runs become segments.
    let mut runs: Vec<(u32, u32, u16)> = Vec::new();
    for (index, code) in codes.iter().enumerate() {
        let glyph = size16(index + 1)?;
        match runs.last_mut() {
            Some((_, end, _)) if *end + 1 == *code => *end = *code,
            _ => runs.push((*code, *code, glyph)),
        }
    }
    let bmp: Vec<_> = runs.iter().filter(|run| run.1 < 0xffff).collect();
    let segments = bmp.len() + 1;
    let mut four = Vec::new();
    put16(&mut four, 4);
    put16(&mut four, size16(16 + segments * 8)?);
    put16(&mut four, 0);
    put16(&mut four, size16(segments * 2)?);
    let (range, selector, shift) = search(segments, 2)?;
    put16(&mut four, range);
    put16(&mut four, selector);
    put16(&mut four, shift);
    for run in &bmp {
        put16(&mut four, size16(run.1 as usize)?);
    }
    put16(&mut four, 0xffff);
    put16(&mut four, 0);
    for run in &bmp {
        put16(&mut four, size16(run.0 as usize)?);
    }
    put16(&mut four, 0xffff);
    for run in &bmp {
        put16(&mut four, run.2.wrapping_sub(size16(run.0 as usize)?));
    }
    put16(&mut four, 1);
    for _ in 0..segments {
        put16(&mut four, 0);
    }
    let mut twelve = Vec::new();
    put16(&mut twelve, 12);
    put16(&mut twelve, 0);
    put32(&mut twelve, size32(16 + runs.len() * 12)?);
    put32(&mut twelve, 0);
    put32(&mut twelve, size32(runs.len())?);
    for (start, end, glyph) in &runs {
        put32(&mut twelve, *start);
        put32(&mut twelve, *end);
        put32(&mut twelve, u32::from(*glyph));
    }
    let mut out = Vec::new();
    put16(&mut out, 0);
    put16(&mut out, 2);
    put16(&mut out, 3);
    put16(&mut out, 1);
    put32(&mut out, 20);
    put16(&mut out, 3);
    put16(&mut out, 10);
    put32(&mut out, size32(20 + four.len())?);
    out.extend_from_slice(&four);
    out.extend_from_slice(&twelve);
    Ok(out)
}

fn names() -> io::Result<Vec<u8>> {
    let entries = [
        (1u16, "House Sans"),
        (2, "Regular"),
        (4, "House Sans"),
        (6, NAME),
    ];
    let header = 6 + entries.len() * 12;
    let mut out = Vec::new();
    let mut strings = Vec::new();
    put16(&mut out, 0);
    put16(&mut out, size16(entries.len())?);
    put16(&mut out, size16(header)?);
    for (id, text) in entries {
        let encoded: Vec<u8> = text.encode_utf16().flat_map(u16::to_be_bytes).collect();
        for value in [
            3,
            1,
            0x0409,
            id,
            size16(encoded.len())?,
            size16(strings.len())?,
        ] {
            put16(&mut out, value);
        }
        strings.extend_from_slice(&encoded);
    }
    out.extend_from_slice(&strings);
    Ok(out)
}

pub(crate) fn program() -> io::Result<Vec<u8>> {
    let codes = repertoire();
    let count = codes.len() + 1;
    let mut glyf = Vec::new();
    let mut loca = Vec::new();
    let mut hmtx = Vec::new();
    let mut widest = 0u16;
    // Glyph 0 (.notdef) is a tall box.
    put32(&mut loca, 0);
    rectangle(&mut glyf, 50, 450, 700);
    put16(&mut hmtx, 500);
    puti(&mut hmtx, 50);
    for code in &codes {
        while !glyf.len().is_multiple_of(4) {
            glyf.push(0);
        }
        put32(&mut loca, size32(glyf.len())?);
        let character = char::from_u32(*code).ok_or_else(|| invalid("code point"))?;
        let width = advance(character);
        widest = widest.max(width);
        let blank = character == ' ' || character == '\u{a0}';
        if !blank {
            let inset = i16::try_from(INSET).map_err(|_| invalid("inset"))?;
            let right = i16::try_from(width - INSET).map_err(|_| invalid("width"))?;
            rectangle(&mut glyf, inset, right, top(character));
        }
        put16(&mut hmtx, width);
        puti(&mut hmtx, if blank { 0 } else { 50 });
    }
    while !glyf.len().is_multiple_of(4) {
        glyf.push(0);
    }
    put32(&mut loca, size32(glyf.len())?);

    let mut head = Vec::new();
    put32(&mut head, 0x0001_0000);
    put32(&mut head, 0x0001_0000);
    put32(&mut head, 0);
    put32(&mut head, 0x5f0f_3cf5);
    put16(&mut head, 0x000b);
    put16(&mut head, UNITS);
    head.extend_from_slice(&[0; 16]);
    for value in [0i16, 0, 950, 700] {
        puti(&mut head, value);
    }
    put16(&mut head, 0);
    put16(&mut head, 8);
    puti(&mut head, 2);
    puti(&mut head, 1);
    puti(&mut head, 0);

    let mut hhea = Vec::new();
    put32(&mut hhea, 0x0001_0000);
    puti(&mut hhea, ASCENT);
    puti(&mut hhea, DESCENT);
    puti(&mut hhea, 0);
    put16(&mut hhea, widest);
    puti(&mut hhea, 0);
    puti(&mut hhea, 50);
    puti(&mut hhea, 950);
    puti(&mut hhea, 1);
    puti(&mut hhea, 0);
    puti(&mut hhea, 0);
    hhea.extend_from_slice(&[0; 8]);
    puti(&mut hhea, 0);
    put16(&mut hhea, size16(count)?);

    let mut maxp = Vec::new();
    put32(&mut maxp, 0x0001_0000);
    put16(&mut maxp, size16(count)?);
    for value in [4u16, 1, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0] {
        put16(&mut maxp, value);
    }

    let mut os2 = Vec::new();
    put16(&mut os2, 2);
    puti(&mut os2, 550);
    put16(&mut os2, 400);
    put16(&mut os2, 5);
    put16(&mut os2, 0);
    os2.extend_from_slice(&[0; 22]);
    os2.extend_from_slice(&[0; 10]);
    os2.extend_from_slice(&[0; 16]);
    os2.extend_from_slice(b"HOME");
    put16(&mut os2, 0x0040);
    put16(&mut os2, 0x0020);
    put16(&mut os2, 0xffff);
    puti(&mut os2, ASCENT);
    puti(&mut os2, DESCENT);
    puti(&mut os2, 0);
    put16(&mut os2, 800);
    put16(&mut os2, 200);
    os2.extend_from_slice(&[0; 8]);
    puti(&mut os2, 500);
    puti(&mut os2, 700);
    put16(&mut os2, 0);
    put16(&mut os2, 32);
    put16(&mut os2, 2);

    let mut post = Vec::new();
    put32(&mut post, 0x0003_0000);
    put32(&mut post, 0);
    puti(&mut post, -100);
    puti(&mut post, 50);
    post.extend_from_slice(&[0; 20]);

    let mut pairs = Vec::new();
    for (left, right, value) in KERN_PAIRS {
        let left = glyph_id(left).ok_or_else(|| invalid("kern glyph"))?;
        let right = glyph_id(right).ok_or_else(|| invalid("kern glyph"))?;
        pairs.push((left, right, value));
    }
    pairs.sort();
    let mut kern = Vec::new();
    put16(&mut kern, 0);
    put16(&mut kern, 1);
    put16(&mut kern, 0);
    put16(&mut kern, size16(14 + pairs.len() * 6)?);
    put16(&mut kern, 0x0001);
    put16(&mut kern, size16(pairs.len())?);
    let (range, selector, shift) = search(pairs.len(), 6)?;
    put16(&mut kern, range);
    put16(&mut kern, selector);
    put16(&mut kern, shift);
    for (left, right, value) in pairs {
        put16(&mut kern, left);
        put16(&mut kern, right);
        puti(&mut kern, value);
    }

    let tables: [([u8; 4], Vec<u8>); 11] = [
        (*b"OS/2", os2),
        (*b"cmap", cmap(&codes)?),
        (*b"glyf", glyf),
        (*b"head", head),
        (*b"hhea", hhea),
        (*b"hmtx", hmtx),
        (*b"kern", kern),
        (*b"loca", loca),
        (*b"maxp", maxp),
        (*b"name", names()?),
        (*b"post", post),
    ];
    let mut out = Vec::new();
    put32(&mut out, 0x0001_0000);
    put16(&mut out, size16(tables.len())?);
    let (range, selector, shift) = search(tables.len(), 16)?;
    put16(&mut out, range);
    put16(&mut out, selector);
    put16(&mut out, shift);
    let mut offset = 12 + tables.len() * 16;
    let mut head_at = 0usize;
    for (tag, bytes) in &tables {
        out.extend_from_slice(tag);
        put32(&mut out, checksum(bytes));
        put32(&mut out, size32(offset)?);
        put32(&mut out, size32(bytes.len())?);
        if tag == b"head" {
            head_at = offset;
        }
        offset += bytes.len().div_ceil(4) * 4;
    }
    for (_, bytes) in &tables {
        out.extend_from_slice(bytes);
        while !out.len().is_multiple_of(4) {
            out.push(0);
        }
    }
    let adjustment = 0xb1b0_afba_u32.wrapping_sub(checksum(&out));
    out.get_mut(head_at + 8..head_at + 12)
        .ok_or_else(|| invalid("head offset"))?
        .copy_from_slice(&adjustment.to_be_bytes());
    Ok(out)
}
