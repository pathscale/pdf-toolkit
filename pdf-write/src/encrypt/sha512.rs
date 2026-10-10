//! SHA-384 and SHA-512 for revision 6 algorithm 2.B.

use std::io;

use super::invalid;

const INITIAL_512: [u64; 8] = [
    0x6a09e667f3bcc908, 0xbb67ae8584caa73b,
    0x3c6ef372fe94f82b, 0xa54ff53a5f1d36f1,
    0x510e527fade682d1, 0x9b05688c2b3e6c1f,
    0x1f83d9abfb41bd6b, 0x5be0cd19137e2179,
];

const INITIAL_384: [u64; 8] = [
    0xcbbb9d5dc1059ed8, 0x629a292a367cd507,
    0x9159015a3070dd17, 0x152fecd8f70e5939,
    0x67332667ffc00b31, 0x8eb44a8768581511,
    0xdb0c2e0d64f98fa7, 0x47b5481dbefa4fa4,
];

const K: [u64; 80] = [
    0x428a2f98d728ae22, 0x7137449123ef65cd,
    0xb5c0fbcfec4d3b2f, 0xe9b5dba58189dbbc,
    0x3956c25bf348b538, 0x59f111f1b605d019,
    0x923f82a4af194f9b, 0xab1c5ed5da6d8118,
    0xd807aa98a3030242, 0x12835b0145706fbe,
    0x243185be4ee4b28c, 0x550c7dc3d5ffb4e2,
    0x72be5d74f27b896f, 0x80deb1fe3b1696b1,
    0x9bdc06a725c71235, 0xc19bf174cf692694,
    0xe49b69c19ef14ad2, 0xefbe4786384f25e3,
    0x0fc19dc68b8cd5b5, 0x240ca1cc77ac9c65,
    0x2de92c6f592b0275, 0x4a7484aa6ea6e483,
    0x5cb0a9dcbd41fbd4, 0x76f988da831153b5,
    0x983e5152ee66dfab, 0xa831c66d2db43210,
    0xb00327c898fb213f, 0xbf597fc7beef0ee4,
    0xc6e00bf33da88fc2, 0xd5a79147930aa725,
    0x06ca6351e003826f, 0x142929670a0e6e70,
    0x27b70a8546d22ffc, 0x2e1b21385c26c926,
    0x4d2c6dfc5ac42aed, 0x53380d139d95b3df,
    0x650a73548baf63de, 0x766a0abb3c77b2a8,
    0x81c2c92e47edaee6, 0x92722c851482353b,
    0xa2bfe8a14cf10364, 0xa81a664bbc423001,
    0xc24b8b70d0f89791, 0xc76c51a30654be30,
    0xd192e819d6ef5218, 0xd69906245565a910,
    0xf40e35855771202a, 0x106aa07032bbd1b8,
    0x19a4c116b8d2d0c8, 0x1e376c085141ab53,
    0x2748774cdf8eeb99, 0x34b0bcb5e19b48a8,
    0x391c0cb3c5c95a63, 0x4ed8aa4ae3418acb,
    0x5b9cca4f7763e373, 0x682e6ff3d6b2b8a3,
    0x748f82ee5defb2fc, 0x78a5636f43172f60,
    0x84c87814a1f0ab72, 0x8cc702081a6439ec,
    0x90befffa23631e28, 0xa4506cebde82bde9,
    0xbef9a3f7b2c67915, 0xc67178f2e372532b,
    0xca273eceea26619c, 0xd186b8c721c0c207,
    0xeada7dd6cde0eb1e, 0xf57d4f7fee6ed178,
    0x06f067aa72176fba, 0x0a637dc5a2c898a6,
    0x113f9804bef90dae, 0x1b710b35131c471b,
    0x28db77f523047d84, 0x32caab7b40c72493,
    0x3c9ebe0a15c9bebc, 0x431d67c49c100d4c,
    0x4cc5d4becb3e42b6, 0x597f299cfc657e2a,
    0x5fcb6fab3ad6faec, 0x6c44198c4a475817,
];

fn block(state: &mut [u64; 8], bytes: &[u8; 128]) {
    let mut words = [0u64; 80];
    for (index, chunk) in bytes.chunks_exact(8).enumerate() {
        words[index] = u64::from_be_bytes([
            chunk[0], chunk[1], chunk[2], chunk[3],
            chunk[4], chunk[5], chunk[6], chunk[7],
        ]);
    }
    for index in 16..80 {
        let x = words[index - 15];
        let y = words[index - 2];
        let s0 = x.rotate_right(1) ^ x.rotate_right(8) ^ (x >> 7);
        let s1 = y.rotate_right(19) ^ y.rotate_right(61) ^ (y >> 6);
        words[index] = words[index - 16]
            .wrapping_add(s0)
            .wrapping_add(words[index - 7])
            .wrapping_add(s1);
    }
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
    for index in 0..80 {
        let s1 = e.rotate_right(14) ^ e.rotate_right(18) ^ e.rotate_right(41);
        let choose = (e & f) ^ (!e & g);
        let t1 = h
            .wrapping_add(s1)
            .wrapping_add(choose)
            .wrapping_add(K[index])
            .wrapping_add(words[index]);
        let s0 = a.rotate_right(28) ^ a.rotate_right(34) ^ a.rotate_right(39);
        let majority = (a & b) ^ (a & c) ^ (b & c);
        let t2 = s0.wrapping_add(majority);
        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }
    for (word, addition) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
        *word = word.wrapping_add(addition);
    }
}

pub(super) fn digest(input: &[u8], sha384: bool) -> io::Result<Vec<u8>> {
    let bits = u128::try_from(input.len())
        .ok()
        .and_then(|length| length.checked_mul(8))
        .ok_or_else(|| invalid("SHA-512 input bit length exceeds u128"))?;
    let mut state = if sha384 { INITIAL_384 } else { INITIAL_512 };
    let mut chunks = input.chunks_exact(128);
    for chunk in &mut chunks {
        let bytes = <[u8; 128]>::try_from(chunk)
            .map_err(|_| invalid("short SHA-512 block"))?;
        block(&mut state, &bytes);
    }
    let remainder = chunks.remainder();
    // remainder is at most 127 bytes; both tails fit this fixed buffer.
    let mut tail = [0u8; 256];
    tail[..remainder.len()].copy_from_slice(remainder);
    tail[remainder.len()] = 0x80;
    let length = if remainder.len() < 112 { 128 } else { 256 };
    tail[length - 16..length].copy_from_slice(&bits.to_be_bytes());
    for chunk in tail[..length].chunks_exact(128) {
        let bytes = <[u8; 128]>::try_from(chunk)
            .map_err(|_| invalid("short SHA-512 padding block"))?;
        block(&mut state, &bytes);
    }
    let count = if sha384 { 6 } else { 8 };
    let mut result = Vec::new();
    result
        .try_reserve_exact(count * 8)
        .map_err(|_| invalid("SHA-512 output allocation failed"))?;
    for word in state.iter().take(count) {
        result.extend_from_slice(&word.to_be_bytes());
    }
    Ok(result)
}
