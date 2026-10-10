//! Self-contained annotation dictionaries and normal appearances.
//!
//! Coordinates use default page user space. No page reference or appearance
//! reference is allocated here. The caller adds /P and /AP after serialization.

use crate::{serialize_value, Dict, Value};

const MAX_POINTS: usize = 65_536;
const MAX_TEXT_BYTES: usize = 65_536;
const MAX_CONTENT_BYTES: usize = 8 << 20;

#[derive(Clone, Debug)]
pub enum AnnotKind {
    Highlight { quads: Vec<[f32; 8]> },
    Note,
    Ink { paths: Vec<Vec<[f32; 2]>>, width: f32 },
    FreeText { text: String, size: f32 },
    Square { width: f32 },
    Circle { width: f32 },
}

#[derive(Clone, Debug)]
pub struct Annot {
    pub kind: AnnotKind,
    pub rect: [f32; 4],
    pub color: [f32; 3],
    pub opacity: f32,
    pub contents: Option<String>,
    pub author: Option<String>,
}

#[derive(Clone, Debug)]
pub struct AnnotParts {
    pub dict: Dict,
    pub ap_dict: Dict,
    pub ap_content: Vec<u8>,
}

fn name(value: &str) -> Value {
    Value::Name(value.to_owned())
}

fn array(values: impl IntoIterator<Item = f32>) -> Value {
    Value::Array(values.into_iter().map(|value| Value::Real(f64::from(value))).collect())
}

fn append(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), &'static str> {
    let requested = out.len().checked_add(bytes.len()).ok_or("annotation_content_limit")?;
    if requested > MAX_CONTENT_BYTES { return Err("annotation_content_limit (maximum 8388608)"); }
    out.try_reserve(bytes.len()).map_err(|_| "annotation_allocation_failed")?;
    out.extend_from_slice(bytes);
    Ok(())
}

fn emit(out: &mut Vec<u8>, values: &[f64], operator: &[u8]) -> Result<(), &'static str> {
    for value in values {
        if !value.is_finite() { return Err("annotation_number_nonfinite"); }
        let bytes = serialize_value(&Value::Real(*value))
            .map_err(|_| "annotation_serialization_failed")?;
        append(out, &bytes)?;
        append(out, b" ")?;
    }
    append(out, operator)?;
    append(out, b"\n")?;
    Ok(())
}

fn text_string(text: &str) -> Result<Value, &'static str> {
    if text.len() > MAX_TEXT_BYTES { return Err("annotation_text_limit (maximum 65536)"); }
    if text.is_ascii() { return Ok(Value::Str(text.as_bytes().to_vec())); }
    let mut bytes = vec![0xfe, 0xff];
    for unit in text.encode_utf16() {
        append(&mut bytes, &unit.to_be_bytes())?;
    }
    Ok(Value::HexStr(bytes))
}

fn width(value: f32) -> Result<(), &'static str> {
    if !value.is_finite() || value < 0.0 { return Err("annotation_width_invalid"); }
    Ok(())
}

fn validate(annot: &Annot) -> Result<(), &'static str> {
    if annot.rect.iter().any(|value| !value.is_finite()) {
        return Err("annotation_rect_nonfinite");
    }
    let [x0, y0, x1, y1] = annot.rect;
    if x1 <= x0 || y1 <= y0 { return Err("annotation_rect_empty"); }
    if annot.color.iter().any(|value| !value.is_finite() || !(0.0..=1.0).contains(value)) {
        return Err("annotation_color_invalid");
    }
    if !annot.opacity.is_finite() || !(0.0..=1.0).contains(&annot.opacity) {
        return Err("annotation_opacity_invalid");
    }
    for text in [annot.contents.as_deref(), annot.author.as_deref()].into_iter().flatten() {
        if text.len() > MAX_TEXT_BYTES { return Err("annotation_text_limit (maximum 65536)"); }
    }
    match &annot.kind {
        AnnotKind::Highlight { quads } => {
            if quads.is_empty() { return Err("annotation_quads_empty"); }
            if quads.len() > MAX_POINTS / 4 {
                return Err("annotation_points_limit (maximum 65536)");
            }
            if quads.iter().flatten().any(|value| !value.is_finite()) {
                return Err("annotation_quad_nonfinite");
            }
        }
        AnnotKind::Ink { paths, width: stroke } => {
            width(*stroke)?;
            if paths.is_empty() || paths.iter().any(Vec::is_empty) {
                return Err("annotation_paths_empty");
            }
            let mut count = 0usize;
            for path in paths {
                count = count.checked_add(path.len()).ok_or("annotation_points_limit")?;
                if count > MAX_POINTS { return Err("annotation_points_limit (maximum 65536)"); }
                if path.iter().flatten().any(|value| !value.is_finite()) {
                    return Err("annotation_ink_nonfinite");
                }
            }
        }
        AnnotKind::FreeText { text, size } => {
            if text.len() > MAX_TEXT_BYTES { return Err("annotation_text_limit (maximum 65536)"); }
            if !size.is_finite() || *size <= 0.0 { return Err("annotation_size_invalid"); }
        }
        AnnotKind::Square { width: stroke } | AnnotKind::Circle { width: stroke } => {
            width(*stroke)?;
            if f64::from(*stroke) >= (f64::from(x1) - f64::from(x0))
                .min(f64::from(y1) - f64::from(y0))
            {
                return Err("annotation_width_exceeds_rect");
            }
        }
        AnnotKind::Note => {}
    }
    Ok(())
}

fn ellipse(out: &mut Vec<u8>, rect: [f64; 4], operator: &[u8]) -> Result<(), &'static str> {
    let [x0, y0, x1, y1] = rect;
    let cx = (x0 + x1) * 0.5;
    let cy = (y0 + y1) * 0.5;
    let rx = (x1 - x0) * 0.5;
    let ry = (y1 - y0) * 0.5;
    let k = 0.552_284_749_830_793_6;
    emit(out, &[x1, cy], b"m")?;
    emit(out, &[x1, cy + k * ry, cx + k * rx, y1, cx, y1], b"c")?;
    emit(out, &[cx - k * rx, y1, x0, cy + k * ry, x0, cy], b"c")?;
    emit(out, &[x0, cy - k * ry, cx - k * rx, y0, cx, y0], b"c")?;
    emit(out, &[cx + k * rx, y0, x1, cy - k * ry, x1, cy], b"c")?;
    append(out, b"h\n")?;
    emit(out, &[], operator)
}

// Helvetica advances for ASCII. Extended WinAnsi glyphs use a conservative
// one-em advance, so wrapping never underestimates their widths.
fn advance(byte: u8) -> f64 {
    const ASCII: [u16; 95] = [
        278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278, 278,
        556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 278, 278, 584, 584, 584, 556,
        1015, 667, 667, 722, 722, 667, 611, 778, 722, 278, 500, 667, 556, 833, 722,
        778, 667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611, 278, 278, 278,
        469, 556, 333, 556, 556, 500, 556, 556, 278, 556, 556, 222, 222, 500, 222,
        833, 556, 556, 556, 556, 333, 500, 278, 556, 500, 722, 500, 500, 500,
        334, 260, 334, 584,
    ];
    let width = byte.checked_sub(32).and_then(|index| ASCII.get(usize::from(index)))
        .copied().unwrap_or(1000);
    f64::from(width) / 1000.0
}

fn free_text(
    out: &mut Vec<u8>, text: &str, size: f32, rect: [f64; 4],
) -> Result<(), &'static str> {
    let [x0, y0, x1, y1] = rect;
    let size = f64::from(size);
    let available = x1 - x0 - 4.0;
    if available <= 0.0 { return Ok(()); }
    let mut baseline = y1 - 2.0 - size;
    append(out, b"BT\n/Helv ")?;
    emit(out, &[size], b"Tf")?;
    for paragraph in text.split('\n') {
        let mut encoded = Vec::new();
        for character in paragraph.strip_suffix('\r').unwrap_or(paragraph).chars() {
            if character == '\t' {
                append(&mut encoded, b"    ")?;
            } else {
                append(&mut encoded, &[crate::doc::win_ansi(character)])?;
            }
        }
        let mut start = 0usize;
        loop {
            if baseline < y0 + 2.0 {
                return append(out, b"ET\n");
            }
            let mut end = start;
            let mut used = 0.0;
            while let Some(byte) = encoded.get(end) {
                let next = used + advance(*byte) * size;
                if next > available && end > start { break; }
                used = next;
                end = end.saturating_add(1);
                if next > available { break; }
            }
            if end < encoded.len() && encoded.get(end) != Some(&b' ') {
                if let Some(space) = encoded.get(start..end)
                    .and_then(|line| line.iter().rposition(|byte| *byte == b' '))
                {
                    if space > 0 { end = start.saturating_add(space); }
                }
            }
            let line = encoded.get(start..end).ok_or("annotation_text_range")?;
            emit(out, &[1.0, 0.0, 0.0, 1.0, x0 + 2.0, baseline], b"Tm")?;
            let string = serialize_value(&Value::Str(line.to_vec()))
                .map_err(|_| "annotation_serialization_failed")?;
            append(out, &string)?;
            append(out, b" Tj\n")?;
            baseline -= size * 1.2;
            if end >= encoded.len() { break; }
            start = end;
            while encoded.get(start) == Some(&b' ') { start = start.saturating_add(1); }
        }
    }
    append(out, b"ET\n")
}

/// Builds a deterministic appearance; /M uses the reproducible epoch date.
/// The fixed error API reports bounded point, text and content refusals by name.
pub fn build(annot: &Annot) -> Result<AnnotParts, &'static str> {
    validate(annot)?;
    let subtype = match &annot.kind {
        AnnotKind::Highlight { .. } => "Highlight",
        AnnotKind::Note => "Text",
        AnnotKind::Ink { .. } => "Ink",
        AnnotKind::FreeText { .. } => "FreeText",
        AnnotKind::Square { .. } => "Square",
        AnnotKind::Circle { .. } => "Circle",
    };
    // A note icon has a fixed 20x20 appearance anchored at the rectangle top-left.
    let annot_rect = match &annot.kind {
        AnnotKind::Note => {
            let left = annot.rect[0].min(annot.rect[2]);
            let top = annot.rect[1].max(annot.rect[3]);
            [left, top - 20.0, left + 20.0, top]
        }
        _ => annot.rect,
    };
    let mut dict = vec![
        ("Type".to_owned(), name("Annot")),
        ("Subtype".to_owned(), name(subtype)),
        ("Rect".to_owned(), array(annot_rect)),
        ("C".to_owned(), array(annot.color)),
        ("CA".to_owned(), Value::Real(f64::from(annot.opacity))),
        ("F".to_owned(), Value::Int(4)),
        ("M".to_owned(), Value::Str(b"D:19700101000000Z".to_vec())),
    ];
    let contents = annot.contents.as_deref().or_else(|| match &annot.kind {
        AnnotKind::FreeText { text, .. } => Some(text.as_str()),
        _ => None,
    });
    if let Some(contents) = contents {
        dict.push(("Contents".to_owned(), text_string(contents)?));
    }
    if let Some(author) = annot.author.as_deref() {
        dict.push(("T".to_owned(), text_string(author)?));
    }
    let blend = if matches!(&annot.kind, AnnotKind::Highlight { .. }) { "Multiply" } else { "Normal" };
    let mut resources = vec![(
        "ExtGState".to_owned(), Value::Dict(vec![(
            "GS".to_owned(), Value::Dict(vec![
                ("Type".to_owned(), name("ExtGState")),
                ("CA".to_owned(), Value::Real(f64::from(annot.opacity))),
                ("ca".to_owned(), Value::Real(f64::from(annot.opacity))),
                ("BM".to_owned(), name(blend)),
            ]),
        )]),
    )];
    if matches!(&annot.kind, AnnotKind::FreeText { .. }) {
        resources.push(("Font".to_owned(), Value::Dict(vec![(
            "Helv".to_owned(), Value::Dict(vec![
                ("Type".to_owned(), name("Font")),
                ("Subtype".to_owned(), name("Type1")),
                ("BaseFont".to_owned(), name("Helvetica")),
                ("Encoding".to_owned(), name("WinAnsiEncoding")),
            ]),
        )])));
    }
    let ap_dict = vec![
        ("Type".to_owned(), name("XObject")),
        ("Subtype".to_owned(), name("Form")),
        ("BBox".to_owned(), array(annot_rect)),
        ("Resources".to_owned(), Value::Dict(resources)),
    ];
    let rect = annot_rect.map(f64::from);
    let [x0, y0, x1, y1] = rect;
    let mut content = Vec::new();
    append(&mut content, b"q\n/GS gs\n")?;
    emit(&mut content, &annot.color.map(f64::from), b"rg")?;
    emit(&mut content, &annot.color.map(f64::from), b"RG")?;
    match &annot.kind {
        AnnotKind::Highlight { quads } => {
            dict.push(("QuadPoints".to_owned(), array(quads.iter().flatten().copied())));
            for quad in quads {
                let q = quad.map(f64::from);
                emit(&mut content, &[q[0], q[1]], b"m")?;
                emit(&mut content, &[q[2], q[3]], b"l")?;
                emit(&mut content, &[q[6], q[7]], b"l")?;
                emit(&mut content, &[q[4], q[5]], b"l")?;
                append(&mut content, b"h f\n")?;
            }
        }
        AnnotKind::Note => {
            dict.push(("Name".to_owned(), name("Comment")));
            emit(&mut content, &[x0, y1 - 20.0, 20.0, 20.0], b"re")?;
            append(&mut content, b"f\n0 0 0 RG\n1 w\n")?;
            for y in [y1 - 5.0, y1 - 9.0, y1 - 13.0] {
                emit(&mut content, &[x0 + 4.0, y], b"m")?;
                emit(&mut content, &[x0 + 16.0, y], b"l")?;
                append(&mut content, b"S\n")?;
            }
        }
        AnnotKind::Ink { paths, width } => {
            dict.push(("InkList".to_owned(), Value::Array(paths.iter().map(|path| {
                array(path.iter().flatten().copied())
            }).collect())));
            dict.push(("BS".to_owned(), Value::Dict(vec![
                ("W".to_owned(), Value::Real(f64::from(*width))),
            ])));
            emit(&mut content, &[f64::from(*width)], b"w")?;
            append(&mut content, b"1 J\n1 j\n")?;
            for path in paths {
                let first = path.first().ok_or("annotation_paths_empty")?;
                if path.iter().all(|point| point == first) {
                    let [x, y] = first.map(f64::from);
                    let radius = f64::from((*width).max(1.0)) * 0.5;
                    ellipse(&mut content, [x - radius, y - radius, x + radius, y + radius], b"f")?;
                } else {
                    emit(&mut content, &first.map(f64::from), b"m")?;
                    for point in path.iter().skip(1) {
                        emit(&mut content, &point.map(f64::from), b"l")?;
                    }
                    append(&mut content, b"S\n")?;
                }
            }
        }
        AnnotKind::FreeText { text, size } => {
            let mut da = b"/Helv ".to_vec();
            emit(&mut da, &[f64::from(*size)], b"Tf")?;
            emit(&mut da, &annot.color.map(f64::from), b"rg")?;
            dict.push(("DA".to_owned(), Value::Str(da)));
            dict.push(("Q".to_owned(), Value::Int(0)));
            free_text(&mut content, text, *size, rect)?;
        }
        AnnotKind::Square { width } | AnnotKind::Circle { width } => {
            let width = f64::from(*width);
            dict.push(("BS".to_owned(), Value::Dict(vec![("W".to_owned(), Value::Real(width))])));
            emit(&mut content, &[width], b"w")?;
            let inset = width * 0.5;
            let inner = [x0 + inset, y0 + inset, x1 - inset, y1 - inset];
            if matches!(&annot.kind, AnnotKind::Square { .. }) {
                emit(&mut content, &[inner[0], inner[1], inner[2] - inner[0], inner[3] - inner[1]], b"re")?;
                append(&mut content, b"S\n")?;
            } else {
                ellipse(&mut content, inner, b"S")?;
            }
        }
    }
    append(&mut content, b"Q\n")?;
    Ok(AnnotParts { dict, ap_dict, ap_content: content })
}

