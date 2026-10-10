use alloc::vec::Vec;

use pdf_core::filter::{decode_image_report, Decoded, FilterKind, ImageParams};
use pdf_core::{
    DictRef, DocError, LimitKind, Object, ObjectRef, Resolver, Value, ValueRef, Warning,
};

use crate::content::{check, decode_string, refusal};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColorSpace {
    Gray,
    Rgb,
    Cmyk,
    Indexed { base: IndexedBase, hival: u8 },
    Unsupported,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IndexedBase {
    Gray,
    Rgb,
    Cmyk,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ImageData {
    Jpeg(Vec<u8>),
    Pixels { rgba: Vec<u8> },
    Unsupported(&'static str),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub color_space: ColorSpace,
    pub bits_per_component: u8,
    pub has_soft_mask: bool,
    pub image_mask: bool,
    pub data: ImageData,
    pub soft_mask: Option<Vec<u8>>,
}

struct ColorSpec {
    space: ColorSpace,
    lookup: Option<Value>,
}

impl ColorSpec {
    fn simple(space: ColorSpace) -> Self {
        Self { space, lookup: None }
    }
}

struct Budget {
    used: u64,
    maximum: u64,
}

impl Budget {
    fn charge(&mut self, bytes: usize) -> Result<(), DocError> {
        let requested = self.used.checked_add(bytes as u64).ok_or_else(|| {
            refusal(
                LimitKind::TotalDecodedBytes,
                u64::MAX,
                self.maximum,
            )
        })?;
        check(LimitKind::TotalDecodedBytes, requested, self.maximum)?;
        self.used = requested;
        Ok(())
    }
}

/// Loads an image XObject. JPEG payloads remain encoded; other supported
/// sample streams become straight-alpha RGBA in PDF row order.
pub fn load_image(
    resolver: &mut Resolver<'_>,
    id: ObjectRef,
    max_pixels: u64,
) -> Result<Image, DocError> {
    if !resolver.document().decryptable() {
        return Err(DocError::Encrypted);
    }

    let object = resolver.get(id)?;
    let value = resolver.resolve(object.root())?;
    let object = value.object().clone();
    let mut budget = Budget {
        used: 0,
        maximum: resolver.document().limits().max_total_decoded_bytes,
    };
    let (mut image, soft) =
        read_image(resolver, &object, max_pixels, true, &mut budget)?;

    if matches!(&image.data, ImageData::Pixels { .. }) {
        if let Some(soft) = soft {
            if let Err(error) = apply_soft_mask(
                resolver,
                &object,
                &mut image,
                &soft,
                max_pixels,
                &mut budget,
            ) {
                recover(resolver, &object, error, "image_soft_mask_failed")?;
            }
        }
    }

    Ok(image)
}

fn read_image(
    resolver: &mut Resolver<'_>,
    object: &Object,
    max_pixels: u64,
    read_soft: bool,
    budget: &mut Budget,
) -> Result<(Image, Option<Value>), DocError> {
    if !object.is_stream() {
        return Err(wrong(object.id(), "image stream"));
    }
    let dict = object.dict().ok_or_else(|| wrong(object.id(), "image dictionary"))?;
    let subtype = resolver.resolve_dict_key(dict, b"Subtype")?;
    if !subtype.as_ref()
        .and_then(|value| value.get().as_name())
        .map(|name| name.eq_bytes(b"Image"))
        .unwrap_or(false)
    {
        return Err(wrong(object.id(), "Image XObject"));
    }

    let width = dimension(resolver, dict, b"Width", object.id())?;
    let height = dimension(resolver, dict, b"Height", object.id())?;
    let pixels = u64::from(width).checked_mul(u64::from(height)).ok_or_else(|| {
        refusal(
            LimitKind::DecodedBytes,
            u64::MAX,
            max_pixels.saturating_mul(4),
        )
    })?;
    if pixels > max_pixels {
        return Err(refusal(
            LimitKind::DecodedBytes,
            pixels.saturating_mul(4),
            max_pixels.saturating_mul(4),
        ));
    }

    let image_mask = match resolver.resolve_dict_key(dict, b"ImageMask")? {
        None => false,
        Some(value) if value.get().is_null() => false,
        Some(value) => value.get().as_bool()
            .ok_or_else(|| wrong(object.id(), "ImageMask boolean"))?,
    };
    let mut bits_per_component = resolver.resolve_dict_key(dict, b"BitsPerComponent")?
        .and_then(|value| value.get().as_i64())
        .and_then(|number| u8::try_from(number).ok())
        .unwrap_or(if image_mask { 1 } else { 0 });

    let mut color = if image_mask {
        ColorSpec::simple(ColorSpace::Gray)
    } else {
        match read_color(resolver, dict) {
            Ok(color) => color,
            Err(error) => {
                recover(resolver, object, error, "image_colorspace_failed")?;
                ColorSpec::simple(ColorSpace::Unsupported)
            }
        }
    };

    let (has_soft_mask, soft) = if read_soft {
        match dict.get(b"SMask") {
            None => (false, None),
            Some(value) => match resolver.resolve(value) {
                Ok(value) if value.get().is_null() || value.get().as_name()
                    .map(|name| name.eq_bytes(b"None")).unwrap_or(false) =>
                {
                    (false, None)
                }
                Ok(value) => (true, Some(value)),
                Err(error) => {
                    recover(resolver, object, error, "image_soft_mask_failed")?;
                    (true, None)
                }
            },
        }
    } else {
        // A soft mask is sampled once. Its own SMask is never followed.
        (false, None)
    };

    let mut image = Image {
        width,
        height,
        color_space: color.space,
        bits_per_component,
        has_soft_mask,
        image_mask,
        data: ImageData::Unsupported("decode"),
        soft_mask: None,
    };
    let mut decoded = match decode(resolver, object, budget) {
        Ok(decoded) => decoded,
        Err(error) => {
            recover(resolver, object, error, "image_decode_failed")?;
            return Ok((image, soft));
        }
    };

    if let Some(codec) = decoded.image {
        if matches!(codec, FilterKind::Jbig2 | FilterKind::CcittFax) {
            let maximum = resolver.document().limits().max_decoded_bytes;
            SampleLayout::new(width, height, 1, 1, maximum)?;
            let result = (|| {
                let params = image_params(
                    resolver, object, dict, codec, width, height, budget,
                )?;
                let mut binary = decode_image_report(
                    codec, &decoded.bytes, &params, max_pixels,
                ).map_err(DocError::from)?;
                check(LimitKind::DecodedBytes, binary.bytes.len() as u64, maximum)?;
                budget.charge(binary.bytes.len())?;
                if codec == FilterKind::Jbig2 {
                    // JBIG2 exposes black bits. PDF gray samples use 0 for
                    // black, before the image's Decode pairs are applied.
                    for byte in &mut binary.bytes {
                        *byte ^= 255;
                    }
                }
                Ok::<_, DocError>(binary)
            })();
            let binary = match result {
                Ok(binary) => binary,
                Err(error) => {
                    recover(resolver, object, error, "image_bilevel_decode_failed")?;
                    image.data = ImageData::Unsupported(
                        if codec == FilterKind::Jbig2 { "jbig2" } else { "ccitt" },
                    );
                    return Ok((image, soft));
                }
            };
            if binary.skipped_refinement {
                warn(resolver, object, "image_jbig2_refinement_skipped");
            }
            if binary.skipped_unsupported {
                warn(resolver, object, "image_jbig2_segment_unsupported");
            }
            decoded.bytes = binary.bytes;
            decoded.truncated |= binary.truncated;
            decoded.image = None;
            color = ColorSpec::simple(ColorSpace::Gray);
            bits_per_component = 1;
            image.color_space = ColorSpace::Gray;
            image.bits_per_component = 1;
        } else {
            if decoded.truncated {
                warn(resolver, object, "image_truncated");
            }
            image.data = match codec {
                FilterKind::Dct => ImageData::Jpeg(decoded.bytes),
                FilterKind::Jpx => ImageData::Unsupported("jpx"),
                _ => ImageData::Unsupported("codec"),
            };
            return Ok((image, soft));
        }
    }

    if decoded.truncated {
        warn(resolver, object, "image_truncated");
    }
    if color.space == ColorSpace::Unsupported {
        warn(resolver, object, "image_colorspace_unsupported");
        image.data = ImageData::Unsupported("colorspace");
        return Ok((image, soft));
    }
    if !matches!(bits_per_component, 1 | 2 | 4 | 8 | 16)
        || (image_mask && bits_per_component != 1)
    {
        warn(resolver, object, "image_bits_per_component_unsupported");
        image.data = ImageData::Unsupported("bits_per_component");
        return Ok((image, soft));
    }

    let channels = channels(color.space);
    let pairs = match read_decode(
        resolver,
        dict,
        channels,
        color.space,
        bits_per_component,
        object.id(),
    ) {
        Ok(pairs) => pairs,
        Err(error) => {
            recover(resolver, object, error, "image_decode_array_failed")?;
            image.data = ImageData::Unsupported("decode_array");
            return Ok((image, soft));
        }
    };
    let palette = match color.lookup.as_ref() {
        Some(value) => match read_lookup(resolver, value, color.space, budget) {
            Ok(palette) => palette,
            Err(error) => {
                recover(resolver, object, error, "image_lookup_failed")?;
                image.data = ImageData::Unsupported("lookup");
                return Ok((image, soft));
            }
        },
        None => Vec::new(),
    };

    let maximum = resolver.document().limits().max_decoded_bytes;
    let layout = SampleLayout::new(width, height, channels, bits_per_component, maximum)?;
    budget.charge(layout.rgba)?;
    let mut rgba = zeroed(layout.rgba, maximum)?;
    let mut missing = decoded.bytes.len() < layout.expected;

    'rows: for (y, output) in rgba.chunks_exact_mut(layout.row_rgba).enumerate() {
        let start = y.checked_mul(layout.stride).ok_or_else(|| size_error(maximum))?;
        let end = start.checked_add(layout.stride).ok_or_else(|| size_error(maximum))?;
        let row = decoded.bytes.get(start..end.min(decoded.bytes.len())).unwrap_or(&[]);
        let mut samples = Samples { bytes: row, bit: 0, bpc: bits_per_component };

        for pixel in output.chunks_exact_mut(4) {
            let mut raw = [0u16; 4];
            for slot in raw.iter_mut().take(channels) {
                let Some(sample) = samples.next() else {
                    missing = true;
                    break 'rows;
                };
                *slot = sample;
            }
            match pixel_rgba(
                color.space,
                image_mask,
                bits_per_component,
                raw,
                &pairs,
                &palette,
            ) {
                Some(value) => pixel.copy_from_slice(&value),
                None => missing = true,
            }
        }
    }

    if missing && !decoded.truncated {
        warn(resolver, object, "image_truncated");
    }
    image.data = ImageData::Pixels { rgba };
    Ok((image, soft))
}

fn image_params(
    resolver: &mut Resolver<'_>,
    object: &Object,
    dict: DictRef<'_>,
    codec: FilterKind,
    width: u32,
    height: u32,
    budget: &mut Budget,
) -> Result<ImageParams, DocError> {
    let mut params = ImageParams { width, height, ..ImageParams::default() };
    let filters = resolver.resolve_dict_key(dict, b"Filter")?
        .ok_or_else(|| wrong(object.id(), "image filter"))?;
    let mut index = None;
    if let Some(array) = filters.get().as_array() {
        for (position, value) in array.iter().enumerate() {
            let value = resolver.resolve(value)?;
            let kind = value.get().as_name()
                .and_then(|name| FilterKind::from_name(name.decoded().as_ref()));
            if kind.map(FilterKind::is_image_codec).unwrap_or(false) {
                if kind == Some(codec) {
                    index = Some(position);
                }
                break;
            }
        }
    } else {
        let kind = filters.get().as_name()
            .and_then(|name| FilterKind::from_name(name.decoded().as_ref()));
        if kind == Some(codec) {
            index = Some(0);
        }
    }
    let index = index.ok_or_else(|| wrong(object.id(), "matching image filter"))?;
    let Some(value) = resolver.resolve_dict_key(dict, b"DecodeParms")? else {
        return Ok(params);
    };
    let value = if let Some(array) = value.get().as_array() {
        let Some(value) = array.get(index) else {
            return Ok(params);
        };
        resolver.resolve(value)?
    } else if index == 0 {
        value
    } else {
        return Ok(params);
    };
    if value.get().is_null() {
        return Ok(params);
    }
    let parameters = value.get().as_dict()
        .ok_or_else(|| wrong(object.id(), "image DecodeParms dictionary"))?;
    if codec == FilterKind::Jbig2 {
        if let Some(globals) = resolver.resolve_dict_key(parameters, b"JBIG2Globals")? {
            if !globals.get().is_null() {
                let globals_object = globals.object();
                if !globals_object.is_stream()
                    || (object.id().is_some() && object.id() == globals_object.id())
                {
                    return Err(wrong(object.id(), "JBIG2Globals stream"));
                }
                let decoded = decode(resolver, globals_object, budget)?;
                if decoded.image.is_some() {
                    return Err(DocError::Unsupported { feature: "JBIG2Globals image codec" });
                }
                if decoded.truncated {
                    warn(resolver, globals_object, "image_jbig2_globals_truncated");
                }
                params.globals = Some(decoded.bytes);
            }
        }
        return Ok(params);
    }
    params.k = i32::try_from(
        image_integer(resolver, parameters, b"K", 0, object.id())?,
    ).map_err(|_| wrong(object.id(), "CCITT K integer"))?;
    params.columns = u32::try_from(
        image_integer(resolver, parameters, b"Columns", 1728, object.id())?,
    ).map_err(|_| wrong(object.id(), "CCITT Columns integer"))?;
    params.rows = u32::try_from(
        image_integer(resolver, parameters, b"Rows", 0, object.id())?,
    ).map_err(|_| wrong(object.id(), "CCITT Rows integer"))?;
    params.damaged_rows_before_error = u32::try_from(
        image_integer(
            resolver, parameters, b"DamagedRowsBeforeError", 0, object.id(),
        )?,
    ).map_err(|_| wrong(object.id(), "CCITT damaged row count"))?;
    params.black_is_1 = image_boolean(
        resolver, parameters, b"BlackIs1", false, object.id(),
    )?;
    params.encoded_byte_align = image_boolean(
        resolver, parameters, b"EncodedByteAlign", false, object.id(),
    )?;
    params.end_of_line = image_boolean(
        resolver, parameters, b"EndOfLine", false, object.id(),
    )?;
    params.end_of_block = image_boolean(
        resolver, parameters, b"EndOfBlock", true, object.id(),
    )?;
    Ok(params)
}

fn image_integer(
    resolver: &mut Resolver<'_>,
    dict: DictRef<'_>,
    key: &[u8],
    default: i64,
    id: Option<ObjectRef>,
) -> Result<i64, DocError> {
    match resolver.resolve_dict_key(dict, key)? {
        None => Ok(default),
        Some(value) if value.get().is_null() => Ok(default),
        Some(value) => value.get().as_i64()
            .ok_or_else(|| wrong(id, "integer image decode parameter")),
    }
}

fn image_boolean(
    resolver: &mut Resolver<'_>,
    dict: DictRef<'_>,
    key: &[u8],
    default: bool,
    id: Option<ObjectRef>,
) -> Result<bool, DocError> {
    match resolver.resolve_dict_key(dict, key)? {
        None => Ok(default),
        Some(value) if value.get().is_null() => Ok(default),
        Some(value) => value.get().as_bool()
            .ok_or_else(|| wrong(id, "boolean image decode parameter")),
    }
}

fn dimension(
    resolver: &mut Resolver<'_>,
    dict: DictRef<'_>,
    key: &[u8],
    id: Option<ObjectRef>,
) -> Result<u32, DocError> {
    resolver.resolve_dict_key(dict, key)?
        .and_then(|value| value.get().as_i64())
        .and_then(|number| u32::try_from(number).ok())
        .filter(|number| *number != 0)
        .ok_or_else(|| wrong(id, "positive image dimension"))
}

fn read_color(
    resolver: &mut Resolver<'_>,
    dict: DictRef<'_>,
) -> Result<ColorSpec, DocError> {
    let Some(value) = resolver.resolve_dict_key(dict, b"ColorSpace")? else {
        return Ok(ColorSpec::simple(ColorSpace::Unsupported));
    };
    let Some(array) = value.get().as_array() else {
        return simple_space(resolver, value.get()).map(ColorSpec::simple);
    };
    let Some(first) = array.get(0) else {
        return Ok(ColorSpec::simple(ColorSpace::Unsupported));
    };
    let first = resolver.resolve(first)?;
    let indexed = first.get().as_name()
        .map(|name| name.eq_bytes(b"Indexed") || name.eq_bytes(b"I"))
        .unwrap_or(false);
    if !indexed {
        return simple_space(resolver, value.get()).map(ColorSpec::simple);
    }
    if array.len() != 4 {
        return Err(wrong(value.object().id(), "Indexed color space"));
    }

    let base_value = array.get(1)
        .ok_or_else(|| wrong(value.object().id(), "Indexed base"))?;
    let base = match simple_space(resolver, base_value)? {
        ColorSpace::Gray => IndexedBase::Gray,
        ColorSpace::Rgb => IndexedBase::Rgb,
        ColorSpace::Cmyk => IndexedBase::Cmyk,
        _ => return Ok(ColorSpec::simple(ColorSpace::Unsupported)),
    };
    let hival = resolver.resolve(array.get(2)
        .ok_or_else(|| wrong(value.object().id(), "Indexed hival"))?)?;
    let hival = hival.get().as_i64()
        .and_then(|number| u8::try_from(number).ok())
        .ok_or_else(|| wrong(value.object().id(), "Indexed hival in 0..255"))?;
    let lookup = array.get(3)
        .ok_or_else(|| wrong(value.object().id(), "Indexed lookup"))?
        .to_owned();
    Ok(ColorSpec {
        space: ColorSpace::Indexed { base, hival },
        lookup: Some(lookup),
    })
}

/// Indexed bases cannot themselves be Indexed. Parsing is nonrecursive,
/// including when the base or ICC profile is indirect.
fn simple_space(
    resolver: &mut Resolver<'_>,
    value: ValueRef<'_>,
) -> Result<ColorSpace, DocError> {
    let value = resolver.resolve(value)?;
    if let Some(name) = value.get().as_name() {
        return Ok(named_space(name));
    }
    let Some(array) = value.get().as_array() else {
        return Ok(ColorSpace::Unsupported);
    };
    let Some(first) = array.get(0) else {
        return Ok(ColorSpace::Unsupported);
    };
    let first = resolver.resolve(first)?;
    let Some(name) = first.get().as_name() else {
        return Ok(ColorSpace::Unsupported);
    };
    if !name.eq_bytes(b"ICCBased") {
        return Ok(named_space(name));
    }

    let profile = resolver.resolve(array.get(1)
        .ok_or_else(|| wrong(value.object().id(), "ICCBased profile"))?)?;
    if !profile.object().is_stream() {
        return Err(wrong(profile.object().id(), "ICC profile stream"));
    }
    let dict = profile.object().dict()
        .ok_or_else(|| wrong(profile.object().id(), "ICC profile dictionary"))?;
    let count = resolver.resolve_dict_key(dict, b"N")?
        .and_then(|value| value.get().as_i64());
    Ok(match count {
        Some(1) => ColorSpace::Gray,
        Some(3) => ColorSpace::Rgb,
        Some(4) => ColorSpace::Cmyk,
        _ => ColorSpace::Unsupported,
    })
}

fn named_space(name: pdf_core::Name<'_>) -> ColorSpace {
    if name.eq_bytes(b"DeviceGray") || name.eq_bytes(b"G") || name.eq_bytes(b"CalGray") {
        ColorSpace::Gray
    } else if name.eq_bytes(b"DeviceRGB") || name.eq_bytes(b"RGB") || name.eq_bytes(b"CalRGB") {
        ColorSpace::Rgb
    } else if name.eq_bytes(b"DeviceCMYK") || name.eq_bytes(b"CMYK") {
        ColorSpace::Cmyk
    } else {
        ColorSpace::Unsupported
    }
}

fn channels(space: ColorSpace) -> usize {
    match space {
        ColorSpace::Gray | ColorSpace::Indexed { .. } => 1,
        ColorSpace::Rgb => 3,
        ColorSpace::Cmyk => 4,
        ColorSpace::Unsupported => 0,
    }
}

fn base_space(base: IndexedBase) -> ColorSpace {
    match base {
        IndexedBase::Gray => ColorSpace::Gray,
        IndexedBase::Rgb => ColorSpace::Rgb,
        IndexedBase::Cmyk => ColorSpace::Cmyk,
    }
}

#[derive(Clone, Copy)]
struct DecodePair {
    low: f64,
    high: f64,
}

impl DecodePair {
    fn map(self, fraction: f64) -> f64 {
        // A weighted sum avoids overflowing high - low for finite endpoints.
        self.low * (1.0 - fraction) + self.high * fraction
    }

    fn byte(self, sample: u16, bpc: u8) -> u8 {
        let fraction = if bpc == 16 {
            // The pixel API intentionally keeps the high byte.
            f64::from(sample >> 8) / 255.0
        } else {
            f64::from(sample) / f64::from(sample_max(bpc))
        };
        (self.map(fraction).clamp(0.0, 1.0) * 255.0 + 0.5) as u8
    }
}

fn sample_max(bpc: u8) -> u16 {
    match bpc {
        1 => 1,
        2 => 3,
        4 => 15,
        8 => 255,
        16 => 65_535,
        _ => 0,
    }
}

fn read_decode(
    resolver: &mut Resolver<'_>,
    dict: DictRef<'_>,
    channels: usize,
    space: ColorSpace,
    bpc: u8,
    id: Option<ObjectRef>,
) -> Result<[DecodePair; 4], DocError> {
    let mut pairs = [DecodePair { low: 0.0, high: 1.0 }; 4];
    if matches!(space, ColorSpace::Indexed { .. }) {
        if let Some(pair) = pairs.first_mut() {
            pair.high = f64::from(sample_max(bpc));
        }
    }
    let Some(value) = resolver.resolve_dict_key(dict, b"Decode")? else {
        return Ok(pairs);
    };
    if value.get().is_null() {
        return Ok(pairs);
    }
    let array = value.get().as_array().ok_or_else(|| wrong(id, "image Decode array"))?;
    let count = channels.checked_mul(2)
        .ok_or_else(|| wrong(id, "image Decode components"))?;
    if array.len() != count {
        return Err(wrong(id, "image Decode component pairs"));
    }
    let mut values = array.iter();
    for pair in pairs.iter_mut().take(channels) {
        let low = resolver.resolve(values.next()
            .ok_or_else(|| wrong(id, "image Decode number"))?)?;
        let high = resolver.resolve(values.next()
            .ok_or_else(|| wrong(id, "image Decode number"))?)?;
        pair.low = low.get().as_f64().filter(|number| number.is_finite())
            .ok_or_else(|| wrong(id, "finite image Decode number"))?;
        pair.high = high.get().as_f64().filter(|number| number.is_finite())
            .ok_or_else(|| wrong(id, "finite image Decode number"))?;
    }
    Ok(pairs)
}

fn read_lookup(
    resolver: &mut Resolver<'_>,
    value: &Value,
    space: ColorSpace,
    budget: &mut Budget,
) -> Result<Vec<u8>, DocError> {
    let ColorSpace::Indexed { base, hival } = space else {
        return Err(wrong(value.object().id(), "Indexed color space"));
    };
    let expected = usize::from(hival).checked_add(1)
        .and_then(|count| count.checked_mul(channels(base_space(base))))
        .ok_or_else(|| size_error(resolver.document().limits().max_decoded_bytes))?;
    let value = resolver.resolve(value.get())?;
    let maximum = resolver.document().limits().max_decoded_bytes;

    if let Some(string) = value.get().as_str() {
        // Escaped strings use the existing bounded, fallible string decoder.
        // Account its worst-case storage before allocating it.
        check(LimitKind::DecodedBytes, string.raw().len() as u64, maximum)?;
        budget.charge(string.raw().len())?;
        let mut storage = Vec::new();
        let bytes = decode_string(string.raw(), &mut storage, maximum)?;
        let count = bytes.len().min(expected);
        budget.charge(count)?;
        let mut palette = zeroed(count, maximum)?;
        palette.copy_from_slice(bytes.get(..count).unwrap_or(&[]));
        return Ok(palette);
    }

    if !value.object().is_stream() {
        return Err(wrong(value.object().id(), "Indexed lookup string or stream"));
    }
    let mut decoded = decode(resolver, value.object(), budget)?;
    if decoded.image.is_some() {
        return Err(DocError::Unsupported { feature: "image palette codec" });
    }
    if decoded.truncated {
        warn(resolver, value.object(), "image_truncated");
    }
    decoded.bytes.truncate(expected);
    Ok(decoded.bytes)
}

struct SampleLayout {
    stride: usize,
    row_rgba: usize,
    rgba: usize,
    expected: usize,
}

impl SampleLayout {
    fn new(
        width: u32,
        height: u32,
        channels: usize,
        bpc: u8,
        maximum: u64,
    ) -> Result<Self, DocError> {
        let width = usize::try_from(width).map_err(|_| size_error(maximum))?;
        let height = usize::try_from(height).map_err(|_| size_error(maximum))?;
        let stride = width.checked_mul(channels)
            .and_then(|samples| samples.checked_mul(usize::from(bpc)))
            .and_then(|bits| bits.checked_add(7))
            .map(|bits| bits / 8)
            .ok_or_else(|| size_error(maximum))?;
        let row_rgba = width.checked_mul(4).ok_or_else(|| size_error(maximum))?;
        let rgba = row_rgba.checked_mul(height).ok_or_else(|| size_error(maximum))?;
        let expected = stride.checked_mul(height).ok_or_else(|| size_error(maximum))?;
        check(
            LimitKind::DecodedBytes,
            rgba as u64,
            maximum.min(isize::MAX as u64),
        )?;
        check(LimitKind::DecodedBytes, expected as u64, maximum)?;
        Ok(Self { stride, row_rgba, rgba, expected })
    }
}

struct Samples<'a> {
    bytes: &'a [u8],
    bit: usize,
    bpc: u8,
}

impl Samples<'_> {
    fn next(&mut self) -> Option<u16> {
        let offset = self.bit / 8;
        let first = self.bytes.get(offset).copied()?;
        let value = match self.bpc {
            16 => {
                let second = self.bytes.get(offset.checked_add(1)?).copied()?;
                u16::from_be_bytes([first, second])
            }
            8 => u16::from(first),
            1 | 2 | 4 => {
                let shift = 8u8.saturating_sub(self.bpc)
                    .saturating_sub((self.bit % 8) as u8);
                u16::from(first >> shift) & sample_max(self.bpc)
            }
            _ => return None,
        };
        self.bit = self.bit.checked_add(usize::from(self.bpc))?;
        Some(value)
    }
}

fn pixel_rgba(
    space: ColorSpace,
    image_mask: bool,
    bpc: u8,
    raw: [u16; 4],
    pairs: &[DecodePair; 4],
    palette: &[u8],
) -> Option<[u8; 4]> {
    let [first, _, _, _] = raw;
    let pair = pairs.first().copied()?;
    if image_mask {
        return Some([0, 0, 0, 255u8.saturating_sub(pair.byte(first, bpc))]);
    }
    if let ColorSpace::Indexed { base, hival } = space {
        let fraction = f64::from(first) / f64::from(sample_max(bpc));
        let index = (pair.map(fraction).clamp(0.0, f64::from(hival)) + 0.5) as usize;
        let space = base_space(base);
        let count = channels(space);
        let start = index.checked_mul(count)?;
        let end = start.checked_add(count)?;
        let [r, g, b] = rgb(space, palette.get(start..end)?)?;
        return Some([r, g, b, 255]);
    }

    let mut values = [0u8; 4];
    for (slot, (sample, pair)) in values.iter_mut().zip(raw.iter().zip(pairs.iter()))
        .take(channels(space))
    {
        *slot = pair.byte(*sample, bpc);
    }
    let [r, g, b] = rgb(space, &values)?;
    Some([r, g, b, 255])
}

fn rgb(space: ColorSpace, values: &[u8]) -> Option<[u8; 3]> {
    match (space, values) {
        (ColorSpace::Gray, [gray, ..]) => Some([*gray, *gray, *gray]),
        (ColorSpace::Rgb, [r, g, b, ..]) => Some([*r, *g, *b]),
        (ColorSpace::Cmyk, [c, m, y, k, ..]) => {
            let black = 255u16.saturating_sub(u16::from(*k));
            let component = |value: u8| {
                (255u16.saturating_sub(u16::from(value))
                    .saturating_mul(black) / 255) as u8
            };
            Some([component(*c), component(*m), component(*y)])
        }
        _ => None,
    }
}

fn apply_soft_mask(
    resolver: &mut Resolver<'_>,
    object: &Object,
    image: &mut Image,
    value: &Value,
    max_pixels: u64,
    budget: &mut Budget,
) -> Result<(), DocError> {
    let value = resolver.resolve(value.get())?;
    let mask_object = value.object();
    if object.id().is_some() && object.id() == mask_object.id() {
        warn(resolver, object, "image_soft_mask_cycle");
        return Ok(());
    }
    let (mask, _) = read_image(resolver, mask_object, max_pixels, false, budget)?;
    if mask.color_space != ColorSpace::Gray || mask.image_mask {
        return Err(DocError::Unsupported { feature: "non-gray soft mask" });
    }
    let ImageData::Pixels { rgba: mask_rgba } = mask.data else {
        return Err(DocError::Unsupported { feature: "soft mask codec" });
    };
    let ImageData::Pixels { rgba } = &mut image.data else {
        return Ok(());
    };

    let maximum = resolver.document().limits().max_decoded_bytes;
    let width = usize::try_from(image.width).map_err(|_| size_error(maximum))?;
    let count = rgba.len() / 4;
    budget.charge(count)?;
    let mut alpha = zeroed(count, maximum)?;

    for (y, row) in alpha.chunks_exact_mut(width).enumerate() {
        let source_y = (y as u64).saturating_mul(u64::from(mask.height))
            / u64::from(image.height);
        for (x, slot) in row.iter_mut().enumerate() {
            let source_x = (x as u64).saturating_mul(u64::from(mask.width))
                / u64::from(image.width);
            let offset = source_y.checked_mul(u64::from(mask.width))
                .and_then(|row| row.checked_add(source_x))
                .and_then(|pixel| pixel.checked_mul(4))
                .and_then(|offset| usize::try_from(offset).ok())
                .ok_or_else(|| size_error(maximum))?;
            let end = offset.checked_add(4).ok_or_else(|| size_error(maximum))?;
            *slot = match mask_rgba.get(offset..end) {
                Some([gray, _, _, valid]) =>
                    (u16::from(*gray).saturating_mul(u16::from(*valid)) / 255) as u8,
                _ => 0,
            };
        }
    }

    for (pixel, soft) in rgba.chunks_exact_mut(4).zip(alpha.iter()) {
        if let Some(current) = pixel.get_mut(3) {
            *current = (u16::from(*current).saturating_mul(u16::from(*soft)) / 255) as u8;
        }
    }
    image.soft_mask = Some(alpha);
    Ok(())
}

fn decode(
    resolver: &mut Resolver<'_>,
    object: &Object,
    budget: &mut Budget,
) -> Result<Decoded, DocError> {
    let decoded = resolver.decode_stream(object)?;
    budget.charge(decoded.bytes.len())?;
    Ok(decoded)
}

fn zeroed(size: usize, maximum: u64) -> Result<Vec<u8>, DocError> {
    let maximum = maximum.min(isize::MAX as u64);
    check(LimitKind::DecodedBytes, size as u64, maximum)?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(size).map_err(|_| size_error(maximum))?;
    bytes.resize(size, 0);
    Ok(bytes)
}

fn size_error(maximum: u64) -> DocError {
    refusal(LimitKind::DecodedBytes, maximum.saturating_add(1), maximum)
}

fn wrong(id: Option<ObjectRef>, expected: &'static str) -> DocError {
    DocError::WrongType { id, expected }
}

fn recover(
    resolver: &mut Resolver<'_>,
    object: &Object,
    error: DocError,
    what: &'static str,
) -> Result<(), DocError> {
    match error {
        error @ (DocError::Encrypted | DocError::Refused(_)) => Err(error),
        _ => {
            warn(resolver, object, what);
            Ok(())
        }
    }
}

fn warn(resolver: &mut Resolver<'_>, object: &Object, what: &'static str) {
    resolver.warn(Warning {
        offset: object.stream_range().map(|range| range.start as u64),
        object: object.id(),
        what,
    });
}
