use std::io;

use super::bounds::*;
use super::{Color, Destination, DocumentBuilder, Font, FontId, ImageId, Link, Target};
use crate::{Value, serialize_value};

/// Nesting limit for `save`, matching common reader limits for `q`.
const MAX_SAVE_DEPTH: usize = 28;

pub struct PageBuilder<'a> {
    pub(super) doc: &'a mut DocumentBuilder,
    pub(super) index: Option<usize>,
}

/// A fill or stroke colour: DeviceRGB or DeviceGray, components in 0..=1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Paint {
    Rgb(Color),
    Gray(f32),
}

impl Paint {
    pub const BLACK: Paint = Paint::Gray(0.0);
    pub const WHITE: Paint = Paint::Gray(1.0);

    pub const fn gray(level: f32) -> Self {
        Self::Gray(level)
    }

    pub const fn rgb(r: f32, g: f32, b: f32) -> Self {
        Self::Rgb(Color { r, g, b })
    }

    pub(super) fn command(self, stroke: bool) -> io::Result<Vec<u8>> {
        match self {
            Self::Rgb(color) => {
                let mut out = numbers(&color.values()?)?;
                append(
                    &mut out,
                    if stroke { b"RG\n" } else { b"rg\n" },
                    MAX_COMMAND_BYTES,
                )?;
                Ok(out)
            }
            Self::Gray(level) => {
                if !level.is_finite() || !(0.0..=1.0).contains(&level) {
                    return Err(invalid(
                        "gray level must be finite and between zero and one",
                    ));
                }
                let mut out = numbers(&[level])?;
                append(
                    &mut out,
                    if stroke { b"G\n" } else { b"g\n" },
                    MAX_COMMAND_BYTES,
                )?;
                Ok(out)
            }
        }
    }
}

impl From<Color> for Paint {
    fn from(color: Color) -> Self {
        Self::Rgb(color)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LineCap {
    Butt,
    Round,
    Square,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LineJoin {
    Miter,
    Round,
    Bevel,
}

fn width_command(width: f32) -> io::Result<Vec<u8>> {
    coordinate(width)?;
    if width < 0.0 {
        return Err(invalid("stroke width must be nonnegative"));
    }
    let mut out = numbers(&[width])?;
    append(&mut out, b"w\n", MAX_COMMAND_BYTES)?;
    Ok(out)
}

pub(crate) fn win_ansi(character: char) -> u8 {
    match character {
        '\u{20}'..='\u{7e}' | '\u{a0}'..='\u{ff}' => character as u8,
        '\u{20ac}' => 0x80,
        '\u{201a}' => 0x82,
        '\u{0192}' => 0x83,
        '\u{201e}' => 0x84,
        '\u{2026}' => 0x85,
        '\u{2020}' => 0x86,
        '\u{2021}' => 0x87,
        '\u{02c6}' => 0x88,
        '\u{2030}' => 0x89,
        '\u{0160}' => 0x8a,
        '\u{2039}' => 0x8b,
        '\u{0152}' => 0x8c,
        '\u{017d}' => 0x8e,
        '\u{2018}' => 0x91,
        '\u{2019}' => 0x92,
        '\u{201c}' => 0x93,
        '\u{201d}' => 0x94,
        '\u{2022}' => 0x95,
        '\u{2013}' => 0x96,
        '\u{2014}' => 0x97,
        '\u{02dc}' => 0x98,
        '\u{2122}' => 0x99,
        '\u{0161}' => 0x9a,
        '\u{203a}' => 0x9b,
        '\u{0153}' => 0x9c,
        '\u{017e}' => 0x9e,
        '\u{0178}' => 0x9f,
        _ => b'?',
    }
}

impl PageBuilder<'_> {
    pub(super) fn perform(
        &mut self,
        work: impl FnOnce(&mut DocumentBuilder, usize) -> io::Result<()>,
    ) {
        if self.doc.error.is_some() {
            return;
        }
        let Some(index) = self.index else { return };
        if let Err(error) = work(self.doc, index) {
            self.doc.fail(error);
        }
    }

    /// Zero-based index of this page, or None when the page was refused.
    pub fn index(&self) -> Option<usize> {
        self.index
    }

    /// MediaBox width and height in points.
    pub fn size(&self) -> (f32, f32) {
        self.index
            .and_then(|index| self.doc.pages.get(index))
            .map_or((0.0, 0.0), |page| (page.width, page.height))
    }

    /// The area inside the margins, as `[x, y, width, height]`.
    pub fn content_box(&self) -> [f32; 4] {
        self.index
            .and_then(|index| self.doc.pages.get(index))
            .map_or([0.0; 4], |page| {
                let margins = page.margins;
                [
                    margins.left,
                    margins.bottom,
                    page.width - margins.left - margins.right,
                    page.height - margins.top - margins.bottom,
                ]
            })
    }

    /// Adds a printable annotation with a self-contained normal appearance.
    pub fn annotation(&mut self, annot: &crate::annot::Annot) {
        self.perform(|doc, index| {
            let count = add(doc.annotations, 1)?;
            check("annotations", add(count, doc.links)?, MAX_LINKS)?;
            let parts = crate::annot::build(annot).map_err(invalid)?;
            let dictionary = serialize_value(&Value::Dict(parts.dict.clone()))?.len();
            let appearance = serialize_value(&Value::Dict(parts.ap_dict.clone()))?.len();
            // Include retained Value storage, not just its serialized bytes.
            let bytes = add(
                add(
                    mul(add(dictionary, appearance)?, 16)?,
                    parts.ap_content.len(),
                )?,
                1024,
            )?;
            doc.charge(bytes)?;
            let page = doc
                .pages
                .get_mut(index)
                .ok_or_else(|| invalid("page handle is invalid"))?;
            push(&mut page.annotations, parts, "annotations", MAX_LINKS)?;
            doc.annotations = count;
            Ok(())
        });
    }

    /// Saves the graphics state (`q`). Saves left open are closed when the
    /// document is finished.
    pub fn save(&mut self) {
        self.perform(|doc, index| {
            let page = doc
                .pages
                .get(index)
                .ok_or_else(|| invalid("page handle is invalid"))?;
            check("graphics_state_depth", add(page.depth, 1)?, MAX_SAVE_DEPTH)?;
            doc.emit(index, b"q\n")?;
            let page = doc
                .pages
                .get_mut(index)
                .ok_or_else(|| invalid("page handle is invalid"))?;
            page.depth = add(page.depth, 1)?;
            Ok(())
        });
    }

    /// Restores the graphics state saved by the matching `save` (`Q`).
    pub fn restore(&mut self) {
        self.perform(|doc, index| {
            let page = doc
                .pages
                .get(index)
                .ok_or_else(|| invalid("page handle is invalid"))?;
            if page.depth == 0 {
                return Err(invalid("restore without a matching save"));
            }
            doc.emit(index, b"Q\n")?;
            let page = doc
                .pages
                .get_mut(index)
                .ok_or_else(|| invalid("page handle is invalid"))?;
            page.depth -= 1;
            Ok(())
        });
    }

    fn state(&mut self, values: &[f32], operator: &[u8]) {
        self.perform(|doc, index| {
            let mut out = numbers(values)?;
            append(&mut out, operator, MAX_COMMAND_BYTES)?;
            doc.emit(index, &out)
        });
    }

    /// Concatenates `[a b c d e f]` to the current transformation matrix.
    pub fn transform(&mut self, matrix: [f32; 6]) {
        let [a, b, c, d, _, _] = matrix;
        if (f64::from(a) * f64::from(d) - f64::from(b) * f64::from(c)).abs() < 1e-12 {
            self.perform(|_, _| Err(invalid("transformation matrix is not invertible")));
            return;
        }
        self.state(&matrix, b"cm\n");
    }

    pub fn translate(&mut self, x: f32, y: f32) {
        self.transform([1.0, 0.0, 0.0, 1.0, x, y]);
    }

    pub fn scale(&mut self, x: f32, y: f32) {
        self.transform([x, 0.0, 0.0, y, 0.0, 0.0]);
    }

    /// Rotates counterclockwise by `degrees` about the current origin.
    pub fn rotate(&mut self, degrees: f32) {
        let radians = f64::from(degrees).to_radians();
        let (sin, cos) = (radians.sin() as f32, radians.cos() as f32);
        self.transform([cos, sin, -sin, cos, 0.0, 0.0]);
    }

    /// Sets the line width for later strokes that do not pass their own.
    pub fn line_width(&mut self, width: f32) {
        self.perform(|doc, index| doc.emit(index, &width_command(width)?));
    }

    /// Sets a dash pattern of alternating on and off lengths. An empty
    /// pattern draws solid lines.
    pub fn dash(&mut self, pattern: &[f32], phase: f32) {
        self.perform(|doc, index| {
            check("dash_entries", pattern.len(), 32)?;
            if pattern.iter().any(|value| *value < 0.0) || phase < 0.0 {
                return Err(invalid("dash lengths and phase must be nonnegative"));
            }
            if !pattern.is_empty() && pattern.iter().all(|value| *value == 0.0) {
                return Err(invalid("dash pattern lengths cannot all be zero"));
            }
            let mut out = copied(b"[", MAX_COMMAND_BYTES)?;
            append(&mut out, &numbers(pattern)?, MAX_COMMAND_BYTES)?;
            append(&mut out, b"] ", MAX_COMMAND_BYTES)?;
            append(&mut out, &numbers(&[phase])?, MAX_COMMAND_BYTES)?;
            append(&mut out, b"d\n", MAX_COMMAND_BYTES)?;
            doc.emit(index, &out)
        });
    }

    pub fn line_cap(&mut self, cap: LineCap) {
        let value = match cap {
            LineCap::Butt => b"0 J\n",
            LineCap::Round => b"1 J\n",
            LineCap::Square => b"2 J\n",
        };
        self.perform(|doc, index| doc.emit(index, value));
    }

    pub fn line_join(&mut self, join: LineJoin) {
        let value = match join {
            LineJoin::Miter => b"0 j\n",
            LineJoin::Round => b"1 j\n",
            LineJoin::Bevel => b"2 j\n",
        };
        self.perform(|doc, index| doc.emit(index, value));
    }

    /// Intersects the clipping path with a rectangle. The clip lasts until
    /// the enclosing `restore`, so wrap it in `save` and `restore`.
    pub fn clip_rect(&mut self, x: f32, y: f32, w: f32, h: f32) {
        self.perform(|doc, index| {
            rect(x, y, w, h)?;
            let mut out = numbers(&[x, y, w, h])?;
            append(&mut out, b"re W n\n", MAX_COMMAND_BYTES)?;
            doc.emit(index, &out)
        });
    }

    pub fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: impl Into<Paint>) {
        let paint = color.into();
        self.perform(|doc, index| {
            rect(x, y, w, h)?;
            let mut out = copied(b"q\n", MAX_COMMAND_BYTES)?;
            append(&mut out, &paint.command(false)?, MAX_COMMAND_BYTES)?;
            append(&mut out, &numbers(&[x, y, w, h])?, MAX_COMMAND_BYTES)?;
            append(&mut out, b"re f\nQ\n", MAX_COMMAND_BYTES)?;
            doc.emit(index, &out)
        });
    }

    pub fn stroke_rect(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        width: f32,
        color: impl Into<Paint>,
    ) {
        let paint = color.into();
        self.perform(|doc, index| {
            rect(x, y, w, h)?;
            let mut out = copied(b"q\n", MAX_COMMAND_BYTES)?;
            append(&mut out, &paint.command(true)?, MAX_COMMAND_BYTES)?;
            append(&mut out, &width_command(width)?, MAX_COMMAND_BYTES)?;
            append(&mut out, &numbers(&[x, y, w, h])?, MAX_COMMAND_BYTES)?;
            append(&mut out, b"re S\nQ\n", MAX_COMMAND_BYTES)?;
            doc.emit(index, &out)
        });
    }

    /// Strokes a straight line using the current dash pattern, cap and join.
    pub fn line(
        &mut self,
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        width: f32,
        color: impl Into<Paint>,
    ) {
        let paint = color.into();
        self.perform(|doc, index| {
            let mut out = copied(b"q\n", MAX_COMMAND_BYTES)?;
            append(&mut out, &paint.command(true)?, MAX_COMMAND_BYTES)?;
            append(&mut out, &width_command(width)?, MAX_COMMAND_BYTES)?;
            append(&mut out, &numbers(&[x0, y0])?, MAX_COMMAND_BYTES)?;
            append(&mut out, b"m\n", MAX_COMMAND_BYTES)?;
            append(&mut out, &numbers(&[x1, y1])?, MAX_COMMAND_BYTES)?;
            append(&mut out, b"l\nS\nQ\n", MAX_COMMAND_BYTES)?;
            doc.emit(index, &out)
        });
    }

    pub fn path(&mut self) -> PathBuilder<'_> {
        PathBuilder {
            doc: &mut *self.doc,
            index: self.index,
            bytes: Vec::new(),
            current: false,
        }
    }

    pub fn text(
        &mut self,
        font: FontId,
        size: f32,
        x: f32,
        y: f32,
        text: &str,
        color: impl Into<Paint>,
    ) {
        let paint = color.into();
        self.perform(|doc, index| {
            check("text_bytes", text.len(), MAX_TEXT_BYTES)?;
            coordinate(size)?;
            if size <= 0.0 {
                return Err(invalid("text size must be positive"));
            }
            let position = numbers(&[x, y])?;
            let color = paint.command(false)?;
            let font_index =
                usize::try_from(font.0).map_err(|_| invalid("font identifier overflow"))?;
            let embedded = matches!(doc.fonts.get(font_index), Some(Font::TrueType(_)));
            let capacity = mul(text.len(), if embedded { 2 } else { 1 })?;
            doc.charge(add(capacity, mul(text.chars().count(), 96)?)?)?;
            let mut encoded = Vec::new();
            match doc.fonts.get_mut(font_index) {
                Some(Font::Standard(_)) => {
                    for character in text.chars() {
                        append(&mut encoded, &[win_ansi(character)], capacity)?;
                    }
                }
                Some(Font::TrueType(font)) => {
                    for character in text.chars() {
                        let glyph = font.glyph(character)?;
                        font.used.entry(glyph).or_insert(character);
                        append(&mut encoded, &glyph.to_be_bytes(), capacity)?;
                    }
                }
                None => return Err(invalid("unknown font identifier")),
            }
            let text_value = if embedded {
                Value::HexStr(encoded)
            } else {
                Value::Str(encoded)
            };
            let mut out = copied(b"q\n", MAX_COMMAND_BYTES)?;
            if embedded {
                append(&mut out, b"/Span << /ActualText ", MAX_COMMAND_BYTES)?;
                let actual = serialize_value(&Value::HexStr(unicode(text, MAX_TEXT_BYTES)?))?;
                append(&mut out, &actual, MAX_COMMAND_BYTES)?;
                append(&mut out, b" >> BDC\n", MAX_COMMAND_BYTES)?;
            }
            append(&mut out, &color, MAX_COMMAND_BYTES)?;
            append(
                &mut out,
                format!("BT\n/F{} ", font.0).as_bytes(),
                MAX_COMMAND_BYTES,
            )?;
            append(&mut out, &numbers(&[size])?, MAX_COMMAND_BYTES)?;
            append(&mut out, b"Tf\n1 0 0 1 ", MAX_COMMAND_BYTES)?;
            append(&mut out, &position, MAX_COMMAND_BYTES)?;
            append(&mut out, b"Tm\n", MAX_COMMAND_BYTES)?;
            append(&mut out, &serialize_value(&text_value)?, MAX_COMMAND_BYTES)?;
            append(&mut out, b" Tj\nET\n", MAX_COMMAND_BYTES)?;
            if embedded {
                append(&mut out, b"EMC\n", MAX_COMMAND_BYTES)?;
            }
            append(&mut out, b"Q\n", MAX_COMMAND_BYTES)?;
            use_font(doc, index, font)?;
            doc.emit(index, &out)
        });
    }

    pub fn image(&mut self, image: ImageId, x: f32, y: f32, w: f32, h: f32) {
        self.perform(|doc, index| {
            rect(x, y, w, h)?;
            let image_index =
                usize::try_from(image.0).map_err(|_| invalid("image identifier overflow"))?;
            if doc.images.get(image_index).is_none() {
                return Err(invalid("unknown image identifier"));
            }
            let mut out = copied(b"q\n", MAX_COMMAND_BYTES)?;
            append(
                &mut out,
                &numbers(&[w, 0.0, 0.0, h, x, y])?,
                MAX_COMMAND_BYTES,
            )?;
            append(
                &mut out,
                format!("cm\n/I{} Do\nQ\n", image.0).as_bytes(),
                MAX_COMMAND_BYTES,
            )?;
            let page = doc
                .pages
                .get(index)
                .ok_or_else(|| invalid("page handle is invalid"))?;
            if !page.images.contains(&image.0) {
                doc.charge(64)?;
                doc.pages
                    .get_mut(index)
                    .ok_or_else(|| invalid("page handle is invalid"))?
                    .images
                    .insert(image.0);
            }
            doc.emit(index, &out)
        });
    }

    /// Non-ASCII UTF-8 bytes are percent-encoded. Control characters and spaces
    /// are refused. The URI is written as data and is never followed.
    pub fn link_uri(&mut self, x: f32, y: f32, w: f32, h: f32, uri: &str) {
        self.perform(|doc, index| {
            check("uri_bytes", uri.len(), 16 << 10)?;
            let mut bytes = Vec::new();
            for byte in uri.bytes() {
                if byte <= 32 || byte == 127 {
                    return Err(invalid("URI contains whitespace or a control character"));
                }
                if byte >= 128 {
                    append(&mut bytes, format!("%{byte:02X}").as_bytes(), 48 << 10)?;
                } else {
                    append(&mut bytes, &[byte], 48 << 10)?;
                }
            }
            add_link(
                doc,
                index,
                Link {
                    rect: rect(x, y, w, h)?,
                    target: Target::Uri(bytes),
                },
            )
        });
    }

    pub fn link_page(&mut self, x: f32, y: f32, w: f32, h: f32, page: usize, top: f32) {
        self.link_to(
            x,
            y,
            w,
            h,
            Destination::Xyz {
                page,
                left: None,
                top: Some(top),
                zoom: None,
            },
        );
    }

    /// Adds an internal link. The destination page may be added later; it is
    /// checked when the document is finished.
    pub fn link_to(&mut self, x: f32, y: f32, w: f32, h: f32, destination: Destination) {
        self.perform(|doc, index| {
            destination.validate()?;
            add_link(
                doc,
                index,
                Link {
                    rect: rect(x, y, w, h)?,
                    target: Target::Destination(destination),
                },
            )
        });
    }
}

pub(super) fn use_font(doc: &mut DocumentBuilder, index: usize, font: FontId) -> io::Result<()> {
    let page = doc
        .pages
        .get(index)
        .ok_or_else(|| invalid("page handle is invalid"))?;
    if !page.fonts.contains(&font.0) {
        doc.charge(64)?;
        doc.pages
            .get_mut(index)
            .ok_or_else(|| invalid("page handle is invalid"))?
            .fonts
            .insert(font.0);
    }
    Ok(())
}

fn add_link(doc: &mut DocumentBuilder, index: usize, link: Link) -> io::Result<()> {
    let count = add(doc.links, 1)?;
    check("links", add(count, doc.annotations)?, MAX_LINKS)?;
    let bytes = match &link.target {
        Target::Uri(bytes) => bytes.len(),
        Target::Destination(_) => 0,
    };
    doc.charge(add(bytes, 256)?)?;
    let page = doc
        .pages
        .get_mut(index)
        .ok_or_else(|| invalid("page handle is invalid"))?;
    push(&mut page.links, link, "links", MAX_LINKS)?;
    doc.links = count;
    Ok(())
}

/// Unpainted paths are discarded. Each paint consumes the current path.
/// Methods return the handle for chaining.
pub struct PathBuilder<'a> {
    doc: &'a mut DocumentBuilder,
    index: Option<usize>,
    bytes: Vec<u8>,
    current: bool,
}

impl PathBuilder<'_> {
    fn segment(&mut self, values: &[f32], operator: &[u8], needs_current: bool) {
        if self.doc.error.is_some() {
            return;
        }
        let result = (|| {
            if needs_current && !self.current {
                return Err(invalid("path segment requires move_to"));
            }
            self.doc.operation()?;
            let mut command = numbers(values)?;
            append(&mut command, operator, MAX_COMMAND_BYTES)?;
            self.doc.charge(command.len())?;
            append(&mut self.bytes, &command, MAX_COMMAND_BYTES)?;
            self.current = true;
            Ok(())
        })();
        if let Err(error) = result {
            self.doc.fail(error);
        }
    }

    pub fn move_to(&mut self, x: f32, y: f32) -> &mut Self {
        self.segment(&[x, y], b"m\n", false);
        self
    }

    pub fn line_to(&mut self, x: f32, y: f32) -> &mut Self {
        self.segment(&[x, y], b"l\n", true);
        self
    }

    pub fn cubic_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x3: f32, y3: f32) -> &mut Self {
        self.segment(&[x1, y1, x2, y2, x3, y3], b"c\n", true);
        self
    }

    /// Adds a closed rectangle subpath.
    pub fn rect(&mut self, x: f32, y: f32, w: f32, h: f32) -> &mut Self {
        if let Err(error) = rect(x, y, w, h) {
            if self.doc.error.is_none() {
                self.doc.fail(error);
            }
            return self;
        }
        self.segment(&[x, y, w, h], b"re\n", false);
        self
    }

    pub fn close(&mut self) -> &mut Self {
        self.segment(&[], b"h\n", true);
        self
    }

    fn paint(&mut self, fill: Option<Paint>, stroke: Option<(f32, Paint)>, even_odd: bool) {
        if self.doc.error.is_some() {
            return;
        }
        let Some(index) = self.index else { return };
        let result = (|| {
            let mut command = copied(b"q\n", MAX_COMMAND_BYTES)?;
            if let Some(fill) = fill {
                append(&mut command, &fill.command(false)?, MAX_COMMAND_BYTES)?;
            }
            if let Some((width, stroke)) = stroke {
                append(&mut command, &stroke.command(true)?, MAX_COMMAND_BYTES)?;
                append(&mut command, &width_command(width)?, MAX_COMMAND_BYTES)?;
            }
            append(&mut command, &self.bytes, MAX_COMMAND_BYTES)?;
            let operator: &[u8] = match (fill.is_some(), stroke.is_some(), even_odd) {
                (true, true, false) => b"B\nQ\n",
                (true, true, true) => b"B*\nQ\n",
                (false, true, _) => b"S\nQ\n",
                (true, false, true) => b"f*\nQ\n",
                _ => b"f\nQ\n",
            };
            append(&mut command, operator, MAX_COMMAND_BYTES)?;
            self.doc.emit(index, &command)?;
            self.bytes.clear();
            self.current = false;
            Ok(())
        })();
        if let Err(error) = result {
            self.doc.fail(error);
        }
    }

    pub fn fill(&mut self, color: impl Into<Paint>, even_odd: bool) -> &mut Self {
        self.paint(Some(color.into()), None, even_odd);
        self
    }

    pub fn stroke(&mut self, width: f32, color: impl Into<Paint>) -> &mut Self {
        self.paint(None, Some((width, color.into())), false);
        self
    }

    pub fn fill_stroke(
        &mut self,
        fill: impl Into<Paint>,
        width: f32,
        stroke: impl Into<Paint>,
        even_odd: bool,
    ) -> &mut Self {
        self.paint(Some(fill.into()), Some((width, stroke.into())), even_odd);
        self
    }

    /// Intersects the clipping path with this path and consumes it without
    /// painting. The clip lasts until the enclosing `PageBuilder::restore`.
    pub fn clip(&mut self, even_odd: bool) -> &mut Self {
        if self.doc.error.is_some() {
            return self;
        }
        let Some(index) = self.index else { return self };
        let result = (|| {
            if !self.current {
                return Err(invalid("clip requires a path"));
            }
            let mut command = copied(&self.bytes, MAX_COMMAND_BYTES)?;
            append(
                &mut command,
                if even_odd { b"W* n\n" } else { b"W n\n" },
                MAX_COMMAND_BYTES,
            )?;
            self.doc.emit(index, &command)?;
            self.bytes.clear();
            self.current = false;
            Ok(())
        })();
        if let Err(error) = result {
            self.doc.fail(error);
        }
        self
    }
}
