use pdf_core::document::Resolver;
use pdf_core::error::{DocError, Warning};
use pdf_core::limits::{LimitKind, Refusal};
use pdf_core::pages::Page;
use pdf_core::work::ObjectRef;
use pdf_render::display::{
    FillRule as PdfFillRule, ImageSource, Item, Paint as PdfPaint, Path as PdfPath, PathEl,
    Stroke as PdfStroke,
};
use pdf_render::{display_list, RenderOptions};
use tiny_skia::{
    Color, FillRule as SkFillRule, FilterQuality, GradientStop, IntSize, LineCap, LineJoin,
    LinearGradient, Mask, Paint as SkPaint, Path as SkPath, PathBuilder, Pixmap, PixmapPaint,
    Point, RadialGradient, Shader, SpreadMode, Stroke as SkStroke, StrokeDash, Transform,
};

const MAX_PATH_ELEMENTS: usize = 1_000_000;
// tiny-skia's fixed-point scan converter panics (and aborts during cleanup) on coordinates near
// 1e12. Clamping to 2^20 device pixels moves visible edges by less than 1/2^20 of their extent.
const MAX_COORDINATE: f32 = 1_048_576.0;
const MAX_GRADIENT_STOPS: usize = 4_096;
const MAX_DASH_ELEMENTS: usize = 4_096;
const MAX_CLIP_DEPTH: usize = 64;

#[derive(Clone, Debug, PartialEq)]
pub struct Raster {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

pub fn render_page(
    resolver: &mut Resolver<'_>,
    page: &Page,
    options: &RenderOptions,
    background: [u8; 4],
) -> Result<Raster, DocError> {
    let display = display_list(resolver, page, options)?;
    let maximum = resolver.document().limits().max_decoded_bytes;
    let buffer_len = checked_rgba_len(display.width, display.height, maximum)?;
    let Some(mut pixmap) = Pixmap::new(display.width, display.height) else {
        return Err(allocation_refusal(maximum));
    };
    pixmap.fill(Color::from_rgba8(
        background[0],
        background[1],
        background[2],
        background[3],
    ));

    let pixel_count = u64::try_from(buffer_len / 4).unwrap_or(u64::MAX);
    let limits = *resolver.document().limits();
    let mut function_work = 0u64;
    let mut texture_bytes = buffer_len as u64;
    let mut clips: Vec<Mask> = Vec::new();
    let mut blend = tiny_skia::BlendMode::SourceOver;

    for item in display.items {
        match item {
            Item::SetBlendMode(mode) => {
                blend = match mode {
                    pdf_render::display::BlendMode::Normal => tiny_skia::BlendMode::SourceOver,
                    pdf_render::display::BlendMode::Multiply => tiny_skia::BlendMode::Multiply,
                };
            }
            Item::PushClip { path, rule } => {
                let requested_depth = u64::try_from(clips.len())
                    .unwrap_or(u64::MAX)
                    .saturating_add(1);
                if clips.len() >= MAX_CLIP_DEPTH {
                    return Err(DocError::Refused(Refusal {
                        limit: LimitKind::Depth,
                        requested: requested_depth,
                        maximum: MAX_CLIP_DEPTH as u64,
                    }));
                }
                let mask_count = u64::try_from(clips.len())
                    .unwrap_or(u64::MAX)
                    .saturating_add(1);
                let mask_bytes = pixel_count.saturating_mul(mask_count);
                let active_bytes = u64::try_from(buffer_len)
                    .unwrap_or(u64::MAX)
                    .saturating_add(mask_bytes);
                let memory_limit = maximum.min(isize::MAX as u64);
                if active_bytes > memory_limit {
                    return Err(DocError::Refused(Refusal {
                        limit: LimitKind::DecodedBytes,
                        requested: active_bytes,
                        maximum: memory_limit,
                    }));
                }

                let Some(mut mask) = Mask::new(display.width, display.height) else {
                    return Err(allocation_refusal(memory_limit));
                };
                if let Some(parent) = clips.last() {
                    mask.data_mut().copy_from_slice(parent.data());
                } else {
                    mask.data_mut().fill(255);
                }
                if let Some(path) = path_to_skia(&path) {
                    mask.intersect_path(&path, sk_fill_rule(rule), true, Transform::identity());
                }
                clips.push(mask);
            }
            Item::PopClip => {
                clips.pop();
            }
            Item::Fill { path, rule, paint } => {
                let texture = paint_texture(
                    &pixmap,
                    &paint,
                    clips.len(),
                    limits,
                    &mut function_work,
                    &mut texture_bytes,
                )?;
                let (Some(path), Some(paint)) = (
                    path_to_skia(&path),
                    paint_to_skia(&paint, blend, texture.as_ref()),
                ) else {
                    continue;
                };
                pixmap.fill_path(
                    &path,
                    &paint,
                    sk_fill_rule(rule),
                    Transform::identity(),
                    clips.last(),
                );
            }
            Item::Stroke {
                path,
                stroke,
                dash,
                paint,
            } => {
                let texture = paint_texture(
                    &pixmap,
                    &paint,
                    clips.len(),
                    limits,
                    &mut function_work,
                    &mut texture_bytes,
                )?;
                let (Some(path), Some(paint), Some(stroke)) = (
                    path_to_skia(&path),
                    paint_to_skia(&paint, blend, texture.as_ref()),
                    stroke_to_skia(&stroke, dash.as_ref()),
                ) else {
                    continue;
                };
                pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), clips.last());
            }
            Item::Image {
                image,
                transform,
                alpha,
            } => {
                draw_image(
                    resolver,
                    &mut pixmap,
                    image,
                    transform,
                    alpha,
                    blend,
                    clips.last(),
                    maximum,
                )?;
            }
            Item::Glyphs {
                glyphs,
                paint,
                stroke,
            } => {
                let texture = paint_texture(
                    &pixmap,
                    &paint,
                    clips.len(),
                    limits,
                    &mut function_work,
                    &mut texture_bytes,
                )?;
                let Some(paint) = paint_to_skia(&paint, blend, texture.as_ref()) else {
                    continue;
                };
                let stroke = stroke
                    .as_ref()
                    .and_then(|stroke| stroke_to_skia(stroke, None));
                for glyph in glyphs {
                    let Some(path) = path_to_skia(&glyph.path) else {
                        continue;
                    };
                    pixmap.fill_path(
                        &path,
                        &paint,
                        SkFillRule::Winding,
                        Transform::identity(),
                        clips.last(),
                    );
                    if let Some(stroke) = stroke.as_ref() {
                        pixmap.stroke_path(
                            &path,
                            &paint,
                            stroke,
                            Transform::identity(),
                            clips.last(),
                        );
                    }
                }
            }
        }
    }

    Ok(Raster {
        width: pixmap.width(),
        height: pixmap.height(),
        rgba: pixmap.take_demultiplied(),
    })
}

pub fn encode_png(raster: &Raster) -> Vec<u8> {
    let maximum = isize::MAX as u64;
    let Ok(buffer_len) = checked_rgba_len(raster.width, raster.height, maximum) else {
        return Vec::new();
    };
    if raster.rgba.len() != buffer_len {
        return Vec::new();
    }
    if let Some(png) = encode_opaque_png(raster) {
        return png;
    }
    let Some(mut pixmap) = Pixmap::new(raster.width, raster.height) else {
        return Vec::new();
    };
    for (destination, source) in pixmap
        .data_mut()
        .chunks_exact_mut(4)
        .zip(raster.rgba.chunks_exact(4))
    {
        let alpha = u16::from(source.get(3).copied().unwrap_or(0));
        let red = (u16::from(source.first().copied().unwrap_or(0))
            .saturating_mul(alpha)
            .saturating_add(127)
            / 255) as u8;
        let green = (u16::from(source.get(1).copied().unwrap_or(0))
            .saturating_mul(alpha)
            .saturating_add(127)
            / 255) as u8;
        let blue = (u16::from(source.get(2).copied().unwrap_or(0))
            .saturating_mul(alpha)
            .saturating_add(127)
            / 255) as u8;
        let bytes = [red, green, blue, alpha as u8];
        destination.copy_from_slice(&bytes);
    }
    pixmap.encode_png().unwrap_or_default()
}

/// Fast PNG for opaque rasters: RGB rows with the Up filter, deflate level 1.
/// Pages are opaque after the white backdrop; anything with alpha goes through
/// tiny-skia so its decoded pixels stay exactly as before.
fn encode_opaque_png(raster: &Raster) -> Option<Vec<u8>> {
    let width = raster.width as usize;
    let height = raster.height as usize;
    if raster.rgba.chunks_exact(4).any(|pixel| pixel[3] != 255) {
        return None;
    }
    let row = width.checked_mul(3)?;
    let mut filtered = Vec::with_capacity(row.checked_add(1)?.checked_mul(height)?);
    let mut previous: Vec<u8> = vec![0; row];
    let mut current: Vec<u8> = Vec::with_capacity(row);
    for line in raster.rgba.chunks_exact(width * 4) {
        current.clear();
        for pixel in line.chunks_exact(4) {
            current.extend_from_slice(&pixel[..3]);
        }
        filtered.push(2);
        filtered.extend(
            current
                .iter()
                .zip(&previous)
                .map(|(byte, above)| byte.wrapping_sub(*above)),
        );
        core::mem::swap(&mut previous, &mut current);
    }
    let compressed = miniz_oxide::deflate::compress_to_vec_zlib(&filtered, 1);
    let mut png = Vec::with_capacity(compressed.len() + 64);
    png.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    let mut header = [0u8; 13];
    header[..4].copy_from_slice(&raster.width.to_be_bytes());
    header[4..8].copy_from_slice(&raster.height.to_be_bytes());
    header[8] = 8;
    header[9] = 2;
    png_chunk(&mut png, b"IHDR", &header);
    png_chunk(&mut png, b"IDAT", &compressed);
    png_chunk(&mut png, b"IEND", &[]);
    Some(png)
}

fn png_chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    png.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = png.len();
    png.extend_from_slice(kind);
    png.extend_from_slice(data);
    let crc = crc32(&png[start..]);
    png.extend_from_slice(&crc.to_be_bytes());
}

fn crc32(bytes: &[u8]) -> u32 {
    static TABLE: [u32; 256] = {
        let mut table = [0u32; 256];
        let mut index = 0;
        while index < 256 {
            let mut value = index as u32;
            let mut bit = 0;
            while bit < 8 {
                value = if value & 1 != 0 {
                    0xEDB8_8320 ^ (value >> 1)
                } else {
                    value >> 1
                };
                bit += 1;
            }
            table[index] = value;
            index += 1;
        }
        table
    };
    let mut crc = !0u32;
    for byte in bytes {
        crc = TABLE[((crc ^ u32::from(*byte)) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

fn checked_rgba_len(width: u32, height: u32, maximum: u64) -> Result<usize, DocError> {
    if width == 0 || height == 0 {
        return Err(DocError::Malformed {
            offset: 0,
            what: "zero-sized raster",
        });
    }
    let requested = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(4))
        .unwrap_or(u64::MAX);
    let maximum = maximum.min(isize::MAX as u64);
    if requested > maximum {
        return Err(DocError::Refused(Refusal {
            limit: LimitKind::DecodedBytes,
            requested,
            maximum,
        }));
    }
    usize::try_from(requested).map_err(|_| {
        DocError::Refused(Refusal {
            limit: LimitKind::DecodedBytes,
            requested,
            maximum,
        })
    })
}

fn allocation_refusal(maximum: u64) -> DocError {
    DocError::Refused(Refusal {
        limit: LimitKind::DecodedBytes,
        requested: maximum.saturating_add(1),
        maximum,
    })
}

fn path_to_skia(path: &PdfPath) -> Option<SkPath> {
    if path.els.len() > MAX_PATH_ELEMENTS {
        return None;
    }
    let c = |value: f32| value.clamp(-MAX_COORDINATE, MAX_COORDINATE);
    let mut builder = PathBuilder::new();
    for element in &path.els {
        match *element {
            PathEl::MoveTo(x, y) => {
                if !x.is_finite() || !y.is_finite() {
                    return None;
                }
                builder.move_to(c(x), c(y));
            }
            PathEl::LineTo(x, y) => {
                if !x.is_finite() || !y.is_finite() {
                    return None;
                }
                builder.line_to(c(x), c(y));
            }
            PathEl::CubicTo(x1, y1, x2, y2, x3, y3) => {
                if ![x1, y1, x2, y2, x3, y3]
                    .iter()
                    .all(|value| value.is_finite())
                {
                    return None;
                }
                builder.cubic_to(c(x1), c(y1), c(x2), c(y2), c(x3), c(y3));
            }
            PathEl::Close => builder.close(),
        }
    }
    builder.finish()
}

fn sk_fill_rule(rule: PdfFillRule) -> SkFillRule {
    match rule {
        PdfFillRule::NonZero => SkFillRule::Winding,
        PdfFillRule::EvenOdd => SkFillRule::EvenOdd,
    }
}

fn color_to_skia(rgba: pdf_render::display::Rgba) -> Option<Color> {
    if ![rgba.r, rgba.g, rgba.b, rgba.a]
        .iter()
        .all(|value| value.is_finite())
    {
        return None;
    }
    Color::from_rgba(
        rgba.r.clamp(0.0, 1.0),
        rgba.g.clamp(0.0, 1.0),
        rgba.b.clamp(0.0, 1.0),
        rgba.a.clamp(0.0, 1.0),
    )
}

fn average_gradient_color(stops: &[(f32, pdf_render::display::Rgba)]) -> Color {
    let mut sums = [0.0f64; 4];
    let mut count = 0u64;
    for (_, stop) in stops.iter().take(MAX_GRADIENT_STOPS) {
        let components = [stop.r, stop.g, stop.b, stop.a];
        if !components.iter().all(|value| value.is_finite()) {
            continue;
        }
        for (sum, value) in sums.iter_mut().zip(components) {
            *sum += f64::from(value.clamp(0.0, 1.0));
        }
        count = count.saturating_add(1);
    }
    if count == 0 {
        return Color::from_rgba8(0, 0, 0, 0);
    }
    let divisor = count as f64;
    Color::from_rgba(
        (sums[0] / divisor) as f32,
        (sums[1] / divisor) as f32,
        (sums[2] / divisor) as f32,
        (sums[3] / divisor) as f32,
    )
    .unwrap_or_else(|| Color::from_rgba8(0, 0, 0, 0))
}

fn gradient_stops(
    stops: &[(f32, pdf_render::display::Rgba)],
    extend: (bool, bool),
) -> Vec<GradientStop> {
    let capacity = stops.len().min(MAX_GRADIENT_STOPS).saturating_add(2);
    let mut output = Vec::with_capacity(capacity);
    let transparent = Color::from_rgba8(0, 0, 0, 0);
    if !extend.0 {
        output.push(GradientStop::new(0.0, transparent));
    }
    for (position, rgba) in stops.iter().take(MAX_GRADIENT_STOPS) {
        if !position.is_finite() {
            continue;
        }
        if let Some(color) = color_to_skia(*rgba) {
            output.push(GradientStop::new(position.clamp(0.0, 1.0), color));
        }
    }
    if !extend.1 {
        output.push(GradientStop::new(1.0, transparent));
    }
    output
}

fn paint_texture(
    destination: &Pixmap,
    paint: &PdfPaint,
    clip_count: usize,
    limits: pdf_core::Limits,
    work: &mut u64,
    total_bytes: &mut u64,
) -> Result<Option<Pixmap>, DocError> {
    let PdfPaint::Function(function) = paint else {
        return Ok(None);
    };
    let length = checked_rgba_len(
        destination.width(),
        destination.height(),
        limits.max_decoded_bytes,
    )?;
    let active = (length as u64)
        .saturating_mul(2)
        .saturating_add((length as u64 / 4).saturating_mul(clip_count as u64));
    let maximum = limits.max_decoded_bytes.min(isize::MAX as u64);
    if active > maximum {
        return Err(DocError::Refused(Refusal {
            limit: LimitKind::DecodedBytes,
            requested: active,
            maximum,
        }));
    }
    *total_bytes = total_bytes
        .checked_add(length as u64)
        .ok_or(DocError::Refused(Refusal {
            limit: LimitKind::TotalDecodedBytes,
            requested: u64::MAX,
            maximum: limits.max_total_decoded_bytes,
        }))?;
    if *total_bytes > limits.max_total_decoded_bytes {
        return Err(DocError::Refused(Refusal {
            limit: LimitKind::TotalDecodedBytes,
            requested: *total_bytes,
            maximum: limits.max_total_decoded_bytes,
        }));
    }
    let Some(mut texture) = Pixmap::new(destination.width(), destination.height()) else {
        return Err(allocation_refusal(maximum));
    };
    let width = destination.width() as usize;
    for (index, pixel) in texture.data_mut().chunks_exact_mut(4).enumerate() {
        *work = work.checked_add(1).ok_or(DocError::Refused(Refusal {
            limit: LimitKind::ContentOps,
            requested: u64::MAX,
            maximum: limits.max_content_ops,
        }))?;
        if *work > limits.max_content_ops {
            return Err(DocError::Refused(Refusal {
                limit: LimitKind::ContentOps,
                requested: *work,
                maximum: limits.max_content_ops,
            }));
        }
        let x = (index % width) as f32 + 0.5;
        let y = (index / width) as f32 + 0.5;
        if let Some(color) = function.sample(x, y, work)? {
            let byte = |value: f32| (value.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            pixel.copy_from_slice(&[byte(color.r), byte(color.g), byte(color.b), byte(color.a)]);
        }
    }
    premultiply(texture.data_mut());
    Ok(Some(texture))
}

fn paint_to_skia<'a>(
    paint: &PdfPaint,
    blend: tiny_skia::BlendMode,
    texture: Option<&'a Pixmap>,
) -> Option<SkPaint<'a>> {
    let shader = match paint {
        PdfPaint::Solid(rgba) => Shader::SolidColor(color_to_skia(*rgba)?),
        PdfPaint::Function(_) => tiny_skia::Pattern::new(
            texture?.as_ref(),
            SpreadMode::Pad,
            FilterQuality::Nearest,
            1.0,
            Transform::identity(),
        ),
        PdfPaint::Axial {
            from,
            to,
            stops,
            extend,
        } => {
            let fallback = || Shader::SolidColor(average_gradient_color(stops));
            let start = Point::from_xy(from.0, from.1);
            let end = Point::from_xy(to.0, to.1);
            LinearGradient::new(
                start,
                end,
                gradient_stops(stops, *extend),
                SpreadMode::Pad,
                Transform::identity(),
            )
            .unwrap_or_else(fallback)
        }
        PdfPaint::Radial {
            from,
            to,
            stops,
            extend,
        } => {
            let fallback = || Shader::SolidColor(average_gradient_color(stops));
            let start = Point::from_xy(from.0, from.1);
            let end = Point::from_xy(to.0, to.1);
            RadialGradient::new(
                start,
                from.2,
                end,
                to.2,
                gradient_stops(stops, *extend),
                SpreadMode::Pad,
                Transform::identity(),
            )
            .unwrap_or_else(fallback)
        }
    };
    let mut paint = sk_paint(shader);
    paint.blend_mode = blend;
    Some(paint)
}

fn sk_paint<'a>(shader: Shader<'a>) -> SkPaint<'a> {
    let mut paint = SkPaint::default();
    paint.shader = shader;
    paint.anti_alias = true;
    paint
}

fn dash_to_skia(dash: &pdf_render::display::Dash) -> Option<StrokeDash> {
    if dash.array.is_empty()
        || dash.array.len() > MAX_DASH_ELEMENTS
        || !dash.phase.is_finite()
        || !dash
            .array
            .iter()
            .all(|value| value.is_finite() && *value >= 0.0)
        || !dash.array.iter().any(|value| *value > 0.0)
    {
        return None;
    }
    let capacity = if dash.array.len() % 2 == 0 {
        dash.array.len()
    } else {
        dash.array.len().checked_mul(2)?
    };
    let mut values = Vec::with_capacity(capacity);
    values.extend(dash.array.iter().copied());
    if dash.array.len() % 2 == 1 {
        values.extend(dash.array.iter().copied());
    }
    StrokeDash::new(values, dash.phase)
}

fn stroke_to_skia(
    stroke: &PdfStroke,
    dash: Option<&pdf_render::display::Dash>,
) -> Option<SkStroke> {
    if !stroke.width.is_finite() || stroke.width < 0.0 {
        return None;
    }
    let mut output = SkStroke::default();
    output.width = stroke.width;
    output.miter_limit = if stroke.miter.is_finite() && stroke.miter >= 1.0 {
        stroke.miter
    } else {
        1.0
    };
    output.line_cap = match stroke.cap {
        1 => LineCap::Round,
        2 => LineCap::Square,
        _ => LineCap::Butt,
    };
    output.line_join = match stroke.join {
        1 => LineJoin::Round,
        2 => LineJoin::Bevel,
        _ => LineJoin::Miter,
    };
    output.dash = dash.and_then(dash_to_skia);
    Some(output)
}

fn draw_image(
    resolver: &mut Resolver<'_>,
    destination: &mut Pixmap,
    source: ImageSource,
    matrix: pdf_render::display::Matrix,
    alpha: f32,
    blend: tiny_skia::BlendMode,
    mask: Option<&Mask>,
    maximum: u64,
) -> Result<(), DocError> {
    let (width, height, mut rgba) = match source {
        ImageSource::Inline {
            width,
            height,
            rgba,
        } => (width, height, rgba),
        ImageSource::Object(id) => {
            let Some(image) = object_image_rgba(resolver, id, maximum)? else {
                return Ok(());
            };
            image
        }
    };
    let expected = checked_rgba_len(width, height, maximum)?;
    if rgba.len() != expected {
        return Ok(());
    }
    let Some(size) = IntSize::from_wh(width, height) else {
        return Ok(());
    };
    let Some(transform) = image_transform(matrix, width, height) else {
        return Ok(());
    };
    premultiply(&mut rgba);
    let Some(source) = Pixmap::from_vec(rgba, size) else {
        return Ok(());
    };
    let mut paint = PixmapPaint::default();
    paint.blend_mode = blend;
    paint.quality = FilterQuality::Bilinear;
    paint.opacity = if alpha.is_finite() {
        alpha.clamp(0.0, 1.0)
    } else {
        0.0
    };
    destination.draw_pixmap(0, 0, source.as_ref(), &paint, transform, mask);
    Ok(())
}

fn image_transform(
    matrix: pdf_render::display::Matrix,
    width: u32,
    height: u32,
) -> Option<Transform> {
    if width == 0 || height == 0 {
        return None;
    }
    let [a, b, c, d, e, f] = matrix.0;
    if ![a, b, c, d, e, f].iter().all(|value| value.is_finite()) {
        return None;
    }
    let width = width as f32;
    let height = height as f32;
    let transform =
        Transform::from_row(a / width, -c / height, b / width, -d / height, e + c, f + d);
    transform.invert().map(|_| transform)
}

fn object_image_rgba(
    resolver: &mut Resolver<'_>,
    id: ObjectRef,
    maximum: u64,
) -> Result<Option<(u32, u32, Vec<u8>)>, DocError> {
    let max_pixels = maximum / 4;
    let image = match pdf_text::images::load_image(resolver, id, max_pixels) {
        Ok(image) => image,
        Err(error) if is_fatal(&error) => return Err(error),
        Err(_) => {
            resolver.warn(Warning {
                offset: None,
                object: Some(id),
                what: "raster_image_failed",
            });
            return Ok(None);
        }
    };
    let is_jpeg = matches!(&image.data, pdf_text::images::ImageData::Jpeg(_));
    let mut rgba = match image.data {
        pdf_text::images::ImageData::Pixels { rgba } => rgba,
        pdf_text::images::ImageData::Jpeg(data) => {
            let Some(rgba) = decode_jpeg(&data, image.width, image.height, maximum)? else {
                resolver.warn(Warning {
                    offset: None,
                    object: Some(id),
                    what: "raster_jpeg_failed",
                });
                return Ok(None);
            };
            rgba
        }
        pdf_text::images::ImageData::Unsupported(_) => return Ok(None),
    };
    let expected = checked_rgba_len(image.width, image.height, maximum)?;
    if rgba.len() != expected {
        resolver.warn(Warning {
            offset: None,
            object: Some(id),
            what: "raster_image_failed",
        });
        return Ok(None);
    }
    if is_jpeg && image.has_soft_mask {
        if let Err(error) =
            apply_external_soft_mask(resolver, id, image.width, image.height, &mut rgba, maximum)
        {
            if is_fatal(&error) {
                return Err(error);
            }
            resolver.warn(Warning {
                offset: None,
                object: Some(id),
                what: "raster_image_soft_mask_failed",
            });
        }
    }
    Ok(Some((image.width, image.height, rgba)))
}

fn decode_jpeg(
    data: &[u8],
    width: u32,
    height: u32,
    maximum: u64,
) -> Result<Option<Vec<u8>>, DocError> {
    use zune_jpeg::zune_core::bytestream::ZCursor;
    use zune_jpeg::zune_core::colorspace::ColorSpace as JpegColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;

    let options = DecoderOptions::default().jpeg_set_out_colorspace(JpegColorSpace::RGBA);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(data), options);
    if decoder.decode_headers().is_err() {
        return Ok(None);
    }
    let Some((jpeg_width, jpeg_height)) = decoder.dimensions() else {
        return Ok(None);
    };
    if jpeg_width != width as usize || jpeg_height != height as usize {
        return Ok(None);
    }
    let expected = checked_rgba_len(width, height, maximum)?;
    let Some(output_len) = decoder.output_buffer_size() else {
        return Ok(None);
    };
    if output_len != expected {
        return Ok(None);
    }
    let mut rgba = vec![0u8; output_len];
    if decoder.decode_into(&mut rgba).is_err() {
        return Ok(None);
    }
    Ok(Some(rgba))
}

fn apply_external_soft_mask(
    resolver: &mut Resolver<'_>,
    image_id: ObjectRef,
    width: u32,
    height: u32,
    rgba: &mut [u8],
    maximum: u64,
) -> Result<(), DocError> {
    let object = match resolver.get(image_id) {
        Ok(object) => object,
        Err(error) => return Err(error),
    };
    let Some(dict) = object.dict() else {
        return Ok(());
    };
    let Some(mask_ref) = dict.get(b"SMask") else {
        return Ok(());
    };
    let mask_value = resolver.resolve(mask_ref)?;
    let Some(mask_id) = mask_value.get().as_ref() else {
        return Ok(());
    };
    let mask = match pdf_text::images::load_image(resolver, mask_id, maximum / 4) {
        Ok(mask) => mask,
        Err(error) => return Err(error),
    };
    if mask.color_space != pdf_text::images::ColorSpace::Gray || mask.image_mask {
        return Ok(());
    }
    let mask_rgba = match mask.data {
        pdf_text::images::ImageData::Pixels { rgba } => rgba,
        _ => return Ok(()),
    };
    let mask_len = checked_rgba_len(mask.width, mask.height, maximum)?;
    if mask_rgba.len() != mask_len {
        return Ok(());
    }
    let image_width = usize::try_from(width).map_err(|_| allocation_refusal(maximum))?;
    if image_width == 0 || height == 0 {
        return Ok(());
    }
    for (index, pixel) in rgba.chunks_exact_mut(4).enumerate() {
        let x = index % image_width;
        let y = index / image_width;
        let source_x = (x as u64).saturating_mul(u64::from(mask.width)) / u64::from(width);
        let source_y = (y as u64).saturating_mul(u64::from(mask.height)) / u64::from(height);
        let offset = source_y
            .checked_mul(u64::from(mask.width))
            .and_then(|row| row.checked_add(source_x))
            .and_then(|pixel| pixel.checked_mul(4))
            .and_then(|offset| usize::try_from(offset).ok());
        let soft = offset
            .and_then(|offset| mask_rgba.get(offset..offset.saturating_add(4)))
            .and_then(|pixel| Some((pixel.first().copied()?, pixel.get(3).copied()?)))
            .map(|(gray, alpha)| u16::from(gray).saturating_mul(u16::from(alpha)) / 255)
            .unwrap_or(0);
        if let Some(alpha) = pixel.get_mut(3) {
            *alpha = (u16::from(*alpha).saturating_mul(soft) / 255) as u8;
        }
    }
    Ok(())
}

fn is_fatal(error: &DocError) -> bool {
    matches!(error, DocError::Encrypted | DocError::Refused(_))
}

fn premultiply(rgba: &mut [u8]) {
    for pixel in rgba.chunks_exact_mut(4) {
        let alpha = u16::from(pixel.get(3).copied().unwrap_or(0));
        for channel in pixel.iter_mut().take(3) {
            *channel = (u16::from(*channel)
                .saturating_mul(alpha)
                .saturating_add(127)
                / 255) as u8;
        }
    }
}
