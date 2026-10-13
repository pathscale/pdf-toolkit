use alloc::vec::Vec;
use miniz_oxide::inflate::core::inflate_flags::{
    TINFL_FLAG_IGNORE_ADLER32, TINFL_FLAG_PARSE_ZLIB_HEADER,
    TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF,
};
use miniz_oxide::inflate::core::{decompress, DecompressorOxide};
use miniz_oxide::inflate::TINFLStatus;

use super::{addressable_limit, finish, reserve_to, Decoded, FilterError, FilterKind};

pub(super) fn decode(input: &[u8], max_output: u64) -> Result<Decoded, FilterError> {
    let mut flags = TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF | TINFL_FLAG_IGNORE_ADLER32;
    if has_zlib_header(input) {
        flags |= TINFL_FLAG_PARSE_ZLIB_HEADER;
    }

    let ceiling = addressable_limit(max_output);
    let initial = input.len().saturating_mul(2).max(64).min(ceiling);
    let mut bytes = Vec::new();
    reserve_to(&mut bytes, initial, max_output)?;
    bytes.resize(initial, 0);

    let mut state = DecompressorOxide::new();
    let mut remaining = input;
    let mut used = 0usize;

    loop {
        let (status, consumed, produced) =
            decompress(&mut state, remaining, &mut bytes, used, flags);

        if consumed > remaining.len() || produced > bytes.len().saturating_sub(used) {
            bytes.truncate(used);
            return finish(
                bytes,
                true,
                FilterKind::Flate,
                input.len().saturating_sub(remaining.len()),
            );
        }

        used = used.saturating_add(produced);
        let Some(rest) = remaining.get(consumed..) else {
            bytes.truncate(used);
            return finish(
                bytes,
                true,
                FilterKind::Flate,
                input.len().saturating_sub(remaining.len()),
            );
        };
        remaining = rest;

        match status {
            TINFLStatus::Done | TINFLStatus::Adler32Mismatch => {
                bytes.truncate(used);
                return finish(bytes, false, FilterKind::Flate, 0);
            }
            TINFLStatus::HasMoreOutput => {
                if bytes.len() >= ceiling {
                    return Err(FilterError::OutputLimit { limit: max_output });
                }

                let size = bytes.len().saturating_mul(2).max(64).min(ceiling);
                if size <= bytes.len() {
                    return Err(FilterError::OutputLimit { limit: max_output });
                }
                reserve_to(&mut bytes, size, max_output)?;
                bytes.resize(size, 0);
            }
            _ => {
                bytes.truncate(used);
                return finish(
                    bytes,
                    true,
                    FilterKind::Flate,
                    input.len().saturating_sub(remaining.len()),
                );
            }
        }
    }
}

fn has_zlib_header(input: &[u8]) -> bool {
    let Some(header) = input.get(..2) else {
        return false;
    };
    let [cmf, flg] = header else {
        return false;
    };
    (*cmf & 0x0f) == 8 && (u16::from(*cmf) * 256 + u16::from(*flg)) % 31 == 0
}
