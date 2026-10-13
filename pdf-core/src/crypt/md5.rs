const SHIFTS: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9,
    14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15,
    21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

const CONSTANTS: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
    0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
    0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
    0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
    0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
    0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
    0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
    0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
];

pub(super) struct Md5 {
    state: [u32; 4],
    buffer: [u8; 64],
    used: usize,
    length: u64,
}

impl Md5 {
    pub(super) fn new() -> Self {
        Self {
            state: [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476],
            buffer: [0; 64],
            used: 0,
            length: 0,
        }
    }

    pub(super) fn update(&mut self, mut bytes: &[u8]) {
        self.length = self.length.wrapping_add(bytes.len() as u64);
        while !bytes.is_empty() {
            let take = (64 - self.used).min(bytes.len());
            let (head, tail) = bytes.split_at(take);
            self.buffer[self.used..self.used + take].copy_from_slice(head);
            self.used += take;
            bytes = tail;
            if self.used == 64 {
                compress(&mut self.state, &self.buffer);
                self.used = 0;
            }
        }
    }

    pub(super) fn finish(mut self) -> [u8; 16] {
        let bits = self.length.wrapping_mul(8);
        self.buffer[self.used] = 0x80;
        self.used += 1;
        self.buffer[self.used..].fill(0);
        if self.used > 56 {
            compress(&mut self.state, &self.buffer);
            self.buffer.fill(0);
        }
        self.buffer[56..].copy_from_slice(&bits.to_le_bytes());
        compress(&mut self.state, &self.buffer);
        let mut result = [0u8; 16];
        for (word, slot) in self.state.iter().zip(result.as_chunks_mut::<4>().0) {
            *slot = word.to_le_bytes();
        }
        result
    }
}

pub(super) fn digest(bytes: &[u8]) -> [u8; 16] {
    let mut hash = Md5::new();
    hash.update(bytes);
    hash.finish()
}

fn compress(state: &mut [u32; 4], block: &[u8; 64]) {
    let mut words = [0u32; 16];
    for (slot, bytes) in words.iter_mut().zip(block.as_chunks::<4>().0) {
        *slot = u32::from_le_bytes(*bytes);
    }
    let [mut a, mut b, mut c, mut d] = *state;
    for index in 0..64 {
        let (function, word) = match index {
            0..=15 => ((b & c) | (!b & d), index),
            16..=31 => ((d & b) | (!d & c), (5 * index + 1) % 16),
            32..=47 => (b ^ c ^ d, (3 * index + 5) % 16),
            _ => (c ^ (b | !d), (7 * index) % 16),
        };
        let next = b.wrapping_add(
            a.wrapping_add(function)
                .wrapping_add(CONSTANTS[index])
                .wrapping_add(words[word])
                .rotate_left(SHIFTS[index]),
        );
        a = d;
        d = c;
        c = b;
        b = next;
    }
    for (slot, word) in state.iter_mut().zip([a, b, c, d]) {
        *slot = slot.wrapping_add(word);
    }
}
