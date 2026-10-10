use alloc::borrow::Cow;
use alloc::vec::Vec;
use core::fmt;

use crate::limits::{LimitKind, Refusal};

mod ascii85;
mod asciihex;
pub mod ccitt;
mod flate;
pub mod jbig2;
mod lzw;
mod predictor;
mod runlength;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FilterKind {
    Flate,
    Lzw,
    Ascii85,
    AsciiHex,
    RunLength,
    Dct,
    Jpx,
    Jbig2,
    CcittFax,
    Crypt,
}

impl FilterKind {
    pub fn from_name(name: &[u8]) -> Option<FilterKind> {
        match name {
            b"FlateDecode" | b"Fl" => Some(Self::Flate),
            b"LZWDecode" | b"LZW" => Some(Self::Lzw),
            b"ASCII85Decode" | b"A85" => Some(Self::Ascii85),
            b"ASCIIHexDecode" | b"AHx" => Some(Self::AsciiHex),
            b"RunLengthDecode" | b"RL" => Some(Self::RunLength),
            b"DCTDecode" | b"DCT" => Some(Self::Dct),
            b"JPXDecode" => Some(Self::Jpx),
            b"JBIG2Decode" => Some(Self::Jbig2),
            b"CCITTFaxDecode" | b"CCF" => Some(Self::CcittFax),
            b"Crypt" => Some(Self::Crypt),
            _ => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Flate => "FlateDecode",
            Self::Lzw => "LZWDecode",
            Self::Ascii85 => "ASCII85Decode",
            Self::AsciiHex => "ASCIIHexDecode",
            Self::RunLength => "RunLengthDecode",
            Self::Dct => "DCTDecode",
            Self::Jpx => "JPXDecode",
            Self::Jbig2 => "JBIG2Decode",
            Self::CcittFax => "CCITTFaxDecode",
            Self::Crypt => "Crypt",
        }
    }

    pub const fn is_image_codec(self) -> bool {
        matches!(self, Self::Dct | Self::Jpx | Self::Jbig2 | Self::CcittFax)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecodeParams {
    pub predictor: u8,
    pub colors: u8,
    pub bits_per_component: u8,
    pub columns: u32,
    pub early_change: bool,
}

impl Default for DecodeParams {
    fn default() -> Self {
        Self {
            predictor: 1,
            colors: 1,
            bits_per_component: 8,
            columns: 1,
            early_change: true,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImageParams {
    pub width: u32,
    pub height: u32,
    pub globals: Option<Vec<u8>>,
    pub k: i32,
    pub columns: u32,
    pub rows: u32,
    pub black_is_1: bool,
    pub encoded_byte_align: bool,
    pub end_of_line: bool,
    pub end_of_block: bool,
    pub damaged_rows_before_error: u32,
}

impl Default for ImageParams {
    fn default() -> Self {
        Self {
            width: 0,
            height: 0,
            globals: None,
            k: 0,
            columns: 1728,
            rows: 0,
            black_is_1: false,
            encoded_byte_align: false,
            end_of_line: false,
            end_of_block: true,
            damaged_rows_before_error: 0,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImageDecoded {
    pub bytes: Vec<u8>,
    pub truncated: bool,
    pub skipped_refinement: bool,
    pub skipped_unsupported: bool,
}

/// Decodes byte-padded, MSB-first, one-bit image rows.
///
/// JBIG2 returns 1 for black. CCITT samples follow `black_is_1`.
/// The image dimensions must match the decoded bitmap.
pub fn decode_image(
    kind: FilterKind,
    data: &[u8],
    params: &ImageParams,
    max_pixels: u64,
) -> Result<Vec<u8>, FilterError> {
    decode_image_report(kind, data, params, max_pixels).map(|decoded| decoded.bytes)
}

/// The reporting form of `decode_image`, for callers that expose warnings.
pub fn decode_image_report(
    kind: FilterKind,
    data: &[u8],
    params: &ImageParams,
    max_pixels: u64,
) -> Result<ImageDecoded, FilterError> {
    if !matches!(kind, FilterKind::Jbig2 | FilterKind::CcittFax) {
        return Err(FilterError::Unsupported { filter: kind });
    }
    if params.width == 0 || params.height == 0 {
        return Err(FilterError::BadParams { filter: kind });
    }
    let pixels = u64::from(params.width).checked_mul(u64::from(params.height))
        .ok_or(FilterError::Refused(Refusal {
            limit: LimitKind::ImagePixels,
            requested: u64::MAX,
            maximum: max_pixels,
        }))?;
    if pixels > max_pixels {
        return Err(FilterError::Refused(Refusal {
            limit: LimitKind::ImagePixels,
            requested: pixels,
            maximum: max_pixels,
        }));
    }
    let expected = ((u64::from(params.width) + 7) / 8)
        .checked_mul(u64::from(params.height))
        .and_then(|size| usize::try_from(size).ok())
        .filter(|&size| size <= isize::MAX as usize)
        .ok_or(FilterError::Refused(Refusal {
            limit: LimitKind::DecodedBytes,
            requested: u64::MAX,
            maximum: isize::MAX as u64,
        }))?;
    let decoded = match kind {
        FilterKind::Jbig2 => {
            let report = jbig2::decode_jbig2_report(
                data,
                params.globals.as_deref(),
                max_pixels,
            )?;
            if report.bitmap.width != params.width || report.bitmap.height != params.height {
                return Err(FilterError::BadParams { filter: kind });
            }
            ImageDecoded {
                bytes: report.bitmap.data,
                truncated: report.truncated,
                skipped_refinement: report.skipped_refinement,
                skipped_unsupported: report.skipped_unsupported,
            }
        }
        FilterKind::CcittFax => {
            let rows = if params.rows == 0 { params.height } else { params.rows };
            if params.columns != params.width || rows != params.height {
                return Err(FilterError::BadParams { filter: kind });
            }
            // Request canonical black bits from the fax decoder, then apply the PDF
            // sample polarity here, independently of the decoder's defaults.
            let fax = ccitt::CcittParams {
                k: params.k,
                columns: params.columns,
                rows,
                black_is_1: true,
                encoded_byte_align: params.encoded_byte_align,
                end_of_line: params.end_of_line,
                end_of_block: params.end_of_block,
                damaged_rows_before_error: params.damaged_rows_before_error,
            };
            let (mut bytes, truncated) = ccitt::decode(data, &fax, max_pixels)?;
            if !params.black_is_1 {
                for byte in &mut bytes {
                    *byte ^= 255;
                }
            }
            ImageDecoded {
                bytes,
                truncated,
                skipped_refinement: false,
                skipped_unsupported: false,
            }
        }
        _ => return Err(FilterError::Unsupported { filter: kind }),
    };
    if decoded.bytes.len() != expected {
        return Err(FilterError::Corrupt { filter: kind, offset: data.len() as u64 });
    }
    Ok(decoded)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FilterStep {
    pub kind: FilterKind,
    pub params: DecodeParams,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Decoded {
    pub bytes: Vec<u8>,
    pub image: Option<FilterKind>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FilterError {
    OutputLimit { limit: u64 },
    Refused(Refusal),
    Unsupported { filter: FilterKind },
    BadParams { filter: FilterKind },
    Corrupt { filter: FilterKind, offset: u64 },
}

impl FilterError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::OutputLimit { .. } => "output_limit",
            Self::Refused(refusal) => refusal.limit.code(),
            Self::Unsupported { .. } => "unsupported",
            Self::BadParams { .. } => "bad_params",
            Self::Corrupt { .. } => "corrupt",
        }
    }
}

impl fmt::Display for FilterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutputLimit { limit } => {
                write!(formatter, "decoded output exceeds the {limit} byte limit")
            }
            Self::Refused(refusal) => {
                write!(
                    formatter,
                    "{} requested {}, maximum {}",
                    refusal.limit.code(),
                    refusal.requested,
                    refusal.maximum
                )
            }
            Self::Unsupported { filter } => {
                write!(formatter, "unsupported filter {}", filter.name())
            }
            Self::BadParams { filter } => {
                write!(formatter, "invalid parameters for {}", filter.name())
            }
            Self::Corrupt { filter, offset } => {
                write!(formatter, "corrupt {} input at byte {offset}", filter.name())
            }
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for FilterError {}

/// Applies filters in order, stopping before the first image codec.
///
/// Each filter's output, including predictor input, is bounded by `max_output`.
/// Corrupt or incomplete input preserves recovered bytes when possible.
pub fn decode(
    input: &[u8],
    steps: &[FilterStep],
    max_output: u64,
) -> Result<Decoded, FilterError> {
    let mut data = Cow::Borrowed(input);
    let mut truncated = false;
    let mut image = None;

    for step in steps {
        if step.kind.is_image_codec() {
            check_size(data.len(), max_output)?;
            image = Some(step.kind);
            break;
        }

        if step.kind == FilterKind::Crypt {
            if step.params != DecodeParams::default() {
                return Err(FilterError::Unsupported {
                    filter: FilterKind::Crypt,
                });
            }
            check_size(data.len(), max_output)?;
            continue;
        }

        let layout = match step.kind {
            FilterKind::Flate | FilterKind::Lzw => {
                predictor::Layout::new(step.params, step.kind)?
            }
            _ => None,
        };

        let mut decoded = match step.kind {
            FilterKind::Flate => flate::decode(data.as_ref(), max_output)?,
            FilterKind::Lzw => {
                lzw::decode(data.as_ref(), step.params.early_change, max_output)?
            }
            FilterKind::Ascii85 => ascii85::decode(data.as_ref(), max_output)?,
            FilterKind::AsciiHex => asciihex::decode(data.as_ref(), max_output)?,
            FilterKind::RunLength => runlength::decode(data.as_ref(), max_output)?,
            _ => {
                return Err(FilterError::Unsupported {
                    filter: step.kind,
                });
            }
        };

        if let Some(layout) = layout {
            let predicted = predictor::decode(decoded.bytes, layout, step.kind)?;
            decoded.bytes = predicted.bytes;
            decoded.truncated |= predicted.truncated;
        }

        truncated |= decoded.truncated;
        data = Cow::Owned(decoded.bytes);
    }

    check_size(data.len(), max_output)?;
    let bytes = match data {
        Cow::Owned(bytes) => bytes,
        Cow::Borrowed(bytes) => {
            let mut output = Output::new(max_output);
            output.extend(bytes)?;
            output.into_vec()
        }
    };

    Ok(Decoded {
        bytes,
        image,
        truncated,
    })
}

fn addressable_limit(max_output: u64) -> usize {
    usize::try_from(max_output)
        .unwrap_or(usize::MAX)
        .min(isize::MAX as usize)
}

fn check_size(size: usize, max_output: u64) -> Result<(), FilterError> {
    if size > addressable_limit(max_output) {
        Err(FilterError::OutputLimit { limit: max_output })
    } else {
        Ok(())
    }
}

/// Reserves geometrically without requesting capacity beyond the output cap.
///
/// The public API has no allocation-failure variant, so an allocation that
/// cannot be satisfied is reported through the same named output refusal.
fn reserve_to(
    bytes: &mut Vec<u8>,
    size: usize,
    max_output: u64,
) -> Result<(), FilterError> {
    check_size(size, max_output)?;
    if size <= bytes.capacity() {
        return Ok(());
    }

    let capacity = bytes
        .capacity()
        .saturating_mul(2)
        .max(64)
        .max(size)
        .min(addressable_limit(max_output));
    bytes
        .try_reserve_exact(capacity.saturating_sub(bytes.len()))
        .map_err(|_| FilterError::OutputLimit { limit: max_output })
}

struct Output {
    bytes: Vec<u8>,
    max_output: u64,
}

impl Output {
    fn new(max_output: u64) -> Self {
        Self {
            bytes: Vec::new(),
            max_output,
        }
    }

    fn prepare(&mut self, additional: usize) -> Result<usize, FilterError> {
        let size = self
            .bytes
            .len()
            .checked_add(additional)
            .ok_or(FilterError::OutputLimit {
                limit: self.max_output,
            })?;
        reserve_to(&mut self.bytes, size, self.max_output)?;
        Ok(size)
    }

    fn push(&mut self, byte: u8) -> Result<(), FilterError> {
        self.prepare(1)?;
        self.bytes.push(byte);
        Ok(())
    }

    fn extend(&mut self, bytes: &[u8]) -> Result<(), FilterError> {
        self.prepare(bytes.len())?;
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    fn repeat(&mut self, byte: u8, count: usize) -> Result<(), FilterError> {
        let size = self.prepare(count)?;
        self.bytes.resize(size, byte);
        Ok(())
    }

    fn into_vec(self) -> Vec<u8> {
        self.bytes
    }
}

fn finish(
    bytes: Vec<u8>,
    truncated: bool,
    filter: FilterKind,
    offset: usize,
) -> Result<Decoded, FilterError> {
    if truncated && bytes.is_empty() {
        return Err(FilterError::Corrupt {
            filter,
            offset: offset as u64,
        });
    }
    Ok(Decoded {
        bytes,
        image: None,
        truncated,
    })
}

fn is_whitespace(byte: u8) -> bool {
    matches!(byte, 0 | 9 | 10 | 12 | 13 | 32)
}
