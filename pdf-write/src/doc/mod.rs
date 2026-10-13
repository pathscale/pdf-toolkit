//! Bounded, deterministic document construction in PDF user space, with y up.
//!
//! Void methods retain their first error; `finish` and `finish_to` return it.
//! Resource IDs belong to the builder that returned them. Pages are registered
//! when `page` is called, so dropping or forgetting a page handle does not lose it.
//! Standard fonts accept WinAnsi characters; other characters become `?`.
//! Their widths come from Adobe's Core 14 metrics; they have no kerning.
//! Embedded text is unshaped and uses one glyph per Unicode scalar: there is no
//! complex-script shaping, no bidirectional reordering and no ligature
//! substitution. Kerning comes from a TrueType `kern` table when present; GPOS
//! kerning is not read. ActualText preserves text when several scalars share a
//! glyph or map to .notdef.
//! Outline levels may start at zero or one, then increase by at most one.
//! `finish_to` buffers the completed PDF before writing to the supplied sink.
//! Identical input produces identical bytes; nothing reads the clock, and
//! dates are written only when the caller supplies them.
//!
//! `encrypt` selects the private PSChaCha security handler. Encrypted output
//! opens only in pdf-toolkit readers (the pdf-toolkit viewer and chuzz); other
//! PDF readers refuse it. See docs/encryption.md.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::sync::Arc;

mod bounds;
mod emit;
mod font;
mod image;
mod page;
mod png;
mod subset;
mod text;

pub use crate::encryption::KdfParams;
pub use bounds::Refusal;
use bounds::*;
use font::TrueType;
use image::Image;
pub(crate) use page::win_ansi;
pub use page::{LineCap, LineJoin, PageBuilder, Paint, PathBuilder};
pub use text::{Align, Flow, TextStyle};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
}

impl Color {
    pub const BLACK: Color = Color {
        r: 0.0,
        g: 0.0,
        b: 0.0,
    };
    pub const WHITE: Color = Color {
        r: 1.0,
        g: 1.0,
        b: 1.0,
    };

    pub const fn rgb(r: f32, g: f32, b: f32) -> Self {
        Self { r, g, b }
    }

    fn values(self) -> io::Result<[f32; 3]> {
        let values = [self.r, self.g, self.b];
        if values
            .iter()
            .any(|value| !value.is_finite() || !(0.0..=1.0).contains(value))
        {
            return Err(invalid(
                "color components must be finite and between zero and one",
            ));
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

    /// Ascender and descender in 1/1000 em, from the Core 14 AFM headers.
    fn vertical(self) -> (f32, f32) {
        match self {
            Self::Helvetica | Self::HelveticaOblique => (718.0, -207.0),
            Self::HelveticaBold => (718.0, -207.0),
            Self::TimesRoman => (683.0, -217.0),
            Self::TimesBold => (683.0, -217.0),
            Self::TimesItalic => (683.0, -217.0),
            Self::Courier | Self::CourierBold => (629.0, -157.0),
        }
    }
}

/// Page size presets in points (1/72 inch), portrait.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PageSize {
    Letter,
    Legal,
    Tabloid,
    A3,
    A4,
    A5,
    B5,
    Custom { width: f32, height: f32 },
}

impl PageSize {
    pub fn dimensions(self) -> (f32, f32) {
        match self {
            Self::Letter => (612.0, 792.0),
            Self::Legal => (612.0, 1008.0),
            Self::Tabloid => (792.0, 1224.0),
            Self::A3 => (841.89, 1190.551),
            Self::A4 => (595.276, 841.89),
            Self::A5 => (419.528, 595.276),
            Self::B5 => (498.898, 708.661),
            Self::Custom { width, height } => (width, height),
        }
    }

    /// The same size with width and height exchanged.
    pub fn landscape(self) -> Self {
        let (width, height) = self.dimensions();
        Self::Custom {
            width: height,
            height: width,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Margins {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub left: f32,
}

impl Margins {
    pub const fn uniform(value: f32) -> Self {
        Self {
            top: value,
            right: value,
            bottom: value,
            left: value,
        }
    }
}

/// A page's MediaBox comes from `size`, with its origin at zero. Optional
/// boxes are `[x0, y0, x1, y1]` and must lie within the MediaBox. Margins do
/// not change the PDF; they define `PageBuilder::content_box` for layout.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PageSetup {
    pub size: PageSize,
    pub margins: Margins,
    pub crop_box: Option<[f32; 4]>,
    pub bleed_box: Option<[f32; 4]>,
    pub trim_box: Option<[f32; 4]>,
    pub art_box: Option<[f32; 4]>,
}

impl Default for PageSetup {
    fn default() -> Self {
        Self::new(PageSize::Letter)
    }
}

impl PageSetup {
    pub fn new(size: PageSize) -> Self {
        Self {
            size,
            margins: Margins::default(),
            crop_box: None,
            bleed_box: None,
            trim_box: None,
            art_box: None,
        }
    }

    pub fn margins(mut self, margins: Margins) -> Self {
        self.margins = margins;
        self
    }

    pub fn crop_box(mut self, rect: [f32; 4]) -> Self {
        self.crop_box = Some(rect);
        self
    }

    pub fn bleed_box(mut self, rect: [f32; 4]) -> Self {
        self.bleed_box = Some(rect);
        self
    }

    pub fn trim_box(mut self, rect: [f32; 4]) -> Self {
        self.trim_box = Some(rect);
        self
    }

    pub fn art_box(mut self, rect: [f32; 4]) -> Self {
        self.art_box = Some(rect);
        self
    }
}

/// A PDF date. The builder never reads the clock; callers pass dates in.
/// `offset_minutes` is the local offset from UTC, between -1439 and 1439.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PdfDate {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    pub offset_minutes: i16,
}

impl PdfDate {
    pub const fn utc(year: u16, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> Self {
        Self {
            year,
            month,
            day,
            hour,
            minute,
            second,
            offset_minutes: 0,
        }
    }

    /// `D:YYYYMMDDHHmmSSZ` or `D:YYYYMMDDHHmmSS+HH'mm'`.
    pub fn to_pdf_string(&self) -> io::Result<String> {
        let leap = self.year.is_multiple_of(4)
            && (!self.year.is_multiple_of(100) || self.year.is_multiple_of(400));
        let days = match self.month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if leap => 29,
            2 => 28,
            _ => return Err(invalid("date month must be between 1 and 12")),
        };
        if self.year > 9999
            || self.day == 0
            || self.day > days
            || self.hour > 23
            || self.minute > 59
            || self.second > 59
            || self.offset_minutes.abs() > 1439
        {
            return Err(invalid("date field is out of range"));
        }
        let mut text = format!(
            "D:{:04}{:02}{:02}{:02}{:02}{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second,
        );
        if self.offset_minutes == 0 {
            text.push('Z');
        } else {
            let sign = if self.offset_minutes < 0 { '-' } else { '+' };
            let minutes = self.offset_minutes.unsigned_abs();
            text.push_str(&format!("{sign}{:02}'{:02}'", minutes / 60, minutes % 60));
        }
        Ok(text)
    }
}

/// Document information. `None` leaves an entry out.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Metadata {
    pub title: Option<String>,
    pub author: Option<String>,
    pub subject: Option<String>,
    pub keywords: Option<String>,
    pub creator: Option<String>,
    pub producer: Option<String>,
    pub creation_date: Option<PdfDate>,
    pub modification_date: Option<PdfDate>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WriteOptions {
    pub compress: bool,
    pub object_streams: bool,
    pub version: (u8, u8),
}

impl Default for WriteOptions {
    fn default() -> Self {
        Self {
            compress: true,
            object_streams: true,
            version: (1, 7),
        }
    }
}

enum Font {
    Standard(StandardFont),
    TrueType(TrueType),
}

struct Page {
    width: f32,
    height: f32,
    boxes: [Option<[f32; 4]>; 4],
    margins: Margins,
    depth: usize,
    content: Vec<u8>,
    fonts: BTreeSet<u32>,
    images: BTreeSet<u32>,
    links: Vec<Link>,
    annotations: Vec<crate::annot::AnnotParts>,
}

/// A destination inside the document. Page indices count from zero.
/// `None` coordinates keep the viewer's current value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Destination {
    /// Scroll to a position, optionally changing the zoom (1.0 is 100 percent).
    Xyz {
        page: usize,
        left: Option<f32>,
        top: Option<f32>,
        zoom: Option<f32>,
    },
    /// Fit the whole page in the window.
    Fit { page: usize },
    /// Fit the page width, with `top` at the top of the window.
    FitWidth { page: usize, top: Option<f32> },
}

impl Destination {
    fn page(&self) -> usize {
        match self {
            Self::Xyz { page, .. } | Self::Fit { page } | Self::FitWidth { page, .. } => *page,
        }
    }

    fn validate(&self) -> io::Result<()> {
        match self {
            Self::Xyz {
                left, top, zoom, ..
            } => {
                for value in [left, top].into_iter().flatten() {
                    coordinate(*value)?;
                }
                if let Some(zoom) = zoom {
                    if !zoom.is_finite() || *zoom <= 0.0 || *zoom > 64.0 {
                        return Err(invalid("destination zoom must be positive and at most 64"));
                    }
                }
                Ok(())
            }
            Self::Fit { .. } => Ok(()),
            Self::FitWidth { top, .. } => top.map_or(Ok(()), coordinate),
        }
    }
}

enum Target {
    Uri(Vec<u8>),
    Destination(Destination),
}

struct Link {
    rect: [f32; 4],
    target: Target,
}

struct Outline {
    title: Vec<u8>,
    level: u8,
    destination: Destination,
}

struct Encrypt {
    password: Vec<u8>,
    costs: Option<KdfParams>,
    fixture: bool,
}

pub struct DocumentBuilder {
    pages: Vec<Page>,
    fonts: Vec<Font>,
    images: Vec<Image>,
    outlines: Vec<Outline>,
    info: BTreeMap<&'static str, Vec<u8>>,
    encrypt: Option<Encrypt>,
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
            encrypt: None,
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
        let page = self
            .pages
            .get_mut(index)
            .ok_or_else(|| invalid("page handle is invalid"))?;
        append(&mut page.content, command, MAX_BYTES)
    }

    /// The first error retained by a void method, if any.
    pub fn error(&self) -> Option<&io::Error> {
        self.error.as_ref()
    }

    pub fn info(&mut self, title: Option<&str>, author: Option<&str>, subject: Option<&str>) {
        if self.error.is_some() {
            return;
        }
        let result = (|| {
            for (key, text) in [("Title", title), ("Author", author), ("Subject", subject)] {
                self.set_info_text(key, text)?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            self.fail(error);
        }
    }

    fn set_info_text(&mut self, key: &'static str, text: Option<&str>) -> io::Result<()> {
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
        Ok(())
    }

    fn set_info_date(&mut self, key: &'static str, date: Option<PdfDate>) -> io::Result<()> {
        match date {
            Some(date) => {
                let bytes = date.to_pdf_string()?.into_bytes();
                self.charge(add(bytes.len(), 128)?)?;
                self.info.insert(key, bytes);
            }
            None => {
                self.info.remove(key);
            }
        }
        Ok(())
    }

    /// Replaces every document information entry.
    pub fn metadata(&mut self, metadata: &Metadata) {
        if self.error.is_some() {
            return;
        }
        let result = (|| {
            for (key, text) in [
                ("Title", &metadata.title),
                ("Author", &metadata.author),
                ("Subject", &metadata.subject),
                ("Keywords", &metadata.keywords),
                ("Creator", &metadata.creator),
                ("Producer", &metadata.producer),
            ] {
                self.set_info_text(key, text.as_deref())?;
            }
            self.set_info_date("CreationDate", metadata.creation_date)?;
            self.set_info_date("ModDate", metadata.modification_date)
        })();
        if let Err(error) = result {
            self.fail(error);
        }
    }

    /// Encrypts the output with the private PSChaCha handler.
    ///
    /// Only pdf-toolkit readers (the pdf-toolkit viewer and chuzz) open the
    /// result; other PDF readers refuse it. Object streams are not used for
    /// encrypted output, whatever `WriteOptions::object_streams` says, so every
    /// string and stream is authenticated separately. Salt, identifiers and
    /// nonces come from the operating system, so encrypted bytes differ per run.
    /// `None` costs select M=65536 KiB, T=3, P=1.
    pub fn encrypt(&mut self, password: &[u8], costs: Option<KdfParams>) {
        if self.error.is_some() {
            return;
        }
        let result = (|| {
            check("password_bytes", password.len(), 1 << 20)?;
            self.charge(add(password.len(), 256)?)?;
            self.encrypt = Some(Encrypt {
                password: copied(password, 1 << 20)?,
                costs,
                fixture: false,
            });
            Ok(())
        })();
        if let Err(error) = result {
            self.fail(error);
        }
    }

    // Fixed entropy is confined to the house-fixture generator.
    pub(crate) fn encrypt_fixture(&mut self, password: &[u8]) {
        self.encrypt(password, None);
        if let Some(encrypt) = self.encrypt.as_mut() {
            encrypt.fixture = true;
        }
    }

    pub fn standard_font(&mut self, font: StandardFont) -> FontId {
        if self.error.is_some() {
            return FontId(u32::MAX);
        }
        if let Some(index) = self
            .fonts
            .iter()
            .position(|entry| matches!(entry, Font::Standard(existing) if *existing == font))
        {
            return FontId(index as u32);
        }
        let result = (|| {
            self.charge(256)?;
            let id =
                u32::try_from(self.fonts.len()).map_err(|_| invalid("font identifier overflow"))?;
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

    /// Embeds a TrueType font, or an OpenType font with TrueType (glyf)
    /// outlines, as an Identity-H Type0 font subset to the glyphs used, with
    /// a ToUnicode CMap. CFF-flavoured OpenType fonts are refused.
    pub fn truetype_font(&mut self, ttf: Arc<[u8]>) -> io::Result<FontId> {
        if self.error.is_some() {
            return Err(invalid("document builder has a pending error"));
        }
        check("fonts", add(self.fonts.len(), 1)?, MAX_FONTS)?;
        let bytes = ttf.len();
        let font = TrueType::new(ttf)?;
        self.charge(add(bytes, 2 << 20)?)?;
        let id =
            u32::try_from(self.fonts.len()).map_err(|_| invalid("font identifier overflow"))?;
        push(&mut self.fonts, Font::TrueType(font), "fonts", MAX_FONTS)?;
        Ok(FontId(id))
    }

    /// The advance width of `text` in points, as `paragraph` measures it.
    pub fn text_width(
        &self,
        font: FontId,
        size: f32,
        text: &str,
        kerning: bool,
    ) -> io::Result<f32> {
        text::measure(self, font, size, text, kerning)
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

    /// The JPEG bytes are written unchanged as DCTDecode; nothing is re-encoded.
    pub fn jpeg(&mut self, data: Arc<[u8]>, width: u32, height: u32, components: u8) -> ImageId {
        if self.error.is_some() {
            return ImageId(u32::MAX);
        }
        let result = (|| {
            check("images", add(self.images.len(), 1)?, MAX_IMAGES)?;
            check(
                "document_bytes",
                add(self.bytes, add(data.len(), 256)?)?,
                MAX_BYTES,
            )?;
            Image::jpeg(data, width, height, components)
        })();
        self.register_image(result)
    }

    /// Like `jpeg`, with the dimensions and component count read from the
    /// JPEG frame header.
    pub fn jpeg_auto(&mut self, data: Arc<[u8]>) -> ImageId {
        if self.error.is_some() {
            return ImageId(u32::MAX);
        }
        match image::jpeg_frame(&data) {
            Ok((width, height, components)) => self.jpeg(data, width, height, components),
            Err(error) => self.register_image(Err(error)),
        }
    }

    pub fn rgba(&mut self, width: u32, height: u32, rgba: &[u8]) -> ImageId {
        if self.error.is_some() {
            return ImageId(u32::MAX);
        }
        let result = (|| {
            check("images", add(self.images.len(), 1)?, MAX_IMAGES)?;
            check(
                "document_bytes",
                add(self.bytes, add(rgba.len(), 256)?)?,
                MAX_BYTES,
            )?;
            Image::rgba(width, height, rgba)
        })();
        self.register_image(result)
    }

    /// Decodes a PNG with pdf-core's inflate and predictor code. The pixels
    /// are written as FlateDecode RGB, with an SMask when any pixel is not opaque.
    /// Interlaced PNGs are refused; 16-bit samples are reduced to 8 bits.
    pub fn png(&mut self, data: &[u8]) -> ImageId {
        if self.error.is_some() {
            return ImageId(u32::MAX);
        }
        let result = (|| {
            check("images", add(self.images.len(), 1)?, MAX_IMAGES)?;
            let (width, height, rgba) = png::decode(data)?;
            check(
                "document_bytes",
                add(self.bytes, add(rgba.len(), 256)?)?,
                MAX_BYTES,
            )?;
            Image::rgba(width, height, &rgba)
        })();
        self.register_image(result)
    }

    /// Pixel width and height of a registered image.
    pub fn image_size(&self, image: ImageId) -> Option<(u32, u32)> {
        let index = usize::try_from(image.0).ok()?;
        self.images
            .get(index)
            .map(|image| (image.width, image.height))
    }

    pub fn page(&mut self, width: f32, height: f32) -> PageBuilder<'_> {
        self.page_with(&PageSetup::new(PageSize::Custom { width, height }))
    }

    pub fn page_with(&mut self, setup: &PageSetup) -> PageBuilder<'_> {
        let result = (|| {
            if self.error.is_some() {
                return Err(invalid("document builder has a pending error"));
            }
            let (width, height) = setup.size.dimensions();
            coordinate(width)?;
            coordinate(height)?;
            if width <= 0.0 || height <= 0.0 {
                return Err(invalid("page dimensions must be positive"));
            }
            let boxes = [
                setup.crop_box,
                setup.bleed_box,
                setup.trim_box,
                setup.art_box,
            ];
            for rect in boxes.iter().flatten() {
                for value in rect {
                    coordinate(*value)?;
                }
                let [x0, y0, x1, y1] = *rect;
                if x0 >= x1 || y0 >= y1 || x0 < 0.0 || y0 < 0.0 || x1 > width || y1 > height {
                    return Err(invalid(
                        "page boxes must be nonempty and inside the media box",
                    ));
                }
            }
            let margins = setup.margins;
            for value in [margins.top, margins.right, margins.bottom, margins.left] {
                coordinate(value)?;
                if value < 0.0 {
                    return Err(invalid("margins must be nonnegative"));
                }
            }
            if margins.left + margins.right >= width || margins.top + margins.bottom >= height {
                return Err(invalid("margins leave no content area"));
            }
            self.charge(256)?;
            let index = self.pages.len();
            push(
                &mut self.pages,
                Page {
                    width,
                    height,
                    boxes,
                    margins,
                    depth: 0,
                    content: Vec::new(),
                    fonts: BTreeSet::new(),
                    images: BTreeSet::new(),
                    links: Vec::new(),
                    annotations: Vec::new(),
                },
                "pages",
                MAX_PAGES,
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

    /// Reopens an existing page so more content can be added to it.
    pub fn page_at(&mut self, index: usize) -> PageBuilder<'_> {
        let index = if self.error.is_none() && index < self.pages.len() {
            Some(index)
        } else {
            if self.error.is_none() {
                self.fail(invalid("page index is out of range"));
            }
            None
        };
        PageBuilder { doc: self, index }
    }

    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    pub fn outline(&mut self, title: &str, level: u8, page: usize, y: f32) {
        self.outline_to(
            title,
            level,
            Destination::Xyz {
                page,
                left: None,
                top: Some(y),
                zoom: None,
            },
        );
    }

    pub fn outline_to(&mut self, title: &str, level: u8, destination: Destination) {
        if self.error.is_some() {
            return;
        }
        let result = (|| {
            destination.validate()?;
            let base = self
                .outlines
                .first()
                .map(|item| item.level)
                .unwrap_or(level);
            if base > 1 || level < base {
                return Err(invalid("outline levels must start at zero or one"));
            }
            check(
                "outline_depth",
                add(usize::from(level - base), 1)?,
                MAX_DEPTH,
            )?;
            if let Some(previous) = self.outlines.last() {
                if usize::from(level) > add(usize::from(previous.level), 1)? {
                    return Err(invalid("outline levels cannot skip a parent"));
                }
            }
            let title = unicode(title, MAX_STRING_BYTES)?;
            self.charge(add(title.len(), 256)?)?;
            push(
                &mut self.outlines,
                Outline {
                    title,
                    level,
                    destination,
                },
                "outline_items",
                MAX_OUTLINES,
            )
        })();
        if let Err(error) = result {
            self.fail(error);
        }
    }

    pub fn finish(self, options: &WriteOptions) -> io::Result<Vec<u8>> {
        emit::finish(self, options)
    }

    pub fn finish_to(self, options: &WriteOptions, sink: &mut dyn io::Write) -> io::Result<u64> {
        let bytes = self.finish(options)?;
        let length = u64::try_from(bytes.len()).map_err(|_| invalid("output length overflow"))?;
        sink.write_all(&bytes)?;
        Ok(length)
    }
}
