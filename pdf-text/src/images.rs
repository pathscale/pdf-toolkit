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
        Self {
            space,
            lookup: None,
        }
    }
}

pub struct ImageDecodeBudget {
    used: u64,
    maximum: u64,
}

impl ImageDecodeBudget {
    pub fn new(maximum: u64) -> Self {
        Self { used: 0, maximum }
    }

    pub fn remaining(&self) -> u64 {
        self.maximum.saturating_sub(self.used)
    }

    pub fn charge(&mut self, bytes: usize) -> Result<(), DocError> {
        let requested = self
            .used
            .checked_add(bytes as u64)
            .ok_or_else(|| refusal(LimitKind::TotalDecodedBytes, u64::MAX, self.maximum))?;
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
    let mut budget = ImageDecodeBudget::new(resolver.document().limits().max_total_decoded_bytes);
    load_image_with_budget(resolver, id, max_pixels, &mut budget)
}

/// Loads an image XObject using the supplied cumulative decode budget.
pub fn load_image_with_budget(
    resolver: &mut Resolver<'_>,
    id: ObjectRef,
    max_pixels: u64,
    budget: &mut ImageDecodeBudget,
) -> Result<Image, DocError> {
    if !resolver.document().decryptable() {
        return Err(DocError::Encrypted);
    }

    let object = resolver.get(id)?;
    let value = resolver.resolve(object.root())?;
    let object = value.object().clone();
    let (mut image, soft) = read_image(resolver, &object, max_pixels, true, budget)?;

    if matches!(&image.data, ImageData::Pixels { .. }) {
        if let Some(soft) = soft {
            if let Err(error) =
                apply_soft_mask(resolver, &object, &mut image, &soft, max_pixels, budget)
            {
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
    budget: &mut ImageDecodeBudget,
) -> Result<(Image, Option<Value>), DocError> {
    if !object.is_stream() {
        return Err(wrong(object.id(), "image stream"));
    }
    let dict = object
        .dict()
        .ok_or_else(|| wrong(object.id(), "image dictionary"))?;
    let subtype = resolver.resolve_dict_key(dict, b"Subtype")?;
    if !subtype
        .as_ref()
        .and_then(|value| value.get().as_name())
        .map(|name| name.eq_bytes(b"Image"))
        .unwrap_or(false)
    {
        return Err(wrong(object.id(), "Image XObject"));
    }

    let width = dimension(resolver, dict, b"Width", object.id())?;
    let height = dimension(resolver, dict, b"Height", object.id())?;
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| {
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
        Some(value) => value
            .get()
            .as_bool()
            .ok_or_else(|| wrong(object.id(), "ImageMask boolean"))?,
    };
    let decoded_result = decode(resolver, object, budget);
    // JPX and JXL carry their own precision. Do not resolve the PDF entry.
    let mut bits_per_component = if matches!(
        &decoded_result,
        Ok(decoded) if matches!(decoded.image, Some(FilterKind::Jpx | FilterKind::Jxl))
    ) {
        0
    } else {
        resolver
            .resolve_dict_key(dict, b"BitsPerComponent")?
            .and_then(|value| value.get().as_i64())
            .and_then(|number| u8::try_from(number).ok())
            .unwrap_or(if image_mask { 1 } else { 0 })
    };

    let mut color = if image_mask {
        ColorSpec::simple(ColorSpace::Gray)
    } else if matches!(
        &decoded_result,
        Ok(decoded) if decoded.image == Some(FilterKind::Jxl)
    ) {
        // JXL's header owns its colour encoding, including when the feature
        // is disabled. Do not follow the PDF ColorSpace entry.
        ColorSpec::simple(ColorSpace::Unsupported)
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
                Ok(value)
                    if value.get().is_null()
                        || value
                            .get()
                            .as_name()
                            .map(|name| name.eq_bytes(b"None"))
                            .unwrap_or(false) =>
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
    let mut decoded = match decoded_result {
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
                let params = image_params(resolver, object, dict, codec, width, height, budget)?;
                let mut binary = decode_image_report(codec, &decoded.bytes, &params, max_pixels)
                    .map_err(DocError::from)?;
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
                    image.data = ImageData::Unsupported(if codec == FilterKind::Jbig2 {
                        "jbig2"
                    } else {
                        "ccitt"
                    });
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
            #[cfg(feature = "jpx")]
            if codec == FilterKind::Jpx {
                let result = decode_jpx(
                    resolver,
                    object,
                    dict,
                    &decoded.bytes,
                    &color,
                    &mut image,
                    budget,
                );
                match result {
                    Ok(true) => return Ok((image, None)),
                    Ok(false) => return Ok((image, soft)),
                    Err(error) => {
                        let reason = match &error {
                            DocError::Unsupported {
                                feature: "jpx_components",
                            } => "jpx_components",
                            _ => "jpx",
                        };
                        recover(resolver, object, error, "jpx_decode")?;
                        image.data = ImageData::Unsupported(reason);
                        return Ok((image, soft));
                    }
                }
            }
            #[cfg(feature = "jxl")]
            if codec == FilterKind::Jxl {
                if let Err(error) = decode_jxl(
                    resolver,
                    object,
                    dict,
                    &decoded.bytes,
                    &mut image,
                    max_pixels,
                    budget,
                ) {
                    recover(resolver, object, error, "jxl_decode")?;
                    image.data = ImageData::Unsupported("jxl");
                }
                return Ok((image, soft));
            }
            image.data = match codec {
                FilterKind::Dct => ImageData::Jpeg(decoded.bytes),
                FilterKind::Jpx => ImageData::Unsupported("jpx"),
                FilterKind::Jxl => ImageData::Unsupported("jxl"),
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
    if !matches!(bits_per_component, 1 | 2 | 4 | 8 | 16) || (image_mask && bits_per_component != 1)
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
        let start = y
            .checked_mul(layout.stride)
            .ok_or_else(|| size_error(maximum))?;
        let end = start
            .checked_add(layout.stride)
            .ok_or_else(|| size_error(maximum))?;
        let row = decoded
            .bytes
            .get(start..end.min(decoded.bytes.len()))
            .unwrap_or(&[]);
        let mut samples = Samples {
            bytes: row,
            bit: 0,
            bpc: bits_per_component,
        };

        for pixel in output.as_chunks_mut::<4>().0 {
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

#[cfg(feature = "jxl")]
fn decode_jxl(
    resolver: &mut Resolver<'_>,
    object: &Object,
    dict: DictRef<'_>,
    bytes: &[u8],
    image: &mut Image,
    max_pixels: u64,
    budget: &mut ImageDecodeBudget,
) -> Result<(), DocError> {
    use jxl_oxide::{EnumColourEncoding, JxlImage, NullCms, RenderingIntent};

    if image.image_mask {
        return Err(DocError::Unsupported { feature: "jxl" });
    }
    // The decoder shares its input as Arc<[u8]> spans; this is the one copy of the codestream.
    let mut source = JxlImage::builder()
        .read_bytes(alloc::sync::Arc::from(bytes))
        .map_err(|_| jxl_decode_error())?;

    let width = source.width();
    let height = source.height();
    let maximum = resolver
        .document()
        .limits()
        .max_decoded_bytes
        .min(isize::MAX as u64);
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| size_error(maximum))?;
    if pixels > max_pixels {
        return Err(refusal(
            LimitKind::DecodedBytes,
            pixels.saturating_mul(4),
            max_pixels.saturating_mul(4),
        ));
    }
    if width != image.width || height != image.height {
        return Err(wrong(
            object.id(),
            "JXL dimensions matching image dictionary",
        ));
    }
    let metadata = &source.image_header().metadata;
    let space = if metadata.grayscale() {
        ColorSpace::Gray
    } else {
        ColorSpace::Rgb
    };
    let count = channels(space);
    let has_alpha = source.pixel_format().has_alpha();
    let associated_alpha = metadata
        .ec_info
        .iter()
        .find_map(|info| info.alpha_associated())
        .unwrap_or(false);
    // Include every extra plane, even those omitted from the output stream.
    // XYB decoding can use three working colour planes for a gray image.
    let plane_count = count
        .max(3)
        .checked_add(metadata.ec_info.len())
        .ok_or_else(|| size_error(maximum))?;
    let plane_bytes = pixels
        .checked_mul(plane_count as u64)
        .and_then(|size| size.checked_mul(4))
        .ok_or_else(|| size_error(maximum))?;
    check(LimitKind::DecodedBytes, plane_bytes, maximum)?;
    budget.charge(usize::try_from(plane_bytes).map_err(|_| size_error(maximum))?)?;
    let layout = SampleLayout::new(width, height, count, 8, maximum)?;
    budget.charge(layout.rgba)?;
    let has_pdf_soft = resolver
        .resolve_dict_key(dict, b"SMask")?
        .map(|value| {
            !value.get().is_null()
                && !value
                    .get()
                    .as_name()
                    .map(|name| name.eq_bytes(b"None"))
                    .unwrap_or(false)
        })
        .unwrap_or(false);
    let use_alpha = has_alpha && !has_pdf_soft;
    let alpha_size = usize::try_from(pixels).map_err(|_| size_error(maximum))?;
    if use_alpha {
        budget.charge(alpha_size)?;
    }

    source.set_cms(NullCms);
    source.request_color_encoding(if space == ColorSpace::Gray {
        EnumColourEncoding::gray_srgb(RenderingIntent::Relative)
    } else {
        EnumColourEncoding::srgb(RenderingIntent::Relative)
    });
    if source.num_loaded_keyframes() == 0 {
        return Err(jxl_decode_error());
    }
    let rendered = source.render_frame(0).map_err(|_| jxl_decode_error())?;
    let mut stream = rendered.stream();
    let stream_count = count
        .checked_add(usize::from(has_alpha))
        .ok_or_else(jxl_decode_error)?;
    if stream.width() != width
        || stream.height() != height
        || usize::try_from(stream.channels()).ok() != Some(stream_count)
        || stream_count > 4
    {
        return Err(jxl_decode_error());
    }
    let mut rgba = zeroed(layout.rgba, maximum)?;
    let mut soft_mask = if use_alpha {
        Some(zeroed(alpha_size, maximum)?)
    } else {
        None
    };
    for (index, pixel) in rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let mut samples = [0u8; 4];
        let output = samples
            .get_mut(..stream_count)
            .ok_or_else(jxl_decode_error)?;
        if stream.write_to_buffer(output) != stream_count {
            return Err(jxl_decode_error());
        }
        let alpha = if has_alpha {
            samples.get(count).copied().ok_or_else(jxl_decode_error)?
        } else {
            255
        };
        let mut values = [0u8; 4];
        for (slot, sample) in values.iter_mut().zip(samples.iter()).take(count) {
            // The pixel API stores straight alpha, including when a PDF
            // soft mask replaces the codestream's opacity.
            *slot = if associated_alpha {
                if alpha == 0 {
                    0
                } else {
                    ((u32::from(*sample) * 255 + u32::from(alpha) / 2) / u32::from(alpha)).min(255)
                        as u8
                }
            } else {
                *sample
            };
        }
        let [r, g, b] = rgb(space, &values).ok_or_else(jxl_decode_error)?;
        let opacity = if use_alpha { alpha } else { 255 };
        pixel.copy_from_slice(&[r, g, b, opacity]);
        if let Some(mask) = &mut soft_mask {
            *mask.get_mut(index).ok_or_else(jxl_decode_error)? = alpha;
        }
    }
    image.color_space = space;
    image.bits_per_component = 8;
    image.has_soft_mask |= use_alpha;
    image.soft_mask = soft_mask;
    image.data = ImageData::Pixels { rgba };
    Ok(())
}

#[cfg(feature = "jxl")]
fn jxl_decode_error() -> DocError {
    DocError::Malformed {
        offset: 0,
        what: "jxl_decode",
    }
}

#[cfg(feature = "jpx")]
fn decode_jpx(
    resolver: &mut Resolver<'_>,
    object: &Object,
    dict: DictRef<'_>,
    bytes: &[u8],
    color: &ColorSpec,
    image: &mut Image,
    budget: &mut ImageDecodeBudget,
) -> Result<bool, DocError> {
    use hayro_jpeg2000::{DecodeSettings, DecoderContext};

    let header = JpxHeader::read(bytes)?;
    let has_pdf_color = resolver
        .resolve_dict_key(dict, b"ColorSpace")?
        .map(|value| !value.get().is_null())
        .unwrap_or(false);
    let settings = DecodeSettings {
        resolve_palette_indices: !matches!(color.space, ColorSpace::Indexed { .. }),
        ..DecodeSettings::default()
    };
    let source = hayro_jpeg2000::Image::new(bytes, &settings).map_err(jpx_error)?;
    let width = source.width();
    let height = source.height();
    let space = if has_pdf_color {
        color.space
    } else {
        match source.color_space().num_channels() {
            1 => ColorSpace::Gray,
            3 => ColorSpace::Rgb,
            4 => ColorSpace::Cmyk,
            _ => return Err(jpx_components()),
        }
    };
    let count = channels(space);
    // Up to four colour planes, plus a separate opacity plane.
    if count == 0 || (image.image_mask && count != 1) {
        return Err(jpx_components());
    }
    let stream_count = usize::from(source.color_space().num_channels())
        .checked_add(usize::from(source.has_alpha()))
        .ok_or_else(jpx_components)?;
    if !(1..=5).contains(&stream_count) {
        return Err(jpx_components());
    }

    let maximum = resolver
        .document()
        .limits()
        .max_decoded_bytes
        .min(isize::MAX as u64);
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| size_error(maximum))?;
    // The public decoder hides the raw component count. SIZ supplies it,
    // including planes that a palette or channel definition might replace.
    let plane_bytes = pixels
        .checked_mul(header.count.max(stream_count) as u64)
        .and_then(|size| size.checked_mul(4))
        .ok_or_else(|| size_error(maximum))?;
    check(LimitKind::DecodedBytes, plane_bytes, maximum)?;
    budget.charge(usize::try_from(plane_bytes).map_err(|_| size_error(maximum))?)?;
    if width != image.width || height != image.height {
        return Err(wrong(
            object.id(),
            "JPX dimensions matching image dictionary",
        ));
    }
    let layout = SampleLayout::new(width, height, count, 8, maximum)?;
    budget.charge(layout.rgba)?;

    let in_data = image_integer(resolver, dict, b"SMaskInData", 0, object.id())?;
    if !(0..=2).contains(&in_data) {
        return Err(wrong(object.id(), "SMaskInData in 0..2"));
    }
    let has_pdf_soft = resolver
        .resolve_dict_key(dict, b"SMask")?
        .map(|value| {
            !value.get().is_null()
                && !value
                    .get()
                    .as_name()
                    .map(|name| name.eq_bytes(b"None"))
                    .unwrap_or(false)
        })
        .unwrap_or(false);
    let use_stream_alpha = in_data != 0 || !has_pdf_soft;
    let alpha_size = usize::try_from(pixels).map_err(|_| size_error(maximum))?;

    let mut context = DecoderContext::default();
    let decoded = source.decode(&mut context).map_err(jpx_error)?;
    let components = decoded.components();
    if !(1..=5).contains(&components.len()) {
        return Err(jpx_components());
    }
    // An explicit PDF space can consume all four planes as CMYK even when
    // the raw codestream was guessed to be RGB with opacity.
    let alpha_index = if components.len() == count {
        None
    } else if source.has_alpha() && components.len() == count.saturating_add(1) {
        Some(count)
    } else {
        return Err(jpx_components());
    };
    if in_data != 0 && alpha_index.is_none() {
        return Err(jpx_components());
    }
    let use_alpha = use_stream_alpha && alpha_index.is_some();
    if use_alpha {
        budget.charge(alpha_size)?;
    }
    let mut planes = [None; 5];
    for (index, component) in components.iter().enumerate() {
        let geometry = header
            .geometry
            .get(index)
            .copied()
            .ok_or_else(jpx_components)?;
        let plane = JpxPlane::new(component, width, height, geometry)?;
        *planes.get_mut(index).ok_or_else(jpx_components)? = Some(plane);
    }
    let first = planes
        .first()
        .and_then(|plane| *plane)
        .ok_or_else(jpx_components)?;
    let depth = components.first().ok_or_else(jpx_components)?.bit_depth();
    let mut pairs = if has_pdf_color {
        read_decode(resolver, dict, count, space, 8, object.id())?
    } else {
        [DecodePair {
            low: 0.0,
            high: 1.0,
        }; 4]
    };
    if matches!(space, ColorSpace::Indexed { .. }) {
        let explicit_decode = resolver
            .resolve_dict_key(dict, b"Decode")?
            .map(|value| !value.get().is_null())
            .unwrap_or(false);
        if !explicit_decode {
            pairs.first_mut().ok_or_else(jpx_components)?.high = first.maximum;
        }
    }
    let palette = match color.lookup.as_ref() {
        Some(value) => read_lookup(resolver, value, space, budget)?,
        None => Vec::new(),
    };
    let mut rgba = zeroed(layout.rgba, maximum)?;
    let mut soft_mask = if use_alpha {
        Some(zeroed(alpha_size, maximum)?)
    } else {
        None
    };
    for (y, row) in rgba.chunks_exact_mut(layout.row_rgba).enumerate() {
        for (x, pixel) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let opacity = if use_alpha {
                let index = alpha_index.ok_or_else(jpx_components)?;
                planes
                    .get(index)
                    .and_then(|plane| *plane)
                    .ok_or_else(jpx_components)?
                    .fraction(x, y, width, height)?
            } else {
                1.0
            };
            let mut fractions = [0.0; 4];
            for (slot, plane) in fractions.iter_mut().zip(planes.iter()).take(count) {
                let fraction = plane
                    .ok_or_else(jpx_components)?
                    .fraction(x, y, width, height)?;
                // SMaskInData 2 supplies premultiplied colour. The pixel API
                // stores straight alpha, so undo that before applying Decode.
                *slot = if in_data == 2 {
                    if opacity > 0.0 {
                        (fraction / opacity).clamp(0.0, 1.0)
                    } else {
                        0.0
                    }
                } else {
                    fraction
                };
            }
            let first = fractions.first().copied().ok_or_else(jpx_components)?;
            let pair = pairs.first().copied().ok_or_else(jpx_components)?;
            let mut value = if image.image_mask {
                [0, 0, 0, 255u8.saturating_sub(jpx_byte(pair.map(first)))]
            } else if let ColorSpace::Indexed { base, hival } = space {
                let index = (pair.map(first).clamp(0.0, f64::from(hival)) + 0.5) as usize;
                let base = base_space(base);
                let start = index
                    .checked_mul(channels(base))
                    .ok_or_else(jpx_components)?;
                let end = start
                    .checked_add(channels(base))
                    .ok_or_else(jpx_components)?;
                let values = palette
                    .get(start..end)
                    .ok_or_else(|| wrong(object.id(), "JPX Indexed lookup entry"))?;
                let [r, g, b] = rgb(base, values).ok_or_else(jpx_components)?;
                [r, g, b, 255]
            } else {
                let mut values = [0u8; 4];
                for (slot, (fraction, pair)) in values
                    .iter_mut()
                    .zip(fractions.iter().zip(pairs.iter()))
                    .take(count)
                {
                    *slot = jpx_byte(pair.map(*fraction));
                }
                let [r, g, b] = rgb(space, &values).ok_or_else(jpx_components)?;
                [r, g, b, 255]
            };
            if use_alpha {
                let alpha = jpx_byte(opacity);
                let offset = y
                    .checked_mul(usize::try_from(width).map_err(|_| size_error(maximum))?)
                    .and_then(|row| row.checked_add(x))
                    .ok_or_else(|| size_error(maximum))?;
                let slot = soft_mask
                    .as_mut()
                    .and_then(|mask| mask.get_mut(offset))
                    .ok_or_else(jpx_components)?;
                *slot = alpha;
                if let Some(current) = value.get_mut(3) {
                    *current = (u16::from(*current).saturating_mul(u16::from(alpha)) / 255) as u8;
                }
            }
            pixel.copy_from_slice(&value);
        }
    }
    image.color_space = space;
    image.bits_per_component = depth;
    image.has_soft_mask |= use_alpha;
    image.soft_mask = soft_mask;
    image.data = ImageData::Pixels { rgba };
    Ok(in_data != 0)
}

#[cfg(feature = "jpx")]
fn jpx_byte(fraction: f64) -> u8 {
    (fraction.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

#[cfg(feature = "jpx")]
fn jpx_components() -> DocError {
    DocError::Unsupported {
        feature: "jpx_components",
    }
}

#[cfg(feature = "jpx")]
fn jpx_error(error: hayro_jpeg2000::DecodeError) -> DocError {
    match error {
        hayro_jpeg2000::DecodeError::Validation(
            hayro_jpeg2000::ValidationError::TooManyChannels,
        ) => jpx_components(),
        _ => DocError::Malformed {
            offset: 0,
            what: "jpx_decode",
        },
    }
}

#[cfg(feature = "jpx")]
#[derive(Clone, Copy)]
struct JpxPlane<'a> {
    samples: &'a [f32],
    width: u32,
    height: u32,
    maximum: f64,
}

#[cfg(feature = "jpx")]
impl<'a> JpxPlane<'a> {
    fn new(
        component: &'a hayro_jpeg2000::ComponentData,
        width: u32,
        height: u32,
        geometry: (u32, u32),
    ) -> Result<Self, DocError> {
        let samples = component.samples();
        let full_size = u64::from(width)
            .checked_mul(u64::from(height))
            .ok_or_else(jpx_components)?;
        // Hayro normally expands subsampled components itself. If a compact
        // plane is returned, retain its SIZ geometry for nearest sampling.
        let (width, height) = if samples.len() as u64 == full_size {
            (width, height)
        } else {
            geometry
        };
        if width == 0
            || height == 0
            || u64::from(width).checked_mul(u64::from(height)) != Some(samples.len() as u64)
        {
            return Err(jpx_components());
        }
        let depth = component.bit_depth();
        let maximum = 1u32
            .checked_shl(u32::from(depth))
            .and_then(|value| value.checked_sub(1))
            .filter(|value| *value != 0)
            .ok_or_else(jpx_components)?;
        Ok(Self {
            samples,
            width,
            height,
            maximum: f64::from(maximum),
        })
    }

    fn fraction(self, x: usize, y: usize, width: u32, height: u32) -> Result<f64, DocError> {
        let source_x = (x as u64)
            .checked_mul(u64::from(self.width))
            .and_then(|value| value.checked_div(u64::from(width)))
            .ok_or_else(jpx_components)?;
        let source_y = (y as u64)
            .checked_mul(u64::from(self.height))
            .and_then(|value| value.checked_div(u64::from(height)))
            .ok_or_else(jpx_components)?;
        let offset = source_y
            .checked_mul(u64::from(self.width))
            .and_then(|row| row.checked_add(source_x))
            .and_then(|offset| usize::try_from(offset).ok())
            .ok_or_else(jpx_components)?;
        let sample = self
            .samples
            .get(offset)
            .copied()
            .filter(|value| value.is_finite())
            .ok_or_else(jpx_components)?;
        Ok((f64::from(sample) / self.maximum).clamp(0.0, 1.0))
    }
}

#[cfg(feature = "jpx")]
struct JpxHeader {
    count: usize,
    geometry: [(u32, u32); 5],
}

#[cfg(feature = "jpx")]
impl JpxHeader {
    fn read(bytes: &[u8]) -> Result<Self, DocError> {
        let stream = jpx_codestream(bytes)?;
        if !stream.starts_with(b"\xff\x4f\xff\x51") {
            return Err(jpx_error(
                hayro_jpeg2000::FormatError::InvalidSignature.into(),
            ));
        }
        let count = usize::from(jpx_u16(stream, 40)?);
        if !(1..=5).contains(&count) {
            return Err(jpx_components());
        }
        let end = count
            .checked_mul(3)
            .and_then(|size| size.checked_add(42))
            .ok_or_else(jpx_components)?;
        let length = usize::from(jpx_u16(stream, 4)?);
        if length.checked_add(4) != Some(end) || stream.get(..end).is_none() {
            return Err(DocError::Malformed {
                offset: 4,
                what: "jpx_decode",
            });
        }
        let right = jpx_u32(stream, 8)?;
        let bottom = jpx_u32(stream, 12)?;
        let left = jpx_u32(stream, 16)?;
        let top = jpx_u32(stream, 20)?;
        if right <= left || bottom <= top {
            return Err(DocError::Malformed {
                offset: 8,
                what: "jpx_decode",
            });
        }
        let mut geometry = [(0, 0); 5];
        for (slot, entry) in geometry.iter_mut().zip(
            stream
                .get(42..end)
                .ok_or_else(jpx_components)?
                .as_chunks::<3>()
                .0,
        ) {
            let [_, sx, sy] = entry;
            let sx = u32::from(*sx);
            let sy = u32::from(*sy);
            if sx == 0 || sy == 0 {
                return Err(jpx_components());
            }
            let ceil = |value: u32, step: u32| {
                (value / step).saturating_add(u32::from(!value.is_multiple_of(step)))
            };
            *slot = (
                ceil(right, sx).saturating_sub(ceil(left, sx)),
                ceil(bottom, sy).saturating_sub(ceil(top, sy)),
            );
        }
        Ok(Self { count, geometry })
    }
}

#[cfg(feature = "jpx")]
fn jpx_codestream(bytes: &[u8]) -> Result<&[u8], DocError> {
    if bytes.starts_with(b"\xff\x4f\xff\x51") {
        return Ok(bytes);
    }
    if !bytes.starts_with(b"\x00\x00\x00\x0c\x6a\x50\x20\x20") {
        return Err(DocError::Malformed {
            offset: 0,
            what: "jpx_decode",
        });
    }
    let mut remaining = bytes;
    let mut codestream = None;
    while !remaining.is_empty() {
        let size = jpx_u32(remaining, 0)?;
        let kind = remaining.get(4..8).ok_or(DocError::Malformed {
            offset: 0,
            what: "jpx_decode",
        })?;
        let (size, header) = match size {
            0 => (remaining.len(), 8),
            1 => {
                let high = u64::from(jpx_u32(remaining, 8)?);
                let low = u64::from(jpx_u32(remaining, 12)?);
                let size =
                    usize::try_from((high << 32) | low).map_err(|_| DocError::Malformed {
                        offset: 8,
                        what: "jpx_decode",
                    })?;
                (size, 16)
            }
            size => (
                usize::try_from(size).map_err(|_| DocError::Malformed {
                    offset: 0,
                    what: "jpx_decode",
                })?,
                8,
            ),
        };
        let payload = remaining.get(header..size).ok_or(DocError::Malformed {
            offset: 0,
            what: "jpx_decode",
        })?;
        if kind == b"jp2c" {
            codestream = Some(payload);
        }
        remaining = remaining.get(size..).ok_or(DocError::Malformed {
            offset: 0,
            what: "jpx_decode",
        })?;
    }
    codestream.ok_or(DocError::Malformed {
        offset: 0,
        what: "jpx_decode",
    })
}

#[cfg(feature = "jpx")]
fn jpx_u16(bytes: &[u8], offset: usize) -> Result<u16, DocError> {
    let end = offset.checked_add(2).ok_or_else(jpx_components)?;
    match bytes.get(offset..end) {
        Some([a, b]) => Ok(u16::from_be_bytes([*a, *b])),
        _ => Err(DocError::Malformed {
            offset: offset as u64,
            what: "jpx_decode",
        }),
    }
}

#[cfg(feature = "jpx")]
fn jpx_u32(bytes: &[u8], offset: usize) -> Result<u32, DocError> {
    let end = offset.checked_add(4).ok_or_else(jpx_components)?;
    match bytes.get(offset..end) {
        Some([a, b, c, d]) => Ok(u32::from_be_bytes([*a, *b, *c, *d])),
        _ => Err(DocError::Malformed {
            offset: offset as u64,
            what: "jpx_decode",
        }),
    }
}

fn image_params(
    resolver: &mut Resolver<'_>,
    object: &Object,
    dict: DictRef<'_>,
    codec: FilterKind,
    width: u32,
    height: u32,
    budget: &mut ImageDecodeBudget,
) -> Result<ImageParams, DocError> {
    let mut params = ImageParams {
        width,
        height,
        ..ImageParams::default()
    };
    let filters = resolver
        .resolve_dict_key(dict, b"Filter")?
        .ok_or_else(|| wrong(object.id(), "image filter"))?;
    let mut index = None;
    if let Some(array) = filters.get().as_array() {
        for (position, value) in array.iter().enumerate() {
            let value = resolver.resolve(value)?;
            let kind = value
                .get()
                .as_name()
                .and_then(|name| FilterKind::from_name(name.decoded().as_ref()));
            if kind.map(FilterKind::is_image_codec).unwrap_or(false) {
                if kind == Some(codec) {
                    index = Some(position);
                }
                break;
            }
        }
    } else {
        let kind = filters
            .get()
            .as_name()
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
    let parameters = value
        .get()
        .as_dict()
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
                    return Err(DocError::Unsupported {
                        feature: "JBIG2Globals image codec",
                    });
                }
                if decoded.truncated {
                    warn(resolver, globals_object, "image_jbig2_globals_truncated");
                }
                params.globals = Some(decoded.bytes);
            }
        }
        return Ok(params);
    }
    params.k = i32::try_from(image_integer(resolver, parameters, b"K", 0, object.id())?)
        .map_err(|_| wrong(object.id(), "CCITT K integer"))?;
    params.columns = u32::try_from(image_integer(
        resolver,
        parameters,
        b"Columns",
        1728,
        object.id(),
    )?)
    .map_err(|_| wrong(object.id(), "CCITT Columns integer"))?;
    params.rows = u32::try_from(image_integer(
        resolver,
        parameters,
        b"Rows",
        0,
        object.id(),
    )?)
    .map_err(|_| wrong(object.id(), "CCITT Rows integer"))?;
    params.damaged_rows_before_error = u32::try_from(image_integer(
        resolver,
        parameters,
        b"DamagedRowsBeforeError",
        0,
        object.id(),
    )?)
    .map_err(|_| wrong(object.id(), "CCITT damaged row count"))?;
    params.black_is_1 = image_boolean(resolver, parameters, b"BlackIs1", false, object.id())?;
    params.encoded_byte_align = image_boolean(
        resolver,
        parameters,
        b"EncodedByteAlign",
        false,
        object.id(),
    )?;
    params.end_of_line = image_boolean(resolver, parameters, b"EndOfLine", false, object.id())?;
    params.end_of_block = image_boolean(resolver, parameters, b"EndOfBlock", true, object.id())?;
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
        Some(value) => value
            .get()
            .as_i64()
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
        Some(value) => value
            .get()
            .as_bool()
            .ok_or_else(|| wrong(id, "boolean image decode parameter")),
    }
}

fn dimension(
    resolver: &mut Resolver<'_>,
    dict: DictRef<'_>,
    key: &[u8],
    id: Option<ObjectRef>,
) -> Result<u32, DocError> {
    resolver
        .resolve_dict_key(dict, key)?
        .and_then(|value| value.get().as_i64())
        .and_then(|number| u32::try_from(number).ok())
        .filter(|number| *number != 0)
        .ok_or_else(|| wrong(id, "positive image dimension"))
}

fn read_color(resolver: &mut Resolver<'_>, dict: DictRef<'_>) -> Result<ColorSpec, DocError> {
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
    let indexed = first
        .get()
        .as_name()
        .map(|name| name.eq_bytes(b"Indexed") || name.eq_bytes(b"I"))
        .unwrap_or(false);
    if !indexed {
        return simple_space(resolver, value.get()).map(ColorSpec::simple);
    }
    if array.len() != 4 {
        return Err(wrong(value.object().id(), "Indexed color space"));
    }

    let base_value = array
        .get(1)
        .ok_or_else(|| wrong(value.object().id(), "Indexed base"))?;
    let base = match simple_space(resolver, base_value)? {
        ColorSpace::Gray => IndexedBase::Gray,
        ColorSpace::Rgb => IndexedBase::Rgb,
        ColorSpace::Cmyk => IndexedBase::Cmyk,
        _ => return Ok(ColorSpec::simple(ColorSpace::Unsupported)),
    };
    let hival = resolver.resolve(
        array
            .get(2)
            .ok_or_else(|| wrong(value.object().id(), "Indexed hival"))?,
    )?;
    let hival = hival
        .get()
        .as_i64()
        .and_then(|number| u8::try_from(number).ok())
        .ok_or_else(|| wrong(value.object().id(), "Indexed hival in 0..255"))?;
    let lookup = array
        .get(3)
        .ok_or_else(|| wrong(value.object().id(), "Indexed lookup"))?
        .to_owned();
    Ok(ColorSpec {
        space: ColorSpace::Indexed { base, hival },
        lookup: Some(lookup),
    })
}

/// Indexed bases cannot themselves be Indexed. Parsing is nonrecursive,
/// including when the base or ICC profile is indirect.
fn simple_space(resolver: &mut Resolver<'_>, value: ValueRef<'_>) -> Result<ColorSpace, DocError> {
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

    let profile = resolver.resolve(
        array
            .get(1)
            .ok_or_else(|| wrong(value.object().id(), "ICCBased profile"))?,
    )?;
    if !profile.object().is_stream() {
        return Err(wrong(profile.object().id(), "ICC profile stream"));
    }
    let dict = profile
        .object()
        .dict()
        .ok_or_else(|| wrong(profile.object().id(), "ICC profile dictionary"))?;
    let count = resolver
        .resolve_dict_key(dict, b"N")?
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
    let mut pairs = [DecodePair {
        low: 0.0,
        high: 1.0,
    }; 4];
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
    let array = value
        .get()
        .as_array()
        .ok_or_else(|| wrong(id, "image Decode array"))?;
    let count = channels
        .checked_mul(2)
        .ok_or_else(|| wrong(id, "image Decode components"))?;
    if array.len() != count {
        return Err(wrong(id, "image Decode component pairs"));
    }
    let mut values = array.iter();
    for pair in pairs.iter_mut().take(channels) {
        let low = resolver.resolve(
            values
                .next()
                .ok_or_else(|| wrong(id, "image Decode number"))?,
        )?;
        let high = resolver.resolve(
            values
                .next()
                .ok_or_else(|| wrong(id, "image Decode number"))?,
        )?;
        pair.low = low
            .get()
            .as_f64()
            .filter(|number| number.is_finite())
            .ok_or_else(|| wrong(id, "finite image Decode number"))?;
        pair.high = high
            .get()
            .as_f64()
            .filter(|number| number.is_finite())
            .ok_or_else(|| wrong(id, "finite image Decode number"))?;
    }
    Ok(pairs)
}

fn read_lookup(
    resolver: &mut Resolver<'_>,
    value: &Value,
    space: ColorSpace,
    budget: &mut ImageDecodeBudget,
) -> Result<Vec<u8>, DocError> {
    let ColorSpace::Indexed { base, hival } = space else {
        return Err(wrong(value.object().id(), "Indexed color space"));
    };
    let expected = usize::from(hival)
        .checked_add(1)
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
        return Err(wrong(
            value.object().id(),
            "Indexed lookup string or stream",
        ));
    }
    let mut decoded = decode(resolver, value.object(), budget)?;
    if decoded.image.is_some() {
        return Err(DocError::Unsupported {
            feature: "image palette codec",
        });
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
        let stride = width
            .checked_mul(channels)
            .and_then(|samples| samples.checked_mul(usize::from(bpc)))
            .and_then(|bits| bits.checked_add(7))
            .map(|bits| bits / 8)
            .ok_or_else(|| size_error(maximum))?;
        let row_rgba = width.checked_mul(4).ok_or_else(|| size_error(maximum))?;
        let rgba = row_rgba
            .checked_mul(height)
            .ok_or_else(|| size_error(maximum))?;
        let expected = stride
            .checked_mul(height)
            .ok_or_else(|| size_error(maximum))?;
        check(
            LimitKind::DecodedBytes,
            rgba as u64,
            maximum.min(isize::MAX as u64),
        )?;
        check(LimitKind::DecodedBytes, expected as u64, maximum)?;
        Ok(Self {
            stride,
            row_rgba,
            rgba,
            expected,
        })
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
                let shift = 8u8
                    .saturating_sub(self.bpc)
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
    for (slot, (sample, pair)) in values
        .iter_mut()
        .zip(raw.iter().zip(pairs.iter()))
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
                (255u16
                    .saturating_sub(u16::from(value))
                    .saturating_mul(black)
                    / 255) as u8
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
    budget: &mut ImageDecodeBudget,
) -> Result<(), DocError> {
    let value = resolver.resolve(value.get())?;
    let mask_object = value.object();
    if object.id().is_some() && object.id() == mask_object.id() {
        warn(resolver, object, "image_soft_mask_cycle");
        return Ok(());
    }
    let (mask, _) = read_image(resolver, mask_object, max_pixels, false, budget)?;
    if mask.color_space != ColorSpace::Gray || mask.image_mask {
        return Err(DocError::Unsupported {
            feature: "non-gray soft mask",
        });
    }
    let ImageData::Pixels { rgba: mask_rgba } = mask.data else {
        return Err(DocError::Unsupported {
            feature: "soft mask codec",
        });
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
        let source_y = (y as u64).saturating_mul(u64::from(mask.height)) / u64::from(image.height);
        for (x, slot) in row.iter_mut().enumerate() {
            let source_x =
                (x as u64).saturating_mul(u64::from(mask.width)) / u64::from(image.width);
            let offset = source_y
                .checked_mul(u64::from(mask.width))
                .and_then(|row| row.checked_add(source_x))
                .and_then(|pixel| pixel.checked_mul(4))
                .and_then(|offset| usize::try_from(offset).ok())
                .ok_or_else(|| size_error(maximum))?;
            let end = offset.checked_add(4).ok_or_else(|| size_error(maximum))?;
            *slot = match mask_rgba.get(offset..end) {
                Some([gray, _, _, valid]) => {
                    (u16::from(*gray).saturating_mul(u16::from(*valid)) / 255) as u8
                }
                _ => 0,
            };
        }
    }

    for ([.., current], soft) in rgba.as_chunks_mut::<4>().0.iter_mut().zip(alpha.iter()) {
        *current = (u16::from(*current).saturating_mul(u16::from(*soft)) / 255) as u8;
    }
    image.soft_mask = Some(alpha);
    Ok(())
}

fn decode(
    resolver: &mut Resolver<'_>,
    object: &Object,
    budget: &mut ImageDecodeBudget,
) -> Result<Decoded, DocError> {
    let maximum = resolver
        .document()
        .limits()
        .max_decoded_bytes
        .min(budget.remaining());
    let decoded = resolver.decode_stream_limited(object, maximum)?;
    budget.charge(decoded.bytes.len())?;
    Ok(decoded)
}

fn zeroed(size: usize, maximum: u64) -> Result<Vec<u8>, DocError> {
    let maximum = maximum.min(isize::MAX as u64);
    check(LimitKind::DecodedBytes, size as u64, maximum)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .map_err(|_| size_error(maximum))?;
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
        error @ (DocError::Encrypted
        | DocError::Refused(_)
        | DocError::Filter(pdf_core::filter::FilterError::OutputLimit { .. })
        | DocError::Filter(pdf_core::filter::FilterError::Refused(_))) => Err(error),
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
