use std::io;
use std::sync::Arc;

use super::bounds::*;
use super::font::{read16, slice};

pub(super) enum Pixels {
    Jpeg {
        data: Arc<[u8]>,
        components: u8,
        invert: bool,
    },
    Rgb {
        data: Vec<u8>,
        alpha: Option<Vec<u8>>,
    },
}

pub(super) struct Image {
    pub width: u32,
    pub height: u32,
    pub pixels: Pixels,
}

fn pixels(width: u32, height: u32) -> io::Result<usize> {
    if width == 0 || height == 0 {
        return Err(invalid("image dimensions must be positive"));
    }
    let width = usize::try_from(width).map_err(|_| invalid("image width overflow"))?;
    let height = usize::try_from(height).map_err(|_| invalid("image height overflow"))?;
    let count = mul(width, height)?;
    check("image_pixels", count, MAX_PIXELS)?;
    Ok(count)
}

/// Width, height and component count from the first JPEG frame header.
pub(super) fn jpeg_frame(data: &[u8]) -> io::Result<(u32, u32, u8)> {
    if !data.starts_with(&[0xff, 0xd8]) {
        return Err(invalid("JPEG start marker is missing"));
    }
    let mut at = 2usize;
    for _ in 0..4096 {
        if data.get(at) != Some(&0xff) {
            return Err(invalid("JPEG marker is invalid"));
        }
        for _ in 0..4096 {
            if data.get(at) != Some(&0xff) {
                break;
            }
            at = add(at, 1)?;
        }
        let marker = *data
            .get(at)
            .ok_or_else(|| invalid("JPEG marker is truncated"))?;
        at = add(at, 1)?;
        let length = usize::from(read16(data, at)?);
        if length < 2 {
            return Err(invalid("JPEG segment length is invalid"));
        }
        let segment = slice(data, at, length)?;
        if matches!(marker, 0xc0..=0xcf) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
            let height = u32::from(read16(segment, 3)?);
            let width = u32::from(read16(segment, 5)?);
            let components = *segment
                .get(7)
                .ok_or_else(|| invalid("JPEG frame is truncated"))?;
            return Ok((width, height, components));
        }
        if marker == 0xda {
            break;
        }
        at = add(at, length)?;
    }
    Err(invalid("JPEG frame is missing"))
}

impl Image {
    pub fn bytes(&self) -> usize {
        match &self.pixels {
            Pixels::Jpeg { data, .. } => data.len(),
            Pixels::Rgb { data, alpha } => data
                .len()
                .saturating_add(alpha.as_ref().map_or(0, Vec::len)),
        }
    }

    pub fn rgba(width: u32, height: u32, rgba: &[u8]) -> io::Result<Self> {
        let count = pixels(width, height)?;
        let expected = mul(count, 4)?;
        if rgba.len() != expected {
            return Err(invalid("RGBA byte length does not match dimensions"));
        }
        let alpha_needed = rgba.chunks_exact(4).any(|pixel| pixel.get(3) != Some(&255));
        let mut rgb = zeroed(0, mul(count, 3)?)?;
        rgb.try_reserve_exact(mul(count, 3)?)
            .map_err(|_| refusal("allocation_bytes", expected, MAX_BYTES))?;
        let mut alpha = if alpha_needed {
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(count)
                .map_err(|_| refusal("allocation_bytes", expected, MAX_BYTES))?;
            Some(bytes)
        } else {
            None
        };
        for pixel in rgba.chunks_exact(4) {
            append(&mut rgb, slice(pixel, 0, 3)?, mul(count, 3)?)?;
            if let Some(alpha) = alpha.as_mut() {
                append(alpha, slice(pixel, 3, 1)?, count)?;
            }
        }
        Ok(Self {
            width,
            height,
            pixels: Pixels::Rgb { data: rgb, alpha },
        })
    }

    pub fn jpeg(data: Arc<[u8]>, width: u32, height: u32, components: u8) -> io::Result<Self> {
        pixels(width, height)?;
        check("jpeg_bytes", data.len(), MAX_FONT_BYTES)?;
        if !matches!(components, 1 | 3 | 4) {
            return Err(invalid("JPEG must have one, three or four components"));
        }
        if !data.starts_with(&[0xff, 0xd8]) || !data.ends_with(&[0xff, 0xd9]) {
            return Err(invalid("JPEG start or end marker is missing"));
        }
        let mut at = 2usize;
        let mut found = false;
        let mut invert = false;
        let mut reached_scan = false;
        for _ in 0..4096 {
            if data.get(at) != Some(&0xff) {
                return Err(invalid("JPEG marker is invalid"));
            }
            for _ in 0..4096 {
                if data.get(at) != Some(&0xff) {
                    break;
                }
                at = add(at, 1)?;
            }
            let marker = *data
                .get(at)
                .ok_or_else(|| invalid("JPEG marker is truncated"))?;
            at = add(at, 1)?;
            if matches!(marker, 0 | 0xd8 | 0xd9 | 0x01 | 0xd0..=0xd7) {
                return Err(invalid("unexpected JPEG header marker"));
            }
            let length = usize::from(read16(&data, at)?);
            if length < 2 {
                return Err(invalid("JPEG segment length is invalid"));
            }
            let segment = slice(&data, at, length)?;
            if marker == 0xee && segment.get(2..7) == Some(b"Adobe".as_slice()) {
                invert = components == 4;
            }
            if matches!(marker, 0xc0..=0xcf) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
                if !matches!(marker, 0xc0 | 0xc1 | 0xc2) {
                    return Err(invalid("unsupported JPEG coding process"));
                }
                if found || segment.get(2) != Some(&8) {
                    return Err(invalid("JPEG frame or sample precision is invalid"));
                }
                let frame_height = u32::from(read16(segment, 3)?);
                let frame_width = u32::from(read16(segment, 5)?);
                if frame_width != width
                    || frame_height != height
                    || segment.get(7) != Some(&components)
                    || length != add(8, mul(usize::from(components), 3)?)?
                {
                    return Err(invalid(
                        "JPEG frame does not match supplied dimensions or components",
                    ));
                }
                found = true;
            }
            at = add(at, length)?;
            if marker == 0xda {
                reached_scan = true;
                break;
            }
        }
        if !reached_scan {
            return Err(refusal("jpeg_header_segments", 4097, 4096));
        }
        if !found {
            return Err(invalid("JPEG frame is missing"));
        }
        Ok(Self {
            width,
            height,
            pixels: Pixels::Jpeg {
                data,
                components,
                invert,
            },
        })
    }
}
