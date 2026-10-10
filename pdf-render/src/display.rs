use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::Range;

use pdf_core::pages::Page;
use pdf_core::{
    DictRef, DocError, LimitKind, Limits, Object, ObjectRef, Refusal, Resolver,
    Value, Warning,
};
use pdf_text::content::{
    container_body, decode_name, decode_string, inline_number, name_eq,
    Content, ContentCursor, Op, Operand, Values,
};
use pdf_text::encoding::BaseEncoding;
use pdf_text::font::{Font, FontCache, FontKind, Glyph, GlyphText};

use crate::glyphs::{GlyphSource, GlyphSourceCache};
use crate::paint::{self, ColorState, ColorSpace};

const PATH_ELEMENTS: u64 = 1_000_000;
const FORM_DEPTH: u64 = 16;
const CLIP_DEPTH: u64 = 64;
const ANNOTATIONS: u64 = 10_000;


#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Matrix(pub [f32; 6]);

impl Matrix {
    pub const IDENTITY: Self = Self([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);

    /// Composition in point traversal order: self, then outer.
    pub fn then(self, outer: Self) -> Self {
        let [a, b, c, d, e, f] = self.0.map(f64::from);
        let [g, h, i, j, k, l] = outer.0.map(f64::from);
        Self([
            g * a + i * b, h * a + j * b,
            g * c + i * d, h * c + j * d,
            g * e + i * f + k, h * e + j * f + l,
        ].map(|value| value as f32))
    }

    pub fn point(self, x: f32, y: f32) -> (f32, f32) {
        let [a, b, c, d, e, f] = self.0.map(f64::from);
        let x = f64::from(x);
        let y = f64::from(y);
        ((a * x + c * y + e) as f32, (b * x + d * y + f) as f32)
    }

    pub fn is_finite(self) -> bool {
        self.0.iter().all(|value| value.is_finite())
    }

    pub(crate) fn mean_scale(self) -> f32 {
        let [a, b, c, d, _, _] = self.0.map(f64::from);
        ((norm(a, b) + norm(c, d)) * 0.5) as f32
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PathEl {
    MoveTo(f32, f32),
    LineTo(f32, f32),
    CubicTo(f32, f32, f32, f32, f32, f32),
    Close,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Path {
    pub els: Vec<PathEl>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FillRule {
    NonZero,
    EvenOdd,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Paint {
    Solid(Rgba),
    Axial {
        from: (f32, f32),
        to: (f32, f32),
        stops: Vec<(f32, Rgba)>,
        extend: (bool, bool),
    },
    Radial {
        from: (f32, f32, f32),
        to: (f32, f32, f32),
        stops: Vec<(f32, Rgba)>,
        extend: (bool, bool),
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stroke {
    pub width: f32,
    pub cap: u8,
    pub join: u8,
    pub miter: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Dash {
    pub array: Vec<f32>,
    pub phase: f32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlendMode {
    Normal,
    Multiply,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    SetBlendMode(BlendMode),
    PushClip { path: Path, rule: FillRule },
    PopClip,
    Fill { path: Path, rule: FillRule, paint: Paint },
    Stroke { path: Path, stroke: Stroke, dash: Option<Dash>, paint: Paint },
    Image { image: ImageSource, transform: Matrix, alpha: f32 },
    Glyphs { glyphs: Vec<GlyphPath>, paint: Paint, stroke: Option<Stroke> },
}

#[derive(Clone, Debug, PartialEq)]
pub enum ImageSource {
    Object(ObjectRef),
    Inline { width: u32, height: u32, rgba: Vec<u8> },
}

#[derive(Clone, Debug, PartialEq)]
pub struct GlyphPath {
    pub path: Path,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DisplayList {
    pub width: u32,
    pub height: u32,
    pub items: Vec<Item>,
    pub missing_fonts: u32,
}

pub struct RenderOptions {
    pub scale: f32,
    pub fallback_font: Option<Arc<[u8]>>,
}

pub(crate) struct Budget {
    used: u64,
    maximum: u64,
}

impl Budget {
    pub(crate) fn charge(&mut self, bytes: usize) -> Result<(), DocError> {
        let requested = self.used.checked_add(bytes as u64).ok_or_else(|| {
            refusal(LimitKind::TotalDecodedBytes, u64::MAX, self.maximum)
        })?;
        check(LimitKind::TotalDecodedBytes, requested, self.maximum)?;
        self.used = requested;
        Ok(())
    }

    pub(crate) fn reserve<T>(
        &mut self,
        values: &mut Vec<T>,
        additional: usize,
    ) -> Result<(), DocError> {
        let bytes = additional.checked_mul(core::mem::size_of::<T>())
            .ok_or_else(|| refusal(LimitKind::TotalDecodedBytes, u64::MAX, self.maximum))?;
        let total = values.len().checked_add(additional)
            .and_then(|count| count.checked_mul(core::mem::size_of::<T>()))
            .ok_or_else(|| refusal(LimitKind::TotalDecodedBytes, u64::MAX, self.maximum))?;
        check(LimitKind::TotalDecodedBytes, total as u64, isize::MAX as u64)?;
        self.charge(bytes)?;
        values.try_reserve(additional).map_err(|_| {
            refusal(LimitKind::TotalDecodedBytes, self.maximum.saturating_add(1), self.maximum)
        })
    }

    pub(crate) fn push<T>(&mut self, values: &mut Vec<T>, value: T) -> Result<(), DocError> {
        self.reserve(values, 1)?;
        values.push(value);
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct TextState {
    character_space: f32,
    word_space: f32,
    horizontal_scale: f32,
    leading: f32,
    size: f32,
    rise: f32,
    mode: u8,
    font: Option<usize>,
}

impl Default for TextState {
    fn default() -> Self {
        Self {
            character_space: 0.0, word_space: 0.0, horizontal_scale: 1.0,
            leading: 0.0, size: 0.0, rise: 0.0, mode: 0, font: None,
        }
    }
}

#[derive(Clone)]
struct GraphicsState {
    ctm: Matrix,
    fill: ColorState,
    stroke_color: ColorState,
    stroke: Stroke,
    dash: Option<Arc<Dash>>,
    fill_alpha: f32,
    stroke_alpha: f32,
    blend: BlendMode,
    text: TextState,
    clips: u64,
}

impl GraphicsState {
    fn new(ctm: Matrix) -> Self {
        Self {
            ctm,
            fill: ColorState::gray(),
            stroke_color: ColorState::gray(),
            stroke: Stroke { width: 1.0, cap: 0, join: 0, miter: 10.0 },
            dash: None,
            fill_alpha: 1.0,
            stroke_alpha: 1.0,
            blend: BlendMode::Normal,
            text: TextState::default(),
            clips: 0,
        }
    }
}

#[derive(Default)]
struct CurrentPath {
    path: Path,
    point: Option<(f32, f32)>,
    start: Option<(f32, f32)>,
    clip: Option<FillRule>,
}

struct Snapshot {
    graphics: GraphicsState,
    path: CurrentPath,
    text_matrix: Matrix,
    line_matrix: Matrix,
    in_text: bool,
    text_clip: Path,
    clip_requested: bool,
    saved_base: usize,
    item_start: usize,
    type3_mode: Option<u8>,
}

#[derive(Clone)]
struct Context {
    bytes: Arc<[u8]>,
    resources: Option<Value>,
    id: Option<ObjectRef>,
    graphics_base: usize,
}

struct Frame {
    context: Context,
    cursor: ContentCursor,
}

struct ArrayTask {
    context: Context,
    range: Range<usize>,
    cursor: ContentCursor,
}

struct Run {
    context: Context,
    range: Range<usize>,
    decoded: Vec<u8>,
    borrowed: bool,
    offset: usize,
    font: Option<Font>,
    font_index: Option<usize>,
    paths: Vec<GlyphPath>,
}

enum Task {
    Stream(Frame),
    Array(ArrayTask),
    Run(Run),
    Restore(Snapshot),
}

struct Invocation {
    object: Object,
    inherited: Option<Value>,
    matrix: Option<Matrix>,
    bbox: Option<[f32; 4]>,
    type3_mode: Option<u8>,
}

enum Action {
    Form(Invocation),
    Show(Range<usize>),
    Array(Range<usize>),
}

enum GlyphName {
    Missing,
    Static(&'static [u8]),
    Pdf(Value),
    Decoded(Arc<[u8]>),
}

impl GlyphName {
    fn bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Missing => None,
            Self::Static(bytes) => Some(bytes),
            Self::Pdf(value) => value.get().as_name().map(|name| name.encoded()),
            Self::Decoded(bytes) => Some(bytes),
        }
    }
}

struct Type3 {
    names: [GlyphName; 256],
    procs: Value,
    resources: Option<Value>,
    bbox: Option<[f32; 4]>,
}

struct LoadedFont {
    dict: Value,
    font: Font,
    source: Option<GlyphSource>,
    type3: Option<Type3>,
}

struct Builder<'c> {
    font_cache: &'c mut FontCache,
    glyph_cache: &'c mut GlyphSourceCache,
    limits: Limits,
    budget: Budget,
    output: DisplayList,
    graphics: GraphicsState,
    saved: Vec<GraphicsState>,
    path: CurrentPath,
    text_matrix: Matrix,
    line_matrix: Matrix,
    in_text: bool,
    text_clip: Path,
    clip_requested: bool,
    fonts: Vec<LoadedFont>,
    names: Vec<u8>,
    strings: Vec<u8>,
    active: Vec<ObjectRef>,
    ops: u64,
    fallback: Option<Arc<[u8]>>,
}

pub fn display_list(
    resolver: &mut Resolver<'_>,
    page: &Page,
    options: &RenderOptions,
) -> Result<DisplayList, DocError> {
    display_list_cached(
        resolver, page, options,
        &mut FontCache::default(), &mut GlyphSourceCache::default(),
    )
}

/// Reuse both caches across pages. Glyph programs also depend on fallback_font,
/// so that cache automatically invalidates when its allocation changes.
pub fn display_list_cached(
    resolver: &mut Resolver<'_>,
    page: &Page,
    options: &RenderOptions,
    cache: &mut FontCache,
    glyph_cache: &mut GlyphSourceCache,
) -> Result<DisplayList, DocError> {
    if !resolver.document().decryptable() {
        return Err(DocError::Encrypted);
    }
    let limits = *resolver.document().limits();
    let (ctm, width, height) = page_matrix(page, options.scale)?;
    let pixels = u64::from(width).checked_mul(u64::from(height))
        .and_then(|count| count.checked_mul(4))
        .ok_or_else(|| refusal(LimitKind::DecodedBytes, u64::MAX, limits.max_decoded_bytes))?;
    check(LimitKind::DecodedBytes, pixels, limits.max_decoded_bytes)?;
    let mut builder = Builder {
        font_cache: cache,
        glyph_cache,
        limits,
        budget: Budget { used: 0, maximum: limits.max_total_decoded_bytes },
        output: DisplayList { width, height, items: Vec::new(), missing_fonts: 0 },
        graphics: GraphicsState::new(ctm),
        saved: Vec::new(),
        path: CurrentPath::default(),
        text_matrix: Matrix::IDENTITY,
        line_matrix: Matrix::IDENTITY,
        in_text: false,
        text_clip: Path::default(),
        clip_requested: false,
        fonts: Vec::new(),
        names: Vec::new(),
        strings: Vec::new(),
        active: Vec::new(),
        ops: 0,
        fallback: options.fallback_font.clone(),
    };
    let mut tasks = Vec::new();
    for id in page.contents.iter().copied() {
        builder.step()?;
        let object = match resolver.get(id) {
            Ok(object) => object,
            Err(error) => {
                recover(resolver, error, Some(id), "render_content_failed")?;
                continue;
            }
        };
        let bytes = match builder.decode(resolver, &object) {
            Ok(bytes) => bytes,
            Err(error) => {
                recover(resolver, error, Some(id), "render_content_failed")?;
                continue;
            }
        };
        let context = Context {
            bytes, resources: page.resources.clone(), id: Some(id), graphics_base: 0,
        };
        builder.active.clear();
        builder.budget.push(&mut builder.active, id)?;
        builder.budget.push(
            &mut tasks,
            Task::Stream(Frame { context, cursor: ContentCursor::default() }),
        )?;
        while let Some(task) = tasks.pop() {
            match task {
                Task::Stream(frame) => builder.stream(resolver, frame, &mut tasks)?,
                Task::Array(array) => builder.array(resolver, array, &mut tasks)?,
                Task::Run(run) => builder.run(resolver, run, &mut tasks)?,
                Task::Restore(snapshot) => builder.restore(resolver, snapshot)?,
            }
        }
    }
    if builder.in_text {
        warn(resolver, Some(page.id), "render_text_unclosed");
        builder.end_text()?;
    }
    if !builder.saved.is_empty() {
        warn(resolver, Some(page.id), "graphics_state_unclosed");
    }
    builder.pop_clips(0)?;
    builder.annotations(resolver, page, ctm)?;
    Ok(builder.output)
}

impl Builder<'_> {
    fn annotations(
        &mut self,
        resolver: &mut Resolver<'_>,
        page: &Page,
        ctm: Matrix,
    ) -> Result<(), DocError> {
        let object = match resolver.get(page.id) {
            Ok(object) => object,
            Err(error) => {
                recover(resolver, error, Some(page.id), "render_annotations_failed")?;
                return Ok(());
            }
        };
        let result = (|| {
            let Some(dict) = object.dict() else { return Ok(None) };
            resolver.resolve_dict_key(dict, b"Annots")
        })();
        let annots = match result {
            Ok(Some(value)) => value,
            Ok(None) => return Ok(()),
            Err(error) => {
                recover(resolver, error, Some(page.id), "render_annotations_failed")?;
                return Ok(());
            }
        };
        let Some(array) = annots.get().as_array() else {
            warn(resolver, Some(page.id), "render_annots_invalid");
            return Ok(());
        };
        check(
            LimitKind::Objects, array.len() as u64,
            ANNOTATIONS.min(u64::from(self.limits.max_objects)),
        )?;
        let mut tasks = Vec::new();
        for value in array.iter() {
            self.step()?;
            let id = value.as_ref();
            let form = match self.annotation_form(resolver, value, page.resources.clone()) {
                Ok(Some(form)) => form,
                Ok(None) => continue,
                Err(error) => {
                    recover(resolver, error, id, "render_annotation_failed")?;
                    continue;
                }
            };
            // Page content and adjacent appearances cannot leak graphics state.
            self.graphics = GraphicsState::new(ctm);
            self.graphics.fill_alpha = form.1;
            self.graphics.stroke_alpha = form.1;
            self.saved.clear();
            self.active.clear();
            self.path = CurrentPath::default();
            self.text_matrix = Matrix::IDENTITY;
            self.line_matrix = Matrix::IDENTITY;
            self.in_text = false;
            self.text_clip = Path::default();
            self.clip_requested = false;
            if let Err(error) = self.enter_form(resolver, form.0, &mut tasks) {
                recover(resolver, error, id, "render_annotation_failed")?;
            }
            while let Some(task) = tasks.pop() {
                match task {
                    Task::Stream(frame) => self.stream(resolver, frame, &mut tasks)?,
                    Task::Array(array) => self.array(resolver, array, &mut tasks)?,
                    Task::Run(run) => self.run(resolver, run, &mut tasks)?,
                    Task::Restore(snapshot) => self.restore(resolver, snapshot)?,
                }
            }
            self.pop_clips(0)?;
        }
        Ok(())
    }

    fn annotation_form(
        &mut self,
        resolver: &mut Resolver<'_>,
        value: pdf_core::ValueRef<'_>,
        inherited: Option<Value>,
    ) -> Result<Option<(Invocation, f32)>, DocError> {
        let annot = resolver.resolve(value)?;
        let Some(dict) = annot.get().as_dict() else { return Ok(None) };
        let flags = match resolver.resolve_dict_key(dict, b"F")? {
            Some(value) => value.get().as_i64()
                .and_then(|value| u32::try_from(value).ok())
                .ok_or_else(|| bad("annotation_flags"))?,
            None => 0,
        };
        if flags & (2 | 32) != 0 { return Ok(None); }
        let Some(rect) = read_array::<4>(resolver, dict, b"Rect")? else { return Ok(None) };
        let opacity = number(resolver, dict, b"CA")?.unwrap_or(1.0).clamp(0.0, 1.0);
        let Some(ap) = resolver.resolve_dict_key(dict, b"AP")? else { return Ok(None) };
        let Some(ap_dict) = ap.get().as_dict() else { return Ok(None) };
        let Some(mut normal) = resolver.resolve_dict_key(ap_dict, b"N")? else {
            return Ok(None);
        };
        if !normal.object().is_stream() {
            let Some(states) = normal.get().as_dict() else { return Ok(None) };
            let Some(state) = resolver.resolve_dict_key(dict, b"AS")? else { return Ok(None) };
            let Some(name) = state.get().as_name() else { return Ok(None) };
            let name = name.decoded();
            let Some(selected) = resolver.resolve_dict_key(states, name.as_ref())? else {
                return Ok(None);
            };
            normal = selected;
        }
        if !normal.object().is_stream() { return Ok(None); }
        let form_dict = normal.object().dict().ok_or_else(|| bad("annotation_appearance"))?;
        let bbox = read_array::<4>(resolver, form_dict, b"BBox")?
            .ok_or_else(|| bad("annotation_bbox"))?;
        let matrix = read_array::<6>(resolver, form_dict, b"Matrix")?
            .map(Matrix).unwrap_or(Matrix::IDENTITY);
        let [x0, y0, x1, y1] = bbox;
        let mut bounds = [f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY];
        for (x, y) in [(x0, y0), (x0, y1), (x1, y0), (x1, y1)] {
            let (x, y) = checked_point(matrix, x, y)?;
            bounds[0] = bounds[0].min(x);
            bounds[1] = bounds[1].min(y);
            bounds[2] = bounds[2].max(x);
            bounds[3] = bounds[3].max(y);
        }
        let [left, bottom, right, top] = rect;
        let left = f64::from(left.min(right));
        let bottom = f64::from(bottom.min(top));
        let right = f64::from(rect[0].max(rect[2]));
        let top = f64::from(rect[1].max(rect[3]));
        let bw = f64::from(bounds[2]) - f64::from(bounds[0]);
        let bh = f64::from(bounds[3]) - f64::from(bounds[1]);
        if bw <= 0.0 || bh <= 0.0 || right <= left || top <= bottom {
            return Err(bad("annotation_geometry"));
        }
        let sx = (right - left) / bw;
        let sy = (top - bottom) / bh;
        let mapping = Matrix([
            finite(sx).ok_or_else(|| bad("annotation_geometry"))?, 0.0,
            0.0, finite(sy).ok_or_else(|| bad("annotation_geometry"))?,
            finite(left - f64::from(bounds[0]) * sx)
                .ok_or_else(|| bad("annotation_geometry"))?,
            finite(bottom - f64::from(bounds[1]) * sy)
                .ok_or_else(|| bad("annotation_geometry"))?,
        ]);
        let transform = matrix.then(mapping);
        if !transform.is_finite() { return Err(bad("annotation_geometry")); }
        Ok(Some((Invocation {
            object: normal.object().clone(), inherited,
            matrix: Some(transform), bbox: Some(bbox), type3_mode: None,
        }, opacity)))
    }

    fn step(&mut self) -> Result<(), DocError> {
        self.ops = self.ops.checked_add(1).ok_or_else(|| {
            refusal(LimitKind::ContentOps, u64::MAX, self.limits.max_content_ops)
        })?;
        check(LimitKind::ContentOps, self.ops, self.limits.max_content_ops)
    }

    fn decode(&mut self, resolver: &mut Resolver<'_>, object: &Object)
        -> Result<Arc<[u8]>, DocError>
    {
        let decoded = resolver.decode_stream(object)?;
        self.budget.charge(decoded.bytes.len())?;
        if decoded.image.is_some() {
            return Err(bad("render_content_codec"));
        }
        if decoded.truncated {
            warn(resolver, object.id(), "render_content_truncated");
        }
        Ok(Arc::from(decoded.bytes))
    }

    fn emit(&mut self, item: Item) -> Result<(), DocError> {
        let paint = match &item {
            Item::Fill { paint, .. } | Item::Stroke { paint, .. }
                | Item::Glyphs { paint, .. } => Some(paint),
            _ => None,
        };
        if let Some(Paint::Axial { stops, .. } | Paint::Radial { stops, .. }) = paint {
            let bytes = stops.len().checked_mul(core::mem::size_of::<(f32, Rgba)>())
                .ok_or_else(|| bad("render_paint_size"))?;
            self.budget.charge(bytes)?;
        }
        if matches!(
            &item, Item::Fill { .. } | Item::Stroke { .. }
                | Item::Glyphs { .. } | Item::Image { .. }
        ) {
            self.budget.push(
                &mut self.output.items, Item::SetBlendMode(self.graphics.blend),
            )?;
        }
        self.budget.push(&mut self.output.items, item)
    }

    fn push_clip(&mut self, path: Path, rule: FillRule) -> Result<(), DocError> {
        let requested = self.graphics.clips.saturating_add(1);
        check(LimitKind::Depth, requested, CLIP_DEPTH)?;
        self.emit(Item::PushClip { path, rule })?;
        self.graphics.clips = requested;
        Ok(())
    }

    fn pop_clips(&mut self, target: u64) -> Result<(), DocError> {
        while self.graphics.clips > target {
            self.emit(Item::PopClip)?;
            self.graphics.clips = self.graphics.clips.saturating_sub(1);
        }
        Ok(())
    }

    fn stream(
        &mut self,
        resolver: &mut Resolver<'_>,
        mut frame: Frame,
        tasks: &mut Vec<Task>,
    ) -> Result<(), DocError> {
        let context = frame.context.clone();
        let mut reader = Content::resume(&context.bytes, &self.limits, frame.cursor)?;
        let mut action = None;
        loop {
            let op = match reader.next_op() {
                Ok(Some(op)) => op,
                Ok(None) => break,
                Err(error) => {
                    recover(resolver, error, context.id, "render_content_failed")?;
                    break;
                }
            };
            self.step()?;
            let result = if op.name == b"ID" {
                let mut header = [Operand::Null; 64];
                let count = op.operands.len().min(header.len());
                if let Some(destination) = header.get_mut(..count) {
                    destination.copy_from_slice(op.operands.get(..count).unwrap_or(&[]));
                }
                self.inline_image(
                    resolver, header.get(..count).unwrap_or(&[]), &mut reader, context.id,
                ).map(|()| None)
            } else {
                self.execute(resolver, op, &context)
            };
            match result {
                Ok(Some(next)) => {
                    action = Some(next);
                    break;
                }
                Ok(None) => {}
                Err(error) => {
                    recover(resolver, error, context.id, "render_operator_failed")?;
                }
            }
        }
        if reader.dropped_operands() != 0 {
            warn(resolver, context.id, "content_operands_dropped");
        }
        if reader.junk_tokens() != 0 {
            warn(resolver, context.id, "content_junk_skipped");
        }
        frame.cursor = reader.cursor();
        if let Some(action) = action {
            self.budget.push(tasks, Task::Stream(frame))?;
            match action {
                Action::Form(invocation) => {
                    if let Err(error) = self.enter_form(resolver, invocation, tasks) {
                        recover(resolver, error, context.id, "render_form_failed")?;
                    }
                }
                Action::Show(range) => {
                    match self.make_run(context.clone(), range) {
                        Ok(run) => self.budget.push(tasks, Task::Run(run))?,
                        Err(error) => {
                            recover(resolver, error, context.id, "render_text_failed")?;
                        }
                    }
                }
                Action::Array(range) => {
                    self.budget.push(tasks, Task::Array(ArrayTask {
                        context, range, cursor: ContentCursor::default(),
                    }))?;
                }
            }
        }
        Ok(())
    }

    fn array(
        &mut self,
        resolver: &mut Resolver<'_>,
        mut task: ArrayTask,
        tasks: &mut Vec<Task>,
    ) -> Result<(), DocError> {
        let context = task.context.clone();
        let bytes = context.bytes.get(task.range.clone()).ok_or_else(|| bad("text_array"))?;
        let mut values = Values::resume(bytes, &self.limits, task.cursor);
        loop {
            let value = match values.next_value() {
                Ok(Some(value)) => value,
                Ok(None) => return Ok(()),
                Err(error) => {
                    recover(resolver, error, context.id, "render_text_array_failed")?;
                    return Ok(());
                }
            };
            self.step()?;
            match value {
                Operand::Number(value) => {
                    if let Some(value) = finite(value) {
                        self.adjust(value);
                    }
                }
                Operand::String(raw) => {
                    let range = borrowed_range(&context.bytes, raw)?;
                    task.cursor = values.cursor();
                    let run = self.make_run(context.clone(), range)?;
                    self.budget.push(tasks, Task::Array(task))?;
                    self.budget.push(tasks, Task::Run(run))?;
                    return Ok(());
                }
                _ => {}
            }
        }
    }

    fn make_run(&mut self, context: Context, range: Range<usize>) -> Result<Run, DocError> {
        let raw = context.bytes.get(range.clone()).ok_or_else(|| bad("text_string"))?;
        let body = raw.get(1..raw.len().saturating_sub(1)).unwrap_or(&[]);
        let borrowed = raw.first() == Some(&b'(')
            && !body.contains(&b'\\') && !body.contains(&b'\r');
        let mut decoded = core::mem::take(&mut self.strings);
        decoded.clear();
        let range = if borrowed {
            range.start.saturating_add(1)..range.end.saturating_sub(1)
        } else {
            let _ = decode_string(raw, &mut decoded, self.limits.max_decoded_bytes)?;
            self.budget.charge(decoded.len())?;
            range
        };
        let font_index = self.graphics.text.font;
        let font = font_index.and_then(|index| self.fonts.get(index))
            .map(|entry| entry.font.clone());
        Ok(Run {
            context, range, decoded, borrowed, offset: 0, font, font_index,
            paths: Vec::new(),
        })
    }

    fn run(
        &mut self,
        resolver: &mut Resolver<'_>,
        mut run: Run,
        tasks: &mut Vec<Task>,
    ) -> Result<(), DocError> {
        loop {
            let bytes = if run.borrowed {
                run.context.bytes.get(run.range.clone()).unwrap_or(&[])
            } else {
                &run.decoded
            };
            let remaining = bytes.get(run.offset..).unwrap_or(&[]);
            let Some(first) = remaining.first().copied() else { break };
            let glyph = run.font.as_ref()
                .and_then(|font| font.glyphs(remaining).next())
                .unwrap_or(Glyph {
                    code: u32::from(first), byte_len: 1, width: 0.5,
                    text: GlyphText::from_char('\u{fffd}'), mapped: false,
                    word_space: first == 32,
                });
            let consumed = usize::from(glyph.byte_len).max(1).min(remaining.len());
            run.offset = run.offset.checked_add(consumed).ok_or_else(|| bad("text_offset"))?;
            self.step()?;
            let text = self.graphics.text;
            let vertical = run.font.as_ref().map(Font::is_vertical).unwrap_or(false);
            let spaces = text.character_space
                + if glyph.word_space { text.word_space } else { 0.0 };
            let (dx, dy) = if vertical {
                (0.0, -text.size + spaces)
            } else {
                (((glyph.width * f64::from(text.size) + f64::from(spaces))
                    * f64::from(text.horizontal_scale)) as f32, 0.0)
            };
            if text.mode >= 4 {
                self.clip_requested = true;
            }
            let transform = Matrix([
                text.size * text.horizontal_scale / 1000.0, 0.0,
                0.0, text.size / 1000.0, 0.0, text.rise,
            ]).then(self.text_matrix).then(self.graphics.ctm);
            let is_type3 = run.font.as_ref().map(|font| font.kind() == FontKind::Type3)
                .unwrap_or(false);
            let mut invocation = None;
            if text.mode != 3 {
                if is_type3 {
                    let result = self.type3_invocation(
                        resolver, run.font_index, glyph.code, run.context.resources.as_ref(),
                    );
                    match result {
                        Ok(Some(mut form)) => {
                            let matrix = run.font.as_ref().map(Font::font_matrix)
                                .unwrap_or([0.001, 0.0, 0.0, 0.001, 0.0, 0.0]);
                            let matrix = matrix.map(|value| value as f32);
                            let scale = Matrix([
                                text.size * text.horizontal_scale, 0.0,
                                0.0, text.size, 0.0, text.rise,
                            ]);
                            form.matrix = Some(Matrix(matrix).then(scale).then(self.text_matrix));
                            form.type3_mode = Some(text.mode);
                            invocation = Some(form);
                        }
                        Ok(None) => {
                            self.output.missing_fonts = self.output.missing_fonts.saturating_add(1);
                        }
                        Err(error) => {
                            recover(resolver, error, run.context.id, "type3_glyph_failed")?;
                            self.output.missing_fonts = self.output.missing_fonts.saturating_add(1);
                        }
                    }
                } else {
                    let outline = run.font_index.and_then(|index| self.fonts.get(index))
                        .and_then(|entry| {
                            entry.source.as_ref().and_then(|source| source.outline(&glyph))
                        });
                    if let Some(mut path) = outline {
                        account_path(&path, &mut self.budget)?;
                        if let Err(error) = transform_path(&mut path, transform) {
                            recover(resolver, error, run.context.id, "text_geometry_failed")?;
                        } else {
                            if text.mode >= 4 {
                                append_path(&mut self.text_clip, &path, &mut self.budget)?;
                            }
                            match text.mode & 3 {
                                0 => self.budget.push(&mut run.paths, GlyphPath { path })?,
                                1 => {
                                    let stroke = self.device_stroke()?;
                                    let dash = self.device_dash()?;
                                    let paint = self.graphics.stroke_color.paint(
                                        resolver, self.graphics.ctm, self.graphics.stroke_alpha,
                                    )?;
                                    self.emit(Item::Stroke { path, stroke, dash, paint })?;
                                }
                                2 => {
                                    let second = copy_path(&path, &mut self.budget)?;
                                    let paint = self.graphics.fill.paint(
                                        resolver, self.graphics.ctm, self.graphics.fill_alpha,
                                    )?;
                                    self.emit(Item::Fill {
                                        path, rule: FillRule::NonZero, paint,
                                    })?;
                                    let stroke = self.device_stroke()?;
                                    let dash = self.device_dash()?;
                                    let paint = self.graphics.stroke_color.paint(
                                        resolver, self.graphics.ctm, self.graphics.stroke_alpha,
                                    )?;
                                    self.emit(Item::Stroke { path: second, stroke, dash, paint })?;
                                }
                                _ => {}
                            }
                        }
                    } else {
                        self.output.missing_fonts = self.output.missing_fonts.saturating_add(1);
                    }
                }
            }
            self.advance(dx, dy);
            if let Some(invocation) = invocation {
                self.flush_glyphs(resolver, &mut run)?;
                let id = run.context.id;
                self.budget.push(tasks, Task::Run(run))?;
                if let Err(error) = self.enter_form(resolver, invocation, tasks) {
                    recover(resolver, error, id, "type3_glyph_failed")?;
                    self.output.missing_fonts = self.output.missing_fonts.saturating_add(1);
                }
                return Ok(());
            }
        }
        self.flush_glyphs(resolver, &mut run)?;
        run.decoded.clear();
        if run.decoded.capacity() > self.strings.capacity() {
            self.strings = run.decoded;
        }
        Ok(())
    }

    fn flush_glyphs(&mut self, resolver: &mut Resolver<'_>, run: &mut Run)
        -> Result<(), DocError>
    {
        if !run.paths.is_empty() {
            let paint = self.graphics.fill.paint(
                resolver, self.graphics.ctm, self.graphics.fill_alpha,
            )?;
            self.emit(Item::Glyphs {
                glyphs: core::mem::take(&mut run.paths), paint, stroke: None,
            })?;
        }
        Ok(())
    }

    fn enter_form(
        &mut self,
        resolver: &mut Resolver<'_>,
        invocation: Invocation,
        tasks: &mut Vec<Task>,
    ) -> Result<(), DocError> {
        let id = invocation.object.id();
        if id.is_some_and(|id| self.active.contains(&id)) {
            warn(resolver, id, "form_cycle");
            return Ok(());
        }
        check(LimitKind::Depth, self.active.len() as u64, FORM_DEPTH)?;
        let dict = invocation.object.dict().ok_or_else(|| bad("form_dictionary"))?;
        let matrix = match invocation.matrix {
            Some(matrix) => matrix,
            None => read_array::<6>(resolver, dict, b"Matrix")?
                .map(Matrix).unwrap_or(Matrix::IDENTITY),
        };
        let ctm = matrix.then(self.graphics.ctm);
        if !ctm.is_finite() {
            return Err(bad("form_matrix"));
        }
        let bbox = match invocation.bbox {
            Some(bbox) => Some(bbox),
            None => read_array::<4>(resolver, dict, b"BBox")?,
        };
        let resources = resolver.resolve_dict_key(dict, b"Resources")?
            .filter(|value| value.get().as_dict().is_some())
            .or(invocation.inherited);
        let bytes = self.decode(resolver, &invocation.object)?;
        let bbox_path = bbox.map(|bbox| rect_path(bbox, ctm, &mut self.budget)).transpose()?;
        let snapshot = Snapshot {
            graphics: self.graphics.clone(),
            path: core::mem::take(&mut self.path),
            text_matrix: self.text_matrix,
            line_matrix: self.line_matrix,
            in_text: self.in_text,
            text_clip: core::mem::take(&mut self.text_clip),
            clip_requested: self.clip_requested,
            saved_base: self.saved.len(),
            item_start: self.output.items.len(),
            type3_mode: invocation.type3_mode,
        };
        self.graphics.ctm = ctm;
        self.in_text = false;
        self.clip_requested = false;
        self.text_matrix = Matrix::IDENTITY;
        self.line_matrix = Matrix::IDENTITY;
        if let Some(path) = bbox_path {
            self.push_clip(path, FillRule::NonZero)?;
        }
        if let Some(id) = id {
            self.budget.push(&mut self.active, id)?;
        } else {
            return Err(bad("indirect_form"));
        }
        let context = Context {
            bytes, resources, id, graphics_base: self.saved.len(),
        };
        self.budget.push(tasks, Task::Restore(snapshot))?;
        self.budget.push(tasks, Task::Stream(Frame {
            context, cursor: ContentCursor::default(),
        }))?;
        Ok(())
    }

    fn restore(&mut self, resolver: &mut Resolver<'_>, snapshot: Snapshot)
        -> Result<(), DocError>
    {
        let mut captured = Path::default();
        if snapshot.type3_mode.is_some_and(|mode| mode >= 4) {
            warn(resolver, self.active.last().copied(), "type3_text_clip_approximate");
            for item in self.output.items.get(snapshot.item_start..).unwrap_or(&[]) {
                match item {
                    Item::Fill { path, .. } | Item::Stroke { path, .. } => {
                        append_path(&mut captured, path, &mut self.budget)?;
                    }
                    Item::Glyphs { glyphs, .. } => {
                        for glyph in glyphs {
                            append_path(&mut captured, &glyph.path, &mut self.budget)?;
                        }
                    }
                    _ => {}
                }
            }
        }
        if self.saved.len() > snapshot.saved_base {
            warn(resolver, self.active.last().copied(), "graphics_state_unclosed");
        }
        self.saved.truncate(snapshot.saved_base);
        self.pop_clips(snapshot.graphics.clips)?;
        if snapshot.type3_mode == Some(7) {
            self.output.items.truncate(snapshot.item_start);
        }
        self.graphics = snapshot.graphics;
        self.path = snapshot.path;
        self.text_matrix = snapshot.text_matrix;
        self.line_matrix = snapshot.line_matrix;
        self.in_text = snapshot.in_text;
        self.text_clip = snapshot.text_clip;
        self.clip_requested = snapshot.clip_requested;
        if snapshot.type3_mode.is_some_and(|mode| mode >= 4) {
            self.clip_requested = true;
            append_path(&mut self.text_clip, &captured, &mut self.budget)?;
        }
        self.active.pop();
        Ok(())
    }

    fn select_font(&mut self, resolver: &mut Resolver<'_>, value: Value)
        -> Result<usize, DocError>
    {
        if let Some(index) = self.fonts.iter().position(|entry| same_value(&entry.dict, &value)) {
            return Ok(index);
        }
        let dict = value.get().as_dict().ok_or_else(|| bad("font_dictionary"))?;
        let font = self.font_cache.load(resolver, &value)?;
        let source = match self.glyph_cache.load(
            resolver, &font, &value, self.fallback.as_ref(),
        ) {
            Ok(source) => source,
            Err(error) => {
                recover(resolver, error, value.object().id(), "font_program_failed")?;
                None
            }
        };
        let type3 = if font.kind() == FontKind::Type3 {
            Some(load_type3(resolver, dict, &mut self.budget)?)
        } else {
            None
        };
        let index = self.fonts.len();
        self.budget.push(&mut self.fonts, LoadedFont { dict: value, font, source, type3 })?;
        Ok(index)
    }

    fn type3_invocation(
        &mut self,
        resolver: &mut Resolver<'_>,
        index: Option<usize>,
        code: u32,
        inherited: Option<&Value>,
    ) -> Result<Option<Invocation>, DocError> {
        let Some(type3) = index.and_then(|index| self.fonts.get(index))
            .and_then(|entry| entry.type3.as_ref()) else { return Ok(None) };
        let Some(name) = usize::try_from(code).ok().and_then(|code| type3.names.get(code))
            .and_then(GlyphName::bytes) else { return Ok(None) };
        let Some(dict) = type3.procs.get().as_dict() else { return Ok(None) };
        let Some(value) = resolver.resolve_dict_key(dict, name)? else { return Ok(None) };
        Ok(Some(Invocation {
            object: value.object().clone(),
            inherited: type3.resources.clone().or_else(|| inherited.cloned()),
            matrix: None,
            bbox: type3.bbox,
            type3_mode: None,
        }))
    }

    fn execute(
        &mut self,
        resolver: &mut Resolver<'_>,
        op: Op<'_, '_>,
        context: &Context,
    ) -> Result<Option<Action>, DocError> {
        let args = op.operands;
        match op.name {
            b"q" => {
                check(
                    LimitKind::Depth, self.saved.len().saturating_add(1) as u64,
                    u64::from(self.limits.max_depth),
                )?;
                self.budget.push(&mut self.saved, self.graphics.clone())?;
            }
            b"Q" => {
                if self.saved.len() > context.graphics_base {
                    if let Some(saved) = self.saved.pop() {
                        self.pop_clips(saved.clips)?;
                        self.graphics = saved;
                    }
                } else {
                    warn(resolver, context.id, "graphics_state_underflow");
                }
            }
            b"cm" => {
                if let Some(values) = numbers::<6>(args) {
                    let matrix = Matrix(values).then(self.graphics.ctm);
                    if !matrix.is_finite() { return Err(bad("graphics_matrix")); }
                    self.graphics.ctm = matrix;
                }
            }
            b"w" => {
                if let Some([width]) = numbers::<1>(args) {
                    if width < 0.0 { return Err(bad("stroke_width")); }
                    self.graphics.stroke.width = width;
                }
            }
            b"J" | b"j" => {
                if let Some([value]) = numbers::<1>(args) {
                    if !(0.0..=2.0).contains(&value) || value != f32::from(value as u8) {
                        return Err(bad("stroke_style"));
                    }
                    if op.name == b"J" {
                        self.graphics.stroke.cap = value as u8;
                    } else {
                        self.graphics.stroke.join = value as u8;
                    }
                }
            }
            b"M" => {
                if let Some([value]) = numbers::<1>(args) {
                    if value < 1.0 { return Err(bad("stroke_miter")); }
                    self.graphics.stroke.miter = value;
                }
            }
            b"d" => {
                if let [Operand::Array(raw), Operand::Number(phase)] = args {
                    let phase = finite(*phase).ok_or_else(|| bad("dash_phase"))?;
                    let mut values = Values::new(container_body(raw), &self.limits);
                    let mut array = Vec::new();
                    while let Some(value) = values.next_value()? {
                        self.step()?;
                        let Operand::Number(value) = value else { return Err(bad("dash_array")) };
                        let value = finite(value).filter(|value| *value >= 0.0)
                            .ok_or_else(|| bad("dash_array"))?;
                        self.budget.push(&mut array, value)?;
                    }
                    self.set_dash(array, phase)?;
                }
            }
            b"gs" => {
                if let [Operand::Name(name)] = args {
                    let name = decode_name(name, &mut self.names, self.limits.max_object_bytes)?;
                    if let Some(value) = resource(
                        resolver, context.resources.as_ref(), b"ExtGState", name,
                    )? {
                        self.ext_gstate(resolver, &value)?;
                    } else {
                        warn(resolver, context.id, "ext_gstate_missing");
                    }
                }
            }
            b"m" | b"l" => {
                if let Some([x, y]) = numbers::<2>(args) {
                    let point = checked_point(self.graphics.ctm, x, y)?;
                    if op.name == b"m" {
                        path_push(&mut self.path.path, PathEl::MoveTo(point.0, point.1), &mut self.budget)?;
                        self.path.start = Some(point);
                        self.path.point = Some(point);
                    } else if self.path.point.is_some() {
                        path_push(&mut self.path.path, PathEl::LineTo(point.0, point.1), &mut self.budget)?;
                        self.path.point = Some(point);
                    }
                }
            }
            b"c" => {
                if let Some([x1, y1, x2, y2, x3, y3]) = numbers::<6>(args) {
                    let a = checked_point(self.graphics.ctm, x1, y1)?;
                    let b = checked_point(self.graphics.ctm, x2, y2)?;
                    let c = checked_point(self.graphics.ctm, x3, y3)?;
                    self.curve(a, b, c)?;
                }
            }
            b"v" => {
                if let (Some(point), Some([x2, y2, x3, y3])) =
                    (self.path.point, numbers::<4>(args))
                {
                    let b = checked_point(self.graphics.ctm, x2, y2)?;
                    let c = checked_point(self.graphics.ctm, x3, y3)?;
                    self.curve(point, b, c)?;
                }
            }
            b"y" => {
                if let Some([x1, y1, x3, y3]) = numbers::<4>(args) {
                    let a = checked_point(self.graphics.ctm, x1, y1)?;
                    let c = checked_point(self.graphics.ctm, x3, y3)?;
                    self.curve(a, c, c)?;
                }
            }
            b"h" => self.close_path()?,
            b"re" => {
                if let Some([x, y, width, height]) = numbers::<4>(args) {
                    let rectangle = rect_path(
                        [x, y, x + width, y + height], self.graphics.ctm, &mut self.budget,
                    )?;
                    append_path(&mut self.path.path, &rectangle, &mut self.budget)?;
                    let point = checked_point(self.graphics.ctm, x, y)?;
                    self.path.point = Some(point);
                    self.path.start = Some(point);
                }
            }
            b"W" => self.path.clip = Some(FillRule::NonZero),
            b"W*" => self.path.clip = Some(FillRule::EvenOdd),
            b"S" | b"s" | b"f" | b"F" | b"f*" | b"B" | b"B*" | b"b" | b"b*" | b"n" => {
                if matches!(op.name, b"s" | b"b" | b"b*") { self.close_path()?; }
                let fill = matches!(op.name, b"f" | b"F" | b"f*" | b"B" | b"B*" | b"b" | b"b*");
                let stroke = matches!(op.name, b"S" | b"s" | b"B" | b"B*" | b"b" | b"b*");
                let rule = if matches!(op.name, b"f*" | b"B*" | b"b*") {
                    FillRule::EvenOdd
                } else {
                    FillRule::NonZero
                };
                self.finish_path(resolver, fill, stroke, rule)?;
            }
            b"CS" | b"cs" => {
                if let [Operand::Name(name)] = args {
                    let name = decode_name(name, &mut self.names, self.limits.max_object_bytes)?;
                    let space = ColorSpace::named_or_resource(
                        resolver, context.resources.as_ref(), name,
                    )?;
                    let color = ColorState::new(space);
                    if op.name == b"CS" {
                        self.graphics.stroke_color = color;
                    } else {
                        self.graphics.fill = color;
                    }
                }
            }
            b"G" | b"g" | b"RG" | b"rg" | b"K" | b"k" => {
                let space = match op.name {
                    b"G" | b"g" => ColorSpace::device_gray(),
                    b"RG" | b"rg" => ColorSpace::device_rgb(),
                    _ => ColorSpace::device_cmyk(),
                };
                let mut color = ColorState::new(space);
                color.set(resolver, context.resources.as_ref(), args)?;
                if matches!(op.name, b"G" | b"RG" | b"K") {
                    self.graphics.stroke_color = color;
                } else {
                    self.graphics.fill = color;
                }
            }
            b"SC" | b"SCN" | b"sc" | b"scn" => {
                if matches!(op.name, b"SC" | b"SCN") {
                    self.graphics.stroke_color.set(resolver, context.resources.as_ref(), args)?;
                } else {
                    self.graphics.fill.set(resolver, context.resources.as_ref(), args)?;
                }
            }
            b"sh" => {
                if let [Operand::Name(name)] = args {
                    let name = decode_name(name, &mut self.names, self.limits.max_object_bytes)?;
                    if let Some(value) = resource(
                        resolver, context.resources.as_ref(), b"Shading", name,
                    )? {
                        let shading = paint::shading(
                            resolver, value.get(), self.graphics.ctm, self.graphics.fill_alpha,
                        )?;
                        let path = match shading.bbox {
                            Some(bbox) => rect_path(bbox, self.graphics.ctm, &mut self.budget)?,
                            None => rect_path(
                                [0.0, 0.0, self.output.width as f32, self.output.height as f32],
                                Matrix::IDENTITY, &mut self.budget,
                            )?,
                        };
                        self.emit(Item::Fill {
                            path, rule: FillRule::NonZero, paint: shading.paint,
                        })?;
                    } else {
                        warn(resolver, context.id, "shading_missing");
                    }
                }
            }
            b"Do" => {
                if let [Operand::Name(name)] = args {
                    let name = decode_name(name, &mut self.names, self.limits.max_object_bytes)?;
                    if let Some(value) = resource(
                        resolver, context.resources.as_ref(), b"XObject", name,
                    )? {
                        let dict = value.get().as_dict().ok_or_else(|| bad("xobject_dictionary"))?;
                        let subtype = resolver.resolve_dict_key(dict, b"Subtype")?;
                        let subtype = subtype.as_ref().and_then(|value| value.get().as_name());
                        if subtype.is_some_and(|name| name.eq_bytes(b"Image")) {
                            if let Some(id) = value.object().id() {
                                if !self.graphics.ctm.is_finite() { return Err(bad("image_matrix")); }
                                self.emit(Item::Image {
                                    image: ImageSource::Object(id),
                                    transform: self.graphics.ctm,
                                    alpha: self.graphics.fill_alpha,
                                })?;
                            }
                        } else if subtype.is_some_and(|name| name.eq_bytes(b"Form")) {
                            return Ok(Some(Action::Form(Invocation {
                                object: value.object().clone(),
                                inherited: context.resources.clone(),
                                matrix: None, bbox: None, type3_mode: None,
                            })));
                        } else {
                            warn(resolver, value.object().id(), "xobject_unsupported");
                        }
                    } else {
                        warn(resolver, context.id, "xobject_missing");
                    }
                }
            }
            b"BT" => {
                if self.in_text {
                    warn(resolver, context.id, "render_text_nested");
                    self.end_text()?;
                }
                self.text_matrix = Matrix::IDENTITY;
                self.line_matrix = Matrix::IDENTITY;
                self.in_text = true;
                self.text_clip = Path::default();
                self.clip_requested = false;
            }
            b"ET" => self.end_text()?,
            b"Tc" | b"Tw" | b"Tz" | b"TL" | b"Ts" | b"Tr" => {
                if let Some([value]) = numbers::<1>(args) {
                    match op.name {
                        b"Tc" => self.graphics.text.character_space = value,
                        b"Tw" => self.graphics.text.word_space = value,
                        b"Tz" => self.graphics.text.horizontal_scale = value / 100.0,
                        b"TL" => self.graphics.text.leading = value,
                        b"Ts" => self.graphics.text.rise = value,
                        _ => {
                            if !(0.0..=7.0).contains(&value) || value != f32::from(value as u8) {
                                return Err(bad("text_render_mode"));
                            }
                            self.graphics.text.mode = value as u8;
                        }
                    }
                }
            }
            b"Tf" => {
                if let [Operand::Name(name), Operand::Number(size)] = args {
                    self.graphics.text.size = finite(*size).ok_or_else(|| bad("font_size"))?;
                    self.graphics.text.font = None;
                    let name = decode_name(name, &mut self.names, self.limits.max_object_bytes)?;
                    if let Some(value) = resource(
                        resolver, context.resources.as_ref(), b"Font", name,
                    )? {
                        self.graphics.text.font = Some(self.select_font(resolver, value)?);
                    } else {
                        warn(resolver, context.id, "font_missing");
                    }
                }
            }
            b"Tm" => {
                if let Some(values) = numbers::<6>(args) {
                    self.text_matrix = Matrix(values);
                    self.line_matrix = self.text_matrix;
                }
            }
            b"Td" | b"TD" => {
                if let Some([x, y]) = numbers::<2>(args) {
                    if op.name == b"TD" { self.graphics.text.leading = -y; }
                    self.move_line(x, y);
                }
            }
            b"T*" => self.next_line(),
            b"'" => {
                self.next_line();
                if self.in_text {
                    if let [Operand::String(raw)] = args {
                        return Ok(Some(Action::Show(borrowed_range(&context.bytes, raw)?)));
                    }
                }
            }
            b"\"" => {
                if let [Operand::Number(word), Operand::Number(character), Operand::String(raw)] = args {
                    self.graphics.text.word_space = finite(*word).ok_or_else(|| bad("text_spacing"))?;
                    self.graphics.text.character_space = finite(*character).ok_or_else(|| bad("text_spacing"))?;
                    self.next_line();
                    if self.in_text {
                        return Ok(Some(Action::Show(borrowed_range(&context.bytes, raw)?)));
                    }
                }
            }
            b"Tj" => {
                if self.in_text {
                    if let [Operand::String(raw)] = args {
                        return Ok(Some(Action::Show(borrowed_range(&context.bytes, raw)?)));
                    }
                }
            }
            b"TJ" => {
                if self.in_text {
                    if let [Operand::Array(raw)] = args {
                        let body = container_body(raw);
                        return Ok(Some(Action::Array(borrowed_range(&context.bytes, body)?)));
                    }
                }
            }
            b"ri" | b"i" | b"BI" | b"EI" | b"d0" | b"d1"
                | b"BMC" | b"BDC" | b"EMC" | b"MP" | b"DP" | b"BX" | b"EX" => {}
            _ => warn(resolver, context.id, "render_operator_unsupported"),
        }
        Ok(None)
    }

    fn curve(&mut self, a: (f32, f32), b: (f32, f32), c: (f32, f32))
        -> Result<(), DocError>
    {
        if self.path.point.is_some() {
            path_push(
                &mut self.path.path, PathEl::CubicTo(a.0, a.1, b.0, b.1, c.0, c.1),
                &mut self.budget,
            )?;
            self.path.point = Some(c);
        }
        Ok(())
    }

    fn close_path(&mut self) -> Result<(), DocError> {
        if self.path.point.is_some() {
            path_push(&mut self.path.path, PathEl::Close, &mut self.budget)?;
            self.path.point = self.path.start;
        }
        Ok(())
    }

    fn finish_path(
        &mut self,
        resolver: &mut Resolver<'_>,
        fill: bool,
        stroke: bool,
        rule: FillRule,
    ) -> Result<(), DocError> {
        let current = core::mem::take(&mut self.path);
        let clip = current.clip.map(|_| copy_path(&current.path, &mut self.budget)).transpose()?;
        let second = if fill && stroke {
            Some(copy_path(&current.path, &mut self.budget)?)
        } else {
            None
        };
        if fill {
            let paint = self.graphics.fill.paint(
                resolver, self.graphics.ctm, self.graphics.fill_alpha,
            )?;
            self.emit(Item::Fill { path: current.path, rule, paint })?;
            if let Some(path) = second {
                self.emit_stroke(resolver, path)?;
            }
        } else if stroke {
            self.emit_stroke(resolver, current.path)?;
        }
        if let (Some(path), Some(rule)) = (clip, current.clip) {
            self.push_clip(path, rule)?;
        }
        Ok(())
    }

    fn emit_stroke(&mut self, resolver: &mut Resolver<'_>, path: Path)
        -> Result<(), DocError>
    {
        let stroke = self.device_stroke()?;
        let dash = self.device_dash()?;
        let paint = self.graphics.stroke_color.paint(
            resolver, self.graphics.ctm, self.graphics.stroke_alpha,
        )?;
        self.emit(Item::Stroke { path, stroke, dash, paint })
    }

    fn device_stroke(&self) -> Result<Stroke, DocError> {
        let width = self.graphics.stroke.width * self.graphics.ctm.mean_scale();
        if !width.is_finite() { return Err(bad("stroke_geometry")); }
        Ok(Stroke { width, ..self.graphics.stroke })
    }

    fn device_dash(&mut self) -> Result<Option<Dash>, DocError> {
        let Some(dash) = self.graphics.dash.as_ref() else { return Ok(None) };
        let scale = self.graphics.ctm.mean_scale();
        let phase = dash.phase * scale;
        if !scale.is_finite() || !phase.is_finite() { return Err(bad("dash_geometry")); }
        let mut array = Vec::new();
        self.budget.reserve(&mut array, dash.array.len())?;
        for value in &dash.array {
            let value = *value * scale;
            if !value.is_finite() { return Err(bad("dash_geometry")); }
            array.push(value);
        }
        Ok(Some(Dash { array, phase }))
    }

    fn set_dash(&mut self, array: Vec<f32>, phase: f32) -> Result<(), DocError> {
        if !phase.is_finite() || !array.iter().all(|value| value.is_finite() && *value >= 0.0)
            || (!array.is_empty() && array.iter().all(|value| *value == 0.0))
        {
            return Err(bad("dash_array"));
        }
        self.graphics.dash = if array.is_empty() { None } else { Some(Arc::new(Dash { array, phase })) };
        Ok(())
    }

    fn ext_gstate(&mut self, resolver: &mut Resolver<'_>, value: &Value)
        -> Result<(), DocError>
    {
        let dict = value.get().as_dict().ok_or_else(|| bad("ext_gstate_dictionary"))?;
        if let Some(width) = number(resolver, dict, b"LW")? {
            if width < 0.0 { return Err(bad("stroke_width")); }
            self.graphics.stroke.width = width;
        }
        for key in [b"LC".as_slice(), b"LJ".as_slice()] {
            if let Some(value) = number(resolver, dict, key)? {
                if !(0.0..=2.0).contains(&value) || value != f32::from(value as u8) {
                    return Err(bad("stroke_style"));
                }
                if key == b"LC" { self.graphics.stroke.cap = value as u8; }
                else { self.graphics.stroke.join = value as u8; }
            }
        }
        if let Some(value) = number(resolver, dict, b"ML")? {
            if value < 1.0 { return Err(bad("stroke_miter")); }
            self.graphics.stroke.miter = value;
        }
        if let Some(value) = number(resolver, dict, b"CA")? {
            self.graphics.stroke_alpha = value.clamp(0.0, 1.0);
        }
        if let Some(value) = number(resolver, dict, b"ca")? {
            self.graphics.fill_alpha = value.clamp(0.0, 1.0);
        }
        if let Some(mode) = blend_mode(resolver, dict)? {
            self.graphics.blend = mode;
        }
        if let Some(value) = resolver.resolve_dict_key(dict, b"D")? {
            let pair = value.get().as_array().filter(|array| array.len() == 2)
                .ok_or_else(|| bad("ext_gstate_dash"))?;
            let array = resolver.resolve(pair.get(0).ok_or_else(|| bad("ext_gstate_dash"))?)?;
            let phase = resolver.resolve(pair.get(1).ok_or_else(|| bad("ext_gstate_dash"))?)?;
            let phase = phase.get().as_f64().and_then(finite).ok_or_else(|| bad("dash_phase"))?;
            let array = array.get().as_array().ok_or_else(|| bad("dash_array"))?;
            let mut values = Vec::new();
            self.budget.reserve(&mut values, array.len())?;
            for value in array.iter() {
                self.step()?;
                let value = resolver.resolve(value)?;
                values.push(value.get().as_f64().and_then(finite)
                    .filter(|value| *value >= 0.0).ok_or_else(|| bad("dash_array"))?);
            }
            self.set_dash(values, phase)?;
        }
        if let Some(value) = resolver.resolve_dict_key(dict, b"Font")? {
            let array = value.get().as_array().filter(|array| array.len() == 2)
                .ok_or_else(|| bad("ext_gstate_font"))?;
            let font = resolver.resolve(array.get(0).ok_or_else(|| bad("ext_gstate_font"))?)?;
            let size = resolver.resolve(array.get(1).ok_or_else(|| bad("ext_gstate_font"))?)?;
            self.graphics.text.size = size.get().as_f64().and_then(finite)
                .ok_or_else(|| bad("font_size"))?;
            self.graphics.text.font = None;
            self.graphics.text.font = Some(self.select_font(resolver, font)?);
        }
        if let Some(mask) = resolver.resolve_dict_key(dict, b"SMask")? {
            if !mask.get().is_null()
                && !mask.get().as_name().is_some_and(|name| name.eq_bytes(b"None"))
            {
                warn(resolver, value.object().id(), "render_soft_mask_unsupported");
            }
        }
        Ok(())
    }

    fn inline_image(
        &mut self,
        resolver: &mut Resolver<'_>,
        header: &[Operand<'_>],
        reader: &mut Content<'_>,
        id: Option<ObjectRef>,
    ) -> Result<(), DocError> {
        let width = inline_dimension(inline_number(header, b"W", b"Width"))?;
        let height = inline_dimension(inline_number(header, b"H", b"Height"))?;
        let bpc = inline_number(header, b"BPC", b"BitsPerComponent");
        let mut channels = 1usize;
        let mut unsupported = bpc != Some(8.0);
        let mut decode = None;
        for pair in header.chunks_exact(2) {
            let Some(Operand::Name(key)) = pair.first() else { continue };
            let value = pair.get(1).copied().unwrap_or(Operand::Null);
            if name_eq(key, b"F") || name_eq(key, b"Filter") {
                unsupported |= !matches!(value, Operand::Null);
            } else if name_eq(key, b"IM") || name_eq(key, b"ImageMask") {
                unsupported |= matches!(value, Operand::Bool(true));
            } else if name_eq(key, b"CS") || name_eq(key, b"ColorSpace") {
                channels = match value {
                    Operand::Name(name) if name_eq(name, b"G") || name_eq(name, b"DeviceGray") => 1,
                    Operand::Name(name) if name_eq(name, b"RGB") || name_eq(name, b"DeviceRGB") => 3,
                    _ => { unsupported = true; 1 }
                };
            } else if name_eq(key, b"D") || name_eq(key, b"Decode") {
                decode = match value {
                    Operand::Array(raw) => Some(raw),
                    _ => { unsupported = true; None }
                };
            }
        }
        if unsupported {
            warn(resolver, id, "inline_image_unsupported");
            return Ok(());
        }
        let pixels = u64::from(width).checked_mul(u64::from(height))
            .ok_or_else(|| refusal(LimitKind::DecodedBytes, u64::MAX, self.limits.max_decoded_bytes))?;
        let rgba_length = pixels.checked_mul(4)
            .ok_or_else(|| refusal(LimitKind::DecodedBytes, u64::MAX, self.limits.max_decoded_bytes))?;
        check(LimitKind::DecodedBytes, rgba_length, self.limits.max_decoded_bytes)?;
        let length = usize::try_from(pixels).ok().and_then(|pixels| pixels.checked_mul(channels))
            .ok_or_else(|| refusal(LimitKind::DecodedBytes, u64::MAX, self.limits.max_decoded_bytes))?;
        let mut pairs = [(0.0f32, 1.0f32); 3];
        if let Some(raw) = decode {
            let mut values = Values::new(container_body(raw), &self.limits);
            for pair in pairs.iter_mut().take(channels) {
                let Some(Operand::Number(low)) = values.next_value()? else { return Err(bad("inline_decode")) };
                let Some(Operand::Number(high)) = values.next_value()? else { return Err(bad("inline_decode")) };
                *pair = (
                    finite(low).ok_or_else(|| bad("inline_decode"))?,
                    finite(high).ok_or_else(|| bad("inline_decode"))?,
                );
            }
            if values.next_value()?.is_some() { return Err(bad("inline_decode")); }
        }
        let samples = reader.inline_bytes(length)?;
        let capacity = usize::try_from(rgba_length)
            .map_err(|_| refusal(LimitKind::DecodedBytes, rgba_length, isize::MAX as u64))?;
        let mut rgba = Vec::new();
        self.budget.reserve(&mut rgba, capacity)?;
        for sample in samples.chunks_exact(channels) {
            let mut rgb = [0u8; 3];
            for (slot, (value, pair)) in rgb.iter_mut().zip(sample.iter().zip(pairs.iter())) {
                let fraction = f32::from(*value) / 255.0;
                let value = pair.0 * (1.0 - fraction) + pair.1 * fraction;
                *slot = (value.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            }
            if channels == 1 {
                rgb = [rgb[0]; 3];
            }
            rgba.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
        }
        if !self.graphics.ctm.is_finite() { return Err(bad("image_matrix")); }
        self.emit(Item::Image {
            image: ImageSource::Inline { width, height, rgba },
            transform: self.graphics.ctm,
            alpha: self.graphics.fill_alpha,
        })
    }

    fn end_text(&mut self) -> Result<(), DocError> {
        if self.in_text && self.clip_requested {
            let path = core::mem::take(&mut self.text_clip);
            self.push_clip(path, FillRule::NonZero)?;
        }
        self.in_text = false;
        self.clip_requested = false;
        self.text_clip.els.clear();
        Ok(())
    }

    fn move_line(&mut self, x: f32, y: f32) {
        self.line_matrix = translation(x, y).then(self.line_matrix);
        self.text_matrix = self.line_matrix;
    }

    fn next_line(&mut self) {
        self.move_line(0.0, -self.graphics.text.leading);
    }

    fn advance(&mut self, x: f32, y: f32) {
        self.text_matrix = translation(x, y).then(self.text_matrix);
    }

    fn adjust(&mut self, value: f32) {
        let vertical = self.graphics.text.font.and_then(|index| self.fonts.get(index))
            .map(|entry| entry.font.is_vertical()).unwrap_or(false);
        let shift = -value / 1000.0 * self.graphics.text.size;
        if vertical { self.advance(0.0, shift); }
        else { self.advance(shift * self.graphics.text.horizontal_scale, 0.0); }
    }
}

fn load_type3(resolver: &mut Resolver<'_>, dict: DictRef<'_>, budget: &mut Budget)
    -> Result<Type3, DocError>
{
    let procs = resolver.resolve_dict_key(dict, b"CharProcs")?
        .filter(|value| value.get().as_dict().is_some())
        .ok_or_else(|| bad("type3_charprocs"))?;
    let encoding = resolver.resolve_dict_key(dict, b"Encoding")?;
    let mut base = BaseEncoding::Standard;
    let mut differences = None;
    if let Some(encoding) = &encoding {
        if let Some(name) = encoding.get().as_name() {
            base = BaseEncoding::from_name(name.decoded().as_ref()).unwrap_or(base);
        } else if let Some(dict) = encoding.get().as_dict() {
            if let Some(value) = resolver.resolve_dict_key(dict, b"BaseEncoding")? {
                base = value.get().as_name()
                    .and_then(|name| BaseEncoding::from_name(name.decoded().as_ref()))
                    .unwrap_or(base);
            }
            differences = resolver.resolve_dict_key(dict, b"Differences")?;
        }
    }
    let mut names = core::array::from_fn(|index| {
        base.glyph_name(index as u8).map(|name| GlyphName::Static(name.as_bytes()))
            .unwrap_or(GlyphName::Missing)
    });
    if let Some(array) = differences.as_ref().and_then(|value| value.get().as_array()) {
        let mut code = None;
        for value in array.iter() {
            let value = resolver.resolve(value)?;
            if let Some(number) = value.get().as_i64() {
                code = u8::try_from(number).ok();
            } else if let Some(name) = value.get().as_name() {
                if let Some(byte) = code {
                    let mapped = if name.encoded().contains(&b'#') {
                        let bytes = name.decoded();
                        budget.charge(bytes.len())?;
                        GlyphName::Decoded(Arc::from(bytes.as_ref()))
                    } else {
                        GlyphName::Pdf(value.clone())
                    };
                    if let Some(slot) = names.get_mut(usize::from(byte)) { *slot = mapped; }
                    code = byte.checked_add(1);
                }
            } else {
                code = None;
            }
        }
    }
    Ok(Type3 {
        names, procs,
        resources: resolver.resolve_dict_key(dict, b"Resources")?
            .filter(|value| value.get().as_dict().is_some()),
        bbox: read_array::<4>(resolver, dict, b"FontBBox")?,
    })
}

fn page_matrix(page: &Page, scale: f32) -> Result<(Matrix, u32, u32), DocError> {
    let [x0, y0, x1, y1] = page.crop_box;
    let unit = page.user_unit * f64::from(scale);
    if !scale.is_finite() || scale <= 0.0 || !unit.is_finite() || unit <= 0.0
        || !page.crop_box.iter().all(|value| value.is_finite())
    {
        return Err(bad("page_display_geometry"));
    }
    let width = (x1 - x0).max(0.0) * unit;
    let height = (y1 - y0).max(0.0) * unit;
    let (matrix, width, height) = match page.rotate % 360 {
        90 => ([0.0, unit, unit, 0.0, -y0 * unit, -x0 * unit], height, width),
        180 => ([-unit, 0.0, 0.0, unit, x1 * unit, -y0 * unit], width, height),
        270 => ([0.0, -unit, -unit, 0.0, y1 * unit, x1 * unit], height, width),
        _ => ([unit, 0.0, 0.0, -unit, -x0 * unit, y1 * unit], width, height),
    };
    let mut result = [0.0; 6];
    for (slot, value) in result.iter_mut().zip(matrix) {
        *slot = finite(value).ok_or_else(|| bad("page_display_geometry"))?;
    }
    Ok((Matrix(result), ceil_dimension(width)?, ceil_dimension(height)?))
}

fn ceil_dimension(value: f64) -> Result<u32, DocError> {
    if !value.is_finite() || value < 0.0 || value > f64::from(u32::MAX) {
        return Err(bad("page_display_geometry"));
    }
    let integer = value as u32;
    if f64::from(integer) < value {
        integer.checked_add(1).ok_or_else(|| bad("page_display_geometry"))
    } else {
        Ok(integer)
    }
}

fn inline_dimension(value: Option<f64>) -> Result<u32, DocError> {
    let value = value.filter(|value| value.is_finite() && *value >= 1.0
        && *value <= f64::from(u32::MAX)).ok_or_else(|| bad("inline_image_dimension"))?;
    let integer = value as u32;
    if f64::from(integer) != value { return Err(bad("inline_image_dimension")); }
    Ok(integer)
}

fn translation(x: f32, y: f32) -> Matrix {
    Matrix([1.0, 0.0, 0.0, 1.0, x, y])
}

fn borrowed_range(owner: &[u8], bytes: &[u8]) -> Result<Range<usize>, DocError> {
    let start = (bytes.as_ptr() as usize).checked_sub(owner.as_ptr() as usize)
        .ok_or_else(|| bad("content_range"))?;
    let end = start.checked_add(bytes.len()).filter(|end| *end <= owner.len())
        .ok_or_else(|| bad("content_range"))?;
    Ok(start..end)
}

pub(crate) fn checked_point(matrix: Matrix, x: f32, y: f32) -> Result<(f32, f32), DocError> {
    let point = matrix.point(x, y);
    if point.0.is_finite() && point.1.is_finite() { Ok(point) }
    else { Err(bad("path_geometry")) }
}

fn path_push(path: &mut Path, element: PathEl, budget: &mut Budget) -> Result<(), DocError> {
    check(LimitKind::Tokens, path.els.len().saturating_add(1) as u64, PATH_ELEMENTS)?;
    budget.push(&mut path.els, element)
}

fn account_path(path: &Path, budget: &mut Budget) -> Result<(), DocError> {
    check(LimitKind::Tokens, path.els.len() as u64, PATH_ELEMENTS)?;
    let bytes = path.els.len().checked_mul(core::mem::size_of::<PathEl>())
        .ok_or_else(|| refusal(LimitKind::Tokens, u64::MAX, PATH_ELEMENTS))?;
    budget.charge(bytes)
}

fn append_path(target: &mut Path, source: &Path, budget: &mut Budget) -> Result<(), DocError> {
    let requested = target.els.len().checked_add(source.els.len())
        .ok_or_else(|| refusal(LimitKind::Tokens, u64::MAX, PATH_ELEMENTS))?;
    check(LimitKind::Tokens, requested as u64, PATH_ELEMENTS)?;
    budget.reserve(&mut target.els, source.els.len())?;
    target.els.extend_from_slice(&source.els);
    Ok(())
}

fn copy_path(path: &Path, budget: &mut Budget) -> Result<Path, DocError> {
    let mut result = Path::default();
    append_path(&mut result, path, budget)?;
    Ok(result)
}

fn rect_path(bbox: [f32; 4], matrix: Matrix, budget: &mut Budget) -> Result<Path, DocError> {
    let [x0, y0, x1, y1] = bbox;
    let points = [
        checked_point(matrix, x0, y0)?, checked_point(matrix, x1, y0)?,
        checked_point(matrix, x1, y1)?, checked_point(matrix, x0, y1)?,
    ];
    let mut path = Path::default();
    budget.reserve(&mut path.els, 5)?;
    for (index, (x, y)) in points.into_iter().enumerate() {
        path.els.push(if index == 0 { PathEl::MoveTo(x, y) } else { PathEl::LineTo(x, y) });
    }
    path.els.push(PathEl::Close);
    Ok(path)
}

fn transform_path(path: &mut Path, matrix: Matrix) -> Result<(), DocError> {
    if !matrix.is_finite() { return Err(bad("text_geometry")); }
    for element in &mut path.els {
        *element = match *element {
            PathEl::MoveTo(x, y) => {
                let (x, y) = checked_point(matrix, x, y)?;
                PathEl::MoveTo(x, y)
            }
            PathEl::LineTo(x, y) => {
                let (x, y) = checked_point(matrix, x, y)?;
                PathEl::LineTo(x, y)
            }
            PathEl::CubicTo(x1, y1, x2, y2, x3, y3) => {
                let a = checked_point(matrix, x1, y1)?;
                let b = checked_point(matrix, x2, y2)?;
                let c = checked_point(matrix, x3, y3)?;
                PathEl::CubicTo(a.0, a.1, b.0, b.1, c.0, c.1)
            }
            PathEl::Close => PathEl::Close,
        };
    }
    Ok(())
}

fn same_value(left: &Value, right: &Value) -> bool {
    Arc::ptr_eq(left.object().buffer(), right.object().buffer())
        && left.get().span() == right.get().span()
}

fn numbers<const N: usize>(operands: &[Operand<'_>]) -> Option<[f32; N]> {
    if operands.len() != N { return None; }
    let mut result = [0.0; N];
    for (slot, operand) in result.iter_mut().zip(operands) {
        let Operand::Number(value) = operand else { return None };
        *slot = finite(*value)?;
    }
    Some(result)
}

pub(crate) fn read_array<const N: usize>(
    resolver: &mut Resolver<'_>,
    dict: DictRef<'_>,
    key: &[u8],
) -> Result<Option<[f32; N]>, DocError> {
    let Some(value) = resolver.resolve_dict_key(dict, key)? else { return Ok(None) };
    let array = value.get().as_array().filter(|array| array.len() == N)
        .ok_or_else(|| bad("render_number_array"))?;
    let mut result = [0.0; N];
    for (slot, value) in result.iter_mut().zip(array.iter()) {
        let value = resolver.resolve(value)?;
        *slot = value.get().as_f64().and_then(finite).ok_or_else(|| bad("render_number"))?;
    }
    Ok(Some(result))
}

fn blend_mode(
    resolver: &mut Resolver<'_>,
    dict: DictRef<'_>,
) -> Result<Option<BlendMode>, DocError> {
    fn supported(value: &Value) -> Option<BlendMode> {
        let name = value.get().as_name()?;
        if name.eq_bytes(b"Normal") || name.eq_bytes(b"Compatible") {
            Some(BlendMode::Normal)
        } else if name.eq_bytes(b"Multiply") {
            Some(BlendMode::Multiply)
        } else {
            None
        }
    }
    let Some(value) = resolver.resolve_dict_key(dict, b"BM")? else { return Ok(None) };
    if let Some(mode) = supported(&value) { return Ok(Some(mode)); }
    if let Some(array) = value.get().as_array() {
        check(
            LimitKind::Tokens, array.len() as u64,
            u64::from(resolver.document().limits().max_tokens_per_object).min(64),
        )?;
        for value in array.iter() {
            let value = resolver.resolve(value)?;
            if let Some(mode) = supported(&value) { return Ok(Some(mode)); }
        }
    }
    warn(resolver, value.object().id(), "render_blend_mode_unsupported");
    Ok(Some(BlendMode::Normal))
}

pub(crate) fn number(
    resolver: &mut Resolver<'_>,
    dict: DictRef<'_>,
    key: &[u8],
) -> Result<Option<f32>, DocError> {
    let Some(value) = resolver.resolve_dict_key(dict, key)? else { return Ok(None) };
    value.get().as_f64().and_then(finite).map(Some).ok_or_else(|| bad("render_number"))
}

pub(crate) fn resource(
    resolver: &mut Resolver<'_>,
    resources: Option<&Value>,
    category: &[u8],
    name: &[u8],
) -> Result<Option<Value>, DocError> {
    let Some(dict) = resources.and_then(|value| value.get().as_dict()) else { return Ok(None) };
    let Some(category) = resolver.resolve_dict_key(dict, category)? else { return Ok(None) };
    let Some(dict) = category.get().as_dict() else { return Ok(None) };
    resolver.resolve_dict_key(dict, name)
}

pub(crate) fn finite(value: f64) -> Option<f32> {
    if value.is_finite() && value.abs() <= f64::from(f32::MAX) { Some(value as f32) }
    else { None }
}

fn norm(x: f64, y: f64) -> f64 {
    let large = x.abs().max(y.abs());
    let small = x.abs().min(y.abs());
    if !large.is_finite() || !small.is_finite() { return f64::NAN; }
    if large == 0.0 { return 0.0; }
    let ratio = small / large;
    let square = 1.0 + ratio * ratio;
    let mut root = 1.5;
    for _ in 0..8 { root = 0.5 * (root + square / root); }
    large * root
}

pub(crate) fn check(kind: LimitKind, requested: u64, maximum: u64) -> Result<(), DocError> {
    if requested > maximum { Err(refusal(kind, requested, maximum)) } else { Ok(()) }
}

pub(crate) fn refusal(limit: LimitKind, requested: u64, maximum: u64) -> DocError {
    DocError::Refused(Refusal { limit, requested, maximum })
}

pub(crate) fn bad(what: &'static str) -> DocError {
    DocError::Malformed { offset: 0, what }
}

pub(crate) fn warn(resolver: &mut Resolver<'_>, object: Option<ObjectRef>, what: &'static str) {
    resolver.warn(Warning { offset: None, object, what });
}

fn recover(
    resolver: &mut Resolver<'_>,
    error: DocError,
    object: Option<ObjectRef>,
    what: &'static str,
) -> Result<(), DocError> {
    match error {
        error @ (DocError::Encrypted | DocError::Refused(_)) => Err(error),
        _ => { warn(resolver, object, what); Ok(()) }
    }
}

