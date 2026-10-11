//! A small PNG encoder for fixtures: it applies the five row filters in turn
//! so the decoder's unfiltering is exercised, and compresses with the house
//! deflate encoder.

use crate::encode;

fn crc32(parts: &[&[u8]]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for part in parts {
        for byte in part.iter() {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xedb8_8320 & mask);
            }
        }
    }
    !crc
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out.extend_from_slice(&crc32(&[kind, body]).to_be_bytes());
}

fn paeth(left: u8, up: u8, corner: u8) -> u8 {
    let estimate = i16::from(left) + i16::from(up) - i16::from(corner);
    let to_left = (estimate - i16::from(left)).abs();
    let to_up = (estimate - i16::from(up)).abs();
    let to_corner = (estimate - i16::from(corner)).abs();
    if to_left <= to_up && to_left <= to_corner {
        left
    } else if to_up <= to_corner {
        up
    } else {
        corner
    }
}

pub(crate) struct Png<'a> {
    pub width: u32,
    pub depth: u8,
    pub color: u8,
    /// Unfiltered rows of packed samples.
    pub rows: &'a [Vec<u8>],
    pub palette: Option<&'a [u8]>,
    pub transparency: Option<&'a [u8]>,
}

pub(crate) fn encode(png: &Png<'_>) -> Vec<u8> {
    let channels = match png.color {
        2 => 3,
        4 => 2,
        6 => 4,
        _ => 1,
    };
    let pixel_bits = channels * usize::from(png.depth);
    let step = (pixel_bits / 8).max(1);
    let mut filtered = Vec::new();
    let empty = Vec::new();
    for (index, row) in png.rows.iter().enumerate() {
        let previous = if index == 0 {
            &empty
        } else {
            &png.rows[index - 1]
        };
        let kind = (index % 5) as u8;
        filtered.push(kind);
        for (at, byte) in row.iter().enumerate() {
            let left = if at >= step { row[at - step] } else { 0 };
            let up = previous.get(at).copied().unwrap_or(0);
            let corner = if at >= step {
                previous.get(at - step).copied().unwrap_or(0)
            } else {
                0
            };
            let predicted = match kind {
                0 => 0,
                1 => left,
                2 => up,
                3 => ((u16::from(left) + u16::from(up)) / 2) as u8,
                _ => paeth(left, up, corner),
            };
            filtered.push(byte.wrapping_sub(predicted));
        }
    }
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut header = Vec::new();
    header.extend_from_slice(&png.width.to_be_bytes());
    header.extend_from_slice(&(png.rows.len() as u32).to_be_bytes());
    header.extend_from_slice(&[png.depth, png.color, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &header);
    if let Some(palette) = png.palette {
        chunk(&mut out, b"PLTE", palette);
    }
    if let Some(transparency) = png.transparency {
        chunk(&mut out, b"tRNS", transparency);
    }
    chunk(&mut out, b"tEXt", b"Comment\0house fixture");
    let compressed = encode::flate(&filtered);
    // Two IDAT chunks, to exercise concatenation.
    let middle = compressed.len() / 2;
    chunk(&mut out, b"IDAT", &compressed[..middle]);
    chunk(&mut out, b"IDAT", &compressed[middle..]);
    chunk(&mut out, b"IEND", &[]);
    out
}
