//! Synchronous scan encoders derived from ITU-T T.4, T.6 and T.88.
//!
//! Inputs have MSB-first, byte-padded rows, with 1 meaning black.
//! Row padding is ignored. No reader implementation is called.

use std::io;

const MAX_SCAN_PIXELS: u64 = 16 * 1024 * 1024;
const MAX_ENCODED_BYTES: usize = 64 * 1024 * 1024;
const GENERIC_CONTEXTS: usize = 65_536;
const MAX_RENORMALIZATION_SHIFTS: usize = 16;

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.to_owned())
}

fn allocation() -> io::Error {
    io::Error::new(io::ErrorKind::Other, "scan allocation failed")
}

fn add(left: usize, right: usize) -> io::Result<usize> {
    left.checked_add(right).ok_or_else(|| invalid("scan size overflow"))
}

fn append(out: &mut Vec<u8>, bytes: &[u8]) -> io::Result<()> {
    let requested = add(out.len(), bytes.len())?;
    if requested > MAX_ENCODED_BYTES {
        return Err(invalid(&format!(
            "scan_encoded_bytes requested {requested}, maximum {MAX_ENCODED_BYTES}",
        )));
    }
    out.try_reserve(bytes.len()).map_err(|_| allocation())?;
    out.extend_from_slice(bytes);
    Ok(())
}

struct Bitmap<'a> {
    data: &'a [u8],
    width: usize,
    height: usize,
    stride: usize,
}

impl<'a> Bitmap<'a> {
    fn new(data: &'a [u8], width: u32, height: u32) -> io::Result<Self> {
        if width == 0 || height == 0 {
            return Err(invalid("scan dimensions must be nonzero"));
        }
        let requested = u64::from(width).checked_mul(u64::from(height))
            .ok_or_else(|| invalid("scan pixel count overflow"))?;
        if requested > MAX_SCAN_PIXELS {
            return Err(invalid(&format!(
                "scan_pixels requested {requested}, maximum {MAX_SCAN_PIXELS}",
            )));
        }
        let width = usize::try_from(width).map_err(|_| invalid("scan width exceeds usize"))?;
        let height = usize::try_from(height).map_err(|_| invalid("scan height exceeds usize"))?;
        let stride = add(width, 7)? / 8;
        let length = stride.checked_mul(height)
            .ok_or_else(|| invalid("scan bitmap length overflow"))?;
        if data.len() != length {
            return Err(invalid("scan input must contain exactly the byte-padded rows"));
        }
        Ok(Self { data, width, height, stride })
    }

    fn pixel(&self, x: i64, y: i64) -> io::Result<u8> {
        let (Ok(x), Ok(y)) = (usize::try_from(x), usize::try_from(y)) else {
            return Ok(0);
        };
        if x >= self.width || y >= self.height {
            return Ok(0);
        }
        let offset = y.checked_mul(self.stride)
            .and_then(|value| value.checked_add(x / 8))
            .ok_or_else(|| invalid("scan pixel offset overflow"))?;
        let byte = self.data.get(offset).copied()
            .ok_or_else(|| invalid("scan pixel outside bitmap"))?;
        Ok(u8::from(byte & (0x80u8 >> (x % 8)) != 0))
    }
}

fn black(row: &[u8], x: usize) -> io::Result<bool> {
    let byte = row.get(x / 8).copied()
        .ok_or_else(|| invalid("fax pixel outside row"))?;
    Ok(byte & (0x80u8 >> (x % 8)) != 0)
}

struct Bits {
    out: Vec<u8>,
    byte: u8,
    used: u8,
}

impl Bits {
    fn new() -> Self {
        Self { out: Vec::new(), byte: 0, used: 0 }
    }

    fn bit(&mut self, value: bool) -> io::Result<()> {
        self.byte = self.byte.checked_mul(2)
            .and_then(|byte| byte.checked_add(u8::from(value)))
            .ok_or_else(|| invalid("fax bit accumulator overflow"))?;
        self.used = self.used.checked_add(1)
            .ok_or_else(|| invalid("fax bit count overflow"))?;
        if self.used == 8 {
            append(&mut self.out, &[self.byte])?;
            self.byte = 0;
            self.used = 0;
        }
        Ok(())
    }

    fn code(&mut self, code: &str) -> io::Result<()> {
        for bit in code.bytes() {
            match bit {
                b'0' => self.bit(false)?,
                b'1' => self.bit(true)?,
                _ => return Err(invalid("invalid fax code table entry")),
            }
        }
        Ok(())
    }

    fn align(&mut self) -> io::Result<()> {
        while self.used != 0 {
            self.bit(false)?;
        }
        Ok(())
    }

    fn eol(&mut self, byte_align: bool) -> io::Result<()> {
        if byte_align {
            let remainder = self.used.checked_add(12)
                .ok_or_else(|| invalid("fax EOL alignment overflow"))? % 8;
            let fill = 8u8.saturating_sub(remainder) % 8;
            for _ in 0..fill {
                self.bit(false)?;
            }
        }
        self.code("000000000001")
    }

    fn finish(mut self) -> io::Result<Vec<u8>> {
        self.align()?;
        Ok(self.out)
    }
}

// T.4 terminating codes, indexed by the run length 0 through 63.
const WHITE_TERMINATING: [&str; 64] = [
    "00110101", "000111", "0111", "1000", "1011", "1100", "1110", "1111",
    "10011", "10100", "00111", "01000", "001000", "000011", "110100", "110101",
    "101010", "101011", "0100111", "0001100", "0001000", "0010111", "0000011", "0000100",
    "0101000", "0101011", "0010011", "0100100", "0011000", "00000010", "00000011", "00011010",
    "00011011", "00010010", "00010011", "00010100", "00010101", "00010110", "00010111", "00101000",
    "00101001", "00101010", "00101011", "00101100", "00101101", "00000100", "00000101", "00001010",
    "00001011", "01010010", "01010011", "01010100", "01010101", "00100100", "00100101", "01011000",
    "01011001", "01011010", "01011011", "01001010", "01001011", "00110010", "00110011", "00110100",
];

const BLACK_TERMINATING: [&str; 64] = [
    "0000110111", "010", "11", "10", "011", "0011", "0010", "00011",
    "000101", "000100", "0000100", "0000101", "0000111", "00000100", "00000111", "000011000",
    "0000010111", "0000011000", "0000001000", "00001100111", "00001101000", "00001101100", "00000110111", "00000101000",
    "00000010111", "00000011000", "000011001010", "000011001011", "000011001100", "000011001101", "000001101000", "000001101001",
    "000001101010", "000001101011", "000011010010", "000011010011", "000011010100", "000011010101", "000011010110", "000011010111",
    "000001101100", "000001101101", "000011011010", "000011011011", "000001010100", "000001010101", "000001010110", "000001010111",
    "000001100100", "000001100101", "000001010010", "000001010011", "000000100100", "000000110111", "000000111000", "000000100111",
    "000000101000", "000001011000", "000001011001", "000000101011", "000000101100", "000001011010", "000001100110", "000001100111",
];

// Entries correspond to 64, 128, ... 1728 pixels.
const WHITE_MAKEUP: [&str; 27] = [
    "11011", "10010", "010111", "0110111", "00110110", "00110111",
    "01100100", "01100101", "01101000", "01100111", "011001100", "011001101",
    "011010010", "011010011", "011010100", "011010101", "011010110", "011010111",
    "011011000", "011011001", "011011010", "011011011", "010011000", "010011001",
    "010011010", "011000", "010011011",
];

const BLACK_MAKEUP: [&str; 27] = [
    "0000001111", "000011001000", "000011001001", "000001011011",
    "000000110011", "000000110100", "000000110101", "0000001101100",
    "0000001101101", "0000001001010", "0000001001011", "0000001001100",
    "0000001001101", "0000001110010", "0000001110011", "0000001110100",
    "0000001110101", "0000001110110", "0000001110111", "0000001010010",
    "0000001010011", "0000001010100", "0000001010101", "0000001011010",
    "0000001011011", "0000001100100", "0000001100101",
];

// Shared extended makeup codes, 1792, 1856, ... 2560 pixels.
const EXTENDED_MAKEUP: [&str; 13] = [
    "00000001000", "00000001100", "00000001101", "000000010010",
    "000000010011", "000000010100", "000000010101", "000000010110",
    "000000010111", "000000011100", "000000011101", "000000011110",
    "000000011111",
];

fn run(bits: &mut Bits, mut length: usize, is_black: bool) -> io::Result<()> {
    let makeup = if is_black { &BLACK_MAKEUP } else { &WHITE_MAKEUP };
    let terminating = if is_black { &BLACK_TERMINATING } else { &WHITE_TERMINATING };
    while length >= 64 {
        let units = (length / 64).min(40);
        let code = if units >= 28 {
            EXTENDED_MAKEUP.get(units.checked_sub(28)
                .ok_or_else(|| invalid("fax extended makeup index"))?)
        } else {
            makeup.get(units.checked_sub(1)
                .ok_or_else(|| invalid("fax makeup index"))?)
        }.copied().ok_or_else(|| invalid("fax makeup code missing"))?;
        bits.code(code)?;
        let consumed = units.checked_mul(64)
            .ok_or_else(|| invalid("fax makeup length overflow"))?;
        length = length.checked_sub(consumed)
            .ok_or_else(|| invalid("fax makeup exceeds run"))?;
    }
    let code = terminating.get(length).copied()
        .ok_or_else(|| invalid("fax terminating code missing"))?;
    bits.code(code)
}

/// T.4 one-dimensional coding for PDF K=0.
/// Set PDF Rows to height and EndOfBlock to false: no RTC is emitted.
/// With EOL and byte alignment, zero fill makes each EOL end on a byte edge.
/// Without EOL, byte alignment pads each complete row.
pub fn ccitt_g3(
    data: &[u8],
    width: u32,
    height: u32,
    end_of_line: bool,
    encoded_byte_align: bool,
) -> io::Result<Vec<u8>> {
    let bitmap = Bitmap::new(data, width, height)?;
    let mut bits = Bits::new();
    for row in bitmap.data.chunks_exact(bitmap.stride) {
        if end_of_line {
            bits.eol(encoded_byte_align)?;
        } else if encoded_byte_align {
            bits.align()?;
        }
        let mut start = 0usize;
        let mut is_black = false;
        while start < bitmap.width {
            let mut end = start;
            while end < bitmap.width && black(row, end)? == is_black {
                end = add(end, 1)?;
            }
            let length = end.checked_sub(start)
                .ok_or_else(|| invalid("fax run moves backwards"))?;
            run(&mut bits, length, is_black)?;
            start = end;
            is_black = !is_black;
        }
    }
    if end_of_line {
        bits.eol(encoded_byte_align)?;
    }
    bits.finish()
}

fn change_storage(width: usize) -> io::Result<Vec<usize>> {
    let count = add(width, 2)?;
    let mut changes = Vec::new();
    changes.try_reserve_exact(count).map_err(|_| allocation())?;
    Ok(changes)
}

fn changes(row: &[u8], width: usize, out: &mut Vec<usize>) -> io::Result<()> {
    out.clear();
    let mut previous = false;
    for x in 0..width {
        let current = black(row, x)?;
        if current != previous {
            out.push(x);
            previous = current;
        }
    }
    // Two alternating imaginary transitions delimit the right edge.
    // Capacity was reserved for at most width real transitions and these two.
    out.push(width);
    out.push(width);
    Ok(())
}

fn position(changes: &[usize], index: usize) -> io::Result<usize> {
    changes.get(index).copied().ok_or_else(|| invalid("fax transition missing"))
}

fn vertical(delta: i64) -> io::Result<&'static str> {
    match delta {
        -3 => Ok("0000010"),
        -2 => Ok("000010"),
        -1 => Ok("010"),
        0 => Ok("1"),
        1 => Ok("011"),
        2 => Ok("000011"),
        3 => Ok("0000011"),
        _ => Err(invalid("fax vertical displacement outside code table")),
    }
}

/// T.6 Group 4 coding for PDF K=-1, without row alignment.
/// The initial reference row is white; EOFB is optional when Rows delimits data.
pub fn ccitt_g4(
    data: &[u8],
    width: u32,
    height: u32,
    eofb: bool,
) -> io::Result<Vec<u8>> {
    let bitmap = Bitmap::new(data, width, height)?;
    let mut current = change_storage(bitmap.width)?;
    let mut reference = change_storage(bitmap.width)?;
    reference.push(bitmap.width);
    reference.push(bitmap.width);
    let mut bits = Bits::new();
    for row in bitmap.data.chunks_exact(bitmap.stride) {
        changes(row, bitmap.width, &mut current)?;
        let mut a0 = 0usize;
        let mut a_index = 0usize;
        let mut b_base = 0usize;
        let mut is_black = false;
        let mut initial = true;
        while a0 < bitmap.width {
            let a1 = position(&current, a_index)?;
            while {
                let value = position(&reference, b_base)?;
                value < a0 || (value == a0 && !initial)
            } {
                b_base = add(b_base, 1)?;
            }
            // Even transitions enter black; odd transitions enter white.
            let b_index = if b_base % 2 == usize::from(is_black) {
                b_base
            } else {
                add(b_base, 1)?
            };
            let b1 = position(&reference, b_index)?;
            let b2 = position(&reference, add(b_index, 1)?)?;
            if b2 < a1 {
                if b2 <= a0 {
                    return Err(invalid("fax pass does not advance"));
                }
                bits.code("0001")?;
                a0 = b2;
            } else {
                let a = i64::try_from(a1).map_err(|_| invalid("fax transition exceeds i64"))?;
                let b = i64::try_from(b1).map_err(|_| invalid("fax transition exceeds i64"))?;
                let delta = a.checked_sub(b)
                    .ok_or_else(|| invalid("fax displacement overflow"))?;
                if (-3..=3).contains(&delta) {
                    if a1 < a0 {
                        return Err(invalid("fax vertical moves backwards"));
                    }
                    bits.code(vertical(delta)?)?;
                    a0 = a1;
                    a_index = add(a_index, 1)?;
                    is_black = !is_black;
                } else {
                    let a2 = position(&current, add(a_index, 1)?)?;
                    if a2 <= a0 {
                        return Err(invalid("fax horizontal does not advance"));
                    }
                    bits.code("001")?;
                    run(&mut bits, a1.checked_sub(a0)
                        .ok_or_else(|| invalid("fax first run moves backwards"))?, is_black)?;
                    run(&mut bits, a2.checked_sub(a1)
                        .ok_or_else(|| invalid("fax second run moves backwards"))?, !is_black)?;
                    a0 = a2;
                    a_index = add(a_index, 2)?;
                }
            }
            initial = false;
        }
        std::mem::swap(&mut current, &mut reference);
    }
    if eofb {
        bits.eol(false)?;
        bits.eol(false)?;
    }
    bits.finish()
}

// T.88 Table E.1: Qe, NMPS, NLPS, SWITCH.
const MQ_STATES: [(u16, u8, u8, bool); 47] = [
    (0x5601, 1, 1, true),
    (0x3401, 2, 6, false),
    (0x1801, 3, 9, false),
    (0x0ac1, 4, 12, false),
    (0x0521, 5, 29, false),
    (0x0221, 38, 33, false),
    (0x5601, 7, 6, true),
    (0x5401, 8, 14, false),
    (0x4801, 9, 14, false),
    (0x3801, 10, 14, false),
    (0x3001, 11, 17, false),
    (0x2401, 12, 18, false),
    (0x1c01, 13, 20, false),
    (0x1601, 29, 21, false),
    (0x5601, 15, 14, true),
    (0x5401, 16, 14, false),
    (0x5101, 17, 15, false),
    (0x4801, 18, 16, false),
    (0x3801, 19, 17, false),
    (0x3401, 20, 18, false),
    (0x3001, 21, 19, false),
    (0x2801, 22, 19, false),
    (0x2401, 23, 20, false),
    (0x2201, 24, 21, false),
    (0x1c01, 25, 22, false),
    (0x1801, 26, 23, false),
    (0x1601, 27, 24, false),
    (0x1401, 28, 25, false),
    (0x1201, 29, 26, false),
    (0x1101, 30, 27, false),
    (0x0ac1, 31, 28, false),
    (0x09c1, 32, 29, false),
    (0x08a1, 33, 30, false),
    (0x0521, 34, 31, false),
    (0x0441, 35, 32, false),
    (0x02a1, 36, 33, false),
    (0x0221, 37, 34, false),
    (0x0141, 38, 35, false),
    (0x0111, 39, 36, false),
    (0x0085, 40, 37, false),
    (0x0049, 41, 38, false),
    (0x0025, 42, 39, false),
    (0x0015, 43, 40, false),
    (0x0009, 44, 41, false),
    (0x0005, 45, 42, false),
    (0x0001, 45, 43, false),
    (0x5601, 46, 46, false),
];

struct Mq {
    a: u32,
    c: u32,
    ct: u8,
    pending: u8,
    live: bool,
    out: Vec<u8>,
}

impl Mq {
    fn new() -> Self {
        // The virtual byte before BPST is zero and is never emitted.
        Self {
            a: 0x8000, c: 0, ct: 12, pending: 0,
            live: false, out: Vec::new(),
        }
    }

    fn byte_out(&mut self) -> io::Result<()> {
        let stuffed = if self.pending == 0xff {
            true
        } else if self.c & 0x0800_0000 != 0 {
            self.pending = self.pending.checked_add(1)
                .ok_or_else(|| invalid("MQ carry overflow"))?;
            self.c &= 0x07ff_ffff;
            self.pending == 0xff
        } else {
            false
        };
        if self.live {
            append(&mut self.out, &[self.pending])?;
        }
        self.live = true;
        let shift = if stuffed { 20 } else { 19 };
        self.pending = u8::try_from(self.c >> shift)
            .map_err(|_| invalid("MQ output byte overflow"))?;
        self.c &= if stuffed { 0x000f_ffff } else { 0x0007_ffff };
        self.ct = if stuffed { 7 } else { 8 };
        Ok(())
    }

    fn renormalize(&mut self) -> io::Result<()> {
        for _ in 0..MAX_RENORMALIZATION_SHIFTS {
            if self.a & 0x8000 != 0 {
                return Ok(());
            }
            self.a = self.a.checked_mul(2)
                .ok_or_else(|| invalid("MQ interval overflow"))?;
            // C is a modular coding register, not an input size or offset.
            self.c = self.c.wrapping_shl(1);
            self.ct = self.ct.checked_sub(1)
                .ok_or_else(|| invalid("MQ bit counter underflow"))?;
            if self.ct == 0 {
                self.byte_out()?;
            }
        }
        if self.a & 0x8000 != 0 {
            Ok(())
        } else {
            Err(invalid("MQ interval failed to renormalize"))
        }
    }

    fn bit(&mut self, context: &mut u8, decision: u8) -> io::Result<()> {
        if decision > 1 {
            return Err(invalid("MQ decision must be binary"));
        }
        let index = usize::from(*context >> 1);
        let mps = *context & 1;
        let (qe, nmps, nlps, switch) = MQ_STATES.get(index).copied()
            .ok_or_else(|| invalid("MQ probability state outside table"))?;
        let qe = u32::from(qe);
        self.a = self.a.checked_sub(qe)
            .ok_or_else(|| invalid("MQ interval underflow"))?;
        if decision == mps {
            if self.a & 0x8000 != 0 {
                self.c = self.c.wrapping_add(qe);
                return Ok(());
            }
            if self.a < qe {
                self.a = qe;
            } else {
                self.c = self.c.wrapping_add(qe);
            }
            *context = (nmps << 1) | mps;
        } else {
            if self.a < qe {
                self.c = self.c.wrapping_add(qe);
            } else {
                self.a = qe;
            }
            let next_mps = if switch { mps ^ 1 } else { mps };
            *context = (nlps << 1) | next_mps;
        }
        self.renormalize()
    }

    fn finish(mut self) -> io::Result<Vec<u8>> {
        // SETBITS, then the two BYTEOUT operations of Annex E FLUSH.
        let upper = self.c.checked_add(self.a)
            .ok_or_else(|| invalid("MQ termination interval overflow"))?;
        self.c |= 0xffff;
        if self.c >= upper {
            self.c = self.c.checked_sub(0x8000)
                .ok_or_else(|| invalid("MQ termination interval underflow"))?;
        }
        self.c = self.c.wrapping_shl(u32::from(self.ct));
        self.byte_out()?;
        self.c = self.c.wrapping_shl(u32::from(self.ct));
        self.byte_out()?;
        append(&mut self.out, &[self.pending])?;
        if self.pending != 0xff {
            append(&mut self.out, &[0xff])?;
        }
        append(&mut self.out, &[0xac])?;
        Ok(self.out)
    }
}

// T.88 template 0 with its nominal adaptive pixels, in context bit order.
const TEMPLATE_0: [(i64, i64); 16] = [
    (-1, 0), (-2, 0), (-3, 0), (-4, 0),
    (3, -1), (2, -1), (1, -1), (0, -1),
    (-1, -1), (-2, -1), (-3, -1),
    (2, -2), (1, -2), (0, -2), (-1, -2), (-2, -2),
];

fn generic(bitmap: &Bitmap<'_>) -> io::Result<Vec<u8>> {
    let mut contexts = Vec::new();
    contexts.try_reserve_exact(GENERIC_CONTEXTS).map_err(|_| allocation())?;
    contexts.resize(GENERIC_CONTEXTS, 0u8);
    let mut mq = Mq::new();
    for y in 0..bitmap.height {
        let y = i64::try_from(y).map_err(|_| invalid("JBIG2 y exceeds i64"))?;
        for x in 0..bitmap.width {
            let x = i64::try_from(x).map_err(|_| invalid("JBIG2 x exceeds i64"))?;
            let mut index = 0usize;
            for (bit, (dx, dy)) in TEMPLATE_0.iter().copied().enumerate() {
                let sx = x.checked_add(dx).ok_or_else(|| invalid("JBIG2 context x overflow"))?;
                let sy = y.checked_add(dy).ok_or_else(|| invalid("JBIG2 context y overflow"))?;
                let shift = u32::try_from(bit).map_err(|_| invalid("JBIG2 context bit"))?;
                let sample = usize::from(bitmap.pixel(sx, sy)?).checked_shl(shift)
                    .ok_or_else(|| invalid("JBIG2 context shift overflow"))?;
                index |= sample;
            }
            let context = contexts.get_mut(index)
                .ok_or_else(|| invalid("JBIG2 context outside table"))?;
            mq.bit(context, bitmap.pixel(x, y)?)?;
        }
    }
    mq.finish()
}

fn number(out: &mut Vec<u8>, value: u32) -> io::Result<()> {
    append(out, &value.to_be_bytes())
}

fn segment(
    out: &mut Vec<u8>,
    id: u32,
    kind: u8,
    payload: &[u8],
) -> io::Result<()> {
    let length = u32::try_from(payload.len())
        .map_err(|_| invalid("JBIG2 segment length exceeds u32"))?;
    number(out, id)?;
    // Short page association 1, zero references, no retention or deferred flag.
    append(out, &[kind, 0, 1])?;
    number(out, length)?;
    append(out, payload)
}

/// Embedded sequential JBIG2: page information, an immediate lossless generic
/// region, and end of page. No file header, globals or end-of-file segment.
/// GBTEMPLATE=0, TPGDON=0, MMR=0, and nominal AT locations are explicit.
pub fn jbig2(data: &[u8], width: u32, height: u32) -> io::Result<Vec<u8>> {
    let bitmap = Bitmap::new(data, width, height)?;
    let arithmetic = generic(&bitmap)?;
    let mut page = Vec::new();
    number(&mut page, width)?;
    number(&mut page, height)?;
    number(&mut page, 0)?; // Unknown horizontal resolution.
    number(&mut page, 0)?; // Unknown vertical resolution.
    append(&mut page, &[1, 0, 0])?; // Lossless, white default, OR, not striped.

    let mut region = Vec::new();
    number(&mut region, width)?;
    number(&mut region, height)?;
    number(&mut region, 0)?; // x
    number(&mut region, 0)?; // y
    append(&mut region, &[0, 0])?; // OR operator; generic region flags.
    append(&mut region, &[3, 0xff, 0xfd, 0xff, 2, 0xfe, 0xfe, 0xfe])?;
    append(&mut region, &arithmetic)?;

    let mut out = Vec::new();
    segment(&mut out, 0, 48, &page)?;
    segment(&mut out, 1, 39, &region)?;
    segment(&mut out, 2, 49, &[])?;
    Ok(out)
}

