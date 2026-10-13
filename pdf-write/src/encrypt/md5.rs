//! MD5 for the legacy PDF standard security handler.

use std::io;

use super::invalid;

const INITIAL: [u32; 4] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476];

const SHIFTS: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9,
    14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15,
    21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

const K: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
    0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
    0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
    0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
    0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
    0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
    0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
    0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
];

fn block(state: &mut [u32; 4], bytes: &[u8; 64]) {
    let mut words = [0u32; 16];
    for (index, chunk) in bytes.chunks_exact(4).enumerate() {
        words[index] = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
    }
    let [mut a, mut b, mut c, mut d] = *state;
    for index in 0..64 {
        let (f, word) = match index {
            0..=15 => ((b & c) | (!b & d), index),
            16..=31 => ((d & b) | (!d & c), (5 * index + 1) % 16),
            32..=47 => (b ^ c ^ d, (3 * index + 5) % 16),
            _ => (c ^ (b | !d), (7 * index) % 16),
        };
        let addition = a
            .wrapping_add(f)
            .wrapping_add(K[index])
            .wrapping_add(words[word])
            .rotate_left(SHIFTS[index]);
        a = d;
        d = c;
        c = b;
        b = b.wrapping_add(addition);
    }
    for (word, addition) in state.iter_mut().zip([a, b, c, d]) {
        *word = word.wrapping_add(addition);
    }
}

pub(super) fn digest(input: &[u8]) -> io::Result<[u8; 16]> {
    let bits = u64::try_from(input.len())
        .ok()
        .and_then(|length| length.checked_mul(8))
        .ok_or_else(|| invalid("MD5 input bit length exceeds u64"))?;
    let mut state = INITIAL;
    let mut chunks = input.chunks_exact(64);
    for chunk in &mut chunks {
        let bytes = <[u8; 64]>::try_from(chunk).map_err(|_| invalid("short MD5 block"))?;
        block(&mut state, &bytes);
    }
    let remainder = chunks.remainder();
    // remainder is at most 63 bytes; both possible tails fit this buffer.
    let mut tail = [0u8; 128];
    tail[..remainder.len()].copy_from_slice(remainder);
    tail[remainder.len()] = 0x80;
    let length = if remainder.len() < 56 { 64 } else { 128 };
    tail[length - 8..length].copy_from_slice(&bits.to_le_bytes());
    for chunk in tail[..length].chunks_exact(64) {
        let bytes = <[u8; 64]>::try_from(chunk).map_err(|_| invalid("short MD5 padding block"))?;
        block(&mut state, &bytes);
    }
    let mut result = [0u8; 16];
    for (target, word) in result.chunks_exact_mut(4).zip(state) {
        target.copy_from_slice(&word.to_le_bytes());
    }
    Ok(result)
}
