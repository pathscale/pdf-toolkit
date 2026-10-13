use std::fmt::Write as _;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ContainerKind {
    Object,
    Array,
}

#[derive(Clone, Copy, Debug)]
struct ContainerFrame {
    kind: ContainerKind,
    count: usize,
    pending_key: bool,
}

/// Writes JSON with fixed indentation and stable number formatting.
pub struct JsonWriter {
    output: String,
    stack: Vec<ContainerFrame>,
    root_written: bool,
}

impl JsonWriter {
    pub fn new() -> Self {
        Self {
            output: String::new(),
            stack: Vec::new(),
            root_written: false,
        }
    }

    pub fn begin_object(&mut self) {
        self.before_value();
        self.output.push('{');
        self.stack.push(ContainerFrame {
            kind: ContainerKind::Object,
            count: 0,
            pending_key: false,
        });
    }

    pub fn end_object(&mut self) {
        self.end_container(ContainerKind::Object, '}');
    }

    pub fn begin_array(&mut self) {
        self.before_value();
        self.output.push('[');
        self.stack.push(ContainerFrame {
            kind: ContainerKind::Array,
            count: 0,
            pending_key: false,
        });
    }

    pub fn end_array(&mut self) {
        self.end_container(ContainerKind::Array, ']');
    }

    pub fn member(&mut self, key: &str) {
        let depth = self.stack.len();
        let Some(frame) = self.stack.last_mut() else {
            return;
        };
        if frame.kind != ContainerKind::Object || frame.pending_key {
            return;
        }

        let count = frame.count;
        frame.count = frame.count.saturating_add(1);
        frame.pending_key = true;

        if count > 0 {
            self.output.push(',');
        }
        self.output.push('\n');
        indent(&mut self.output, depth);
        write_quoted(&mut self.output, key.as_bytes());
        self.output.push_str(": ");
    }

    pub fn string(&mut self, value: &str) {
        self.before_value();
        write_quoted(&mut self.output, value.as_bytes());
    }

    pub fn bytes_string(&mut self, value: &[u8]) {
        self.before_value();
        write_quoted(&mut self.output, value);
    }

    pub fn number(&mut self, value: u64) {
        self.before_value();
        let _ = write!(self.output, "{value}");
    }

    pub fn float(&mut self, value: f64) {
        if !value.is_finite() {
            self.null();
            return;
        }
        self.before_value();
        self.output.push_str(&format_number(value));
    }

    pub fn boolean(&mut self, value: bool) {
        self.before_value();
        if value {
            self.output.push_str("true");
        } else {
            self.output.push_str("false");
        }
    }

    pub fn null(&mut self) {
        self.before_value();
        self.output.push_str("null");
    }

    pub fn finish(mut self) -> String {
        if !self.root_written {
            self.output.push_str("null");
        }
        if !self.output.ends_with('\n') {
            self.output.push('\n');
        }
        self.output
    }

    fn before_value(&mut self) {
        let depth = self.stack.len();
        let mut array_index = None;

        if let Some(frame) = self.stack.last_mut() {
            match frame.kind {
                ContainerKind::Object => {
                    if frame.pending_key {
                        frame.pending_key = false;
                    }
                }
                ContainerKind::Array => {
                    array_index = Some(frame.count);
                    frame.count = frame.count.saturating_add(1);
                }
            }
        } else if !self.root_written {
            self.root_written = true;
        }

        if let Some(index) = array_index {
            if index > 0 {
                self.output.push(',');
            }
            self.output.push('\n');
            indent(&mut self.output, depth);
        }
    }

    fn end_container(&mut self, expected: ContainerKind, close: char) {
        let Some(frame) = self.stack.pop() else {
            return;
        };
        if frame.kind != expected {
            self.stack.push(frame);
            return;
        }
        if frame.count > 0 {
            self.output.push('\n');
            indent(&mut self.output, self.stack.len());
        }
        self.output.push(close);
    }
}

impl Default for JsonWriter {
    fn default() -> Self {
        Self::new()
    }
}

fn indent(output: &mut String, depth: usize) {
    for _ in 0..depth.saturating_mul(2) {
        output.push(' ');
    }
}

fn write_quoted(output: &mut String, bytes: &[u8]) {
    output.push('"');
    if let Ok(text) = std::str::from_utf8(bytes) {
        for character in text.chars() {
            match character {
                '"' => output.push_str("\\\""),
                '\\' => output.push_str("\\\\"),
                '\u{08}' => output.push_str("\\b"),
                '\u{0c}' => output.push_str("\\f"),
                '\n' => output.push_str("\\n"),
                '\r' => output.push_str("\\r"),
                '\t' => output.push_str("\\t"),
                character if character <= '\u{1f}' => {
                    let byte = character as u8;
                    output.push_str("\\u00");
                    output.push(hex_digit(byte >> 4));
                    output.push(hex_digit(byte & 0x0f));
                }
                character => output.push(character),
            }
        }
    } else {
        for byte in bytes.iter().copied() {
            match byte {
                b'"' => output.push_str("\\\""),
                b'\\' => output.push_str("\\\\"),
                b'\x08' => output.push_str("\\b"),
                b'\x0c' => output.push_str("\\f"),
                b'\n' => output.push_str("\\n"),
                b'\r' => output.push_str("\\r"),
                b'\t' => output.push_str("\\t"),
                0x00..=0x1f | 0x80..=0xff => {
                    output.push_str("\\u00");
                    output.push(hex_digit(byte >> 4));
                    output.push(hex_digit(byte & 0x0f));
                }
                _ => output.push(byte as char),
            }
        }
    }
    output.push('"');
}

fn hex_digit(value: u8) -> char {
    match value {
        0..=9 => (b'0' + value) as char,
        10..=15 => (b'a' + value - 10) as char,
        _ => '0',
    }
}

/// Formats a number to at most three decimal places and removes trailing zeroes.
pub fn format_number(value: f64) -> String {
    if !value.is_finite() || value == 0.0 {
        return "0".to_string();
    }
    let mut text = format!("{value:.3}");
    while text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.pop();
    }
    if text == "-0" {
        return "0".to_string();
    }
    text
}

/// Incremental SHA-256 implementation for content stream concatenation.
pub struct Sha256 {
    state: [u32; 8],
    buffer: [u8; 64],
    buffer_len: usize,
    total_len: u64,
}

impl Sha256 {
    pub fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            buffer: [0; 64],
            buffer_len: 0,
            total_len: 0,
        }
    }

    pub fn update(&mut self, mut bytes: &[u8]) {
        self.total_len = self.total_len.wrapping_add(bytes.len() as u64);

        if self.buffer_len > 0 {
            let available = 64usize.saturating_sub(self.buffer_len);
            let copied = available.min(bytes.len());
            let end = self.buffer_len.saturating_add(copied);
            self.buffer[self.buffer_len..end].copy_from_slice(&bytes[..copied]);
            self.buffer_len = end;
            bytes = &bytes[copied..];
            if self.buffer_len == 64 {
                compress_block(&mut self.state, &self.buffer);
                self.buffer = [0; 64];
                self.buffer_len = 0;
            }
        }

        while bytes.len() >= 64 {
            let (block, rest) = bytes.split_at(64);
            compress_block(&mut self.state, block);
            bytes = rest;
        }

        if !bytes.is_empty() {
            self.buffer[..bytes.len()].copy_from_slice(bytes);
            self.buffer_len = bytes.len();
        }
    }

    pub fn finalize(mut self) -> [u8; 32] {
        let bit_length = self.total_len.wrapping_mul(8);

        if self.buffer_len < 64 {
            self.buffer[self.buffer_len] = 0x80;
            self.buffer_len = self.buffer_len.saturating_add(1);
        }

        if self.buffer_len > 56 {
            self.buffer[self.buffer_len..].fill(0);
            compress_block(&mut self.state, &self.buffer);
            self.buffer = [0; 64];
            self.buffer_len = 0;
        }

        self.buffer[self.buffer_len..56].fill(0);
        self.buffer[56..64].copy_from_slice(&bit_length.to_be_bytes());
        compress_block(&mut self.state, &self.buffer);

        let mut digest = [0u8; 32];
        for (index, word) in self.state.iter().copied().enumerate() {
            let start = index.saturating_mul(4);
            digest[start..start + 4].copy_from_slice(&word.to_be_bytes());
        }
        digest
    }
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    digest_hex(&hasher.finalize())
}

pub fn digest_hex(digest: &[u8; 32]) -> String {
    let mut output = String::with_capacity(64);
    for byte in digest.iter().copied() {
        output.push(hex_digit(byte >> 4));
        output.push(hex_digit(byte & 0x0f));
    }
    output
}

fn compress_block(state: &mut [u32; 8], block: &[u8]) {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let mut words = [0u32; 64];
    for (index, word) in words.iter_mut().take(16).enumerate() {
        let start = index.saturating_mul(4);
        if let Some(chunk) = block.get(start..start + 4) {
            if let (Some(a), Some(b), Some(c), Some(d)) =
                (chunk.first(), chunk.get(1), chunk.get(2), chunk.get(3))
            {
                *word = u32::from_be_bytes([*a, *b, *c, *d]);
            }
        }
    }

    for index in 16..64 {
        let x = words[index - 15];
        let y = words[index - 2];
        let s0 = x.rotate_right(7) ^ x.rotate_right(18) ^ (x >> 3);
        let s1 = y.rotate_right(17) ^ y.rotate_right(19) ^ (y >> 10);
        words[index] = words[index - 16]
            .wrapping_add(s0)
            .wrapping_add(words[index - 7])
            .wrapping_add(s1);
    }

    let mut a = state[0];
    let mut b = state[1];
    let mut c = state[2];
    let mut d = state[3];
    let mut e = state[4];
    let mut f = state[5];
    let mut g = state[6];
    let mut h = state[7];

    for index in 0..64 {
        let sigma1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let choose = (e & f) ^ (!e & g);
        let first = h
            .wrapping_add(sigma1)
            .wrapping_add(choose)
            .wrapping_add(K[index])
            .wrapping_add(words[index]);
        let sigma0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let majority = (a & b) ^ (a & c) ^ (b & c);
        let second = sigma0.wrapping_add(majority);

        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(first);
        d = c;
        c = b;
        b = a;
        a = first.wrapping_add(second);
    }

    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
    state[4] = state[4].wrapping_add(e);
    state[5] = state[5].wrapping_add(f);
    state[6] = state[6].wrapping_add(g);
    state[7] = state[7].wrapping_add(h);
}
