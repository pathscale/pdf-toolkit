use alloc::vec::Vec;

use super::{finish, DecodeParams, Decoded, FilterError, FilterKind};

#[derive(Clone, Copy)]
pub(super) struct Layout {
    predictor: u8,
    colors: usize,
    bits_per_component: u8,
    samples: usize,
    row_bytes: usize,
    bytes_per_pixel: usize,
}

impl Layout {
    pub(super) fn new(
        params: DecodeParams,
        filter: FilterKind,
    ) -> Result<Option<Self>, FilterError> {
        if params.predictor == 1 {
            return Ok(None);
        }

        let bad = FilterError::BadParams { filter };
        if !matches!(params.predictor, 2 | 10..=15)
            || !(1..=32).contains(&params.colors)
            || !matches!(params.bits_per_component, 1 | 2 | 4 | 8 | 16)
            || params.columns == 0
        {
            return Err(bad);
        }

        let colors = usize::from(params.colors);
        let columns = usize::try_from(params.columns).map_err(|_| bad.clone())?;
        let samples = colors.checked_mul(columns).ok_or_else(|| bad.clone())?;
        let row_bits = samples
            .checked_mul(usize::from(params.bits_per_component))
            .ok_or_else(|| bad.clone())?;
        let row_bytes = row_bits
            .checked_add(7)
            .map(|bits| bits / 8)
            .filter(|&bytes| bytes != 0)
            .ok_or_else(|| bad.clone())?;
        let pixel_bits = colors
            .checked_mul(usize::from(params.bits_per_component))
            .ok_or(bad)?;

        Ok(Some(Self {
            predictor: params.predictor,
            colors,
            bits_per_component: params.bits_per_component,
            samples,
            row_bytes,
            bytes_per_pixel: (pixel_bits / 8).max(1),
        }))
    }
}

pub(super) fn decode(
    bytes: Vec<u8>,
    layout: Layout,
    filter: FilterKind,
) -> Result<Decoded, FilterError> {
    if layout.predictor == 2 {
        decode_tiff(bytes, layout, filter)
    } else {
        decode_png(bytes, layout, filter)
    }
}

fn decode_tiff(
    mut bytes: Vec<u8>,
    layout: Layout,
    filter: FilterKind,
) -> Result<Decoded, FilterError> {
    let truncated = bytes.len() % layout.row_bytes != 0;
    let offset = bytes.len();

    for row in bytes.chunks_mut(layout.row_bytes) {
        let mut previous = [0u16; 32];
        let Some(previous) = previous.get_mut(..layout.colors) else {
            return Err(FilterError::BadParams { filter });
        };

        match layout.bits_per_component {
            8 => {
                for pixel in row.chunks_mut(layout.colors) {
                    for (byte, left) in pixel.iter_mut().zip(previous.iter_mut()) {
                        *byte = byte.wrapping_add(*left as u8);
                        *left = u16::from(*byte);
                    }
                }
            }
            16 => {
                let mut component = 0usize;
                let mut pairs = row.chunks_exact_mut(2);
                for pair in pairs.by_ref() {
                    if let [high, low] = pair {
                        let encoded = (u16::from(*high) << 8) | u16::from(*low);
                        let recovered = add_component(
                            encoded,
                            previous,
                            &mut component,
                            u16::MAX,
                        );
                        let [new_high, new_low] = recovered.to_be_bytes();
                        *high = new_high;
                        *low = new_low;
                    }
                }

                // A short final sample has no low byte. Recover its available
                // high byte with a zero low byte and retain the truncation flag.
                if let Some(high) = pairs.into_remainder().first_mut() {
                    let recovered = add_component(
                        u16::from(*high) << 8,
                        previous,
                        &mut component,
                        u16::MAX,
                    );
                    *high = (recovered >> 8) as u8;
                }
            }
            bits => {
                let mask = (1u16 << bits) - 1;
                let slots = 8u8 / bits;
                let mut component = 0usize;
                let mut samples_left = layout.samples;

                for byte in row.iter_mut() {
                    for slot in 0..slots {
                        if samples_left == 0 {
                            break;
                        }
                        let shift = 8u8
                            .saturating_sub(bits)
                            .saturating_sub(slot.saturating_mul(bits));
                        let encoded = u16::from(*byte >> shift) & mask;
                        let recovered = add_component(
                            encoded,
                            previous,
                            &mut component,
                            mask,
                        );
                        let byte_mask = (mask as u8) << shift;
                        *byte = (*byte & !byte_mask) | ((recovered as u8) << shift);
                        samples_left = samples_left.saturating_sub(1);
                    }
                }
            }
        }
    }

    finish(bytes, truncated, filter, offset)
}

fn add_component(
    encoded: u16,
    previous: &mut [u16],
    component: &mut usize,
    mask: u16,
) -> u16 {
    let recovered = if let Some(left) = previous.get_mut(*component) {
        let recovered = encoded.wrapping_add(*left) & mask;
        *left = recovered;
        recovered
    } else {
        encoded & mask
    };

    *component = (*component).saturating_add(1);
    if *component >= previous.len() {
        *component = 0;
    }
    recovered
}

fn decode_png(
    mut bytes: Vec<u8>,
    layout: Layout,
    filter: FilterKind,
) -> Result<Decoded, FilterError> {
    let input_len = bytes.len();
    let mut read = 0usize;
    let mut written = 0usize;
    let mut previous_start = 0usize;
    let mut truncated = false;
    let mut fault_offset = input_len;

    while read < input_len {
        let Some(tag) = bytes.get(read).copied() else {
            truncated = true;
            fault_offset = read;
            break;
        };
        if tag > 4 {
            truncated = true;
            fault_offset = read;
            break;
        }

        let Some(start) = read.checked_add(1).filter(|&start| start <= input_len) else {
            truncated = true;
            fault_offset = read;
            break;
        };
        let count = input_len.saturating_sub(start).min(layout.row_bytes);
        let Some(end) = start.checked_add(count).filter(|&end| end <= input_len) else {
            truncated = true;
            fault_offset = read;
            break;
        };
        let Some(next_written) = written
            .checked_add(count)
            .filter(|&end| end <= input_len)
        else {
            truncated = true;
            fault_offset = read;
            break;
        };

        // `start` has been checked against the buffer length. The preceding
        // decoded row lives in the compacted prefix and cannot overlap this row.
        {
            let (prefix, tail) = bytes.split_at_mut(start);
            let Some(previous) = prefix.get(previous_start..written) else {
                truncated = true;
                fault_offset = read;
                break;
            };
            let Some(row) = tail.get_mut(..count) else {
                truncated = true;
                fault_offset = read;
                break;
            };
            png_row(row, previous, layout.bytes_per_pixel, tag);
        }

        // Both the source range and destination end have been checked above.
        // Moving toward the start never overwrites the next encoded row.
        bytes.copy_within(start..end, written);
        previous_start = written;
        written = next_written;
        read = end;

        if count < layout.row_bytes {
            truncated = true;
            fault_offset = end;
            break;
        }
    }

    bytes.truncate(written);
    finish(bytes, truncated, filter, fault_offset)
}

fn png_row(row: &mut [u8], previous: &[u8], bytes_per_pixel: usize, tag: u8) {
    // Validated colors and bit depth cap bytes_per_pixel at 64.
    let mut left = [0u8; 64];
    let mut upper_left = [0u8; 64];
    let mut previous_pixels = previous.chunks(bytes_per_pixel);

    for pixel in row.chunks_mut(bytes_per_pixel) {
        let upper = previous_pixels.next().unwrap_or(&[]);
        for ((byte, (left, upper_left)), up) in pixel
            .iter_mut()
            .zip(left.iter_mut().zip(upper_left.iter_mut()))
            .zip(upper.iter().copied().chain(core::iter::repeat(0)))
        {
            let prediction = match tag {
                0 => 0,
                1 => *left,
                2 => up,
                3 => ((u16::from(*left) + u16::from(up)) / 2) as u8,
                4 => paeth(*left, up, *upper_left),
                _ => 0,
            };
            *byte = byte.wrapping_add(prediction);
            *left = *byte;
            *upper_left = up;
        }
    }
}

fn paeth(left: u8, up: u8, upper_left: u8) -> u8 {
    let left_value = i32::from(left);
    let up_value = i32::from(up);
    let corner_value = i32::from(upper_left);
    let prediction = left_value + up_value - corner_value;
    let left_distance = (prediction - left_value).abs();
    let up_distance = (prediction - up_value).abs();
    let corner_distance = (prediction - corner_value).abs();

    if left_distance <= up_distance && left_distance <= corner_distance {
        left
    } else if up_distance <= corner_distance {
        up
    } else {
        upper_left
    }
}
