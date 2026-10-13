//! Independent RC4 encryption for deterministic fixtures.
//!
//! Passwords are fixed: an empty user password and the ASCII owner password
//! "owner". File identifiers and keys are deterministic. This module is private
//! to the fixture generator.

use std::io;

use crate::{sha256, Dict, Value};

mod md5;

const PASSWORD_PADDING: [u8; 32] = [
    0x28, 0xbf, 0x4e, 0x5e, 0x4e, 0x75, 0x8a, 0x41, 0x64, 0x00, 0x4e, 0x56, 0xff, 0xfa, 0x01, 0x08,
    0x2e, 0x2e, 0x00, 0xb6, 0xd0, 0x68, 0x3e, 0x80, 0x2f, 0x0c, 0xa9, 0xfe, 0x64, 0x53, 0x69, 0x7a,
];

pub(super) struct Encryption {
    key: Vec<u8>,
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
    <[u8; N]>::try_from(bytes).map_err(|_| invalid("wrong encryption value length"))
}

fn derived(seed: &[u8; 32], label: &[u8]) -> io::Result<[u8; 32]> {
    Ok(sha256::digest(&joined(&[seed, label])?))
}

fn name(value: &str) -> Value {
    Value::Name(value.to_owned())
}

impl Encryption {
    pub(super) fn new(label: &[u8]) -> io::Result<Self> {
        let seed = sha256::digest(&joined(&[b"pdf-toolkit encrypted fixtures v1:", label])?);
        let id = prefix::<16>(&derived(&seed, b"document-id")?)?;
        let (key, mut entries) = legacy_entries(&id)?;
        let mut dictionary = vec![
            ("Filter".to_owned(), name("Standard")),
            ("V".to_owned(), Value::Int(2)),
            ("R".to_owned(), Value::Int(3)),
            ("Length".to_owned(), Value::Int(128)),
            ("P".to_owned(), Value::Int(-4)),
            ("EncryptMetadata".to_owned(), Value::Bool(true)),
        ];
        dictionary.append(&mut entries);
        Ok(Self {
            key,
            id,
            dictionary,
        })
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
    pub(super) fn encrypt(&self, number: u32, plain: &[u8]) -> io::Result<Vec<u8>> {
        if number == 0 {
            return Err(invalid("cannot encrypt object zero"));
        }
        let number = number.to_le_bytes();
        let number = number
            .get(..3)
            .ok_or_else(|| invalid("short object number"))?;
        let material = joined(&[&self.key, number, &[0, 0]])?;
        let key = md5::digest(&material)?;
        rc4(&key, plain)
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
        *byte = u8::try_from(index).map_err(|_| invalid("RC4 state index exceeds one byte"))?;
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
        let position = state[usize::from(i)].wrapping_add(state[usize::from(j)]);
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

/// Algorithms 2, 3 and 5 for a 128-bit key, revision 3.
/// Metadata is encrypted, so algorithm 2 does not append four 0xff bytes.
fn legacy_entries(id: &[u8; 16]) -> io::Result<(Vec<u8>, Dict)> {
    let mut owner_key = md5::digest(&padded_password(b"owner"))?;
    for _ in 0..50 {
        owner_key = md5::digest(&owner_key)?;
    }
    let owner = rc4_rounds(&owner_key, &PASSWORD_PADDING)?;
    let permissions = (-4i32).to_le_bytes();
    let mut key = md5::digest(&joined(&[&PASSWORD_PADDING, &owner, &permissions, id])?)?;
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
