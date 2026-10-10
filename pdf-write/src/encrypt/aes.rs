//! AES-128 and AES-256 encryption, with raw CBC and single-block ECB.
//! Padding and PDF IV prefixes are handled by the caller.

use std::io;

use super::invalid;

const fn xtime(value: u8) -> u8 {
    (value << 1) ^ if value & 0x80 != 0 { 0x1b } else { 0 }
}

const fn multiply(mut a: u8, mut b: u8) -> u8 {
    let mut result = 0u8;
    let mut bit = 0;
    while bit < 8 {
        if b & 1 != 0 {
            result ^= a;
        }
        a = xtime(a);
        b >>= 1;
        bit += 1;
    }
    result
}

const fn make_sbox() -> [u8; 256] {
    let mut table = [0u8; 256];
    let mut index = 0;
    while index < 256 {
        let mut power = 254u8;
        let mut base = index as u8;
        let mut inverse = 1u8;
        while power != 0 {
            if power & 1 != 0 {
                inverse = multiply(inverse, base);
            }
            base = multiply(base, base);
            power >>= 1;
        }
        table[index] = inverse
            ^ inverse.rotate_left(1)
            ^ inverse.rotate_left(2)
            ^ inverse.rotate_left(3)
            ^ inverse.rotate_left(4)
            ^ 0x63;
        index += 1;
    }
    table
}

const SBOX: [u8; 256] = make_sbox();

fn sub_word(word: u32) -> u32 {
    u32::from_be_bytes(word.to_be_bytes().map(|byte| SBOX[usize::from(byte)]))
}

struct Cipher {
    words: [u32; 60],
    rounds: usize,
}

impl Cipher {
    fn new(key: &[u8]) -> io::Result<Self> {
        let (nk, rounds) = match key.len() {
            16 => (4usize, 10usize),
            32 => (8usize, 14usize),
            _ => return Err(invalid("AES key must contain 16 or 32 bytes")),
        };
        let mut words = [0u32; 60];
        for (index, chunk) in key.chunks_exact(4).enumerate() {
            let bytes = <[u8; 4]>::try_from(chunk)
                .map_err(|_| invalid("short AES key word"))?;
            words[index] = u32::from_be_bytes(bytes);
        }
        // nk, rounds and every schedule index are bounded by the two
        // validated key lengths above.
        let count = 4 * (rounds + 1);
        let mut rcon = 1u8;
        for index in nk..count {
            let mut previous = words[index - 1];
            if index % nk == 0 {
                previous = sub_word(previous.rotate_left(8))
                    ^ (u32::from(rcon) << 24);
                rcon = xtime(rcon);
            } else if nk == 8 && index % nk == 4 {
                previous = sub_word(previous);
            }
            words[index] = words[index - nk] ^ previous;
        }
        Ok(Self { words, rounds })
    }

    fn add_key(&self, state: &mut [u8; 16], round: usize) {
        for (column, word) in self.words.iter().skip(round * 4).take(4).enumerate() {
            for (row, byte) in word.to_be_bytes().into_iter().enumerate() {
                state[column * 4 + row] ^= byte;
            }
        }
    }

    fn block(&self, mut state: [u8; 16]) -> [u8; 16] {
        self.add_key(&mut state, 0);
        for round in 1..=self.rounds {
            let previous = state;
            for column in 0..4 {
                for row in 0..4 {
                    let source = previous[((column + row) % 4) * 4 + row];
                    state[column * 4 + row] = SBOX[usize::from(source)];
                }
            }
            if round != self.rounds {
                for column in state.chunks_exact_mut(4) {
                    // chunks_exact_mut guarantees four bytes here.
                    let a = column[0];
                    let b = column[1];
                    let c = column[2];
                    let d = column[3];
                    let all = a ^ b ^ c ^ d;
                    column[0] = a ^ all ^ xtime(a ^ b);
                    column[1] = b ^ all ^ xtime(b ^ c);
                    column[2] = c ^ all ^ xtime(c ^ d);
                    column[3] = d ^ all ^ xtime(d ^ a);
                }
            }
            self.add_key(&mut state, round);
        }
        state
    }
}

pub(super) fn ecb(key: &[u8], block: &[u8; 16]) -> io::Result<[u8; 16]> {
    Ok(Cipher::new(key)?.block(*block))
}

/// Encrypt complete blocks without padding or an IV prefix.
pub(super) fn cbc(
    key: &[u8],
    iv: &[u8; 16],
    input: &[u8],
) -> io::Result<Vec<u8>> {
    if input.len() % 16 != 0 {
        return Err(invalid("AES-CBC input must contain complete blocks"));
    }
    let cipher = Cipher::new(key)?;
    let mut previous = *iv;
    let mut result = Vec::new();
    result
        .try_reserve_exact(input.len())
        .map_err(|_| invalid("AES-CBC output allocation failed"))?;
    for chunk in input.chunks_exact(16) {
        let mut block = <[u8; 16]>::try_from(chunk)
            .map_err(|_| invalid("short AES-CBC block"))?;
        for (byte, chain) in block.iter_mut().zip(previous) {
            *byte ^= chain;
        }
        previous = cipher.block(block);
        result.extend_from_slice(&previous);
    }
    Ok(result)
}
