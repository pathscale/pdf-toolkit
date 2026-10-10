use std::io;

use super::bounds::*;
use super::{Color, DocumentBuilder, Font, FontId, ImageId, Link, Target};
use crate::{serialize_value, Value};

pub struct PageBuilder<'a> {
    pub(super) doc: &'a mut DocumentBuilder,
    pub(super) index: Option<usize>,
}

fn color_command(color: Color, stroke: bool) -> io::Result<Vec<u8>> {
    let mut out = numbers(&color.values()?)?;
    append(&mut out, if stroke { b"RG\n" } else { b"rg\n" }, MAX_COMMAND_BYTES)?;
    Ok(out)
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

fn win_ansi(character: char) -> u8 {
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
    fn perform(&mut self, work: impl FnOnce(&mut DocumentBuilder, usize) -> io::Result<()>) {
        if self.doc.error.is_some() {
            return;
        }
        let Some(index) = self.index else { return };
        if let Err(error) = work(self.doc, index) {
            self.doc.fail(error);
        }
    }

    pub fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: Color) {
        self.perform(|doc, index| {
            rect(x, y, w, h)?;
            let mut out = copied(b"q\n", MAX_COMMAND_BYTES)?;
            append(&mut out, &color_command(color, false)?, MAX_COMMAND_BYTES)?;
            append(&mut out, &numbers(&[x, y, w, h])?, MAX_COMMAND_BYTES)?;
            append(&mut out, b"re f\nQ\n", MAX_COMMAND_BYTES)?;
            doc.emit(index, &out)
        });
    }

    pub fn stroke_rect(
        &mut self, x: f32, y: f32, w: f32, h: f32, width: f32, color: Color,
    ) {
        self.perform(|doc, index| {
            rect(x, y, w, h)?;
            let mut out = copied(b"q\n", MAX_COMMAND_BYTES)?;
            append(&mut out, &color_command(color, true)?, MAX_COMMAND_BYTES)?;
            append(&mut out, &width_command(width)?, MAX_COMMAND_BYTES)?;
            append(&mut out, &numbers(&[x, y, w, h])?, MAX_COMMAND_BYTES)?;
            append(&mut out, b"re S\nQ\n", MAX_COMMAND_BYTES)?;
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
        &mut self, font: FontId, size: f32, x: f32, y: f32, text: &str, color: Color,
    ) {
        self.perform(|doc, index| {
            check("text_bytes", text.len(), MAX_TEXT_BYTES)?;
            coordinate(size)?;
            if size <= 0.0 {
                return Err(invalid("text size must be positive"));
            }
            let position = numbers(&[x, y])?;
            let color = color_command(color, false)?;
            let font_index = usize::try_from(font.0).map_err(|_| invalid("font identifier overflow"))?;
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
            let text_value = if embedded { Value::HexStr(encoded) } else { Value::Str(encoded) };
            let mut out = copied(b"q\n", MAX_COMMAND_BYTES)?;
            if embedded {
                append(&mut out, b"/Span << /ActualText ", MAX_COMMAND_BYTES)?;
                let actual = serialize_value(&Value::HexStr(unicode(text, MAX_TEXT_BYTES)?))?;
                append(&mut out, &actual, MAX_COMMAND_BYTES)?;
                append(&mut out, b" >> BDC\n", MAX_COMMAND_BYTES)?;
            }
            append(&mut out, &color, MAX_COMMAND_BYTES)?;
            append(&mut out, format!("BT\n/F{} ", font.0).as_bytes(), MAX_COMMAND_BYTES)?;
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
            let page = doc.pages.get(index).ok_or_else(|| invalid("page handle is invalid"))?;
            if !page.fonts.contains(&font.0) {
                doc.charge(64)?;
                doc.pages.get_mut(index).ok_or_else(|| invalid("page handle is invalid"))?
                    .fonts.insert(font.0);
            }
            doc.emit(index, &out)
        });
    }

    pub fn image(&mut self, image: ImageId, x: f32, y: f32, w: f32, h: f32) {
        self.perform(|doc, index| {
            rect(x, y, w, h)?;
            let image_index = usize::try_from(image.0).map_err(|_| invalid("image identifier overflow"))?;
            if doc.images.get(image_index).is_none() {
                return Err(invalid("unknown image identifier"));
            }
            let mut out = copied(b"q\n", MAX_COMMAND_BYTES)?;
            append(&mut out, &numbers(&[w, 0.0, 0.0, h, x, y])?, MAX_COMMAND_BYTES)?;
            append(&mut out, format!("cm\n/I{} Do\nQ\n", image.0).as_bytes(), MAX_COMMAND_BYTES)?;
            let page = doc.pages.get(index).ok_or_else(|| invalid("page handle is invalid"))?;
            if !page.images.contains(&image.0) {
                doc.charge(64)?;
                doc.pages.get_mut(index).ok_or_else(|| invalid("page handle is invalid"))?
                    .images.insert(image.0);
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
            add_link(doc, index, Link { rect: rect(x, y, w, h)?, target: Target::Uri(bytes) })
        });
    }

    pub fn link_page(&mut self, x: f32, y: f32, w: f32, h: f32, page: usize, top: f32) {
        self.perform(|doc, index| {
            coordinate(top)?;
            add_link(
                doc, index,
                Link { rect: rect(x, y, w, h)?, target: Target::Page { page, top } },
            )
        });
    }
}

fn add_link(doc: &mut DocumentBuilder, index: usize, link: Link) -> io::Result<()> {
    let count = add(doc.links, 1)?;
    check("links", count, MAX_LINKS)?;
    let bytes = match &link.target {
        Target::Uri(bytes) => bytes.len(),
        Target::Page { .. } => 0,
    };
    doc.charge(add(bytes, 256)?)?;
    let page = doc.pages.get_mut(index).ok_or_else(|| invalid("page handle is invalid"))?;
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

    pub fn cubic_to(
        &mut self, x1: f32, y1: f32, x2: f32, y2: f32, x3: f32, y3: f32,
    ) -> &mut Self {
        self.segment(&[x1, y1, x2, y2, x3, y3], b"c\n", true);
        self
    }

    pub fn close(&mut self) -> &mut Self {
        self.segment(&[], b"h\n", true);
        self
    }

    fn paint(&mut self, color: Color, width: Option<f32>, even_odd: bool) {
        if self.doc.error.is_some() {
            return;
        }
        let Some(index) = self.index else { return };
        let result = (|| {
            let mut command = copied(b"q\n", MAX_COMMAND_BYTES)?;
            append(&mut command, &color_command(color, width.is_some())?, MAX_COMMAND_BYTES)?;
            if let Some(width) = width {
                append(&mut command, &width_command(width)?, MAX_COMMAND_BYTES)?;
            }
            append(&mut command, &self.bytes, MAX_COMMAND_BYTES)?;
            let operator: &[u8] = if width.is_some() {
                b"S\nQ\n"
            } else if even_odd {
                b"f*\nQ\n"
            } else {
                b"f\nQ\n"
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

    pub fn fill(&mut self, color: Color, even_odd: bool) -> &mut Self {
        self.paint(color, None, even_odd);
        self
    }

    pub fn stroke(&mut self, width: f32, color: Color) -> &mut Self {
        self.paint(color, Some(width), false);
        self
    }
}

