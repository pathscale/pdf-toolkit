use alloc::sync::Arc;
use alloc::vec::Vec;
use core::mem::size_of;

use crate::limits::{LimitKind, Refusal};

use super::{FilterError, FilterKind};

const MAX_SEGMENTS: u64 = 65_536;
const MAX_SYMBOLS: u64 = 1_000_000;
const MAX_CODEC_BYTES: u64 = 512 << 20;
const MAX_CODEC_WORK: u64 = 2_000_000_000;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    /// MSB first, byte-padded rows, with 1 representing black.
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Report {
    pub bitmap: Bitmap,
    pub truncated: bool,
    pub skipped_refinement: bool,
    pub skipped_unsupported: bool,
}

/// Decodes sequential, embedded JBIG2 segments without a file header.
///
/// Globals and the image stream share segment numbers and dictionaries.
/// Use `decode_jbig2_report` when skipped features must be reported.
pub fn decode_jbig2(
    data: &[u8],
    globals: Option<&[u8]>,
    max_pixels: u64,
) -> Result<Bitmap, FilterError> {
    decode_jbig2_report(data, globals, max_pixels).map(|report| report.bitmap)
}

pub fn decode_jbig2_report(
    data: &[u8],
    globals: Option<&[u8]>,
    max_pixels: u64,
) -> Result<Report, FilterError> {
    let mut decoder = Decoder {
        budget: Budget::new(max_pixels),
        entries: Vec::new(),
        segments: 0,
        symbols: 0,
        page: None,
        truncated: false,
        skipped_refinement: false,
        skipped_unsupported: false,
    };
    if let Some(globals) = globals {
        decoder.consume(globals, true)?;
    }
    decoder.consume(data, false)?;
    let mut page = decoder.page.take().ok_or_else(|| corrupt(0))?;
    if page.bitmap.width == 0 || page.bitmap.height == 0 {
        return Err(corrupt(0));
    }
    page.bitmap.clear_padding()?;
    Ok(Report {
        bitmap: page.bitmap,
        truncated: decoder.truncated || !page.ended,
        skipped_refinement: decoder.skipped_refinement,
        skipped_unsupported: decoder.skipped_unsupported,
    })
}

fn corrupt(offset: usize) -> FilterError {
    FilterError::Corrupt {
        filter: FilterKind::Jbig2,
        offset: offset as u64,
    }
}

fn unsupported() -> FilterError {
    FilterError::Unsupported {
        filter: FilterKind::Jbig2,
    }
}

fn refusal(limit: LimitKind, requested: u64, maximum: u64) -> FilterError {
    FilterError::Refused(Refusal {
        limit,
        requested,
        maximum,
    })
}

fn check(limit: LimitKind, requested: u64, maximum: u64) -> Result<(), FilterError> {
    if requested > maximum {
        Err(refusal(limit, requested, maximum))
    } else {
        Ok(())
    }
}

struct Budget {
    max_pixels: u64,
    bytes: u64,
    max_bytes: u64,
    work: u64,
    max_work: u64,
}

impl Budget {
    fn new(max_pixels: u64) -> Self {
        Self {
            max_pixels,
            bytes: 0,
            max_bytes: max_pixels
                .saturating_mul(8)
                .saturating_add(4 << 20)
                .min(MAX_CODEC_BYTES)
                .min(isize::MAX as u64),
            work: 0,
            max_work: max_pixels
                .saturating_mul(32)
                .saturating_add(4_000_000)
                .min(MAX_CODEC_WORK),
        }
    }

    fn pixels(&self, width: u32, height: u32) -> Result<(), FilterError> {
        let pixels = u64::from(width)
            .checked_mul(u64::from(height))
            .ok_or_else(|| refusal(LimitKind::ImagePixels, u64::MAX, self.max_pixels))?;
        check(LimitKind::ImagePixels, pixels, self.max_pixels)
    }

    fn charge(&mut self, bytes: u64) -> Result<(), FilterError> {
        let requested = self
            .bytes
            .checked_add(bytes)
            .ok_or_else(|| refusal(LimitKind::CodecBytes, u64::MAX, self.max_bytes))?;
        check(LimitKind::CodecBytes, requested, self.max_bytes)?;
        self.bytes = requested;
        Ok(())
    }

    fn step(&mut self, count: u64) -> Result<(), FilterError> {
        let requested = self
            .work
            .checked_add(count)
            .ok_or_else(|| refusal(LimitKind::CodecWork, u64::MAX, self.max_work))?;
        check(LimitKind::CodecWork, requested, self.max_work)?;
        self.work = requested;
        Ok(())
    }

    fn allocation_error(&self) -> FilterError {
        refusal(
            LimitKind::CodecBytes,
            self.max_bytes.saturating_add(1),
            self.max_bytes,
        )
    }

    fn vector<T>(&mut self, count: usize) -> Result<Vec<T>, FilterError> {
        let bytes = count
            .checked_mul(size_of::<T>())
            .ok_or_else(|| self.allocation_error())?;
        self.charge(bytes as u64)?;
        let mut result = Vec::new();
        result
            .try_reserve_exact(count)
            .map_err(|_| self.allocation_error())?;
        Ok(result)
    }

    fn reserve_one<T>(&mut self, vector: &mut Vec<T>) -> Result<(), FilterError> {
        if vector.len() < vector.capacity() {
            return Ok(());
        }
        let needed = vector
            .len()
            .checked_add(1)
            .ok_or_else(|| self.allocation_error())?;
        let capacity = vector.capacity().saturating_mul(2).max(8).max(needed);
        let extra = capacity
            .checked_sub(vector.capacity())
            .and_then(|count| count.checked_mul(size_of::<T>()))
            .ok_or_else(|| self.allocation_error())?;
        self.charge(extra as u64)?;
        vector
            .try_reserve_exact(capacity.saturating_sub(vector.len()))
            .map_err(|_| self.allocation_error())
    }

    fn zeros(&mut self, size: usize) -> Result<Vec<u8>, FilterError> {
        self.step(size as u64)?;
        let mut bytes = self.vector(size)?;
        bytes.resize(size, 0);
        Ok(bytes)
    }

    fn bitmap(&mut self, width: u32, height: u32, black: bool) -> Result<Bitmap, FilterError> {
        self.pixels(width, height)?;
        let size = bitmap_size(width, height)?;
        let mut data = self.zeros(size)?;
        if black {
            data.fill(255);
        }
        let mut bitmap = Bitmap {
            width,
            height,
            data,
        };
        bitmap.clear_padding()?;
        Ok(bitmap)
    }

    fn shared(&mut self, bitmap: Bitmap) -> Result<Arc<Bitmap>, FilterError> {
        self.charge((size_of::<Bitmap>() as u64).saturating_add((2 * size_of::<usize>()) as u64))?;
        Ok(Arc::new(bitmap))
    }
}

fn stride(width: u32) -> Result<usize, FilterError> {
    usize::try_from((u64::from(width) + 7) / 8).map_err(|_| corrupt(0))
}

fn bitmap_size(width: u32, height: u32) -> Result<usize, FilterError> {
    stride(width)?
        .checked_mul(usize::try_from(height).map_err(|_| corrupt(0))?)
        .filter(|&size| size <= isize::MAX as usize)
        .ok_or_else(|| corrupt(0))
}

impl Bitmap {
    fn pixel(&self, x: i64, y: i64) -> u8 {
        let (Ok(x), Ok(y)) = (u32::try_from(x), u32::try_from(y)) else {
            return 0;
        };
        if x >= self.width || y >= self.height {
            return 0;
        }
        let offset = stride(self.width)
            .ok()
            .and_then(|row| usize::try_from(y).ok()?.checked_mul(row))
            .and_then(|row| row.checked_add(usize::try_from(x / 8).ok()?));
        offset
            .and_then(|offset| self.data.get(offset))
            .copied()
            .map(|byte| (byte >> (7 - (x & 7))) & 1)
            .unwrap_or(0)
    }

    fn put(&mut self, x: u32, y: u32, value: u8) -> Result<(), FilterError> {
        if x >= self.width || y >= self.height {
            return Err(corrupt(0));
        }
        let offset = usize::try_from(y)
            .ok()
            .and_then(|y| y.checked_mul(stride(self.width).ok()?))
            .and_then(|row| row.checked_add(usize::try_from(x / 8).ok()?))
            .ok_or_else(|| corrupt(0))?;
        let slot = self.data.get_mut(offset).ok_or_else(|| corrupt(0))?;
        let mask = 1u8 << (7 - (x & 7));
        *slot = (*slot & !mask) | ((value & 1) << (7 - (x & 7)));
        Ok(())
    }

    fn clear_padding(&mut self) -> Result<(), FilterError> {
        let remainder = self.width & 7;
        if remainder == 0 || self.height == 0 {
            return Ok(());
        }
        let row_bytes = stride(self.width)?;
        if row_bytes == 0 {
            return Err(corrupt(0));
        }
        let mask = 255u8 << (8 - remainder);
        for row in self.data.chunks_exact_mut(row_bytes) {
            if let Some(last) = row.last_mut() {
                *last &= mask;
            }
        }
        Ok(())
    }

    fn copy_previous(&mut self, y: u32, budget: &mut Budget) -> Result<(), FilterError> {
        if y == 0 {
            return Ok(());
        }
        let row_bytes = stride(self.width)?;
        budget.step(row_bytes as u64)?;
        let destination = usize::try_from(y)
            .ok()
            .and_then(|y| y.checked_mul(row_bytes))
            .ok_or_else(|| corrupt(0))?;
        let source = destination
            .checked_sub(row_bytes)
            .ok_or_else(|| corrupt(0))?;
        let end = destination
            .checked_add(row_bytes)
            .ok_or_else(|| corrupt(0))?;
        if end > self.data.len() {
            return Err(corrupt(0));
        }
        self.data.copy_within(source..destination, destination);
        Ok(())
    }
}

fn compose(
    destination: &mut Bitmap,
    source: &Bitmap,
    dx: i64,
    dy: i64,
    operator: u8,
    budget: &mut Budget,
) -> Result<(), FilterError> {
    if operator > 4 {
        return Err(corrupt(0));
    }
    let right = dx
        .checked_add(i64::from(source.width))
        .ok_or_else(|| corrupt(0))?
        .min(i64::from(destination.width));
    let bottom = dy
        .checked_add(i64::from(source.height))
        .ok_or_else(|| corrupt(0))?
        .min(i64::from(destination.height));
    let left = dx.max(0);
    let top = dy.max(0);
    if right <= left || bottom <= top {
        return Ok(());
    }
    let count = u64::try_from(right - left)
        .ok()
        .and_then(|width| width.checked_mul(u64::try_from(bottom - top).ok()?))
        .ok_or_else(|| corrupt(0))?;
    budget.step(count)?;
    for y in top..bottom {
        for x in left..right {
            let old = destination.pixel(x, y);
            let new = source.pixel(
                x.checked_sub(dx).ok_or_else(|| corrupt(0))?,
                y.checked_sub(dy).ok_or_else(|| corrupt(0))?,
            );
            let value = match operator {
                0 => old | new,
                1 => old & new,
                2 => old ^ new,
                3 => (old ^ new) ^ 1,
                4 => new,
                _ => return Err(corrupt(0)),
            };
            destination.put(
                u32::try_from(x).map_err(|_| corrupt(0))?,
                u32::try_from(y).map_err(|_| corrupt(0))?,
                value,
            )?;
        }
    }
    Ok(())
}

struct Cursor<'a> {
    bytes: &'a [u8],
    position: usize,
    base: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8], base: usize) -> Self {
        Self {
            bytes,
            position: 0,
            base,
        }
    }

    fn offset(&self) -> usize {
        self.base.saturating_add(self.position)
    }

    fn remaining(&self) -> &'a [u8] {
        self.bytes.get(self.position..).unwrap_or(&[])
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], FilterError> {
        let end = self
            .position
            .checked_add(count)
            .ok_or_else(|| corrupt(self.offset()))?;
        let bytes = self
            .bytes
            .get(self.position..end)
            .ok_or_else(|| corrupt(self.offset()))?;
        self.position = end;
        Ok(bytes)
    }

    fn byte(&mut self) -> Result<u8, FilterError> {
        self.take(1)?
            .first()
            .copied()
            .ok_or_else(|| corrupt(self.offset()))
    }

    fn word(&mut self) -> Result<u16, FilterError> {
        match self.take(2)? {
            [a, b] => Ok(u16::from_be_bytes([*a, *b])),
            _ => Err(corrupt(self.offset())),
        }
    }

    fn number(&mut self) -> Result<u32, FilterError> {
        match self.take(4)? {
            [a, b, c, d] => Ok(u32::from_be_bytes([*a, *b, *c, *d])),
            _ => Err(corrupt(self.offset())),
        }
    }
}

struct Header {
    number: u32,
    kind: u8,
    page: u32,
    references: Vec<u32>,
    length: u32,
}

impl Header {
    fn read(cursor: &mut Cursor<'_>, budget: &mut Budget) -> Result<Self, FilterError> {
        let number = cursor.number()?;
        let flags = cursor.byte()?;
        let first = cursor.byte()?;
        let count = match first >> 5 {
            0..=4 => u32::from(first >> 5),
            7 => {
                let a = cursor.byte()?;
                let b = cursor.byte()?;
                let c = cursor.byte()?;
                let count = u32::from_be_bytes([first, a, b, c]) & 0x1fff_ffff;
                check(LimitKind::Jbig2Segments, u64::from(count), MAX_SEGMENTS)?;
                let retention = (u64::from(count) + 8) / 8;
                cursor.take(usize::try_from(retention).map_err(|_| corrupt(cursor.offset()))?)?;
                count
            }
            _ => return Err(corrupt(cursor.offset())),
        };
        check(LimitKind::Jbig2Segments, u64::from(count), MAX_SEGMENTS)?;
        budget.step(u64::from(count))?;
        let mut references =
            budget.vector(usize::try_from(count).map_err(|_| corrupt(cursor.offset()))?)?;
        for _ in 0..count {
            let reference = if number <= 256 {
                u32::from(cursor.byte()?)
            } else if number <= 65_536 {
                u32::from(cursor.word()?)
            } else {
                cursor.number()?
            };
            if reference >= number {
                return Err(corrupt(cursor.offset()));
            }
            references.push(reference);
        }
        let page = if flags & 0x40 != 0 {
            cursor.number()?
        } else {
            u32::from(cursor.byte()?)
        };
        let length = cursor.number()?;
        Ok(Self {
            number,
            kind: flags & 63,
            page,
            references,
            length,
        })
    }

    fn payload<'a>(
        &self,
        cursor: &mut Cursor<'a>,
        budget: &mut Budget,
    ) -> Result<(&'a [u8], Option<u32>), FilterError> {
        if self.length != u32::MAX {
            return Ok((
                cursor.take(usize::try_from(self.length).map_err(|_| corrupt(cursor.offset()))?)?,
                None,
            ));
        }
        if self.kind != 38 {
            return Err(corrupt(cursor.offset()));
        }
        let remaining = cursor.remaining();
        let flags = remaining
            .get(17)
            .copied()
            .ok_or_else(|| corrupt(cursor.offset()))?;
        let mmr = flags & 1 != 0;
        let template = (flags >> 1) & 3;
        let start = if mmr {
            18usize
        } else if template == 0 {
            26
        } else {
            20
        };
        let marker = if mmr { [0, 0] } else { [255, 172] };
        let candidates = remaining.len().saturating_sub(5);
        for offset in start..candidates {
            budget.step(1)?;
            let marker_end = offset
                .checked_add(2)
                .ok_or_else(|| corrupt(cursor.offset()))?;
            if remaining.get(offset..marker_end) != Some(marker.as_slice()) {
                continue;
            }
            let end = marker_end
                .checked_add(4)
                .ok_or_else(|| corrupt(cursor.offset()))?;
            let rows = match remaining.get(marker_end..end) {
                Some([a, b, c, d]) => u32::from_be_bytes([*a, *b, *c, *d]),
                _ => return Err(corrupt(cursor.offset())),
            };
            let whole = cursor.take(end)?;
            let payload = whole
                .get(..marker_end)
                .ok_or_else(|| corrupt(cursor.offset()))?;
            return Ok((payload, Some(rows)));
        }
        Err(corrupt(cursor.offset()))
    }
}

struct Region {
    width: u32,
    height: u32,
    x: i64,
    y: i64,
    operator: u8,
}

impl Region {
    fn read(cursor: &mut Cursor<'_>) -> Result<Self, FilterError> {
        let width = cursor.number()?;
        let height = cursor.number()?;
        let x = i64::from(cursor.number()?);
        let y = i64::from(cursor.number()?);
        let flags = cursor.byte()?;
        if flags & 0xf8 != 0 || flags & 7 > 4 {
            return Err(corrupt(cursor.offset()));
        }
        Ok(Self {
            width,
            height,
            x,
            y,
            operator: flags & 7,
        })
    }
}

struct Dictionary {
    symbols: Vec<Arc<Bitmap>>,
    contexts: Option<Vec<u8>>,
    template: u8,
}

struct Entry {
    number: u32,
    page: u32,
    is_dictionary: bool,
    dictionary: Option<Dictionary>,
}

struct Page {
    number: u32,
    bitmap: Bitmap,
    unknown_height: bool,
    striped: bool,
    next_stripe: u32,
    default_black: bool,
    default_operator: u8,
    allow_override: bool,
    ended: bool,
}

impl Page {
    fn grow(&mut self, height: u32, budget: &mut Budget) -> Result<(), FilterError> {
        if height <= self.bitmap.height {
            return Ok(());
        }
        if !self.unknown_height {
            return Err(corrupt(0));
        }
        budget.pixels(self.bitmap.width, height)?;
        let size = bitmap_size(self.bitmap.width, height)?;
        let additional = size
            .checked_sub(self.bitmap.data.len())
            .ok_or_else(|| corrupt(0))?;
        budget.charge(additional as u64)?;
        budget.step(additional as u64)?;
        self.bitmap
            .data
            .try_reserve_exact(additional)
            .map_err(|_| budget.allocation_error())?;
        self.bitmap
            .data
            .resize(size, if self.default_black { 255 } else { 0 });
        self.bitmap.height = height;
        self.bitmap.clear_padding()
    }
}

struct Decoder {
    budget: Budget,
    entries: Vec<Entry>,
    segments: u64,
    symbols: u64,
    page: Option<Page>,
    truncated: bool,
    skipped_refinement: bool,
    skipped_unsupported: bool,
}

impl Decoder {
    fn entry(&self, number: u32) -> Option<&Entry> {
        self.entries
            .binary_search_by_key(&number, |entry| entry.number)
            .ok()
            .and_then(|index| self.entries.get(index))
    }

    fn consume(&mut self, data: &[u8], globals: bool) -> Result<(), FilterError> {
        let mut cursor = Cursor::new(data, 0);
        while !cursor.remaining().is_empty() {
            self.segments = self
                .segments
                .checked_add(1)
                .ok_or_else(|| refusal(LimitKind::Jbig2Segments, u64::MAX, MAX_SEGMENTS))?;
            check(LimitKind::Jbig2Segments, self.segments, MAX_SEGMENTS)?;
            self.budget.step(1)?;
            let header = Header::read(&mut cursor, &mut self.budget)?;
            if globals && header.page != 0 {
                return Err(corrupt(cursor.offset()));
            }
            let insertion = match self
                .entries
                .binary_search_by_key(&header.number, |entry| entry.number)
            {
                Ok(_) => return Err(corrupt(cursor.offset())),
                Err(index) => index,
            };
            let base = cursor.offset();
            let (payload, rows) = header.payload(&mut cursor, &mut self.budget)?;
            let result = self.segment(&header, payload, rows, base);
            let dictionary = match result {
                Ok(dictionary) => dictionary,
                Err(error @ (FilterError::Refused(_) | FilterError::OutputLimit { .. })) => {
                    return Err(error);
                }
                Err(error) if matches!(header.kind, 48..=51) => return Err(error),
                Err(FilterError::Unsupported { .. }) => None,
                Err(_) => {
                    self.truncated = true;
                    None
                }
            };
            self.budget.reserve_one(&mut self.entries)?;
            self.budget
                .step(self.entries.len().saturating_sub(insertion) as u64)?;
            self.entries.insert(
                insertion,
                Entry {
                    number: header.number,
                    page: header.page,
                    is_dictionary: header.kind == 0,
                    dictionary,
                },
            );
            if header.kind == 51 {
                if !cursor.remaining().is_empty() {
                    self.truncated = true;
                }
                break;
            }
        }
        Ok(())
    }

    fn segment(
        &mut self,
        header: &Header,
        payload: &[u8],
        rows: Option<u32>,
        base: usize,
    ) -> Result<Option<Dictionary>, FilterError> {
        for reference in &header.references {
            let entry = self.entry(*reference).ok_or_else(|| corrupt(base))?;
            if entry.page != 0 && entry.page != header.page {
                return Err(corrupt(base));
            }
        }
        // Only segments that paint or end the page need its information segment
        // first: a symbol dictionary or table may precede it (common in PDF scans).
        if header.page != 0 && !matches!(header.kind, 0 | 4 | 36 | 40 | 48 | 50 | 52 | 53 | 62) {
            let page = self.page.as_ref().ok_or_else(|| corrupt(base))?;
            if page.number != header.page || page.ended {
                return Err(corrupt(base));
            }
        }
        match header.kind {
            0 => self.dictionary(header, payload, base).map(Some),
            4 | 6 | 7 => {
                let (region, bitmap) = self.text(header, payload, base)?;
                if header.kind != 4 {
                    self.paint(header.page, &region, &bitmap)?;
                }
                Ok(None)
            }
            36 | 38 | 39 => {
                let (region, bitmap) = self.generic(payload, rows, base)?;
                if header.kind != 36 {
                    self.paint(header.page, &region, &bitmap)?;
                }
                Ok(None)
            }
            40 | 42 | 43 => {
                self.skipped_refinement = true;
                Err(unsupported())
            }
            48 => {
                self.page_information(header.page, payload, base)?;
                Ok(None)
            }
            49 => {
                if !payload.is_empty() {
                    return Err(corrupt(base));
                }
                let page = self.page.as_mut().ok_or_else(|| corrupt(base))?;
                if header.page == 0 || page.bitmap.height == 0 {
                    return Err(corrupt(base));
                }
                page.ended = true;
                Ok(None)
            }
            50 => {
                let mut cursor = Cursor::new(payload, base);
                let last = cursor.number()?;
                if !cursor.remaining().is_empty() {
                    return Err(corrupt(base));
                }
                let page = self.page.as_mut().ok_or_else(|| corrupt(base))?;
                let height = last.checked_add(1).ok_or_else(|| corrupt(base))?;
                if !page.striped || height <= page.next_stripe {
                    return Err(corrupt(base));
                }
                if page.unknown_height {
                    page.grow(height, &mut self.budget)?;
                } else if height > page.bitmap.height {
                    return Err(corrupt(base));
                }
                page.next_stripe = height;
                Ok(None)
            }
            51 => {
                if !payload.is_empty() {
                    return Err(corrupt(base));
                }
                Ok(None)
            }
            52 | 62 => Ok(None),
            _ => {
                self.skipped_unsupported = true;
                Err(unsupported())
            }
        }
    }

    fn page_information(
        &mut self,
        number: u32,
        payload: &[u8],
        base: usize,
    ) -> Result<(), FilterError> {
        if number == 0 || self.page.is_some() {
            return Err(corrupt(base));
        }
        let mut cursor = Cursor::new(payload, base);
        let width = cursor.number()?;
        let height = cursor.number()?;
        cursor.number()?;
        cursor.number()?;
        let flags = cursor.byte()?;
        let stripe = cursor.word()?;
        let unknown_height = height == u32::MAX;
        let striped = stripe & 0x8000 != 0;
        if width == 0
            || height == 0
            || flags & 0x80 != 0
            || (unknown_height && !striped)
            || !cursor.remaining().is_empty()
        {
            return Err(corrupt(base));
        }
        let default_black = flags & 4 != 0;
        let bitmap = self.budget.bitmap(
            width,
            if unknown_height { 0 } else { height },
            default_black,
        )?;
        self.page = Some(Page {
            number,
            bitmap,
            unknown_height,
            striped,
            next_stripe: 0,
            default_black,
            default_operator: (flags >> 3) & 3,
            allow_override: flags & 0x40 != 0,
            ended: false,
        });
        Ok(())
    }

    fn paint(&mut self, number: u32, region: &Region, bitmap: &Bitmap) -> Result<(), FilterError> {
        let page = self.page.as_mut().ok_or_else(|| corrupt(0))?;
        if number == 0 || page.number != number || page.ended {
            return Err(corrupt(0));
        }
        if page.unknown_height {
            let bottom = region
                .y
                .checked_add(i64::from(region.height))
                .and_then(|height| u32::try_from(height).ok())
                .ok_or_else(|| corrupt(0))?;
            page.grow(bottom, &mut self.budget)?;
        }
        let operator = if page.allow_override {
            region.operator
        } else {
            page.default_operator
        };
        compose(
            &mut page.bitmap,
            bitmap,
            region.x,
            region.y,
            operator,
            &mut self.budget,
        )
    }

    fn input_symbols(&mut self, header: &Header) -> Result<Vec<Arc<Bitmap>>, FilterError> {
        let mut count = 0usize;
        for reference in &header.references {
            let entry = self.entry(*reference).ok_or_else(|| corrupt(0))?;
            if entry.is_dictionary {
                let dictionary = entry.dictionary.as_ref().ok_or_else(|| corrupt(0))?;
                count = count
                    .checked_add(dictionary.symbols.len())
                    .ok_or_else(|| refusal(LimitKind::Jbig2Symbols, u64::MAX, MAX_SYMBOLS))?;
                check(LimitKind::Jbig2Symbols, count as u64, MAX_SYMBOLS)?;
            }
        }
        self.budget.step(count as u64)?;
        let mut symbols = self.budget.vector(count)?;
        for reference in &header.references {
            let entry = self.entry(*reference).ok_or_else(|| corrupt(0))?;
            if let Some(dictionary) = &entry.dictionary {
                symbols.extend(dictionary.symbols.iter().map(Arc::clone));
            }
        }
        Ok(symbols)
    }

    fn generic(
        &mut self,
        payload: &[u8],
        rows: Option<u32>,
        base: usize,
    ) -> Result<(Region, Bitmap), FilterError> {
        let mut cursor = Cursor::new(payload, base);
        let mut region = Region::read(&mut cursor)?;
        if let Some(rows) = rows {
            if region.height != u32::MAX && region.height != rows {
                return Err(corrupt(base));
            }
            region.height = rows;
        }
        let flags = cursor.byte()?;
        if flags & 0xf0 != 0 {
            self.skipped_unsupported = true;
            return Err(unsupported());
        }
        let mmr = flags & 1 != 0;
        let template = (flags >> 1) & 3;
        if mmr {
            if flags & 0x0e != 0 {
                return Err(corrupt(base));
            }
            self.budget.pixels(region.width, region.height)?;
            let size = bitmap_size(region.width, region.height)?;
            self.budget.charge(size as u64)?;
            self.budget
                .step(u64::from(region.width).saturating_mul(u64::from(region.height)))?;
            let data = crate::filter::ccitt::decode_g4(
                cursor.remaining(),
                region.width,
                region.height,
                true,
            )
            .ok_or_else(|| corrupt(cursor.offset()))?;
            if data.len() != size {
                return Err(corrupt(cursor.offset()));
            }
            let mut bitmap = Bitmap {
                width: region.width,
                height: region.height,
                data,
            };
            bitmap.clear_padding()?;
            return Ok((region, bitmap));
        }
        let adaptive = adaptive_pixels(&mut cursor, template)?;
        let mut contexts = self.budget.zeros(context_count(template)?)?;
        let mut bitmap = self.budget.bitmap(region.width, region.height, false)?;
        if region.width != 0 && region.height != 0 {
            let mut mq = Mq::new(cursor.remaining(), cursor.offset())?;
            generic_bitmap(
                &mut bitmap,
                template,
                &adaptive,
                flags & 8 != 0,
                &mut contexts,
                &mut mq,
                &mut self.budget,
            )?;
        }
        Ok((region, bitmap))
    }

    fn dictionary(
        &mut self,
        header: &Header,
        payload: &[u8],
        base: usize,
    ) -> Result<Dictionary, FilterError> {
        let mut cursor = Cursor::new(payload, base);
        let flags = cursor.word()?;
        if flags & 2 != 0 {
            self.skipped_refinement = true;
            return Err(unsupported());
        }
        if flags & 1 != 0 || flags & 0xe000 != 0 {
            self.skipped_unsupported = true;
            return Err(unsupported());
        }
        let template = ((flags >> 10) & 3) as u8;
        let adaptive = adaptive_pixels(&mut cursor, template)?;
        let exported = cursor.number()?;
        let new = cursor.number()?;
        check(LimitKind::Jbig2Symbols, u64::from(exported), MAX_SYMBOLS)?;
        let requested = self
            .symbols
            .checked_add(u64::from(new))
            .ok_or_else(|| refusal(LimitKind::Jbig2Symbols, u64::MAX, MAX_SYMBOLS))?;
        check(LimitKind::Jbig2Symbols, requested, MAX_SYMBOLS)?;
        self.symbols = requested;
        let mut symbols = self.input_symbols(header)?;
        let imported = symbols.len();
        let total = imported
            .checked_add(usize::try_from(new).map_err(|_| corrupt(base))?)
            .ok_or_else(|| refusal(LimitKind::Jbig2Symbols, u64::MAX, MAX_SYMBOLS))?;
        check(LimitKind::Jbig2Symbols, total as u64, MAX_SYMBOLS)?;
        if u64::from(exported) > total as u64 {
            return Err(corrupt(base));
        }
        if total > symbols.capacity() {
            let additional = total.saturating_sub(symbols.capacity());
            self.budget
                .charge((additional as u64).saturating_mul(size_of::<Arc<Bitmap>>() as u64))?;
            symbols
                .try_reserve_exact(total.saturating_sub(symbols.len()))
                .map_err(|_| self.budget.allocation_error())?;
        }
        let mut contexts = self.budget.zeros(context_count(template)?)?;
        if flags & 0x100 != 0 {
            let retained = header
                .references
                .iter()
                .rev()
                .find_map(|reference| {
                    self.entry(*reference)?
                        .dictionary
                        .as_ref()
                        .filter(|dictionary| dictionary.contexts.is_some())
                })
                .ok_or_else(|| corrupt(base))?;
            let previous = retained.contexts.as_ref().ok_or_else(|| corrupt(base))?;
            if retained.template != template || previous.len() != contexts.len() {
                return Err(corrupt(base));
            }
            contexts.copy_from_slice(previous);
        }
        let mut output = self
            .budget
            .vector(usize::try_from(exported).map_err(|_| corrupt(base))?)?;
        if total != 0 {
            let mut mq = Mq::new(cursor.remaining(), cursor.offset())?;
            let mut dh = Integer::new();
            let mut dw = Integer::new();
            let mut ex = Integer::new();
            let mut height = 0i64;
            let mut decoded = 0u32;
            while decoded < new {
                self.budget.step(1)?;
                height = add(height, dh.required(&mut mq, &mut self.budget)?)?;
                let symbol_height = u32::try_from(height).map_err(|_| corrupt(base))?;
                let mut width = 0i64;
                let before = decoded;
                loop {
                    self.budget.step(1)?;
                    let Some(delta) = dw.read(&mut mq, &mut self.budget)? else {
                        break;
                    };
                    if decoded >= new {
                        return Err(corrupt(base));
                    }
                    width = add(width, delta)?;
                    let symbol_width = u32::try_from(width).map_err(|_| corrupt(base))?;
                    let mut bitmap = self.budget.bitmap(symbol_width, symbol_height, false)?;
                    generic_bitmap(
                        &mut bitmap,
                        template,
                        &adaptive,
                        false,
                        &mut contexts,
                        &mut mq,
                        &mut self.budget,
                    )?;
                    symbols.push(self.budget.shared(bitmap)?);
                    decoded = decoded.checked_add(1).ok_or_else(|| corrupt(base))?;
                }
                if decoded == before {
                    return Err(corrupt(base));
                }
            }
            let mut position = 0usize;
            let mut exporting = false;
            while position < total {
                self.budget.step(1)?;
                let run = ex.required(&mut mq, &mut self.budget)?;
                let run = usize::try_from(run).map_err(|_| corrupt(base))?;
                let end = position
                    .checked_add(run)
                    .filter(|&end| end <= total)
                    .ok_or_else(|| corrupt(base))?;
                if exporting {
                    let requested = output.len().checked_add(run).ok_or_else(|| corrupt(base))?;
                    if requested > usize::try_from(exported).map_err(|_| corrupt(base))? {
                        return Err(corrupt(base));
                    }
                    let selected = symbols.get(position..end).ok_or_else(|| corrupt(base))?;
                    self.budget.step(run as u64)?;
                    output.extend(selected.iter().map(Arc::clone));
                }
                position = end;
                exporting = !exporting;
            }
        }
        if output.len() != usize::try_from(exported).map_err(|_| corrupt(base))? {
            return Err(corrupt(base));
        }
        Ok(Dictionary {
            symbols: output,
            contexts: if flags & 0x200 != 0 {
                Some(contexts)
            } else {
                None
            },
            template,
        })
    }

    fn text(
        &mut self,
        header: &Header,
        payload: &[u8],
        base: usize,
    ) -> Result<(Region, Bitmap), FilterError> {
        let mut cursor = Cursor::new(payload, base);
        let region = Region::read(&mut cursor)?;
        let flags = cursor.word()?;
        if flags & 2 != 0 {
            self.skipped_refinement = true;
            return Err(unsupported());
        }
        if flags & 1 != 0 {
            self.skipped_unsupported = true;
            return Err(unsupported());
        }
        let instances = cursor.number()?;
        check(LimitKind::Jbig2Symbols, u64::from(instances), MAX_SYMBOLS)?;
        let symbols = self.input_symbols(header)?;
        let mut bitmap = self
            .budget
            .bitmap(region.width, region.height, flags & 0x200 != 0)?;
        if instances == 0 {
            return Ok((region, bitmap));
        }
        if symbols.is_empty() {
            return Err(corrupt(base));
        }
        let strip_size = 1i64 << ((flags >> 2) & 3);
        let corner = ((flags >> 4) & 3) as u8;
        let transposed = flags & 0x40 != 0;
        let operator = ((flags >> 7) & 3) as u8;
        let encoded_offset = i64::from((flags >> 10) & 31);
        let offset = if encoded_offset & 16 != 0 {
            encoded_offset - 32
        } else {
            encoded_offset
        };
        let bits = if symbols.len() <= 1 {
            0
        } else {
            usize::BITS - symbols.len().saturating_sub(1).leading_zeros()
        };
        let context_size = 1usize.checked_shl(bits).ok_or_else(|| corrupt(base))?;
        let mut id_contexts = self.budget.zeros(context_size)?;
        let mut mq = Mq::new(cursor.remaining(), cursor.offset())?;
        let mut dt = Integer::new();
        let mut fs = Integer::new();
        let mut ds = Integer::new();
        let mut it = Integer::new();
        let initial = dt.required(&mut mq, &mut self.budget)?;
        let mut strip_t = initial
            .checked_mul(strip_size)
            .and_then(i64::checked_neg)
            .ok_or_else(|| corrupt(base))?;
        let mut first_s = 0i64;
        let mut placed = 0u32;
        while placed < instances {
            self.budget.step(1)?;
            let delta = dt
                .required(&mut mq, &mut self.budget)?
                .checked_mul(strip_size)
                .ok_or_else(|| corrupt(base))?;
            strip_t = add(strip_t, delta)?;
            first_s = add(first_s, fs.required(&mut mq, &mut self.budget)?)?;
            let mut current_s = first_s;
            loop {
                self.budget.step(1)?;
                if placed >= instances {
                    return Err(corrupt(base));
                }
                let current_t = if strip_size == 1 {
                    0
                } else {
                    it.required(&mut mq, &mut self.budget)?
                };
                if !(0..strip_size).contains(&current_t) {
                    return Err(corrupt(base));
                }
                let t = add(strip_t, current_t)?;
                let id = identifier(&mut mq, &mut id_contexts, bits, &mut self.budget)?;
                let symbol = symbols.get(id).ok_or_else(|| corrupt(base))?;
                let width = i64::from(symbol.width) - 1;
                let height = i64::from(symbol.height) - 1;
                let right = corner & 2 != 0;
                let bottom = corner & 1 == 0;
                if !transposed && right {
                    current_s = add(current_s, width)?;
                } else if transposed && bottom {
                    current_s = add(current_s, height)?;
                }
                let (x, y) = if transposed {
                    (t, current_s)
                } else {
                    (current_s, t)
                };
                let x = if right {
                    x.checked_sub(width).ok_or_else(|| corrupt(base))?
                } else {
                    x
                };
                let y = if bottom {
                    y.checked_sub(height).ok_or_else(|| corrupt(base))?
                } else {
                    y
                };
                compose(&mut bitmap, symbol, x, y, operator, &mut self.budget)?;
                if !transposed && !right {
                    current_s = add(current_s, width)?;
                } else if transposed && !bottom {
                    current_s = add(current_s, height)?;
                }
                placed = placed.checked_add(1).ok_or_else(|| corrupt(base))?;
                let Some(delta_s) = ds.read(&mut mq, &mut self.budget)? else {
                    break;
                };
                if placed >= instances {
                    return Err(corrupt(base));
                }
                current_s = add(add(current_s, delta_s)?, offset)?;
            }
        }
        Ok((region, bitmap))
    }
}

fn add(left: i64, right: i64) -> Result<i64, FilterError> {
    left.checked_add(right).ok_or_else(|| corrupt(0))
}

fn adaptive_pixels(cursor: &mut Cursor<'_>, template: u8) -> Result<[(i64, i64); 4], FilterError> {
    let mut adaptive = [(0, 0); 4];
    let count = if template == 0 { 4 } else { 1 };
    for slot in adaptive.iter_mut().take(count) {
        let x = i64::from(cursor.byte()? as i8);
        let y = i64::from(cursor.byte()? as i8);
        if y > 0 || (y == 0 && x >= 0) {
            return Err(corrupt(cursor.offset()));
        }
        *slot = (x, y);
    }
    Ok(adaptive)
}

fn context_count(template: u8) -> Result<usize, FilterError> {
    match template {
        0 => Ok(65_536),
        1 => Ok(8_192),
        2 | 3 => Ok(1_024),
        _ => Err(corrupt(0)),
    }
}

// Coordinates run from the least significant context bit to the most
// significant one. Adaptive pixels occupy their nominal template slots.
const TEMPLATE_0: [(i64, i64); 16] = [
    (-1, 0),
    (-2, 0),
    (-3, 0),
    (-4, 0),
    (3, -1),
    (2, -1),
    (1, -1),
    (0, -1),
    (-1, -1),
    (-2, -1),
    (-3, -1),
    (2, -2),
    (1, -2),
    (0, -2),
    (-1, -2),
    (-2, -2),
];
const TEMPLATE_1: [(i64, i64); 13] = [
    (-1, 0),
    (-2, 0),
    (-3, 0),
    (3, -1),
    (2, -1),
    (1, -1),
    (0, -1),
    (-1, -1),
    (-2, -1),
    (2, -2),
    (1, -2),
    (0, -2),
    (-1, -2),
];
const TEMPLATE_2: [(i64, i64); 10] = [
    (-1, 0),
    (-2, 0),
    (2, -1),
    (1, -1),
    (0, -1),
    (-1, -1),
    (-2, -1),
    (1, -2),
    (0, -2),
    (-1, -2),
];
const TEMPLATE_3: [(i64, i64); 10] = [
    (-1, 0),
    (-2, 0),
    (-3, 0),
    (-4, 0),
    (2, -1),
    (1, -1),
    (0, -1),
    (-1, -1),
    (-2, -1),
    (-3, -1),
];

fn generic_bitmap(
    bitmap: &mut Bitmap,
    template: u8,
    adaptive: &[(i64, i64); 4],
    typical_prediction: bool,
    contexts: &mut [u8],
    mq: &mut Mq<'_>,
    budget: &mut Budget,
) -> Result<(), FilterError> {
    let (fixed, typical): (&[(i64, i64)], usize) = match template {
        0 => (&TEMPLATE_0, 0x9b25),
        1 => (&TEMPLATE_1, 0x0795),
        2 => (&TEMPLATE_2, 0x00e5),
        3 => (&TEMPLATE_3, 0x0195),
        _ => return Err(corrupt(0)),
    };
    if contexts.len() != context_count(template)? {
        return Err(corrupt(0));
    }
    if bitmap.width == 0 || bitmap.height == 0 {
        return Ok(());
    }
    let mut offsets = [(0i64, 0i64); 16];
    let used = offsets.get_mut(..fixed.len()).ok_or_else(|| corrupt(0))?;
    used.copy_from_slice(fixed);
    match template {
        0 => {
            for (slot, pixel) in [4usize, 10, 11, 15].iter().zip(adaptive.iter()) {
                *used.get_mut(*slot).ok_or_else(|| corrupt(0))? = *pixel;
            }
        }
        1 => {
            *used.get_mut(3).ok_or_else(|| corrupt(0))? =
                *adaptive.first().ok_or_else(|| corrupt(0))?
        }
        2 => {
            *used.get_mut(2).ok_or_else(|| corrupt(0))? =
                *adaptive.first().ok_or_else(|| corrupt(0))?
        }
        3 => {
            *used.get_mut(4).ok_or_else(|| corrupt(0))? =
                *adaptive.first().ok_or_else(|| corrupt(0))?
        }
        _ => return Err(corrupt(0)),
    }
    let mut ltp = 0u8;
    for y in 0..bitmap.height {
        budget.step(1)?;
        if typical_prediction {
            let context = contexts.get_mut(typical).ok_or_else(|| corrupt(0))?;
            ltp ^= mq.bit(context, budget)?;
        }
        if ltp != 0 {
            bitmap.copy_previous(y, budget)?;
            continue;
        }
        for x in 0..bitmap.width {
            let mut index = 0usize;
            for (bit, &(dx, dy)) in used.iter().enumerate() {
                let sample_x = i64::from(x).checked_add(dx).ok_or_else(|| corrupt(0))?;
                let sample_y = i64::from(y).checked_add(dy).ok_or_else(|| corrupt(0))?;
                index |= usize::from(bitmap.pixel(sample_x, sample_y)) << bit;
            }
            let context = contexts.get_mut(index).ok_or_else(|| corrupt(0))?;
            let value = mq.bit(context, budget)?;
            bitmap.put(x, y, value)?;
        }
    }
    Ok(())
}

const QE: [u16; 47] = [
    0x5601, 0x3401, 0x1801, 0x0ac1, 0x0521, 0x0221, 0x5601, 0x5401, 0x4801, 0x3801, 0x3001, 0x2401,
    0x1c01, 0x1601, 0x5601, 0x5401, 0x5101, 0x4801, 0x3801, 0x3401, 0x3001, 0x2801, 0x2401, 0x2201,
    0x1c01, 0x1801, 0x1601, 0x1401, 0x1201, 0x1101, 0x0ac1, 0x09c1, 0x08a1, 0x0521, 0x0441, 0x02a1,
    0x0221, 0x0141, 0x0111, 0x0085, 0x0049, 0x0025, 0x0015, 0x0009, 0x0005, 0x0001, 0x5601,
];
const NMPS: [u8; 47] = [
    1, 2, 3, 4, 5, 38, 7, 8, 9, 10, 11, 12, 13, 29, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26,
    27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 45, 46,
];
const NLPS: [u8; 47] = [
    1, 6, 9, 12, 29, 33, 6, 14, 14, 14, 17, 18, 20, 21, 14, 14, 15, 16, 17, 18, 19, 19, 20, 21, 22,
    23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 46,
];

struct Mq<'a> {
    bytes: &'a [u8],
    position: usize,
    base: usize,
    a: u32,
    c: u32,
    ct: u8,
}

impl<'a> Mq<'a> {
    fn new(bytes: &'a [u8], base: usize) -> Result<Self, FilterError> {
        let first = bytes.first().copied().ok_or_else(|| corrupt(base))?;
        let mut mq = Self {
            bytes,
            position: 0,
            base,
            a: 0x8000,
            c: u32::from(first) << 16,
            ct: 0,
        };
        mq.byte_in()?;
        mq.c <<= 7;
        mq.ct = mq.ct.checked_sub(7).ok_or_else(|| corrupt(base))?;
        Ok(mq)
    }

    fn offset(&self) -> usize {
        self.base.saturating_add(self.position)
    }

    fn byte_in(&mut self) -> Result<(), FilterError> {
        let current = self
            .bytes
            .get(self.position)
            .copied()
            .ok_or_else(|| corrupt(self.offset()))?;
        let next_position = self
            .position
            .checked_add(1)
            .ok_or_else(|| corrupt(self.offset()))?;
        let next = self
            .bytes
            .get(next_position)
            .copied()
            .ok_or_else(|| corrupt(self.offset()))?;
        // C is a modular 32-bit coding register, not a size or offset.
        if current == 255 {
            if next > 0x8f {
                if next != 0xac {
                    return Err(corrupt(self.offset()));
                }
                self.c = self.c.wrapping_add(0xff00);
                self.ct = 8;
            } else {
                self.position = next_position;
                self.c = self.c.wrapping_add(u32::from(next) << 9);
                self.ct = 7;
            }
        } else {
            self.position = next_position;
            self.c = self.c.wrapping_add(u32::from(next) << 8);
            self.ct = 8;
        }
        Ok(())
    }

    fn renormalize(&mut self) -> Result<(), FilterError> {
        for _ in 0..16 {
            if self.a & 0x8000 != 0 {
                return Ok(());
            }
            if self.ct == 0 {
                self.byte_in()?;
            }
            self.a <<= 1;
            self.c <<= 1;
            self.ct = self
                .ct
                .checked_sub(1)
                .ok_or_else(|| corrupt(self.offset()))?;
        }
        if self.a & 0x8000 != 0 {
            Ok(())
        } else {
            Err(corrupt(self.offset()))
        }
    }

    fn bit(&mut self, context: &mut u8, budget: &mut Budget) -> Result<u8, FilterError> {
        budget.step(1)?;
        let index = usize::from(*context >> 1);
        let mps = *context & 1;
        let qe = u32::from(*QE.get(index).ok_or_else(|| corrupt(self.offset()))?);
        let nmps = *NMPS.get(index).ok_or_else(|| corrupt(self.offset()))?;
        let nlps = *NLPS.get(index).ok_or_else(|| corrupt(self.offset()))?;
        self.a = self
            .a
            .checked_sub(qe)
            .ok_or_else(|| corrupt(self.offset()))?;
        let decision;
        if self.c >> 16 < qe {
            if self.a < qe {
                decision = mps;
                *context = (nmps << 1) | mps;
            } else {
                decision = mps ^ 1;
                let switched = if matches!(index, 0 | 6 | 14) {
                    mps ^ 1
                } else {
                    mps
                };
                *context = (nlps << 1) | switched;
            }
            self.a = qe;
            self.renormalize()?;
        } else {
            self.c = self
                .c
                .checked_sub(qe << 16)
                .ok_or_else(|| corrupt(self.offset()))?;
            if self.a & 0x8000 != 0 {
                return Ok(mps);
            }
            if self.a < qe {
                decision = mps ^ 1;
                let switched = if matches!(index, 0 | 6 | 14) {
                    mps ^ 1
                } else {
                    mps
                };
                *context = (nlps << 1) | switched;
            } else {
                decision = mps;
                *context = (nmps << 1) | mps;
            }
            self.renormalize()?;
        }
        Ok(decision)
    }
}

struct Integer {
    contexts: [u8; 512],
}

impl Integer {
    fn new() -> Self {
        Self { contexts: [0; 512] }
    }

    fn next(
        &mut self,
        previous: &mut usize,
        mq: &mut Mq<'_>,
        budget: &mut Budget,
    ) -> Result<u8, FilterError> {
        let context = self
            .contexts
            .get_mut(*previous)
            .ok_or_else(|| corrupt(mq.offset()))?;
        let bit = mq.bit(context, budget)?;
        let next = (*previous << 1) | usize::from(bit);
        *previous = if *previous < 256 {
            next
        } else {
            (next & 511) | 256
        };
        Ok(bit)
    }

    fn read(&mut self, mq: &mut Mq<'_>, budget: &mut Budget) -> Result<Option<i64>, FilterError> {
        let mut previous = 1usize;
        let sign = self.next(&mut previous, mq, budget)?;
        let categories = [(2u8, 0i64), (4, 4), (6, 20), (8, 84), (12, 340)];
        let mut selected = (32u8, 4436i64);
        for category in categories {
            if self.next(&mut previous, mq, budget)? == 0 {
                selected = category;
                break;
            }
        }
        let (bits, offset) = selected;
        let mut value = 0i64;
        for _ in 0..bits {
            value = value
                .checked_mul(2)
                .and_then(|value| {
                    value.checked_add(i64::from(self.next(&mut previous, mq, budget).ok()?))
                })
                .ok_or_else(|| corrupt(mq.offset()))?;
        }
        value = add(value, offset)?;
        if sign != 0 && value == 0 {
            Ok(None)
        } else if sign != 0 {
            Ok(Some(
                value.checked_neg().ok_or_else(|| corrupt(mq.offset()))?,
            ))
        } else {
            Ok(Some(value))
        }
    }

    fn required(&mut self, mq: &mut Mq<'_>, budget: &mut Budget) -> Result<i64, FilterError> {
        self.read(mq, budget)?.ok_or_else(|| corrupt(mq.offset()))
    }
}

fn identifier(
    mq: &mut Mq<'_>,
    contexts: &mut [u8],
    bits: u32,
    budget: &mut Budget,
) -> Result<usize, FilterError> {
    if bits > 20 {
        return Err(refusal(LimitKind::Jbig2Symbols, u64::MAX, MAX_SYMBOLS));
    }
    let mut previous = 1usize;
    for _ in 0..bits {
        let context = contexts
            .get_mut(previous)
            .ok_or_else(|| corrupt(mq.offset()))?;
        let bit = mq.bit(context, budget)?;
        previous = previous
            .checked_mul(2)
            .and_then(|value| value.checked_add(usize::from(bit)))
            .ok_or_else(|| corrupt(mq.offset()))?;
    }
    previous
        .checked_sub(
            1usize
                .checked_shl(bits)
                .ok_or_else(|| corrupt(mq.offset()))?,
        )
        .ok_or_else(|| corrupt(mq.offset()))
}
