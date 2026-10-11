//! PNG decoding for the document builder. Inflate and row unfiltering are
//! pdf-core's FlateDecode with PNG predictors; this module only reads chunks
//! and expands samples to 8-bit RGBA.

use std::io;

use pdf_core::filter::{decode as filter_decode, DecodeParams, FilterKind, FilterStep};

use super::bounds::*;
use super::font::{read32, slice};

const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
const MAX_CHUNKS: usize = 1 << 16;

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

struct Header {
    width: u32,
    height: u32,
    depth: u8,
    color: u8,
}

impl Header {
    fn channels(&self) -> usize {
        match self.color {
            2 => 3,
            4 => 2,
            6 => 4,
            _ => 1,
        }
    }
}

/// Returns width, height and RGBA bytes.
pub(super) fn decode(data: &[u8]) -> io::Result<(u32, u32, Vec<u8>)> {
    check("png_bytes", data.len(), MAX_FONT_BYTES)?;
    if data.get(..8) != Some(SIGNATURE.as_slice()) {
        return Err(invalid("PNG signature is missing"));
    }
    let mut at = 8usize;
    let mut header = None;
    let mut palette: Option<&[u8]> = None;
    let mut transparency: Option<&[u8]> = None;
    let mut compressed = Vec::new();
    let mut ended = false;
    for _ in 0..MAX_CHUNKS {
        let length =
            usize::try_from(read32(data, at)?).map_err(|_| invalid("PNG chunk length overflow"))?;
        if length > 0x7fff_ffff {
            return Err(invalid("PNG chunk length is invalid"));
        }
        let kind = slice(data, add(at, 4)?, 4)?;
        let body = slice(data, add(at, 8)?, length)?;
        let stored = read32(data, add(add(at, 8)?, length)?)?;
        if crc32(&[kind, body]) != stored {
            return Err(invalid("PNG chunk CRC does not match"));
        }
        at = add(add(at, 12)?, length)?;
        match kind {
            b"IHDR" => {
                if header.is_some() || body.len() != 13 {
                    return Err(invalid("PNG IHDR is invalid"));
                }
                let width = read32(body, 0)?;
                let height = read32(body, 4)?;
                let depth = *body
                    .get(8)
                    .ok_or_else(|| invalid("PNG IHDR is truncated"))?;
                let color = *body
                    .get(9)
                    .ok_or_else(|| invalid("PNG IHDR is truncated"))?;
                if body.get(10..12) != Some([0u8, 0].as_slice()) {
                    return Err(invalid("PNG compression or filter method is unknown"));
                }
                if body.get(12) != Some(&0) {
                    return Err(invalid("interlaced PNG is not supported"));
                }
                let valid = match color {
                    0 => matches!(depth, 1 | 2 | 4 | 8 | 16),
                    3 => matches!(depth, 1 | 2 | 4 | 8),
                    2 | 4 | 6 => matches!(depth, 8 | 16),
                    _ => false,
                };
                if !valid {
                    return Err(invalid("PNG colour type and bit depth are invalid"));
                }
                if width == 0 || height == 0 {
                    return Err(invalid("PNG dimensions must be positive"));
                }
                let pixels = mul(
                    usize::try_from(width).map_err(|_| invalid("PNG width overflow"))?,
                    usize::try_from(height).map_err(|_| invalid("PNG height overflow"))?,
                )?;
                check("image_pixels", pixels, MAX_PIXELS)?;
                header = Some(Header {
                    width,
                    height,
                    depth,
                    color,
                });
            }
            b"PLTE" => {
                if body.is_empty() || !body.len().is_multiple_of(3) || body.len() > 768 {
                    return Err(invalid("PNG palette is invalid"));
                }
                palette = Some(body);
            }
            b"tRNS" => transparency = Some(body),
            b"IDAT" => {
                if header.is_none() {
                    return Err(invalid("PNG IDAT precedes IHDR"));
                }
                append(&mut compressed, body, MAX_BYTES)?;
            }
            b"IEND" => {
                ended = true;
                break;
            }
            _ => {
                if kind.first().is_some_and(|byte| byte.is_ascii_uppercase()) {
                    return Err(invalid("PNG has an unknown critical chunk"));
                }
            }
        }
    }
    if !ended {
        return Err(invalid("PNG IEND is missing"));
    }
    let header = header.ok_or_else(|| invalid("PNG IHDR is missing"))?;
    if header.color == 3 && palette.is_none() {
        return Err(invalid("indexed PNG has no palette"));
    }
    let channels = header.channels();
    let width = usize::try_from(header.width).map_err(|_| invalid("PNG width overflow"))?;
    let height = usize::try_from(header.height).map_err(|_| invalid("PNG height overflow"))?;
    let row_bits = mul(mul(width, channels)?, usize::from(header.depth))?;
    let row_bytes = add(row_bits, 7)? / 8;
    let expected = mul(row_bytes, height)?;
    let step = FilterStep {
        kind: FilterKind::Flate,
        params: DecodeParams {
            predictor: 15,
            colors: u8::try_from(channels).map_err(|_| invalid("PNG channel count"))?,
            bits_per_component: header.depth,
            columns: header.width,
            early_change: true,
        },
    };
    // The inflated stream carries one filter-type byte per row.
    let inflated = add(expected, height)?;
    let decoded = filter_decode(&compressed, &[step], inflated as u64)
        .map_err(|_| invalid("PNG image data is corrupt"))?;
    if decoded.truncated || decoded.bytes.len() < expected {
        return Err(invalid("PNG image data is truncated"));
    }
    let samples = decoded.bytes;
    let count = mul(width, height)?;
    let mut rgba = zeroed(mul(count, 4)?, MAX_BYTES)?;
    let depth = usize::from(header.depth);
    let maximum = (1u32 << header.depth.min(8)) - 1;
    // Sample `index` of row `row`, reduced to eight bits.
    let sample = |row: usize, index: usize| -> io::Result<(u8, u16)> {
        let base = mul(row, row_bytes)?;
        if depth == 16 {
            let at = add(base, mul(index, 2)?)?;
            let high = *samples.get(at).ok_or_else(|| invalid("PNG sample"))?;
            let low = *samples
                .get(add(at, 1)?)
                .ok_or_else(|| invalid("PNG sample"))?;
            return Ok((high, u16::from_be_bytes([high, low])));
        }
        if depth == 8 {
            let value = *samples
                .get(add(base, index)?)
                .ok_or_else(|| invalid("PNG sample"))?;
            return Ok((value, u16::from(value)));
        }
        let bit = mul(index, depth)?;
        let byte = *samples
            .get(add(base, bit / 8)?)
            .ok_or_else(|| invalid("PNG sample"))?;
        let shift = 8 - depth - bit % 8;
        let raw = (u32::from(byte) >> shift) & maximum;
        Ok(((raw * 255 / maximum) as u8, raw as u16))
    };
    let key = |index: usize| -> Option<u16> {
        transparency.and_then(|bytes| {
            let at = index.checked_mul(2)?;
            Some(u16::from_be_bytes([*bytes.get(at)?, *bytes.get(at + 1)?]))
        })
    };
    for row in 0..height {
        for column in 0..width {
            let pixel = add(mul(row, width)?, column)?;
            let first = mul(column, channels)?;
            let out = mul(pixel, 4)?;
            let value = match header.color {
                0 => {
                    let (gray, raw) = sample(row, first)?;
                    let alpha = if key(0) == Some(raw) { 0 } else { 255 };
                    [gray, gray, gray, alpha]
                }
                2 => {
                    let (r, raw_r) = sample(row, first)?;
                    let (g, raw_g) = sample(row, add(first, 1)?)?;
                    let (b, raw_b) = sample(row, add(first, 2)?)?;
                    let transparent =
                        key(0) == Some(raw_r) && key(1) == Some(raw_g) && key(2) == Some(raw_b);
                    [r, g, b, if transparent { 0 } else { 255 }]
                }
                3 => {
                    let (_, raw) = sample(row, first)?;
                    let entry = usize::from(raw);
                    let colors = palette.ok_or_else(|| invalid("PNG palette"))?;
                    let at = mul(entry, 3)?;
                    let rgb = colors
                        .get(at..add(at, 3)?)
                        .ok_or_else(|| invalid("PNG palette index is out of range"))?;
                    let alpha = transparency
                        .and_then(|bytes| bytes.get(entry))
                        .copied()
                        .unwrap_or(255);
                    [
                        *rgb.first().ok_or_else(|| invalid("PNG palette"))?,
                        *rgb.get(1).ok_or_else(|| invalid("PNG palette"))?,
                        *rgb.get(2).ok_or_else(|| invalid("PNG palette"))?,
                        alpha,
                    ]
                }
                4 => {
                    let (gray, _) = sample(row, first)?;
                    let (alpha, _) = sample(row, add(first, 1)?)?;
                    [gray, gray, gray, alpha]
                }
                _ => {
                    let (r, _) = sample(row, first)?;
                    let (g, _) = sample(row, add(first, 1)?)?;
                    let (b, _) = sample(row, add(first, 2)?)?;
                    let (alpha, _) = sample(row, add(first, 3)?)?;
                    [r, g, b, alpha]
                }
            };
            rgba.get_mut(out..add(out, 4)?)
                .ok_or_else(|| invalid("PNG output"))?
                .copy_from_slice(&value);
        }
    }
    Ok((header.width, header.height, rgba))
}
