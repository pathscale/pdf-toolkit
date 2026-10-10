use alloc::vec::Vec;

use super::{FilterError, FilterKind};

/// The dimension-only MMR entry point has no caller-supplied budget.
pub const G4_MAX_PIXELS: u64 = 256 * 1024 * 1024;

/// PDF CCITTFaxDecode parameters.
///
/// Returned rows are always MSB first, byte padded, with 1 meaning black.
/// `black_is_1` selects the PDF sample convention used inside the decoder.
/// A caller applying a PDF Decode array must convert the normalized black
/// bits back to that sample convention before applying the array.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CcittParams {
    pub k: i32,
    pub columns: u32,
    pub rows: u32,
    pub black_is_1: bool,
    pub encoded_byte_align: bool,
    pub end_of_line: bool,
    pub end_of_block: bool,
    pub damaged_rows_before_error: u32,
}

impl Default for CcittParams {
    fn default() -> Self {
        Self {
            k: 0,
            columns: 1728,
            rows: 0,
            black_is_1: false,
            encoded_byte_align: false,
            end_of_line: false,
            end_of_block: true,
            damaged_rows_before_error: 0,
        }
    }
}

/// Decodes dimension-delimited Group 4 data, including embedded JBIG2 MMR.
///
/// EOFB is optional because the dimensions delimit this payload. Incomplete
/// rows, unsupported extension codes and dimension refusals return None.
/// Output polarity is normalized to 1 = black for either sample convention.
pub fn decode_g4(
    data: &[u8],
    width: u32,
    height: u32,
    black_is_1: bool,
) -> Option<Vec<u8>> {
    if width == 0 || height == 0 {
        return None;
    }
    let pixels = u64::from(width).checked_mul(u64::from(height))?;
    if pixels > G4_MAX_PIXELS {
        return None;
    }

    let params = CcittParams {
        k: -1,
        columns: width,
        rows: height,
        black_is_1,
        end_of_block: false,
        ..CcittParams::default()
    };
    match decode(data, &params, G4_MAX_PIXELS).ok()? {
        (bytes, false) => Some(bytes),
        (_, true) => None,
    }
}

/// Decodes T.4 one-dimensional or mixed data, or T.6 Group 4 data.
///
/// Output contains complete rows only, with zero padding bits and 1 = black.
/// With EndOfBlock enabled, RTC or EOFB terminates decoding and overrides
/// Rows. Otherwise a nonzero Rows value delimits the payload; Rows = 0
/// decodes until the input ends.
///
/// Framed Group 3 damage may be replaced by the preceding row, up to the
/// consecutive damaged-row allowance. Any damage sets the truncation flag.
/// Unrecoverable damage stops decoding. With no recovered rows it is an
/// error. Optional uncompressed extension codes also stop as damaged input.
///
/// The existing FilterError output refusal carries `max_pixels` for this
/// pixel-budgeted API. Allocation and addressability failures use the same
/// refusal. All output and row storage is reserved before the row loop.
pub fn decode(
    data: &[u8],
    params: &CcittParams,
    max_pixels: u64,
) -> Result<(Vec<u8>, bool), FilterError> {
    if params.columns == 0 {
        return Err(FilterError::BadParams {
            filter: FilterKind::CcittFax,
        });
    }

    let columns = u64::from(params.columns);
    let dimension_delimited = !params.end_of_block && params.rows != 0;
    let row_limit = if dimension_delimited {
        let pixels = columns
            .checked_mul(u64::from(params.rows))
            .ok_or_else(|| output_limit(max_pixels))?;
        if pixels > max_pixels {
            return Err(output_limit(max_pixels));
        }
        u64::from(params.rows)
    } else {
        max_pixels / columns
    };
    if row_limit == 0 {
        return Err(output_limit(max_pixels));
    }

    let width = usize::try_from(params.columns)
        .map_err(|_| output_limit(max_pixels))?;
    let row_bytes = columns
        .checked_add(7)
        .map(|bits| bits / 8)
        .and_then(|bytes| usize::try_from(bytes).ok())
        .ok_or_else(|| output_limit(max_pixels))?;

    if data.is_empty() {
        return Err(FilterError::Corrupt {
            filter: FilterKind::CcittFax,
            offset: 0,
        });
    }

    // Every emitted row consumes at least one encoded bit, including a
    // damaged row whose recovery consumes framing or scans to another EOL.
    // This bounds reservation for short inputs without allocating per row.
    let input_bits = data.len().saturating_mul(8) as u64;
    let storage_rows = usize::try_from(row_limit.min(input_bits))
        .map_err(|_| output_limit(max_pixels))?;
    let output_bytes = row_bytes
        .checked_mul(storage_rows)
        .ok_or_else(|| output_limit(max_pixels))?;

    let mut output = Vec::new();
    reserve(&mut output, output_bytes, max_pixels)?;

    let white = if params.black_is_1 { 0 } else { 255 };
    let mut reference = row_storage(row_bytes, white, max_pixels)?;
    let mut current = row_storage(row_bytes, white, max_pixels)?;
    let mut reader = BitReader::new(data);
    let mut truncated = false;
    let mut damaged = 0u32;

    // The extra boundary iteration recognizes a terminator at the budget
    // edge without allowing an additional output row.
    for row_index in 0..=row_limit {
        if dimension_delimited && row_index == row_limit {
            return finish(output, truncated, reader.offset());
        }

        let one_dimensional = match boundary(&mut reader, params) {
            Boundary::End => {
                let short = dimension_delimited && row_index < row_limit;
                return finish(output, truncated || short, reader.offset());
            }
            Boundary::Exhausted => {
                let missing = params.end_of_block
                    || (dimension_delimited && row_index < row_limit);
                return finish(output, truncated || missing, reader.offset());
            }
            Boundary::Row(one_dimensional) => Some(one_dimensional),
            Boundary::Damaged => None,
        };

        if row_index == row_limit {
            return if one_dimensional.is_some() {
                Err(output_limit(max_pixels))
            } else {
                finish(output, true, reader.offset())
            };
        }

        current.fill(white);
        let complete = match one_dimensional {
            Some(true) => decode_1d(
                &mut reader,
                &mut current,
                width,
                params.black_is_1,
            )
            .is_some(),
            Some(false) => decode_2d(
                &mut reader,
                &reference,
                &mut current,
                width,
                params.black_is_1,
            )
            .is_some(),
            None => false,
        };

        if complete {
            damaged = 0;
        } else {
            truncated = true;
            if params.k < 0
                || !params.end_of_line
                || damaged >= params.damaged_rows_before_error
                || !reader.seek_eol()
            {
                return finish(output, true, reader.offset());
            }
            damaged = damaged.saturating_add(1);
            // Both buffers retain their validated, identical row lengths.
            current.copy_from_slice(&reference);
        }

        append_row(
            &mut output,
            &current,
            params.black_is_1,
            max_pixels,
        )?;
        core::mem::swap(&mut current, &mut reference);
    }

    finish(output, true, reader.offset())
}

fn output_limit(limit: u64) -> FilterError {
    FilterError::OutputLimit { limit }
}

fn reserve(
    bytes: &mut Vec<u8>,
    size: usize,
    limit: u64,
) -> Result<(), FilterError> {
    if size > isize::MAX as usize {
        return Err(output_limit(limit));
    }
    bytes
        .try_reserve_exact(size.saturating_sub(bytes.len()))
        .map_err(|_| output_limit(limit))
}

fn row_storage(
    size: usize,
    white: u8,
    limit: u64,
) -> Result<Vec<u8>, FilterError> {
    let mut row = Vec::new();
    reserve(&mut row, size, limit)?;
    row.resize(size, white);
    Ok(row)
}

fn append_row(
    output: &mut Vec<u8>,
    row: &[u8],
    black_is_1: bool,
    limit: u64,
) -> Result<(), FilterError> {
    let end = output
        .len()
        .checked_add(row.len())
        .ok_or_else(|| output_limit(limit))?;
    if end > output.capacity() {
        return Err(output_limit(limit));
    }
    for byte in row.iter().copied() {
        output.push(if black_is_1 { byte } else { !byte });
    }
    Ok(())
}

fn finish(
    bytes: Vec<u8>,
    truncated: bool,
    offset: usize,
) -> Result<(Vec<u8>, bool), FilterError> {
    if truncated && bytes.is_empty() {
        Err(FilterError::Corrupt {
            filter: FilterKind::CcittFax,
            offset: offset as u64,
        })
    } else {
        Ok((bytes, truncated))
    }
}

enum Boundary {
    End,
    Exhausted,
    Row(bool),
    Damaged,
}

fn boundary(reader: &mut BitReader<'_>, params: &CcittParams) -> Boundary {
    if reader.end_marker(params.k) {
        return Boundary::End;
    }
    if reader.only_padding() {
        return Boundary::Exhausted;
    }

    if params.k < 0 {
        if params.encoded_byte_align && reader.align_zero().is_none() {
            return Boundary::Damaged;
        }
        if reader.end_marker(params.k) {
            return Boundary::End;
        }
        if reader.only_padding() {
            return Boundary::Exhausted;
        }
        return Boundary::Row(false);
    }

    // Recognize fill and EOL before alignment. This accepts both byte-aligned
    // line starts and fax fill that makes the EOL itself end on a byte edge.
    let mut eol = reader.eol();
    if !eol && params.encoded_byte_align {
        if reader.align_zero().is_none() {
            return Boundary::Damaged;
        }
        eol = reader.eol();
    }
    if params.end_of_line && !eol {
        return Boundary::Damaged;
    }
    if reader.only_padding() {
        return Boundary::Exhausted;
    }

    if params.k > 0 {
        match reader.take(1) {
            Some(1) => Boundary::Row(true),
            Some(0) => Boundary::Row(false),
            _ => Boundary::Exhausted,
        }
    } else {
        Boundary::Row(true)
    }
}

fn decode_1d(
    reader: &mut BitReader<'_>,
    row: &mut [u8],
    width: usize,
    black_is_1: bool,
) -> Option<()> {
    let mut x = 0usize;
    let mut black = false;
    let steps = (width as u64).checked_mul(2)?.checked_add(2)?;

    for _ in 0..steps {
        if x == width {
            return Some(());
        }
        let length = run(reader, black, width.checked_sub(x)?)?;
        let end = x.checked_add(length)?;
        if black {
            paint_black(row, x, end, black_is_1)?;
        }
        x = end;
        black = !black;
    }

    if x == width { Some(()) } else { None }
}

fn decode_2d(
    reader: &mut BitReader<'_>,
    previous: &[u8],
    row: &mut [u8],
    width: usize,
    black_is_1: bool,
) -> Option<()> {
    let mut reference = Reference::new(previous, width, black_is_1)?;
    let mut x = 0usize;
    let mut black = false;
    let mut initial = true;
    let steps = (width as u64).checked_mul(2)?.checked_add(2)?;

    for _ in 0..steps {
        if x == width {
            return Some(());
        }

        match mode(reader)? {
            Mode::Pass => {
                let (_, b2) = reference.pair(x, black, initial)?;
                if b2 <= x || b2 > width {
                    return None;
                }
                if black {
                    paint_black(row, x, b2, black_is_1)?;
                }
                x = b2;
            }
            Mode::Horizontal => {
                let first = run(reader, black, width.checked_sub(x)?)?;
                let middle = x.checked_add(first)?;
                let second = run(reader, !black, width.checked_sub(middle)?)?;
                let end = middle.checked_add(second)?;
                if end <= x || end > width {
                    return None;
                }
                if black {
                    paint_black(row, x, middle, black_is_1)?;
                } else {
                    paint_black(row, middle, end, black_is_1)?;
                }
                x = end;
            }
            Mode::Vertical(delta) => {
                let (b1, _) = reference.pair(x, black, initial)?;
                let end = if delta < 0 {
                    b1.checked_sub((-i16::from(delta)) as usize)?
                } else {
                    b1.checked_add(delta as usize)?
                };
                if end < x || end > width {
                    return None;
                }
                if black {
                    paint_black(row, x, end, black_is_1)?;
                }
                x = end;
                black = !black;
            }
            Mode::Invalid => return None,
        }
        initial = false;
    }

    if x == width { Some(()) } else { None }
}

fn paint_black(
    row: &mut [u8],
    start: usize,
    end: usize,
    black_is_1: bool,
) -> Option<()> {
    if start > end {
        return None;
    }
    let mut x = start;
    while x < end {
        let offset = x % 8;
        let count = (8usize.checked_sub(offset)?).min(end.checked_sub(x)?);
        let shift = 8usize.checked_sub(offset)?.checked_sub(count)?;
        let mask = ((0xffu16 >> (8usize.checked_sub(count)?)) as u8) << shift;
        let byte = row.get_mut(x / 8)?;
        if black_is_1 {
            *byte |= mask;
        } else {
            *byte &= !mask;
        }
        x = x.checked_add(count)?;
    }
    Some(())
}

/// A three-transition window avoids allocating a changing-element list.
/// Its base advances only past a0, independently of the coding-line color.
/// Keeping that base is necessary when a negative vertical displacement
/// leaves an earlier reference transition relevant to the next operation.
struct Reference<'a> {
    row: &'a [u8],
    width: usize,
    black_is_1: bool,
    positions: [usize; 3],
    after: bool,
}

impl<'a> Reference<'a> {
    fn new(row: &'a [u8], width: usize, black_is_1: bool) -> Option<Self> {
        let first = change(row, 0, width, false, black_is_1)?;
        let second = change(row, first, width, true, black_is_1)?;
        let third = change(row, second, width, false, black_is_1)?;
        Some(Self {
            row,
            width,
            black_is_1,
            positions: [first, second, third],
            after: true,
        })
    }

    fn pair(
        &mut self,
        x: usize,
        black: bool,
        initial: bool,
    ) -> Option<(usize, usize)> {
        if x >= self.width {
            return None;
        }
        for _ in 0..=self.width {
            let first = self.positions.first().copied()?;
            if first > x || (initial && first == x) {
                let choice = usize::from(self.after == black);
                let next = choice.checked_add(1)?;
                return Some((
                    *self.positions.get(choice)?,
                    *self.positions.get(next)?,
                ));
            }

            let [_, second, third] = self.positions;
            let next = change(
                self.row,
                third,
                self.width,
                self.after,
                self.black_is_1,
            )?;
            self.positions = [second, third, next];
            self.after = !self.after;
        }
        None
    }
}

fn change(
    row: &[u8],
    start: usize,
    width: usize,
    black: bool,
    black_is_1: bool,
) -> Option<usize> {
    if start > width {
        return None;
    }
    let mut x = start;
    while x < width {
        let byte = *row.get(x / 8)?;
        let difference = if black == black_is_1 { !byte } else { byte };
        let difference = difference & (0xffu8 >> (x % 8));
        if difference != 0 {
            let position = (x / 8)
                .checked_mul(8)?
                .checked_add(difference.leading_zeros() as usize)?;
            return Some(position.min(width));
        }
        x = x.checked_add(8usize.checked_sub(x % 8)?)?.min(width);
    }
    Some(width)
}

fn run(
    reader: &mut BitReader<'_>,
    black: bool,
    maximum: usize,
) -> Option<usize> {
    let table = if black { &BLACK_TABLE } else { &WHITE_TABLE };
    let mut length = 0usize;

    // Each makeup code adds at least 64 pixels. A terminating code is
    // mandatory even when makeup codes already reach the row boundary.
    for _ in 0..=maximum / 64 {
        let (prefix, available) = reader.peek(RUN_BITS as u8);
        let entry = *table.get(usize::from(prefix))?;
        if entry.bits == 0 || entry.bits > available {
            return None;
        }
        reader.take(entry.bits)?;
        length = length.checked_add(usize::from(entry.run))?;
        if length > maximum {
            return None;
        }
        if entry.run < 64 {
            return Some(length);
        }
    }
    None
}

#[derive(Clone, Copy)]
enum Mode {
    Invalid,
    Pass,
    Horizontal,
    Vertical(i8),
}

#[derive(Clone, Copy)]
struct ModeEntry {
    mode: Mode,
    bits: u8,
}

fn mode(reader: &mut BitReader<'_>) -> Option<Mode> {
    let (prefix, available) = reader.peek(7);
    let entry = *MODE_TABLE.get(usize::from(prefix))?;
    if entry.bits == 0 || entry.bits > available {
        return None;
    }
    reader.take(entry.bits)?;
    Some(entry.mode)
}

const MODE_CODES: &[(u8, u8, Mode)] = &[
    (0b1, 1, Mode::Vertical(0)),
    (0b011, 3, Mode::Vertical(1)),
    (0b010, 3, Mode::Vertical(-1)),
    (0b000011, 6, Mode::Vertical(2)),
    (0b000010, 6, Mode::Vertical(-2)),
    (0b0000011, 7, Mode::Vertical(3)),
    (0b0000010, 7, Mode::Vertical(-3)),
    (0b001, 3, Mode::Horizontal),
    (0b0001, 4, Mode::Pass),
];

const fn mode_table() -> [ModeEntry; 128] {
    let mut table = [ModeEntry { mode: Mode::Invalid, bits: 0 }; 128];
    let mut code_index = 0;
    while code_index < MODE_CODES.len() {
        let (code, bits, mode) = MODE_CODES[code_index];
        let shift = 7usize - bits as usize;
        let start = (code as usize) << shift;
        let end = start + (1usize << shift);
        let mut index = start;
        while index < end {
            table[index] = ModeEntry { mode, bits };
            index += 1;
        }
        code_index += 1;
    }
    table
}

const MODE_TABLE: [ModeEntry; 128] = mode_table();

#[derive(Clone)]
struct BitReader<'a> {
    remaining: &'a [u8],
    consumed: usize,
    word: u32,
    bits: u8,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            remaining: data,
            consumed: 0,
            word: 0,
            bits: 0,
        }
    }

    fn offset(&self) -> usize {
        self.consumed
            .saturating_sub(usize::from(self.bits.saturating_add(7) / 8))
    }

    fn budget(&self) -> usize {
        self.remaining
            .len()
            .saturating_mul(8)
            .saturating_add(usize::from(self.bits))
    }

    /// Pads lookahead with zeros, but separately reports actual availability.
    /// All callers check that an entry fits before consuming its bits.
    fn peek(&mut self, width: u8) -> (u16, u8) {
        if width > RUN_BITS as u8 {
            return (0, 0);
        }
        while self.bits < width {
            let Some((&byte, rest)) = self.remaining.split_first() else {
                break;
            };
            self.remaining = rest;
            self.consumed = self.consumed.saturating_add(1);
            self.word = (self.word << 8) | u32::from(byte);
            self.bits = self.bits.saturating_add(8);
        }

        let value = if self.bits >= width {
            self.word >> self.bits.saturating_sub(width)
        } else {
            self.word << width.saturating_sub(self.bits)
        };
        let mask = (1u32 << width) - 1;
        ((value & mask) as u16, self.bits.min(width))
    }

    fn take(&mut self, width: u8) -> Option<u16> {
        let (value, available) = self.peek(width);
        if available < width {
            return None;
        }
        self.bits = self.bits.checked_sub(width)?;
        self.word &= (1u32 << self.bits) - 1;
        Some(value)
    }

    fn align_zero(&mut self) -> Option<()> {
        let padding = self.bits % 8;
        if self.take(padding)? == 0 {
            Some(())
        } else {
            None
        }
    }

    fn only_padding(&self) -> bool {
        let mut scan = self.clone();
        let (value, available) = scan.peek(8);
        available < 8 && value == 0
    }

    /// Recognizes EOL with any preceding zero fill without consuming a
    /// non-EOL prefix. The scan is bounded by remaining encoded bits.
    fn eol(&mut self) -> bool {
        let mut scan = self.clone();
        let mut zeros = 0u8;
        for _ in 0..scan.budget() {
            match scan.take(1) {
                Some(0) => zeros = zeros.saturating_add(1).min(11),
                Some(1) if zeros == 11 => {
                    *self = scan;
                    return true;
                }
                _ => return false,
            }
        }
        false
    }

    fn sequence(&mut self, count: usize, tagged: bool) -> bool {
        let mut scan = self.clone();
        for index in 0..count {
            if !scan.eol() {
                return false;
            }
            if tagged {
                // Accept mixed RTC with or without its final 1-D tag.
                if index.saturating_add(1) == count {
                    let mut tail = scan.clone();
                    if tail.take(1) == Some(1) {
                        scan = tail;
                    }
                } else if scan.take(1) != Some(1) {
                    return false;
                }
            }
        }
        *self = scan;
        true
    }

    fn end_marker(&mut self, k: i32) -> bool {
        if k < 0 {
            self.sequence(2, false)
        } else if k > 0 {
            self.sequence(6, true) || self.sequence(6, false)
        } else {
            self.sequence(6, false)
        }
    }

    /// Leaves the reader before the recovered EOL so normal framing can
    /// distinguish a new line from RTC and read any mixed-mode tag.
    fn seek_eol(&mut self) -> bool {
        let mut scan = self.clone();
        let mut start = scan.clone();
        let mut zeros = 0u8;
        for _ in 0..scan.budget() {
            let before = scan.clone();
            match scan.take(1) {
                Some(0) => {
                    if zeros == 0 {
                        start = before;
                    }
                    zeros = zeros.saturating_add(1).min(11);
                }
                Some(1) if zeros == 11 => {
                    *self = start;
                    return true;
                }
                Some(1) => zeros = 0,
                _ => return false,
            }
        }
        false
    }
}

const RUN_BITS: usize = 13;
const RUN_TABLE_SIZE: usize = 1 << RUN_BITS;

#[derive(Clone, Copy)]
struct RunEntry {
    run: u16,
    bits: u8,
}

// These are the T.4 terminating and makeup codewords, represented as
// (MSB-first codeword, bit length, run length). T.6 horizontal mode uses
// the same tables. Only literal standard tables enter the const builder.
const WHITE_CODES: &[(u16, u8, u16)] = &[
    (0b00110101, 8, 0),
    (0b000111, 6, 1),
    (0b0111, 4, 2),
    (0b1000, 4, 3),
    (0b1011, 4, 4),
    (0b1100, 4, 5),
    (0b1110, 4, 6),
    (0b1111, 4, 7),
    (0b10011, 5, 8),
    (0b10100, 5, 9),
    (0b00111, 5, 10),
    (0b01000, 5, 11),
    (0b001000, 6, 12),
    (0b000011, 6, 13),
    (0b110100, 6, 14),
    (0b110101, 6, 15),
    (0b101010, 6, 16),
    (0b101011, 6, 17),
    (0b0100111, 7, 18),
    (0b0001100, 7, 19),
    (0b0001000, 7, 20),
    (0b0010111, 7, 21),
    (0b0000011, 7, 22),
    (0b0000100, 7, 23),
    (0b0101000, 7, 24),
    (0b0101011, 7, 25),
    (0b0010011, 7, 26),
    (0b0100100, 7, 27),
    (0b0011000, 7, 28),
    (0b00000010, 8, 29),
    (0b00000011, 8, 30),
    (0b00011010, 8, 31),
    (0b00011011, 8, 32),
    (0b00010010, 8, 33),
    (0b00010011, 8, 34),
    (0b00010100, 8, 35),
    (0b00010101, 8, 36),
    (0b00010110, 8, 37),
    (0b00010111, 8, 38),
    (0b00101000, 8, 39),
    (0b00101001, 8, 40),
    (0b00101010, 8, 41),
    (0b00101011, 8, 42),
    (0b00101100, 8, 43),
    (0b00101101, 8, 44),
    (0b00000100, 8, 45),
    (0b00000101, 8, 46),
    (0b00001010, 8, 47),
    (0b00001011, 8, 48),
    (0b01010010, 8, 49),
    (0b01010011, 8, 50),
    (0b01010100, 8, 51),
    (0b01010101, 8, 52),
    (0b00100100, 8, 53),
    (0b00100101, 8, 54),
    (0b01011000, 8, 55),
    (0b01011001, 8, 56),
    (0b01011010, 8, 57),
    (0b01011011, 8, 58),
    (0b01001010, 8, 59),
    (0b01001011, 8, 60),
    (0b00110010, 8, 61),
    (0b00110011, 8, 62),
    (0b00110100, 8, 63),
    (0b11011, 5, 64),
    (0b10010, 5, 128),
    (0b010111, 6, 192),
    (0b0110111, 7, 256),
    (0b00110110, 8, 320),
    (0b00110111, 8, 384),
    (0b01100100, 8, 448),
    (0b01100101, 8, 512),
    (0b01101000, 8, 576),
    (0b01100111, 8, 640),
    (0b011001100, 9, 704),
    (0b011001101, 9, 768),
    (0b011010010, 9, 832),
    (0b011010011, 9, 896),
    (0b011010100, 9, 960),
    (0b011010101, 9, 1024),
    (0b011010110, 9, 1088),
    (0b011010111, 9, 1152),
    (0b011011000, 9, 1216),
    (0b011011001, 9, 1280),
    (0b011011010, 9, 1344),
    (0b011011011, 9, 1408),
    (0b010011000, 9, 1472),
    (0b010011001, 9, 1536),
    (0b010011010, 9, 1600),
    (0b011000, 6, 1664),
    (0b010011011, 9, 1728),
];

const BLACK_CODES: &[(u16, u8, u16)] = &[
    (0b0000110111, 10, 0),
    (0b010, 3, 1),
    (0b11, 2, 2),
    (0b10, 2, 3),
    (0b011, 3, 4),
    (0b0011, 4, 5),
    (0b0010, 4, 6),
    (0b00011, 5, 7),
    (0b000101, 6, 8),
    (0b000100, 6, 9),
    (0b0000100, 7, 10),
    (0b0000101, 7, 11),
    (0b0000111, 7, 12),
    (0b00000100, 8, 13),
    (0b00000111, 8, 14),
    (0b000011000, 9, 15),
    (0b0000010111, 10, 16),
    (0b0000011000, 10, 17),
    (0b0000001000, 10, 18),
    (0b00001100111, 11, 19),
    (0b00001101000, 11, 20),
    (0b00001101100, 11, 21),
    (0b00000110111, 11, 22),
    (0b00000101000, 11, 23),
    (0b00000010111, 11, 24),
    (0b00000011000, 11, 25),
    (0b000011001010, 12, 26),
    (0b000011001011, 12, 27),
    (0b000011001100, 12, 28),
    (0b000011001101, 12, 29),
    (0b000001101000, 12, 30),
    (0b000001101001, 12, 31),
    (0b000001101010, 12, 32),
    (0b000001101011, 12, 33),
    (0b000011010010, 12, 34),
    (0b000011010011, 12, 35),
    (0b000011010100, 12, 36),
    (0b000011010101, 12, 37),
    (0b000011010110, 12, 38),
    (0b000011010111, 12, 39),
    (0b000001101100, 12, 40),
    (0b000001101101, 12, 41),
    (0b000011011010, 12, 42),
    (0b000011011011, 12, 43),
    (0b000001010100, 12, 44),
    (0b000001010101, 12, 45),
    (0b000001010110, 12, 46),
    (0b000001010111, 12, 47),
    (0b000001100100, 12, 48),
    (0b000001100101, 12, 49),
    (0b000001010010, 12, 50),
    (0b000001010011, 12, 51),
    (0b000000100100, 12, 52),
    (0b000000110111, 12, 53),
    (0b000000111000, 12, 54),
    (0b000000100111, 12, 55),
    (0b000000101000, 12, 56),
    (0b000001011000, 12, 57),
    (0b000001011001, 12, 58),
    (0b000000101011, 12, 59),
    (0b000000101100, 12, 60),
    (0b000001011010, 12, 61),
    (0b000001100110, 12, 62),
    (0b000001100111, 12, 63),
    (0b0000001111, 10, 64),
    (0b000011001000, 12, 128),
    (0b000011001001, 12, 192),
    (0b000001011011, 12, 256),
    (0b000000110011, 12, 320),
    (0b000000110100, 12, 384),
    (0b000000110101, 12, 448),
    (0b0000001101100, 13, 512),
    (0b0000001101101, 13, 576),
    (0b0000001001010, 13, 640),
    (0b0000001001011, 13, 704),
    (0b0000001001100, 13, 768),
    (0b0000001001101, 13, 832),
    (0b0000001110010, 13, 896),
    (0b0000001110011, 13, 960),
    (0b0000001110100, 13, 1024),
    (0b0000001110101, 13, 1088),
    (0b0000001110110, 13, 1152),
    (0b0000001110111, 13, 1216),
    (0b0000001010010, 13, 1280),
    (0b0000001010011, 13, 1344),
    (0b0000001010100, 13, 1408),
    (0b0000001010101, 13, 1472),
    (0b0000001011010, 13, 1536),
    (0b0000001011011, 13, 1600),
    (0b0000001100100, 13, 1664),
    (0b0000001100101, 13, 1728),
];

const COMMON_CODES: &[(u16, u8, u16)] = &[
    (0b00000001000, 11, 1792),
    (0b00000001100, 11, 1856),
    (0b00000001101, 11, 1920),
    (0b000000010010, 12, 1984),
    (0b000000010011, 12, 2048),
    (0b000000010100, 12, 2112),
    (0b000000010101, 12, 2176),
    (0b000000010110, 12, 2240),
    (0b000000010111, 12, 2304),
    (0b000000011100, 12, 2368),
    (0b000000011101, 12, 2432),
    (0b000000011110, 12, 2496),
    (0b000000011111, 12, 2560),
];

const fn run_table(codes: &[(u16, u8, u16)]) -> [RunEntry; RUN_TABLE_SIZE] {
    let mut table = [RunEntry { run: 0, bits: 0 }; RUN_TABLE_SIZE];
    let mut group = 0;
    while group < 2 {
        let source = if group == 0 { codes } else { COMMON_CODES };
        let mut code_index = 0;
        while code_index < source.len() {
            let (code, bits, run) = source[code_index];
            let shift = RUN_BITS - bits as usize;
            let start = (code as usize) << shift;
            let end = start + (1usize << shift);
            let mut index = start;
            while index < end {
                table[index] = RunEntry { run, bits };
                index += 1;
            }
            code_index += 1;
        }
        group += 1;
    }
    table
}

const WHITE_TABLE: [RunEntry; RUN_TABLE_SIZE] = run_table(WHITE_CODES);
const BLACK_TABLE: [RunEntry; RUN_TABLE_SIZE] = run_table(BLACK_CODES);
