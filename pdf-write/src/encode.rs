//! Fixture encoders, independent of the reader's filter implementations.

use std::collections::BTreeMap;
use std::io;

#[path = "encode_scan.rs"]
pub mod scan;

pub fn flate(input: &[u8]) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec_zlib(input, 6)
}

pub fn ascii_hex(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for byte in input {
        out.extend_from_slice(format!("{byte:02X}").as_bytes());
    }
    out.push(b'>');
    out
}

/// PDF ASCII85 data has no opening <~ marker.
pub fn ascii85(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for chunk in input.chunks(4) {
        let mut padded = [0u8; 4];
        for (target, source) in padded.iter_mut().zip(chunk) {
            *target = *source;
        }
        let mut word = u32::from_be_bytes(padded);
        if chunk.len() == 4 && word == 0 {
            out.push(b'z');
            continue;
        }
        let mut digits = [0u8; 5];
        for digit in digits.iter_mut().rev() {
            *digit = (word % 85) as u8 + b'!';
            word /= 85;
        }
        out.extend(digits.iter().take(chunk.len() + 1).copied());
    }
    out.extend_from_slice(b"~>");
    out
}

fn repeat_at(input: &[u8], position: usize) -> usize {
    let Some(tail) = input.get(position..) else {
        return 0;
    };
    let Some(first) = tail.first() else {
        return 0;
    };
    tail.iter()
        .take(128)
        .take_while(|byte| *byte == first)
        .count()
}

pub fn run_length(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut position = 0usize;
    while let Some(byte) = input.get(position).copied() {
        let run = repeat_at(input, position);
        if run >= 3 {
            out.push((257 - run) as u8);
            out.push(byte);
            position = position.saturating_add(run);
            continue;
        }
        let mut end = position.saturating_add(1);
        while end < input.len() && end - position < 128 && repeat_at(input, end) < 3 {
            end = end.saturating_add(1);
        }
        if let Some(literal) = input.get(position..end) {
            out.push((literal.len() - 1) as u8);
            out.extend_from_slice(literal);
        }
        position = end;
    }
    out.push(128);
    out
}

/// PNG predictor 12, with an explicit Up filter byte before every complete row.
/// Padding belongs to the authored plain content, rather than to this encoder.
pub fn png_up(input: &[u8], columns: usize) -> io::Result<Vec<u8>> {
    if columns == 0 || input.len() % columns != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "PNG Up input must consist of complete nonempty-width rows",
        ));
    }
    let mut previous = vec![0u8; columns];
    let mut out = Vec::new();
    for row in input.chunks_exact(columns) {
        out.push(2);
        for (current, prior) in row.iter().zip(&previous) {
            out.push(current.wrapping_sub(*prior));
        }
        previous.copy_from_slice(row);
    }
    Ok(out)
}

struct LzwBits {
    out: Vec<u8>,
    accumulator: u32,
    bits: u8,
    width: u8,
    decoder_next: u16,
    has_previous: bool,
}

impl LzwBits {
    fn new() -> Self {
        Self {
            out: Vec::new(),
            accumulator: 0,
            bits: 0,
            width: 9,
            decoder_next: 258,
            has_previous: false,
        }
    }

    fn emit(&mut self, code: u16) {
        self.accumulator = (self.accumulator << self.width) | u32::from(code);
        self.bits += self.width;
        while self.bits >= 8 {
            self.bits -= 8;
            self.out.push((self.accumulator >> self.bits) as u8);
        }
        self.accumulator &= (1u32 << self.bits) - 1;

        if code == 256 {
            self.width = 9;
            self.decoder_next = 258;
            self.has_previous = false;
        } else if code != 257 {
            if self.has_previous && self.decoder_next < 4096 {
                self.decoder_next += 1;
                if self.width < 12 && self.decoder_next + 1 == (1u16 << self.width) {
                    self.width += 1;
                }
            }
            self.has_previous = true;
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.bits != 0 {
            self.out.push((self.accumulator << (8 - self.bits)) as u8);
        }
        self.out
    }
}

/// PDF LZW with EarlyChange=1, clear/EOD codes and a 4096-entry dictionary.
/// Width changes track the decoder's insertion count, including before EOD.
pub fn lzw(input: &[u8]) -> Vec<u8> {
    let mut bits = LzwBits::new();
    bits.emit(256);
    let mut input = input.iter().copied();
    let Some(first) = input.next() else {
        bits.emit(257);
        return bits.finish();
    };
    let mut dictionary = BTreeMap::<(u16, u8), u16>::new();
    let mut next = 258u16;
    let mut prefix = u16::from(first);
    for byte in input {
        if let Some(code) = dictionary.get(&(prefix, byte)).copied() {
            prefix = code;
            continue;
        }
        bits.emit(prefix);
        if next < 4096 {
            dictionary.insert((prefix, byte), next);
            next += 1;
        } else {
            bits.emit(256);
            dictionary.clear();
            next = 258;
        }
        prefix = u16::from(byte);
    }
    bits.emit(prefix);
    bits.emit(257);
    bits.finish()
}
