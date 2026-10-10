//! Bounded, deterministic document construction in PDF user space, with y up.
//!
//! Void methods retain their first error; `finish` and `finish_to` return it.
//! Resource IDs belong to the builder that returned them. Pages are registered
//! when `page` is called, so dropping or forgetting a page handle does not lose it.
//! Standard fonts accept WinAnsi characters; other characters become `?`.
//! Embedded text is unshaped and uses one glyph per Unicode scalar. ActualText
//! preserves text when several scalars share a glyph or map to .notdef.
//! Outline levels may start at zero or one, then increase by at most one.
//! `finish_to` buffers the completed PDF before writing to the supplied sink.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::sync::Arc;

mod bounds;
mod emit;
mod font;
mod image;
mod page;
mod subset;

use bounds::*;
use font::TrueType;
use image::Image;
pub use bounds::Refusal;
pub use page::{PageBuilder, PathBuilder};
pub(crate) use page::win_ansi;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
}

impl Color {
    fn values(self) -> io::Result<[f32; 3]> {
        let values = [self.r, self.g, self.b];
        if values.iter().any(|value| !value.is_finite() || !(0.0..=1.0).contains(value)) {
            return Err(invalid("color components must be finite and between zero and one"));
        }
        Ok(values)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct FontId(pub u32);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ImageId(pub u32);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum StandardFont {
    Helvetica,
    HelveticaBold,
    HelveticaOblique,
    TimesRoman,
    TimesBold,
    TimesItalic,
    Courier,
    CourierBold,
}

impl StandardFont {
    fn name(self) -> &'static str {
        match self {
            Self::Helvetica => "Helvetica",
            Self::HelveticaBold => "Helvetica-Bold",
            Self::HelveticaOblique => "Helvetica-Oblique",
            Self::TimesRoman => "Times-Roman",
            Self::TimesBold => "Times-Bold",
            Self::TimesItalic => "Times-Italic",
            Self::Courier => "Courier",
            Self::CourierBold => "Courier-Bold",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WriteOptions {
    pub compress: bool,
    pub object_streams: bool,
    pub version: (u8, u8),
}

impl Default for WriteOptions {
    fn default() -> Self {
        Self { compress: true, object_streams: true, version: (1, 7) }
    }
}

enum Font {
    Standard(StandardFont),
    TrueType(TrueType),
}

struct Page {
    width: f32,
    height: f32,
    content: Vec<u8>,
    fonts: BTreeSet<u32>,
    images: BTreeSet<u32>,
    links: Vec<Link>,
    annotations: Vec<crate::annot::AnnotParts>,
}

enum Target {
    Uri(Vec<u8>),
    Page { page: usize, top: f32 },
}

struct Link {
    rect: [f32; 4],
    target: Target,
}

struct Outline {
    title: Vec<u8>,
    level: u8,
    page: usize,
    y: f32,
}

pub struct DocumentBuilder {
    pages: Vec<Page>,
    fonts: Vec<Font>,
    images: Vec<Image>,
    outlines: Vec<Outline>,
    info: BTreeMap<&'static str, Vec<u8>>,
    bytes: usize,
    operations: usize,
    links: usize,
    annotations: usize,
    error: Option<io::Error>,
}

impl Default for DocumentBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl DocumentBuilder {
    pub fn new() -> Self {
        Self {
            pages: Vec::new(),
            fonts: Vec::new(),
            images: Vec::new(),
            outlines: Vec::new(),
            info: BTreeMap::new(),
            bytes: 0,
            operations: 0,
            links: 0,
            annotations: 0,
            error: None,
        }
    }

    fn fail(&mut self, error: io::Error) {
        if self.error.is_none() {
            self.error = Some(error);
        }
    }

    fn charge(&mut self, bytes: usize) -> io::Result<()> {
        let requested = add(self.bytes, bytes)?;
        check("document_bytes", requested, MAX_BYTES)?;
        self.bytes = requested;
        Ok(())
    }

    fn operation(&mut self) -> io::Result<()> {
        let requested = add(self.operations, 1)?;
        check("content_operations", requested, MAX_OPS)?;
        self.operations = requested;
        Ok(())
    }

    fn emit(&mut self, index: usize, command: &[u8]) -> io::Result<()> {
        check("command_bytes", command.len(), MAX_COMMAND_BYTES)?;
        self.operation()?;
        self.charge(command.len())?;
        let page = self.pages.get_mut(index).ok_or_else(|| invalid("page handle is invalid"))?;
        append(&mut page.content, command, MAX_BYTES)
    }

    pub fn info(&mut self, title: Option<&str>, author: Option<&str>, subject: Option<&str>) {
        if self.error.is_some() {
            return;
        }
        let result = (|| {
            for (key, text) in [("Title", title), ("Author", author), ("Subject", subject)] {
                match text {
                    Some(text) => {
                        let bytes = unicode(text, MAX_STRING_BYTES)?;
                        self.charge(add(bytes.len(), 128)?)?;
                        self.info.insert(key, bytes);
                    }
                    None => {
                        self.info.remove(key);
                    }
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            self.fail(error);
        }
    }

    pub fn standard_font(&mut self, font: StandardFont) -> FontId {
        if self.error.is_some() {
            return FontId(u32::MAX);
        }
        if let Some(index) = self.fonts.iter().position(|entry| {
            matches!(entry, Font::Standard(existing) if *existing == font)
        }) {
            return FontId(index as u32);
        }
        let result = (|| {
            self.charge(256)?;
            let id = u32::try_from(self.fonts.len())
                .map_err(|_| invalid("font identifier overflow"))?;
            push(&mut self.fonts, Font::Standard(font), "fonts", MAX_FONTS)?;
            Ok(FontId(id))
        })();
        match result {
            Ok(id) => id,
            Err(error) => {
                self.fail(error);
                FontId(u32::MAX)
            }
        }
    }

    pub fn truetype_font(&mut self, ttf: Arc<[u8]>) -> io::Result<FontId> {
        if self.error.is_some() {
            return Err(invalid("document builder has a pending error"));
        }
        check("fonts", add(self.fonts.len(), 1)?, MAX_FONTS)?;
        let bytes = ttf.len();
        let font = TrueType::new(ttf)?;
        self.charge(add(bytes, 2 << 20)?)?;
        let id = u32::try_from(self.fonts.len())
            .map_err(|_| invalid("font identifier overflow"))?;
        push(&mut self.fonts, Font::TrueType(font), "fonts", MAX_FONTS)?;
        Ok(FontId(id))
    }

    fn register_image(&mut self, result: io::Result<Image>) -> ImageId {
        if self.error.is_some() {
            return ImageId(u32::MAX);
        }
        let result = result.and_then(|image| {
            self.charge(add(image.bytes(), 256)?)?;
            let id = u32::try_from(self.images.len())
                .map_err(|_| invalid("image identifier overflow"))?;
            push(&mut self.images, image, "images", MAX_IMAGES)?;
            Ok(ImageId(id))
        });
        match result {
            Ok(id) => id,
            Err(error) => {
                self.fail(error);
                ImageId(u32::MAX)
            }
        }
    }

    pub fn jpeg(&mut self, data: Arc<[u8]>, width: u32, height: u32, components: u8) -> ImageId {
        if self.error.is_some() {
            return ImageId(u32::MAX);
        }
        let result = (|| {
            check("images", add(self.images.len(), 1)?, MAX_IMAGES)?;
            check("document_bytes", add(self.bytes, add(data.len(), 256)?)?, MAX_BYTES)?;
            Image::jpeg(data, width, height, components)
        })();
        self.register_image(result)
    }

    pub fn rgba(&mut self, width: u32, height: u32, rgba: &[u8]) -> ImageId {
        if self.error.is_some() {
            return ImageId(u32::MAX);
        }
        let result = (|| {
            check("images", add(self.images.len(), 1)?, MAX_IMAGES)?;
            check("document_bytes", add(self.bytes, add(rgba.len(), 256)?)?, MAX_BYTES)?;
            Image::rgba(width, height, rgba)
        })();
        self.register_image(result)
    }

    pub fn page(&mut self, width: f32, height: f32) -> PageBuilder<'_> {
        let result = (|| {
            if self.error.is_some() {
                return Err(invalid("document builder has a pending error"));
            }
            coordinate(width)?;
            coordinate(height)?;
            if width <= 0.0 || height <= 0.0 {
                return Err(invalid("page dimensions must be positive"));
            }
            self.charge(256)?;
            let index = self.pages.len();
            push(
                &mut self.pages,
                Page {
                    width, height, content: Vec::new(),
                    fonts: BTreeSet::new(), images: BTreeSet::new(), links: Vec::new(),
                    annotations: Vec::new(),
                },
                "pages", MAX_PAGES,
            )?;
            Ok(index)
        })();
        let index = match result {
            Ok(index) => Some(index),
            Err(error) => {
                self.fail(error);
                None
            }
        };
        PageBuilder { doc: self, index }
    }

    pub fn outline(&mut self, title: &str, level: u8, page: usize, y: f32) {
        if self.error.is_some() {
            return;
        }
        let result = (|| {
            coordinate(y)?;
            let base = self.outlines.first().map(|item| item.level).unwrap_or(level);
            if base > 1 || level < base {
                return Err(invalid("outline levels must start at zero or one"));
            }
            check("outline_depth", add(usize::from(level - base), 1)?, MAX_DEPTH)?;
            if let Some(previous) = self.outlines.last() {
                if usize::from(level) > add(usize::from(previous.level), 1)? {
                    return Err(invalid("outline levels cannot skip a parent"));
                }
            }
            let title = unicode(title, MAX_STRING_BYTES)?;
            self.charge(add(title.len(), 256)?)?;
            push(
                &mut self.outlines,
                Outline { title, level, page, y },
                "outline_items", MAX_OUTLINES,
            )
        })();
        if let Err(error) = result {
            self.fail(error);
        }
    }

    pub fn finish(self, options: &WriteOptions) -> io::Result<Vec<u8>> {
        emit::finish(self, options)
    }

    pub fn finish_to(
        self,
        options: &WriteOptions,
        sink: &mut dyn io::Write,
    ) -> io::Result<u64> {
        let bytes = self.finish(options)?;
        let length = u64::try_from(bytes.len()).map_err(|_| invalid("output length overflow"))?;
        sink.write_all(&bytes)?;
        Ok(length)
    }
}

