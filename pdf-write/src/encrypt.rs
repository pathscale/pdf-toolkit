//! Independent ISO 32000-2 section 7.6 encryption for deterministic fixtures.
//!
//! Passwords are fixed: an empty user password and the ASCII owner password
//! "owner". Seeds, salts, file keys and IVs are deliberately reproducible.
//! This module is private to the fixture generator.

use std::io;

use crate::{sha256, Dict, Value};

mod aes;
mod md5;
mod sha512;

const PASSWORD_PADDING: [u8; 32] = [
    0x28, 0xbf, 0x4e, 0x5e, 0x4e, 0x75, 0x8a, 0x41,
    0x64, 0x00, 0x4e, 0x56, 0xff, 0xfa, 0x01, 0x08,
    0x2e, 0x2e, 0x00, 0xb6, 0xd0, 0x68, 0x3e, 0x80,
    0x2f, 0x0c, 0xa9, 0xfe, 0x64, 0x53, 0x69, 0x7a,
];

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum Method {
    Rc4,
    Aes128,
    Aes256,
}

pub(super) struct Encryption {
    method: Method,
    key: Vec<u8>,
    seed: [u8; 32],
    id: [u8; 16],
    dictionary: Dict,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn joined(parts: &[&[u8]]) -> io::Result<Vec<u8>> {
    let length = parts.iter().try_fold(0usize, |length, part| {
        length
            .checked_add(part.len())
            .ok_or_else(|| invalid("encryption buffer length overflow"))
    })?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| invalid("encryption buffer allocation failed"))?;
    for part in parts {
        bytes.extend_from_slice(part);
    }
    Ok(bytes)
}

fn prefix<const N: usize>(bytes: &[u8]) -> io::Result<[u8; N]> {
    let bytes = bytes
        .get(..N)
        .ok_or_else(|| invalid("short encryption value"))?;
    <[u8; N]>::try_from(bytes)
        .map_err(|_| invalid("wrong encryption value length"))
}

fn derived(seed: &[u8; 32], label: &[u8]) -> io::Result<[u8; 32]> {
    Ok(sha256::digest(&joined(&[seed, label])?))
}

fn name(value: &str) -> Value {
    Value::Name(value.to_owned())
}

fn crypt_filter(method: &str, length: i64) -> Dict {
    vec![
        (
            "CF".to_owned(),
            Value::Dict(vec![(
                "StdCF".to_owned(),
                Value::Dict(vec![
                    ("CFM".to_owned(), name(method)),
                    ("Length".to_owned(), Value::Int(length)),
                    ("AuthEvent".to_owned(), name("DocOpen")),
                ]),
            )]),
        ),
        ("StmF".to_owned(), name("StdCF")),
        ("StrF".to_owned(), name("StdCF")),
    ]
}

impl Encryption {
    pub(super) fn new(method: Method, label: &[u8]) -> io::Result<Self> {
        let seed = sha256::digest(&joined(&[
            b"pdf-toolkit encrypted fixtures v1:",
            label,
        ])?);
        let id = prefix::<16>(&derived(&seed, b"document-id")?)?;
        let (version, revision, bits) = match method {
            Method::Rc4 => (2, 3, 128),
            Method::Aes128 => (4, 4, 128),
            Method::Aes256 => (5, 6, 256),
        };
        let (key, mut entries) = match method {
            Method::Rc4 | Method::Aes128 => legacy_entries(&id)?,
            Method::Aes256 => modern_entries(&seed)?,
        };
        let mut dictionary = vec![
            ("Filter".to_owned(), name("Standard")),
            ("V".to_owned(), Value::Int(version)),
            ("R".to_owned(), Value::Int(revision)),
            ("Length".to_owned(), Value::Int(bits)),
            ("P".to_owned(), Value::Int(-4)),
            ("EncryptMetadata".to_owned(), Value::Bool(true)),
        ];
        dictionary.append(&mut entries);
        match method {
            Method::Rc4 => {}
            Method::Aes128 => dictionary.extend(crypt_filter("AESV2", 16)),
            Method::Aes256 => dictionary.extend(crypt_filter("AESV3", 32)),
        }
        Ok(Self { method, key, seed, id, dictionary })
    }

    /// Encryption dictionary strings are exempt from object encryption.
    pub(super) fn dictionary(&self) -> Value {
        Value::Dict(self.dictionary.clone())
    }

    /// Trailer identifiers are also exempt from encryption.
    pub(super) fn identifiers(&self) -> Value {
        Value::Array(vec![
            Value::HexStr(self.id.to_vec()),
            Value::HexStr(self.id.to_vec()),
        ])
    }

    /// Encrypt one string or an already-filtered stream at generation zero.
    pub(super) fn encrypt(
        &self,
        number: u32,
        plain: &[u8],
        domain: &[u8],
    ) -> io::Result<Vec<u8>> {
        if number == 0 {
            return Err(invalid("cannot encrypt object zero"));
        }
        let key = match self.method {
            Method::Aes256 => joined(&[&self.key])?,
            Method::Rc4 | Method::Aes128 => {
                let number = number.to_le_bytes();
                let number = number
                    .get(..3)
                    .ok_or_else(|| invalid("short object number"))?;
                let salt: &[u8] = if self.method == Method::Aes128 {
                    b"sAlT"
                } else {
                    b""
                };
                let material = joined(&[&self.key, number, &[0, 0], salt])?;
                // A 128-bit file key yields a 128-bit object key.
                md5::digest(&material)?.to_vec()
            }
        };
        if self.method == Method::Rc4 {
            return rc4(&key, plain);
        }
        let iv = prefix::<16>(&sha256::digest(&joined(&[
            &self.seed,
            domain,
            &number.to_le_bytes(),
            &sha256::digest(plain),
        ])?))?;
        let padding = 16usize
            .checked_sub(plain.len() % 16)
            .ok_or_else(|| invalid("invalid AES padding length"))?;
        let length = plain
            .len()
            .checked_add(padding)
            .ok_or_else(|| invalid("padded encryption length overflow"))?;
        let padding = u8::try_from(padding)
            .map_err(|_| invalid("AES padding exceeds one byte"))?;
        let mut padded = Vec::new();
        padded
            .try_reserve_exact(length)
            .map_err(|_| invalid("padded encryption allocation failed"))?;
        padded.extend_from_slice(plain);
        padded.resize(length, padding);
        let ciphertext = aes::cbc(&key, &iv, &padded)?;
        joined(&[&iv, &ciphertext])
    }
}

fn padded_password(password: &[u8]) -> [u8; 32] {
    let length = password.len().min(32);
    let mut result = [0u8; 32];
    // Both ranges are bounded by the fixed 32-byte password buffer.
    result[..length].copy_from_slice(&password[..length]);
    result[length..].copy_from_slice(&PASSWORD_PADDING[..32 - length]);
    result
}

fn rc4(key: &[u8], input: &[u8]) -> io::Result<Vec<u8>> {
    if key.is_empty() {
        return Err(invalid("empty RC4 key"));
    }
    let mut state = [0u8; 256];
    for (index, byte) in state.iter_mut().enumerate() {
        *byte = u8::try_from(index)
            .map_err(|_| invalid("RC4 state index exceeds one byte"))?;
    }
    let mut j = 0u8;
    for i in 0..256usize {
        let key_byte = key
            .get(i % key.len())
            .copied()
            .ok_or_else(|| invalid("short RC4 key"))?;
        j = j.wrapping_add(state[i]).wrapping_add(key_byte);
        state.swap(i, usize::from(j));
    }
    let mut result = joined(&[input])?;
    let mut i = 0u8;
    j = 0;
    for byte in &mut result {
        i = i.wrapping_add(1);
        j = j.wrapping_add(state[usize::from(i)]);
        state.swap(usize::from(i), usize::from(j));
        let position = state[usize::from(i)]
            .wrapping_add(state[usize::from(j)]);
        *byte ^= state[usize::from(position)];
    }
    Ok(result)
}

fn rc4_rounds(key: &[u8], input: &[u8]) -> io::Result<Vec<u8>> {
    let mut output = joined(&[input])?;
    for round in 0..20u8 {
        let mut round_key = joined(&[key])?;
        for byte in &mut round_key {
            *byte ^= round;
        }
        output = rc4(&round_key, &output)?;
    }
    Ok(output)
}

/// Algorithms 2, 3 and 5 for a 128-bit key, revisions 3 and 4.
/// Metadata is encrypted, so algorithm 2 does not append four 0xff bytes.
fn legacy_entries(id: &[u8; 16]) -> io::Result<(Vec<u8>, Dict)> {
    let mut owner_key = md5::digest(&padded_password(b"owner"))?;
    for _ in 0..50 {
        owner_key = md5::digest(&owner_key)?;
    }
    let owner = rc4_rounds(&owner_key, &PASSWORD_PADDING)?;
    let permissions = (-4i32).to_le_bytes();
    let mut key = md5::digest(&joined(&[
        &PASSWORD_PADDING,
        &owner,
        &permissions,
        id,
    ])?)?;
    for _ in 0..50 {
        key = md5::digest(&key)?;
    }
    let user_digest = md5::digest(&joined(&[&PASSWORD_PADDING, id])?)?;
    let user_prefix = rc4_rounds(&key, &user_digest)?;
    let user = joined(&[&user_prefix, &[0u8; 16]])?;
    Ok((
        key.to_vec(),
        vec![
            ("O".to_owned(), Value::HexStr(owner)),
            ("U".to_owned(), Value::HexStr(user)),
        ],
    ))
}

/// Revision 6 algorithm 2.B. The fixed passwords need no SASLprep changes.
fn r6_hash(
    password: &[u8],
    salt: &[u8],
    user: &[u8],
) -> io::Result<[u8; 32]> {
    if password.len() > 127
        || salt.len() != 8
        || (!user.is_empty() && user.len() != 48)
    {
        return Err(invalid("invalid revision 6 hash arguments"));
    }
    let mut k = sha256::digest(&joined(&[password, salt, user])?).to_vec();
    // E's last byte is at most 255, so the termination condition must hold
    // by round 287. Keep an explicit bound rather than an unbounded loop.
    for iteration in 0u16..288 {
        let unit = joined(&[password, &k, user])?;
        let length = unit
            .len()
            .checked_mul(64)
            .ok_or_else(|| invalid("revision 6 repeated buffer overflow"))?;
        let mut repeated = Vec::new();
        repeated
            .try_reserve_exact(length)
            .map_err(|_| invalid("revision 6 buffer allocation failed"))?;
        for _ in 0..64 {
            repeated.extend_from_slice(&unit);
        }
        let aes_key = prefix::<16>(&k)?;
        let iv = prefix::<16>(
            k.get(16..32)
                .ok_or_else(|| invalid("short revision 6 IV"))?,
        )?;
        let e = aes::cbc(&aes_key, &iv, &repeated)?;
        // 256 is congruent to 1 modulo 3, so summing these bytes gives
        // the same remainder as the big-endian 128-bit integer.
        let selector = e
            .get(..16)
            .ok_or_else(|| invalid("short revision 6 ciphertext"))?
            .iter()
            .fold(0u16, |sum, byte| (sum + u16::from(*byte)) % 3);
        k = match selector {
            0 => sha256::digest(&e).to_vec(),
            1 => sha512::digest(&e, true)?,
            _ => sha512::digest(&e, false)?,
        };
        let round = iteration
            .checked_add(1)
            .ok_or_else(|| invalid("revision 6 round overflow"))?;
        let last = e
            .last()
            .copied()
            .ok_or_else(|| invalid("empty revision 6 ciphertext"))?;
        if round >= 64 && u16::from(last) <= round.saturating_sub(32) {
            return prefix::<32>(&k);
        }
    }
    Err(invalid("revision 6 hash exceeded its round bound"))
}

/// Revision 6 algorithms 8, 9 and 10.
fn modern_entries(seed: &[u8; 32]) -> io::Result<(Vec<u8>, Dict)> {
    let key = derived(seed, b"file-key")?;
    let user_validation = prefix::<8>(&derived(seed, b"user-validation")?)?;
    let user_key_salt = prefix::<8>(&derived(seed, b"user-key")?)?;
    let owner_validation = prefix::<8>(&derived(seed, b"owner-validation")?)?;
    let owner_key_salt = prefix::<8>(&derived(seed, b"owner-key")?)?;

    let user_hash = r6_hash(b"", &user_validation, b"")?;
    let user = joined(&[&user_hash, &user_validation, &user_key_salt])?;
    let user_key = r6_hash(b"", &user_key_salt, b"")?;
    let ue = aes::cbc(&user_key, &[0u8; 16], &key)?;

    let owner_hash = r6_hash(b"owner", &owner_validation, &user)?;
    let owner = joined(&[&owner_hash, &owner_validation, &owner_key_salt])?;
    let owner_key = r6_hash(b"owner", &owner_key_salt, &user)?;
    let oe = aes::cbc(&owner_key, &[0u8; 16], &key)?;

    let mut permissions = [
        0xfc, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff, 0xff,
        b'T', b'a', b'd', b'b',
        0, 0, 0, 0,
    ];
    let random = prefix::<4>(&derived(seed, b"permissions")?)?;
    permissions[12..16].copy_from_slice(&random);
    let perms = aes::ecb(&key, &permissions)?;

    Ok((
        key.to_vec(),
        vec![
            ("O".to_owned(), Value::HexStr(owner)),
            ("U".to_owned(), Value::HexStr(user)),
            ("OE".to_owned(), Value::HexStr(oe)),
            ("UE".to_owned(), Value::HexStr(ue)),
            ("Perms".to_owned(), Value::HexStr(perms.to_vec())),
        ],
    ))
}
