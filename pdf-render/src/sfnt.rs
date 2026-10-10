use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::Range;

use pdf_core::{DocError, LimitKind, Refusal};
use skrifa::instance::{LocationRef, Size};
use skrifa::outline::{
    DrawSettings, Hinting, OutlineGlyphCollection, OutlineGlyphFormat, OutlinePen,
};
use skrifa::raw::ps::cff::CffFontRef;
use skrifa::raw::tables::cmap::{CmapSubtable, PlatformId};
use skrifa::raw::{FontData, FontRead, TableProvider};
use skrifa::{FontRef, GlyphId, Tag};

use crate::display::{Path, PathEl};

const MAX_FONT_TABLES: u64 = 256;
const SFNT_HEADER_BYTES: usize = 124;

#[derive(Clone)]
pub(crate) struct Bytes {
    data: Arc<[u8]>,
    range: Range<usize>,
}

impl Bytes {
    pub(crate) fn shared(data: Arc<[u8]>) -> Self {
        let length = data.len();
        Self { data, range: 0..length }
    }

    pub(crate) fn owned(data: Vec<u8>) -> Self {
        Self::shared(Arc::<[u8]>::from(data))
    }

    pub(crate) fn range(data: Arc<[u8]>, range: Range<usize>) -> Option<Self> {
        data.get(range.clone())?;
        Some(Self { data, range })
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        self.data.get(self.range.clone()).unwrap_or(&[])
    }

    pub(crate) fn len(&self) -> usize {
        self.range.end.saturating_sub(self.range.start)
    }
}

pub(crate) struct Program {
    bytes: Bytes,
    format: OutlineGlyphFormat,
    glyph_count: u16,
    maximum_bytes: u64,
    maximum_elements: u64,
}

impl Program {
    pub(crate) fn new(
        bytes: Bytes,
        maximum_bytes: u64,
        maximum_elements: u64,
    ) -> Result<Option<Self>, DocError> {
        check(LimitKind::DecodedBytes, bytes.len() as u64, maximum_bytes)?;
        let Ok(font) = FontRef::new(bytes.bytes()) else { return Ok(None) };
        check(
            LimitKind::Tokens,
            font.table_directory().table_records().len() as u64,
            MAX_FONT_TABLES,
        )?;
        let Some((format, glyph_count)) = (|| {
            let upem = font.head().ok()?.units_per_em();
            let glyph_count = font.maxp().ok()?.num_glyphs();
            if upem == 0 || glyph_count == 0 {
                return None;
            }
            let format = if font.table_data(Tag::new(b"glyf")).is_some() {
                OutlineGlyphFormat::Glyf
            } else if font.table_data(Tag::new(b"CFF2")).is_some() {
                OutlineGlyphFormat::Cff2
            } else {
                OutlineGlyphFormat::Cff
            };
            OutlineGlyphCollection::with_format(&font, format)?;
            if matches!(format, OutlineGlyphFormat::Cff | OutlineGlyphFormat::Cff2) {
                let tag = if format == OutlineGlyphFormat::Cff {
                    Tag::new(b"CFF ")
                } else {
                    Tag::new(b"CFF2")
                };
                let table = font.table_data(tag)?;
                let cff = CffFontRef::new(table.as_bytes(), 0, Some(i32::from(upem))).ok()?;
                if cff.num_glyphs() != u32::from(glyph_count) {
                    return None;
                }
            }
            Some((format, glyph_count))
        })() else { return Ok(None) };
        Ok(Some(Self {
            bytes,
            format,
            glyph_count,
            maximum_bytes,
            maximum_elements,
        }))
    }

    pub(crate) fn font(&self) -> Option<FontRef<'_>> {
        FontRef::new(self.bytes.bytes()).ok()
    }

    pub(crate) fn glyph_count(&self) -> u16 {
        self.glyph_count
    }

    pub(crate) fn outline_checked(&self, gid: u16) -> Result<Option<Path>, DocError> {
        if gid == 0 || gid >= self.glyph_count {
            return Ok(None);
        }
        let font = self.font().ok_or_else(failed)?;
        // Select a conventional outline source explicitly. VARC is not
        // traversed when a font also supplies ordinary glyf outlines.
        let collection = OutlineGlyphCollection::with_format(&font, self.format)
            .ok_or_else(failed)?;
        let glyph = collection.get(GlyphId::new(u32::from(gid)))
            .ok_or_else(failed)?;
        let memory_size = glyph.draw_memory_size(Hinting::None);
        check(
            LimitKind::DecodedBytes,
            memory_size as u64,
            self.maximum_bytes,
        )?;
        let mut memory = zeroed(memory_size, self.maximum_bytes)?;
        let maximum_path_bytes = self.maximum_bytes.saturating_sub(memory_size as u64);
        let mut counter = PathPen::new(
            self.maximum_elements,
            maximum_path_bytes,
            None,
        );
        glyph.draw(
            DrawSettings::unhinted(Size::new(1000.0), LocationRef::default())
                .with_memory(Some(memory.as_mut_slice())),
            &mut counter,
        ).map_err(|_| failed())?;
        if let Some(error) = counter.error.take() {
            return Err(error);
        }

        let count = counter.count;
        let mut elements = Vec::new();
        elements.try_reserve_exact(count)
            .map_err(|_| allocation_failure(maximum_path_bytes))?;
        let mut pen = PathPen::new(
            self.maximum_elements,
            maximum_path_bytes,
            Some(elements),
        );
        glyph.draw(
            DrawSettings::unhinted(Size::new(1000.0), LocationRef::default())
                .with_memory(Some(memory.as_mut_slice())),
            &mut pen,
        ).map_err(|_| failed())?;
        if let Some(error) = pen.error.take() {
            return Err(error);
        }
        if pen.count != count {
            return Err(failed());
        }
        Ok(pen.elements.take().map(|els| Path { els }))
    }
}

pub(crate) fn cmap_index(
    font: &FontRef<'_>,
    platform: PlatformId,
    encoding: u16,
) -> Option<u16> {
    let cmap = font.cmap().ok()?;
    for (index, record) in cmap.encoding_records().iter().enumerate() {
        if record.platform_id() != platform || record.encoding_id() != encoding {
            continue;
        }
        let Ok(subtable) = record.subtable(cmap.offset_data()) else { continue };
        if matches!(
            subtable,
            CmapSubtable::Format0(_)
                | CmapSubtable::Format4(_)
                | CmapSubtable::Format6(_)
                | CmapSubtable::Format10(_)
                | CmapSubtable::Format12(_)
                | CmapSubtable::Format13(_)
        ) {
            return u16::try_from(index).ok();
        }
    }
    None
}

pub(crate) fn map(
    font: &FontRef<'_>,
    index: Option<u16>,
    codepoint: u32,
) -> Option<u16> {
    let cmap = font.cmap().ok()?;
    let subtable = cmap.subtable(index?).ok()?;
    let gid = subtable.map_codepoint(codepoint)?;
    if gid == GlyphId::NOTDEF {
        return None;
    }
    u16::try_from(gid.to_u32()).ok()
}

/// Wraps a single CFF1 font set in the tables required by skrifa.
/// Source CFF bytes are copied only because the SFNT container must be contiguous.
pub(crate) fn wrap_cff(
    source: &Bytes,
    maximum_bytes: u64,
) -> Result<Option<Bytes>, DocError> {
    check(LimitKind::DecodedBytes, source.len() as u64, maximum_bytes)?;
    let data = source.bytes();
    let Ok(table) = skrifa::raw::tables::cff::Cff::read(FontData::new(data)) else {
        return Ok(None);
    };
    if table.names().count() != 1 || table.top_dicts().count() != 1 {
        return Ok(None);
    }
    let Ok(cff) = CffFontRef::new_cff(data, 0, None) else { return Ok(None) };
    let Some(glyph_count) = u16::try_from(cff.num_glyphs()).ok()
        .filter(|count| *count != 0)
    else { return Ok(None) };
    let Some(upem) = u16::try_from(cff.upem()).ok().filter(|upem| *upem != 0) else {
        return Ok(None);
    };
    let metrics_size = usize::from(glyph_count).checked_mul(4)
        .ok_or_else(|| overflow(maximum_bytes))?;
    let tables = [
        (b"CFF ", source.len()),
        (b"cmap", 4usize),
        (b"head", 54usize),
        (b"hhea", 36usize),
        (b"hmtx", metrics_size),
        (b"maxp", 6usize),
        (b"post", 32usize),
    ];
    let mut offsets = [0usize; 7];
    let mut total = SFNT_HEADER_BYTES;
    for ((_, length), offset) in tables.iter().zip(offsets.iter_mut()) {
        *offset = total;
        total = total.checked_add(*length)
            .and_then(|size| size.checked_add(3))
            .map(|size| size & !3usize)
            .ok_or_else(|| overflow(maximum_bytes))?;
    }
    check(LimitKind::DecodedBytes, total as u64, maximum_bytes)?;
    check(LimitKind::DecodedBytes, total as u64, u64::from(u32::MAX))?;
    let mut output = zeroed(total, maximum_bytes)?;
    let built = (|| -> Option<()> {
        put(&mut output, 0, b"OTTO")?;
        put16(&mut output, 4, 7)?;
        put16(&mut output, 6, 64)?;
        put16(&mut output, 8, 2)?;
        put16(&mut output, 10, 48)?;
        for (index, ((tag, length), offset)) in tables.iter().zip(offsets.iter()).enumerate() {
            let record = index.checked_mul(16)?.checked_add(12)?;
            put(&mut output, record, tag.as_slice())?;
            put32(&mut output, record.checked_add(8)?, u32::try_from(*offset).ok()?)?;
            put32(&mut output, record.checked_add(12)?, u32::try_from(*length).ok()?)?;
        }
        let [cff, _, head, hhea, hmtx, maxp, post] = offsets;
        put(&mut output, cff, data)?;
        put32(&mut output, head, 0x0001_0000)?;
        put32(&mut output, head.checked_add(12)?, 0x5f0f_3cf5)?;
        put16(&mut output, head.checked_add(18)?, upem)?;
        put32(&mut output, hhea, 0x0001_0000)?;
        put16(&mut output, hhea.checked_add(10)?, upem)?;
        put16(&mut output, hhea.checked_add(34)?, glyph_count)?;
        let metrics_end = hmtx.checked_add(metrics_size)?;
        for metric in output.get_mut(hmtx..metrics_end)?.chunks_exact_mut(4) {
            metric.get_mut(..2)?.copy_from_slice(&upem.to_be_bytes());
        }
        put32(&mut output, maxp, 0x0000_5000)?;
        put16(&mut output, maxp.checked_add(4)?, glyph_count)?;
        put32(&mut output, post, 0x0003_0000)?;
        // cmap is version 0 with no encoding records; post is version 3.
        // Checksums, checkSumAdjustment and unused metrics remain zero.
        Some(())
    })();
    if built.is_none() {
        return Ok(None);
    }
    Ok(Some(Bytes::owned(output)))
}

fn put(output: &mut [u8], offset: usize, bytes: &[u8]) -> Option<()> {
    let end = offset.checked_add(bytes.len())?;
    output.get_mut(offset..end)?.copy_from_slice(bytes);
    Some(())
}

fn put16(output: &mut [u8], offset: usize, value: u16) -> Option<()> {
    put(output, offset, &value.to_be_bytes())
}

fn put32(output: &mut [u8], offset: usize, value: u32) -> Option<()> {
    put(output, offset, &value.to_be_bytes())
}

struct PathPen {
    elements: Option<Vec<PathEl>>,
    count: usize,
    current: Option<(f32, f32)>,
    start: Option<(f32, f32)>,
    maximum_elements: u64,
    maximum_bytes: u64,
    error: Option<DocError>,
}

impl PathPen {
    fn new(
        maximum_elements: u64,
        maximum_bytes: u64,
        elements: Option<Vec<PathEl>>,
    ) -> Self {
        Self {
            elements,
            count: 0,
            current: None,
            start: None,
            maximum_elements,
            maximum_bytes,
            error: None,
        }
    }

    fn emit(&mut self, element: PathEl, coordinates: &[f32]) {
        if self.error.is_some() {
            return;
        }
        if !coordinates.iter().all(|coordinate| coordinate.is_finite()) {
            self.error = Some(failed());
            return;
        }
        let Some(count) = self.count.checked_add(1) else {
            self.error = Some(overflow(self.maximum_bytes));
            return;
        };
        if let Err(error) = check(
            LimitKind::ContentOps,
            count as u64,
            self.maximum_elements,
        ) {
            self.error = Some(error);
            return;
        }
        let size = count.checked_mul(core::mem::size_of::<PathEl>());
        let Some(size) = size else {
            self.error = Some(overflow(self.maximum_bytes));
            return;
        };
        if let Err(error) = check(
            LimitKind::DecodedBytes,
            size as u64,
            self.maximum_bytes,
        ) {
            self.error = Some(error);
            return;
        }
        if let Some(elements) = self.elements.as_mut() {
            // Capacity was determined by the first draw. A changed command
            // stream fails instead of allocating while emitting elements.
            if elements.len() >= elements.capacity() {
                self.error = Some(failed());
                return;
            }
            elements.push(element);
        }
        self.count = count;
    }

    fn require_current(&mut self) -> Option<(f32, f32)> {
        match self.current {
            Some(point) => Some(point),
            None => {
                if self.error.is_none() {
                    self.error = Some(failed());
                }
                None
            }
        }
    }
}

impl OutlinePen for PathPen {
    fn move_to(&mut self, x: f32, y: f32) {
        self.emit(PathEl::MoveTo(x, y), &[x, y]);
        self.current = Some((x, y));
        self.start = Some((x, y));
    }

    fn line_to(&mut self, x: f32, y: f32) {
        if self.require_current().is_none() {
            return;
        }
        self.emit(PathEl::LineTo(x, y), &[x, y]);
        self.current = Some((x, y));
    }

    fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        let Some((from_x, from_y)) = self.require_current() else { return };
        // Weighted sums avoid overflowing a control-point difference.
        let x0 = from_x / 3.0 + cx * (2.0 / 3.0);
        let y0 = from_y / 3.0 + cy * (2.0 / 3.0);
        let x1 = x / 3.0 + cx * (2.0 / 3.0);
        let y1 = y / 3.0 + cy * (2.0 / 3.0);
        self.emit(
            PathEl::CubicTo(x0, y0, x1, y1, x, y),
            &[cx, cy, x0, y0, x1, y1, x, y],
        );
        self.current = Some((x, y));
    }

    fn curve_to(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, x: f32, y: f32) {
        if self.require_current().is_none() {
            return;
        }
        self.emit(
            PathEl::CubicTo(x0, y0, x1, y1, x, y),
            &[x0, y0, x1, y1, x, y],
        );
        self.current = Some((x, y));
    }

    fn close(&mut self) {
        if let Some(start) = self.start.take() {
            self.emit(PathEl::Close, &[]);
            self.current = Some(start);
        }
    }
}

pub(crate) fn validate_path(
    path: &Path,
    maximum_elements: u64,
    maximum_bytes: u64,
) -> Result<(), DocError> {
    check(
        LimitKind::ContentOps,
        path.els.len() as u64,
        maximum_elements,
    )?;
    let bytes = path.els.len().checked_mul(core::mem::size_of::<PathEl>())
        .ok_or_else(|| overflow(maximum_bytes))?;
    check(LimitKind::DecodedBytes, bytes as u64, maximum_bytes)?;
    for element in &path.els {
        let finite = match *element {
            PathEl::MoveTo(x, y) | PathEl::LineTo(x, y) => x.is_finite() && y.is_finite(),
            PathEl::CubicTo(x0, y0, x1, y1, x, y) =>
                [x0, y0, x1, y1, x, y].iter().all(|value| value.is_finite()),
            PathEl::Close => true,
        };
        if !finite {
            return Err(failed());
        }
    }
    Ok(())
}

fn zeroed(size: usize, maximum: u64) -> Result<Vec<u8>, DocError> {
    check(
        LimitKind::DecodedBytes,
        size as u64,
        maximum.min(isize::MAX as u64),
    )?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(size).map_err(|_| allocation_failure(maximum))?;
    bytes.resize(size, 0);
    Ok(bytes)
}

pub(crate) fn check(
    limit: LimitKind,
    requested: u64,
    maximum: u64,
) -> Result<(), DocError> {
    if requested > maximum {
        Err(DocError::Refused(Refusal { limit, requested, maximum }))
    } else {
        Ok(())
    }
}

fn overflow(maximum: u64) -> DocError {
    DocError::Refused(Refusal {
        limit: LimitKind::DecodedBytes,
        requested: u64::MAX,
        maximum,
    })
}

fn allocation_failure(maximum: u64) -> DocError {
    DocError::Refused(Refusal {
        limit: LimitKind::DecodedBytes,
        requested: maximum.saturating_add(1),
        maximum,
    })
}

fn failed() -> DocError {
    DocError::Malformed { offset: 0, what: "font_program_failed" }
}
