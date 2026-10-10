use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use pdf_core::pages::Page;
use pdf_core::{DictRef, DocError, LimitKind, Limits, Object, ObjectRef, Resolver, Value, Warning};

use crate::content::{
    check, container_body, decode_name, decode_string, inline_number, name_eq, refusal,
    Content, ContentCursor, Op, Operand, Values,
};
use crate::data::encodings::{PDF_DOC, WIN_ANSI};
use crate::font::{Font, FontFlags, FontKind, Glyph, GlyphText};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PositionedGlyph {
    pub text: GlyphText,
    pub mapped: bool,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub size: f32,
    pub angle: i16,
    pub font: u16,
    pub seq: u32,
}

#[derive(Clone, Debug)]
pub struct FontInfo {
    pub name: String,
    pub flags: FontFlags,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ImageInfo {
    pub bbox: [f32; 4],
    pub width: u32,
    pub height: u32,
    pub inline: bool,
}

#[derive(Clone, Debug)]
pub struct PageText {
    pub width: f32,
    pub height: f32,
    pub glyphs: Vec<PositionedGlyph>,
    pub fonts: Vec<FontInfo>,
    pub images: Vec<ImageInfo>,
    pub ops: u64,
    pub unmapped: u32,
}

#[derive(Clone, Copy)]
struct Matrix([f64; 6]);

impl Matrix {
    const IDENTITY: Self = Self([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);

    fn translation(x: f64, y: f64) -> Self {
        Self([1.0, 0.0, 0.0, 1.0, x, y])
    }

    /// Composition in point traversal order: self, then outer.
    fn then(self, outer: Self) -> Self {
        let [a, b, c, d, e, f] = self.0;
        let [g, h, i, j, k, l] = outer.0;
        Self([
            g * a + i * b,
            h * a + j * b,
            g * c + i * d,
            h * c + j * d,
            g * e + i * f + k,
            h * e + j * f + l,
        ])
    }

    fn point(self, x: f64, y: f64) -> [f64; 2] {
        let [a, b, c, d, e, f] = self.0;
        [a * x + c * y + e, b * x + d * y + f]
    }

    fn vector(self, x: f64, y: f64) -> [f64; 2] {
        let [a, b, c, d, _, _] = self.0;
        [a * x + c * y, b * x + d * y]
    }
}

#[derive(Clone, Copy)]
struct TextState {
    character_space: f64,
    word_space: f64,
    horizontal_scale: f64,
    leading: f64,
    size: f64,
    rise: f64,
    render_mode: u8,
    font: usize,
}

impl Default for TextState {
    fn default() -> Self {
        Self {
            character_space: 0.0,
            word_space: 0.0,
            horizontal_scale: 1.0,
            leading: 0.0,
            size: 0.0,
            rise: 0.0,
            render_mode: 0,
            font: 0,
        }
    }
}

#[derive(Clone, Copy)]
struct GraphicsState {
    ctm: Matrix,
    text: TextState,
}

impl Default for GraphicsState {
    fn default() -> Self {
        Self { ctm: Matrix::IDENTITY, text: TextState::default() }
    }
}

#[derive(Clone, Copy)]
struct Snapshot {
    graphics: GraphicsState,
    text_matrix: Matrix,
    line_matrix: Matrix,
    in_text: bool,
}

struct CachedFont {
    resources: Option<Value>,
    name: Vec<u8>,
    font: Option<Font>,
    info: u16,
}

struct Marked {
    start: usize,
    actual: Option<Vec<GlyphText>>,
}

struct Frame {
    bytes: Vec<u8>,
    cursor: ContentCursor,
    resources: Option<Value>,
    id: ObjectRef,
    restore: Option<Snapshot>,
    graphics_base: usize,
    marked_base: usize,
}

struct Interpreter {
    limits: Limits,
    display: Matrix,
    output: PageText,
    graphics: GraphicsState,
    text_matrix: Matrix,
    line_matrix: Matrix,
    in_text: bool,
    saved: Vec<GraphicsState>,
    marked: Vec<Marked>,
    fonts: Vec<CachedFont>,
    strings: Vec<u8>,
    names: Vec<u8>,
    seq: u64,
    decoded: u64,
}

pub fn page_text(resolver: &mut Resolver<'_>, page: &Page) -> Result<PageText, DocError> {
    if resolver.document().encrypted() {
        return Err(DocError::Encrypted);
    }
    let limits = *resolver.document().limits();
    let (display, width, height) = display_matrix(page)?;
    let mut interpreter = Interpreter {
        limits,
        display,
        output: PageText {
            width,
            height,
            glyphs: Vec::new(),
            fonts: Vec::new(),
            images: Vec::new(),
            ops: 0,
            unmapped: 0,
        },
        graphics: GraphicsState::default(),
        text_matrix: Matrix::IDENTITY,
        line_matrix: Matrix::IDENTITY,
        in_text: false,
        saved: Vec::new(),
        marked: Vec::new(),
        fonts: Vec::new(),
        strings: Vec::new(),
        names: Vec::new(),
        seq: 0,
        decoded: 0,
    };
    push(
        &mut interpreter.output.fonts,
        FontInfo { name: String::from("Fallback"), flags: FontFlags::default() },
        limits,
    )?;
    push(
        &mut interpreter.fonts,
        CachedFont { resources: None, name: Vec::new(), font: None, info: 0 },
        limits,
    )?;

    let mut frames = Vec::new();
    // Streams share graphics, text, and marked-content state. Each reader has
    // its own lexer and token budget, as if a separator occurred between streams.
    for id in page.contents.iter().copied() {
        let stream = match resolver.get(id) {
            Ok(stream) => stream,
            Err(error) => {
                recover(resolver, error, Some(id), "content_stream_failed")?;
                continue;
            }
        };
        let Some(bytes) = interpreter.decode(resolver, &stream)? else {
            continue;
        };
        push(
            &mut frames,
            Frame {
                bytes,
                cursor: ContentCursor::default(),
                resources: page.resources.clone(),
                id,
                restore: None,
                graphics_base: 0,
                marked_base: 0,
            },
            limits,
        )?;

        while let Some(mut frame) = frames.pop() {
            let (cursor, invocation) = {
                let mut reader = Content::resume(&frame.bytes, &limits, frame.cursor)?;
                let mut invocation = None;
                loop {
                    let op = match reader.next_op() {
                        Ok(Some(op)) => op,
                        Ok(None) => break,
                        Err(error) => {
                            recover(
                                resolver,
                                error,
                                Some(frame.id),
                                "content_stream_failed",
                            )?;
                            break;
                        }
                    };
                    interpreter.output.ops = interpreter.output.ops.saturating_add(1);
                    check(
                        LimitKind::ContentOps,
                        interpreter.output.ops,
                        limits.max_content_ops,
                    )?;
                    match interpreter.execute(
                        resolver,
                        op,
                        frame.resources.as_ref(),
                        frame.graphics_base,
                        frame.marked_base,
                        frame.id,
                    ) {
                        Ok(Some(object)) => {
                            invocation = Some(object);
                            break;
                        }
                        Ok(None) => {}
                        Err(error) => {
                            recover(
                                resolver,
                                error,
                                Some(frame.id),
                                "content_operator_failed",
                            )?;
                        }
                    }
                }
                if reader.dropped_operands() != 0 {
                    warn(resolver, Some(frame.id), "content_operands_dropped");
                }
                if reader.junk_tokens() != 0 {
                    warn(resolver, Some(frame.id), "content_junk_skipped");
                }
                (reader.cursor(), invocation)
            };
            frame.cursor = cursor;

            if let Some(object) = invocation {
                let depth = frames.len().saturating_add(1);
                let cycle = object.id().map(|id| {
                    id == frame.id || frames.iter().any(|ancestor| ancestor.id == id)
                }).unwrap_or(false);
                if cycle {
                    warn(resolver, object.id(), "form_cycle");
                    push(&mut frames, frame, limits)?;
                    continue;
                }
                let child = interpreter.form(
                    resolver,
                    object,
                    frame.resources.as_ref(),
                    depth,
                );
                match child {
                    Ok(Some(child)) => {
                        push(&mut frames, frame, limits)?;
                        push(&mut frames, child, limits)?;
                        continue;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        recover(resolver, error, Some(frame.id), "form_failed")?;
                    }
                }
                // Resume after Do even when its Form could not be interpreted.
                push(&mut frames, frame, limits)?;
                continue;
            }

            if let Some(snapshot) = frame.restore {
                if interpreter.marked.len() > frame.marked_base {
                    warn(resolver, Some(frame.id), "marked_content_unclosed");
                }
                while interpreter.marked.len() > frame.marked_base {
                    interpreter.end_marked(resolver, Some(frame.id))?;
                }
                if interpreter.saved.len() > frame.graphics_base {
                    warn(resolver, Some(frame.id), "graphics_state_unclosed");
                }
                interpreter.saved.truncate(frame.graphics_base);
                interpreter.restore(snapshot);
            }
        }
    }

    if !interpreter.marked.is_empty() {
        warn(resolver, Some(page.id), "marked_content_unclosed");
    }
    while !interpreter.marked.is_empty() {
        interpreter.end_marked(resolver, Some(page.id))?;
    }
    if !interpreter.saved.is_empty() {
        warn(resolver, Some(page.id), "graphics_state_unclosed");
    }
    interpreter.output.unmapped = interpreter.output.glyphs.iter().fold(0u32, |count, glyph| {
        count.saturating_add(u32::from(!glyph.mapped))
    });
    Ok(interpreter.output)
}

impl Interpreter {
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            graphics: self.graphics,
            text_matrix: self.text_matrix,
            line_matrix: self.line_matrix,
            in_text: self.in_text,
        }
    }

    fn restore(&mut self, snapshot: Snapshot) {
        self.graphics = snapshot.graphics;
        self.text_matrix = snapshot.text_matrix;
        self.line_matrix = snapshot.line_matrix;
        self.in_text = snapshot.in_text;
    }

    fn decode(
        &mut self,
        resolver: &mut Resolver<'_>,
        object: &Object,
    ) -> Result<Option<Vec<u8>>, DocError> {
        let decoded = match resolver.decode_stream(object) {
            Ok(decoded) => decoded,
            Err(error) => {
                recover(resolver, error, object.id(), "content_stream_failed")?;
                return Ok(None);
            }
        };
        self.decoded = self.decoded.saturating_add(decoded.bytes.len() as u64);
        check(
            LimitKind::TotalDecodedBytes,
            self.decoded,
            self.limits.max_total_decoded_bytes,
        )?;
        if decoded.image.is_some() {
            warn(resolver, object.id(), "content_stream_failed");
            return Ok(None);
        }
        if decoded.truncated {
            warn(resolver, object.id(), "content_stream_truncated");
        }
        Ok(Some(decoded.bytes))
    }

    fn form(
        &mut self,
        resolver: &mut Resolver<'_>,
        object: Object,
        inherited: Option<&Value>,
        depth: usize,
    ) -> Result<Option<Frame>, DocError> {
        let maximum = u64::from(self.limits.max_depth).min(16);
        check(LimitKind::Depth, depth as u64, maximum)?;
        let dict = object.dict().ok_or(DocError::WrongType {
            id: object.id(),
            expected: "Form dictionary",
        })?;
        let matrix = match read_matrix(resolver, dict) {
            Ok(matrix) => matrix,
            Err(error) => {
                recover(resolver, error, object.id(), "form_matrix_failed")?;
                Matrix::IDENTITY
            }
        };
        let resources = match resolver.resolve_dict_key(dict, b"Resources") {
            Ok(Some(resources)) if resources.get().as_dict().is_some() => Some(resources),
            Ok(None) => inherited.cloned(),
            Ok(Some(_)) => {
                warn(resolver, object.id(), "form_resources_failed");
                inherited.cloned()
            }
            Err(error) => {
                recover(resolver, error, object.id(), "form_resources_failed")?;
                inherited.cloned()
            }
        };
        let Some(bytes) = self.decode(resolver, &object)? else {
            return Ok(None);
        };
        let id = object.id().ok_or(DocError::WrongType {
            id: None,
            expected: "indirect Form stream",
        })?;
        let restore = self.snapshot();
        let child = Frame {
            bytes,
            cursor: ContentCursor::default(),
            resources,
            id,
            restore: Some(restore),
            graphics_base: self.saved.len(),
            marked_base: self.marked.len(),
        };
        self.graphics.ctm = matrix.then(self.graphics.ctm);
        self.in_text = false;
        Ok(Some(child))
    }

    fn execute(
        &mut self,
        resolver: &mut Resolver<'_>,
        op: Op<'_, '_>,
        resources: Option<&Value>,
        graphics_base: usize,
        marked_base: usize,
        id: ObjectRef,
    ) -> Result<Option<Object>, DocError> {
        let operands = op.operands;
        match op.name {
            b"q" => {
                check(
                    LimitKind::Depth,
                    self.saved.len().saturating_add(1) as u64,
                    u64::from(self.limits.max_depth),
                )?;
                push(&mut self.saved, self.graphics, self.limits)?;
            }
            b"Q" => {
                if self.saved.len() > graphics_base {
                    if let Some(saved) = self.saved.pop() {
                        self.graphics = saved;
                    }
                } else {
                    warn(resolver, Some(id), "graphics_state_underflow");
                }
            }
            b"cm" => {
                if let Some(numbers) = numbers::<6>(operands) {
                    self.graphics.ctm = Matrix(numbers).then(self.graphics.ctm);
                }
            }
            b"BT" => {
                self.text_matrix = Matrix::IDENTITY;
                self.line_matrix = Matrix::IDENTITY;
                self.in_text = true;
            }
            b"ET" => self.in_text = false,
            b"Tc" => {
                if let Some([value]) = numbers::<1>(operands) {
                    self.graphics.text.character_space = value;
                }
            }
            b"Tw" => {
                if let Some([value]) = numbers::<1>(operands) {
                    self.graphics.text.word_space = value;
                }
            }
            b"Tz" => {
                if let Some([value]) = numbers::<1>(operands) {
                    self.graphics.text.horizontal_scale = value / 100.0;
                }
            }
            b"TL" => {
                if let Some([value]) = numbers::<1>(operands) {
                    self.graphics.text.leading = value;
                }
            }
            b"Ts" => {
                if let Some([value]) = numbers::<1>(operands) {
                    self.graphics.text.rise = value;
                }
            }
            b"Tr" => {
                if let Some([value]) = numbers::<1>(operands) {
                    if (0.0..=7.0).contains(&value) {
                        self.graphics.text.render_mode = value as u8;
                    }
                }
            }
            b"Tf" => {
                if let [Operand::Name(name), Operand::Number(size)] = operands {
                    let name = decode_name(name, &mut self.names, self.limits.max_object_bytes)?;
                    let index = load_font(
                        resolver,
                        resources,
                        name,
                        &mut self.fonts,
                        &mut self.output.fonts,
                        self.limits,
                    )?;
                    self.graphics.text.font = index;
                    self.graphics.text.size = *size;
                }
            }
            b"Tm" => {
                if let Some(numbers) = numbers::<6>(operands) {
                    self.text_matrix = Matrix(numbers);
                    self.line_matrix = self.text_matrix;
                }
            }
            b"Td" | b"TD" => {
                if let Some([x, y]) = numbers::<2>(operands) {
                    if op.name == b"TD" {
                        self.graphics.text.leading = -y;
                    }
                    self.move_line(x, y);
                }
            }
            b"T*" => self.next_line(),
            b"Tj" => {
                if let [Operand::String(raw)] = operands {
                    self.show(resolver, raw, Some(id))?;
                }
            }
            b"TJ" => {
                if self.in_text {
                    if let [Operand::Array(raw)] = operands {
                        let mut values = Values::new(container_body(raw), &self.limits);
                        while let Some(value) = values.next_value()? {
                            match value {
                                Operand::String(raw) => self.show(resolver, raw, Some(id))?,
                                Operand::Number(value) => self.adjust(value),
                                _ => {}
                            }
                        }
                    }
                }
            }
            b"'" => {
                self.next_line();
                if let [Operand::String(raw)] = operands {
                    self.show(resolver, raw, Some(id))?;
                }
            }
            b"\"" => {
                if let [
                    Operand::Number(word),
                    Operand::Number(character),
                    Operand::String(raw),
                ] = operands {
                    self.graphics.text.word_space = *word;
                    self.graphics.text.character_space = *character;
                    self.next_line();
                    self.show(resolver, raw, Some(id))?;
                }
            }
            b"BMC" | b"BDC" => {
                let actual = if op.name == b"BDC" {
                    actual_text(
                        resolver,
                        resources,
                        operands.get(1).copied(),
                        &mut self.names,
                        &mut self.strings,
                        self.limits,
                    )?
                } else {
                    None
                };
                check(
                    LimitKind::Depth,
                    self.marked.len().saturating_add(1) as u64,
                    u64::from(self.limits.max_depth),
                )?;
                push(
                    &mut self.marked,
                    Marked { start: self.output.glyphs.len(), actual },
                    self.limits,
                )?;
            }
            b"EMC" => {
                if self.marked.len() > marked_base {
                    self.end_marked(resolver, Some(id))?;
                } else {
                    warn(resolver, Some(id), "marked_content_underflow");
                }
            }
            b"ID" => {
                let width = dimension(inline_number(operands, b"W", b"Width"));
                let height = dimension(inline_number(operands, b"H", b"Height"));
                self.image(resolver, width, height, true, Some(id))?;
            }
            b"Do" => {
                if let [Operand::Name(name)] = operands {
                    let name = decode_name(name, &mut self.names, self.limits.max_object_bytes)?;
                    let value = match resource(resolver, resources, b"XObject", name) {
                        Ok(Some(value)) => value,
                        Ok(None) => {
                            warn(resolver, Some(id), "xobject_failed");
                            return Ok(None);
                        }
                        Err(error) => {
                            recover(resolver, error, Some(id), "xobject_failed")?;
                            return Ok(None);
                        }
                    };
                    let object = value.object().clone();
                    let Some(dict) = value.get().as_dict() else {
                        warn(resolver, object.id(), "xobject_failed");
                        return Ok(None);
                    };
                    let subtype = resolver.resolve_dict_key(dict, b"Subtype")?;
                    let subtype = subtype.as_ref().and_then(|value| value.get().as_name());
                    if subtype.map(|name| name.eq_bytes(b"Image")).unwrap_or(false) {
                        let width = dimension(read_number(resolver, dict, b"Width")?);
                        let height = dimension(read_number(resolver, dict, b"Height")?);
                        self.image(resolver, width, height, false, object.id())?;
                    } else if subtype.map(|name| name.eq_bytes(b"Form")).unwrap_or(false) {
                        return Ok(Some(object));
                    } else {
                        warn(resolver, object.id(), "xobject_unsupported");
                    }
                }
            }
            // d0, d1, BI, EI, path, clipping, color, and unknown operators
            // have no text-state effect. Invisible text is deliberately retained.
            _ => {}
        }
        Ok(None)
    }

    fn move_line(&mut self, x: f64, y: f64) {
        self.line_matrix = Matrix::translation(x, y).then(self.line_matrix);
        self.text_matrix = self.line_matrix;
    }

    fn next_line(&mut self) {
        self.move_line(0.0, -self.graphics.text.leading);
    }

    fn advance(&mut self, x: f64, y: f64) {
        self.text_matrix = Matrix::translation(x, y).then(self.text_matrix);
    }

    fn adjust(&mut self, value: f64) {
        let vertical = self.fonts.get(self.graphics.text.font)
            .and_then(|entry| entry.font.as_ref())
            .map(Font::is_vertical)
            .unwrap_or(false);
        let shift = -value / 1000.0 * self.graphics.text.size;
        if vertical {
            self.advance(0.0, shift);
        } else {
            self.advance(shift * self.graphics.text.horizontal_scale, 0.0);
        }
    }

    fn show(
        &mut self,
        resolver: &mut Resolver<'_>,
        raw: &[u8],
        id: Option<ObjectRef>,
    ) -> Result<(), DocError> {
        if !self.in_text {
            return Ok(());
        }
        let (font, info) = self.fonts.get(self.graphics.text.font)
            .map(|entry| (entry.font.clone(), entry.info))
            .unwrap_or((None, 0));
        // Temporarily move the reusable buffer so glyph emission can borrow self.
        let mut buffer = core::mem::take(&mut self.strings);
        let result = (|| {
            let bytes = decode_string(raw, &mut buffer, self.limits.max_decoded_bytes)?;
            if let Some(font) = font.as_ref() {
                let vertical = font.is_vertical();
                let matrix = if font.kind() == FontKind::Type3 {
                    font.font_matrix()
                } else {
                    [0.001, 0.0, 0.0, 0.001, 0.0, 0.0]
                };
                // A valid glyph consumes at least one byte. This also bounds a
                // damaged font iterator independently of its implementation.
                for glyph in font.glyphs(bytes).take(bytes.len()) {
                    self.glyph(resolver, glyph, info, vertical, matrix, id)?;
                }
            } else {
                for byte in bytes.iter().copied() {
                    let unicode = WIN_ANSI.get(usize::from(byte)).copied().unwrap_or(0);
                    let mapped = unicode != 0;
                    let text = char::from_u32(u32::from(unicode))
                        .filter(|_| mapped)
                        .unwrap_or('\u{fffd}');
                    self.glyph(
                        resolver,
                        Glyph {
                            code: u32::from(byte),
                            byte_len: 1,
                            width: 0.5,
                            text: GlyphText::from_char(text),
                            mapped,
                            word_space: byte == 32,
                        },
                        info,
                        false,
                        [0.001, 0.0, 0.0, 0.001, 0.0, 0.0],
                        id,
                    )?;
                }
            }
            Ok(())
        })();
        self.strings = buffer;
        result
    }

    fn glyph(
        &mut self,
        resolver: &mut Resolver<'_>,
        glyph: Glyph,
        info: u16,
        vertical: bool,
        font_matrix: [f64; 6],
        id: Option<ObjectRef>,
    ) -> Result<(), DocError> {
        let text = self.graphics.text;
        let spaces = text.character_space
            + if glyph.word_space { text.word_space } else { 0.0 };
        // Font::glyphs has already applied Type3 FontMatrix to glyph.width.
        let (dx, dy) = if vertical {
            (0.0, -text.size + spaces)
        } else {
            ((glyph.width * text.size + spaces) * text.horizontal_scale, 0.0)
        };
        let transform = self.text_matrix.then(self.graphics.ctm).then(self.display);
        let origin = transform.point(
            font_matrix[4] * text.size * text.horizontal_scale,
            text.rise + font_matrix[5] * text.size,
        );
        let baseline = transform.vector(
            font_matrix[0] * 1000.0 * text.size * text.horizontal_scale,
            font_matrix[1] * 1000.0 * text.size,
        );
        let up = transform.vector(
            font_matrix[2] * 1000.0 * text.size * text.horizontal_scale,
            font_matrix[3] * 1000.0 * text.size,
        );
        let advance = transform.vector(dx, dy);
        let size = norm(up[0], up[1]);
        let mut width = norm(advance[0], advance[1]);
        let direction = if vertical {
            transform.vector(0.0, -1.0)
        } else {
            transform.vector(text.horizontal_scale, 0.0)
        };
        if advance[0] * direction[0] + advance[1] * direction[1] < 0.0 {
            width = -width;
        }
        let geometry = (
            finite_f32(origin[0]),
            finite_f32(origin[1]),
            finite_f32(width),
            finite_f32(size),
        );
        if let (Some(x), Some(y), Some(width), Some(size)) = geometry {
            check(LimitKind::ContentOps, self.seq, u64::from(u32::MAX))?;
            push(
                &mut self.output.glyphs,
                PositionedGlyph {
                    text: glyph.text,
                    mapped: glyph.mapped,
                    x,
                    y,
                    width,
                    size,
                    angle: angle(baseline[0], baseline[1]),
                    font: info,
                    seq: self.seq as u32,
                },
                self.limits,
            )?;
            self.seq = self.seq.saturating_add(1);
        } else {
            warn(resolver, id, "text_geometry_failed");
        }
        self.advance(dx, dy);
        Ok(())
    }

    fn image(
        &mut self,
        resolver: &mut Resolver<'_>,
        width: u32,
        height: u32,
        inline: bool,
        id: Option<ObjectRef>,
    ) -> Result<(), DocError> {
        let transform = self.graphics.ctm.then(self.display);
        let points = [
            transform.point(0.0, 0.0),
            transform.point(1.0, 0.0),
            transform.point(0.0, 1.0),
            transform.point(1.0, 1.0),
        ];
        let mut bounds = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
        for [x, y] in points {
            if !x.is_finite() || !y.is_finite() {
                warn(resolver, id, "image_geometry_failed");
                return Ok(());
            }
            bounds[0] = bounds[0].min(x);
            bounds[1] = bounds[1].min(y);
            bounds[2] = bounds[2].max(x);
            bounds[3] = bounds[3].max(y);
        }
        let mut bbox = [0.0f32; 4];
        for (slot, value) in bbox.iter_mut().zip(bounds) {
            let Some(value) = finite_f32(value) else {
                warn(resolver, id, "image_geometry_failed");
                return Ok(());
            };
            *slot = value;
        }
        push(
            &mut self.output.images,
            ImageInfo { bbox, width, height, inline },
            self.limits,
        )
    }

    fn end_marked(
        &mut self,
        resolver: &mut Resolver<'_>,
        id: Option<ObjectRef>,
    ) -> Result<(), DocError> {
        let Some(marked) = self.marked.pop() else {
            return Ok(());
        };
        let Some(actual) = marked.actual else {
            return Ok(());
        };
        let Some(first) = self.output.glyphs.get(marked.start).copied() else {
            return Ok(());
        };
        let width = self.output.glyphs.get(marked.start..).unwrap_or(&[])
            .iter().fold(0.0f64, |sum, glyph| sum + f64::from(glyph.width));
        let width = finite_f32(width).unwrap_or_else(|| {
            warn(resolver, id, "actualtext_geometry_failed");
            0.0
        });
        self.output.glyphs.truncate(marked.start);
        let mut sequence = u64::from(first.seq);
        for (index, text) in actual.into_iter().enumerate() {
            check(LimitKind::ContentOps, sequence, u64::from(u32::MAX))?;
            push(
                &mut self.output.glyphs,
                PositionedGlyph {
                    text,
                    mapped: true,
                    width: if index == 0 { width } else { 0.0 },
                    seq: sequence as u32,
                    ..first
                },
                self.limits,
            )?;
            sequence = sequence.saturating_add(1);
        }
        self.seq = self.seq.max(sequence);
        Ok(())
    }
}

fn load_font(
    resolver: &mut Resolver<'_>,
    resources: Option<&Value>,
    name: &[u8],
    cache: &mut Vec<CachedFont>,
    infos: &mut Vec<FontInfo>,
    limits: Limits,
) -> Result<usize, DocError> {
    if let Some(index) = cache.iter().position(|entry| {
        entry.name == name && same_resources(entry.resources.as_ref(), resources)
    }) {
        return Ok(index);
    }
    let loaded = (|| {
        let value = resource(resolver, resources, b"Font", name)?
            .ok_or(DocError::WrongType { id: None, expected: "font resource" })?;
        let dict = value.get().as_dict().ok_or(DocError::WrongType {
            id: value.object().id(),
            expected: "font dictionary",
        })?;
        Font::load(resolver, dict, value.object().id())
    })();
    let font = match loaded {
        Ok(font) => Some(font),
        Err(error) => {
            recover(resolver, error, None, "font_failed")?;
            None
        }
    };
    let info_name = font.as_ref().map(Font::base_name).unwrap_or(name);
    let flags = font.as_ref().map(Font::flags).unwrap_or_default();
    let info = u16::try_from(infos.len()).map_err(|_| {
        refusal(LimitKind::Objects, infos.len().saturating_add(1) as u64, 65_536)
    })?;
    let mut owned_name = Vec::new();
    owned_name.try_reserve_exact(name.len()).map_err(|_| {
        refusal(
            LimitKind::ObjectBytes,
            limits.max_object_bytes.saturating_add(1),
            limits.max_object_bytes,
        )
    })?;
    owned_name.extend_from_slice(name);
    let info_name = String::from_utf8_lossy(info_name).into_owned();
    push(infos, FontInfo { name: info_name, flags }, limits)?;
    let index = cache.len();
    push(
        cache,
        CachedFont { resources: resources.cloned(), name: owned_name, font, info },
        limits,
    )?;
    Ok(index)
}

fn same_resources(left: Option<&Value>, right: Option<&Value>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => {
            Arc::ptr_eq(left.object().buffer(), right.object().buffer())
                && left.get().span() == right.get().span()
        }
        _ => false,
    }
}

fn resource(
    resolver: &mut Resolver<'_>,
    resources: Option<&Value>,
    category: &[u8],
    name: &[u8],
) -> Result<Option<Value>, DocError> {
    let Some(dict) = resources.and_then(|value| value.get().as_dict()) else {
        return Ok(None);
    };
    let Some(category) = resolver.resolve_dict_key(dict, category)? else {
        return Ok(None);
    };
    let Some(dict) = category.get().as_dict() else {
        return Ok(None);
    };
    resolver.resolve_dict_key(dict, name)
}

fn actual_text(
    resolver: &mut Resolver<'_>,
    resources: Option<&Value>,
    property: Option<Operand<'_>>,
    names: &mut Vec<u8>,
    strings: &mut Vec<u8>,
    limits: Limits,
) -> Result<Option<Vec<GlyphText>>, DocError> {
    match property {
        Some(Operand::Dict(raw)) => {
            let mut values = Values::new(container_body(raw), &limits);
            let mut actual = None;
            while let Some(key) = values.next_value()? {
                let Some(value) = values.next_value()? else {
                    break;
                };
                if let Operand::Name(key) = key {
                    if name_eq(key, b"ActualText") {
                        actual = match value {
                            Operand::String(raw) => Some(raw),
                            _ => None,
                        };
                    }
                }
            }
            match actual {
                Some(raw) => make_actual(raw, strings, limits).map(Some),
                None => Ok(None),
            }
        }
        Some(Operand::Name(name)) => {
            let name = decode_name(name, names, limits.max_object_bytes)?;
            let Some(property) = resource(resolver, resources, b"Properties", name)? else {
                return Ok(None);
            };
            let Some(dict) = property.get().as_dict() else {
                return Ok(None);
            };
            let Some(actual) = resolver.resolve_dict_key(dict, b"ActualText")? else {
                return Ok(None);
            };
            match actual.get().as_str() {
                Some(text) => make_actual(text.raw(), strings, limits).map(Some),
                None => Ok(None),
            }
        }
        _ => Ok(None),
    }
}

fn make_actual(
    raw: &[u8],
    buffer: &mut Vec<u8>,
    limits: Limits,
) -> Result<Vec<GlyphText>, DocError> {
    let bytes = decode_string(raw, buffer, limits.max_decoded_bytes)?;
    let mut result = Vec::new();
    if bytes.starts_with(&[0xfe, 0xff]) || bytes.starts_with(&[0xff, 0xfe]) {
        let little = bytes.starts_with(&[0xff, 0xfe]);
        let body = bytes.get(2..).unwrap_or(&[]);
        let units = body.chunks_exact(2).map(|pair| {
            let first = pair.first().copied().unwrap_or(0);
            let second = pair.get(1).copied().unwrap_or(0);
            if little {
                u16::from_le_bytes([first, second])
            } else {
                u16::from_be_bytes([first, second])
            }
        });
        for character in core::char::decode_utf16(units) {
            append_actual(&mut result, character.unwrap_or('\u{fffd}'), limits)?;
        }
        if body.len() % 2 != 0 {
            append_actual(&mut result, '\u{fffd}', limits)?;
        }
    } else if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        let text = String::from_utf8_lossy(bytes.get(3..).unwrap_or(&[]));
        for character in text.chars() {
            append_actual(&mut result, character, limits)?;
        }
    } else {
        for byte in bytes.iter().copied() {
            let unicode = PDF_DOC.get(usize::from(byte)).copied().unwrap_or(0);
            let character = char::from_u32(u32::from(unicode))
                .filter(|_| unicode != 0)
                .unwrap_or('\u{fffd}');
            append_actual(&mut result, character, limits)?;
        }
    }
    Ok(result)
}

fn append_actual(
    result: &mut Vec<GlyphText>,
    character: char,
    limits: Limits,
) -> Result<(), DocError> {
    if result.last_mut().map(|text| text.push(character)).unwrap_or(false) {
        return Ok(());
    }
    push(result, GlyphText::from_char(character), limits)
}

fn read_matrix(resolver: &mut Resolver<'_>, dict: DictRef<'_>) -> Result<Matrix, DocError> {
    let Some(value) = resolver.resolve_dict_key(dict, b"Matrix")? else {
        return Ok(Matrix::IDENTITY);
    };
    let array = value.get().as_array().ok_or(DocError::WrongType {
        id: value.object().id(),
        expected: "Form Matrix array",
    })?;
    if array.len() != 6 {
        return Err(DocError::WrongType {
            id: value.object().id(),
            expected: "six Form Matrix numbers",
        });
    }
    let mut matrix = [0.0; 6];
    for (slot, component) in matrix.iter_mut().zip(array.iter()) {
        let component = resolver.resolve(component)?;
        *slot = component.get().as_f64().filter(|number| number.is_finite())
            .ok_or(DocError::WrongType {
                id: value.object().id(),
                expected: "Form Matrix number",
            })?;
    }
    Ok(Matrix(matrix))
}

fn read_number(
    resolver: &mut Resolver<'_>,
    dict: DictRef<'_>,
    key: &[u8],
) -> Result<Option<f64>, DocError> {
    Ok(resolver.resolve_dict_key(dict, key)?
        .and_then(|value| value.get().as_f64())
        .filter(|number| number.is_finite()))
}

fn dimension(value: Option<f64>) -> u32 {
    value.filter(|number| *number >= 0.0)
        .map(|number| number.min(f64::from(u32::MAX)) as u32)
        .unwrap_or(0)
}

fn numbers<const N: usize>(operands: &[Operand<'_>]) -> Option<[f64; N]> {
    if operands.len() != N {
        return None;
    }
    let mut values = [0.0; N];
    for (slot, operand) in values.iter_mut().zip(operands) {
        let Operand::Number(number) = operand else {
            return None;
        };
        if !number.is_finite() {
            return None;
        }
        *slot = *number;
    }
    Some(values)
}

fn display_matrix(page: &Page) -> Result<(Matrix, f32, f32), DocError> {
    let [x0, y0, x1, y1] = page.crop_box;
    let unit = page.user_unit;
    if !unit.is_finite() || unit <= 0.0
        || !page.crop_box.iter().all(|number| number.is_finite())
    {
        return Err(DocError::Malformed { offset: 0, what: "page_display_geometry" });
    }
    let width = (x1 - x0).max(0.0) * unit;
    let height = (y1 - y0).max(0.0) * unit;
    let (matrix, width, height) = match page.rotate % 360 {
        90 => (
            Matrix([0.0, unit, unit, 0.0, -y0 * unit, -x0 * unit]),
            height,
            width,
        ),
        180 => (
            Matrix([-unit, 0.0, 0.0, unit, x1 * unit, -y0 * unit]),
            width,
            height,
        ),
        270 => (
            Matrix([0.0, -unit, -unit, 0.0, y1 * unit, x1 * unit]),
            height,
            width,
        ),
        _ => (
            Matrix([unit, 0.0, 0.0, -unit, -x0 * unit, y1 * unit]),
            width,
            height,
        ),
    };
    let width = finite_f32(width).ok_or(DocError::Malformed {
        offset: 0,
        what: "page_display_geometry",
    })?;
    let height = finite_f32(height).ok_or(DocError::Malformed {
        offset: 0,
        what: "page_display_geometry",
    })?;
    if !matrix.0.iter().all(|number| number.is_finite()) {
        return Err(DocError::Malformed { offset: 0, what: "page_display_geometry" });
    }
    Ok((matrix, width, height))
}

fn finite_f32(number: f64) -> Option<f32> {
    if number.is_finite() && number.abs() <= f64::from(f32::MAX) {
        Some(number as f32)
    } else {
        None
    }
}

/// Bounded no_std hypot, with Newton iteration only on the range [1, 2].
fn norm(x: f64, y: f64) -> f64 {
    let x = x.abs();
    let y = y.abs();
    let large = x.max(y);
    let small = x.min(y);
    if !x.is_finite() || !y.is_finite() {
        return f64::NAN;
    }
    if large == 0.0 {
        return 0.0;
    }
    let ratio = small / large;
    let square = 1.0 + ratio * ratio;
    let mut root = 1.5;
    for _ in 0..8 {
        root = 0.5 * (root + square / root);
    }
    large * root
}

/// Range-reduced atan series, avoiding a new math dependency in no_std.
fn angle(x: f64, y: f64) -> i16 {
    if !x.is_finite() || !y.is_finite() || (x == 0.0 && y == 0.0) {
        return 0;
    }
    let ax = x.abs();
    let ay = y.abs();
    let mut radians = if ax >= ay {
        atan_unit(ay / ax)
    } else {
        core::f64::consts::FRAC_PI_2 - atan_unit(ax / ay)
    };
    if x < 0.0 {
        radians = core::f64::consts::PI - radians;
    }
    if y < 0.0 {
        radians = -radians;
    }
    let degrees = radians * (180.0 / core::f64::consts::PI);
    let rounded = if degrees >= 0.0 { degrees + 0.5 } else { degrees - 0.5 };
    rounded as i16
}

fn atan_unit(value: f64) -> f64 {
    let (value, offset) = if value > 0.414213562373095 {
        ((value - 1.0) / (value + 1.0), core::f64::consts::FRAC_PI_4)
    } else {
        (value, 0.0)
    };
    let square = value * value;
    let mut power = value;
    let mut sum = 0.0;
    for index in 0..20u32 {
        let term = power / f64::from(index.saturating_mul(2).saturating_add(1));
        if index % 2 == 0 {
            sum += term;
        } else {
            sum -= term;
        }
        power *= square;
    }
    offset + sum
}

fn push<T>(values: &mut Vec<T>, value: T, limits: Limits) -> Result<(), DocError> {
    let requested = (values.len().saturating_add(1) as u64)
        .saturating_mul(core::mem::size_of::<T>() as u64);
    let maximum = limits.max_total_decoded_bytes;
    check(LimitKind::TotalDecodedBytes, requested, maximum)?;
    values.try_reserve(1).map_err(|_| {
        refusal(LimitKind::TotalDecodedBytes, maximum.saturating_add(1), maximum)
    })?;
    values.push(value);
    Ok(())
}

fn recover(
    resolver: &mut Resolver<'_>,
    error: DocError,
    object: Option<ObjectRef>,
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

fn warn(resolver: &mut Resolver<'_>, object: Option<ObjectRef>, what: &'static str) {
    resolver.warn(Warning { offset: None, object, what });
}

