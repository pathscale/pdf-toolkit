//! PSChaCha authoring. Production salts, identifiers and nonces use OS randomness.

use std::io;
use std::sync::Arc;

use pdf_core::encryption::ChaChaKey;
use pdf_core::{Limits, ObjectRef};

use crate::{invalid, set, Dict, Value, Writer};

pub use pdf_core::encryption::KdfParams;

enum Nonces {
    Random,
    Fixture(u64),
}

pub(crate) struct Encryption {
    key: Arc<ChaChaKey>,
    encrypt_id: u32,
    identifier: [u8; 16],
    nonces: Nonces,
}

fn random<const N: usize>() -> io::Result<[u8; N]> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes)
        .map_err(|error| io::Error::other(format!("OS randomness: {error}")))?;
    Ok(bytes)
}

fn random_nonce() -> io::Result<[u8; 24]> {
    loop {
        let nonce = random()?;
        if nonce != [0; 24] {
            return Ok(nonce);
        }
    }
}

impl Encryption {
    fn nonce(&mut self) -> io::Result<[u8; 24]> {
        match &mut self.nonces {
            Nonces::Random => random_nonce(),
            Nonces::Fixture(counter) => {
                let next = counter
                    .checked_add(1)
                    .ok_or_else(|| invalid("fixture nonce overflow"))?;
                let mut nonce = [0x4eu8; 24];
                nonce[16..].copy_from_slice(&counter.to_le_bytes());
                *counter = next;
                Ok(nonce)
            }
        }
    }

    pub(crate) fn bytes(
        &mut self,
        number: u32,
        generation: u16,
        plain: &[u8],
    ) -> io::Result<Vec<u8>> {
        if number == self.encrypt_id {
            return Err(invalid("cannot replace the PSChaCha security dictionary"));
        }
        let nonce = self.nonce()?;
        self.key
            .seal(ObjectRef::new(number, generation), nonce, plain)
            .map_err(|error| invalid(&error.to_string()))
    }

    pub(crate) fn value(
        &mut self,
        number: u32,
        generation: u16,
        value: &Value,
    ) -> io::Result<Value> {
        if number == self.encrypt_id {
            return Err(invalid("cannot replace the PSChaCha security dictionary"));
        }
        transform(value, &mut |plain| self.bytes(number, generation, plain))
    }

    pub(crate) fn trailer(&self, extra: &mut Dict) -> io::Result<()> {
        if let Some((_, existing)) = extra.iter().rev().find(|(key, _)| key == "Encrypt") {
            if !matches!(existing, Value::Ref(number, 0) if *number == self.encrypt_id) {
                return Err(invalid("conflicting PSChaCha security dictionary"));
            }
        }
        set(extra, "Encrypt", Value::Ref(self.encrypt_id, 0));
        if !extra.iter().any(|(key, _)| key == "ID") {
            set(
                extra,
                "ID",
                Value::Array(vec![
                    Value::HexStr(self.identifier.to_vec()),
                    Value::HexStr(self.identifier.to_vec()),
                ]),
            );
        }
        Ok(())
    }
}

impl Writer {
    /// Writes PSChaCha PDFs with independent random nonces for every value.
    /// None selects M=65536 KiB, T=3, P=1. An empty password is allowed.
    /// Stream input must already have its declared filters applied.
    pub fn new_encrypted(
        version: &str,
        password: &[u8],
        costs: Option<KdfParams>,
    ) -> io::Result<Self> {
        Self::with_encryption(
            version,
            password,
            costs.unwrap_or_default(),
            random()?,
            random()?,
            Nonces::Random,
        )
    }

    fn with_encryption(
        version: &str,
        password: &[u8],
        costs: KdfParams,
        salt: [u8; 16],
        identifier: [u8; 16],
        nonces: Nonces,
    ) -> io::Result<Self> {
        let mut writer = Self::new(version)?;
        let key = Arc::new(
            ChaChaKey::derive(password, &salt, costs, &Limits::default())
                .map_err(|error| invalid(&error.to_string()))?,
        );
        let token = key
            .password_check(&salt, costs)
            .map_err(|error| invalid(&error.to_string()))?;
        let encrypt_id = writer.alloc()?;
        writer.object(
            encrypt_id,
            &Value::Dict(vec![
                ("Filter".to_owned(), Value::Name("PSChaCha".to_owned())),
                ("V".to_owned(), Value::Int(1)),
                ("Length".to_owned(), Value::Int(256)),
                ("Salt".to_owned(), Value::HexStr(salt.to_vec())),
                ("KDF".to_owned(), Value::Name("Argon2id".to_owned())),
                ("M".to_owned(), Value::Int(i64::from(costs.memory_kib))),
                ("T".to_owned(), Value::Int(i64::from(costs.iterations))),
                ("P".to_owned(), Value::Int(i64::from(costs.parallelism))),
                ("Check".to_owned(), Value::HexStr(token.to_vec())),
            ]),
        )?;
        writer.encryption = Some(Encryption {
            key,
            encrypt_id,
            identifier,
            nonces,
        });
        Ok(writer)
    }

    // Fixed entropy is confined to the house-fixture generator.
    pub(crate) fn new_encrypted_fixture(version: &str, password: &[u8]) -> io::Result<Self> {
        Self::with_encryption(
            version,
            password,
            KdfParams::default(),
            [0x53; 16],
            [0x49; 16],
            Nonces::Fixture(1),
        )
    }
}

/// Encrypt strings in a direct value for an incremental object.
/// The caller supplies the existing authenticated file key and actual generation.
pub fn encrypt_value(key: &ChaChaKey, id: ObjectRef, value: &Value) -> io::Result<Value> {
    transform(value, &mut |plain| {
        key.seal(id, random_nonce()?, plain)
            .map_err(|error| invalid(&error.to_string()))
    })
}

/// Encrypt already-filtered stream data for an incremental object.
pub fn encrypt_stream(key: &ChaChaKey, id: ObjectRef, data: &[u8]) -> io::Result<Vec<u8>> {
    key.seal(id, random_nonce()?, data)
        .map_err(|error| invalid(&error.to_string()))
}

fn transform<F>(value: &Value, seal: &mut F) -> io::Result<Value>
where
    F: FnMut(&[u8]) -> io::Result<Vec<u8>>,
{
    enum Task<'a> {
        Visit(&'a Value),
        Array(usize),
        Dict(Vec<String>),
    }

    let mut tasks = vec![Task::Visit(value)];
    let mut values = Vec::new();
    while let Some(task) = tasks.pop() {
        match task {
            Task::Array(count) => {
                let start = values
                    .len()
                    .checked_sub(count)
                    .ok_or_else(|| invalid("encryption value stack"))?;
                let children = values.split_off(start);
                values.push(Value::Array(children));
            }
            Task::Dict(keys) => {
                let start = values
                    .len()
                    .checked_sub(keys.len())
                    .ok_or_else(|| invalid("encryption dictionary stack"))?;
                let children = values.split_off(start);
                values.push(Value::Dict(keys.into_iter().zip(children).collect()));
            }
            Task::Visit(value) => {
                let result = match value {
                    Value::Array(children) => {
                        tasks.push(Task::Array(children.len()));
                        for child in children.iter().rev() {
                            tasks.push(Task::Visit(child));
                        }
                        continue;
                    }
                    Value::Dict(entries) => {
                        tasks.push(Task::Dict(
                            entries.iter().map(|(key, _)| key.clone()).collect(),
                        ));
                        for (_, child) in entries.iter().rev() {
                            tasks.push(Task::Visit(child));
                        }
                        continue;
                    }
                    Value::Str(bytes) | Value::HexStr(bytes) => Value::HexStr(seal(bytes)?),
                    Value::Null => Value::Null,
                    Value::Bool(value) => Value::Bool(*value),
                    Value::Int(value) => Value::Int(*value),
                    Value::Real(value) => Value::Real(*value),
                    Value::Name(value) => Value::Name(value.clone()),
                    Value::Ref(number, generation) => Value::Ref(*number, *generation),
                };
                values.push(result);
            }
        }
    }
    values
        .pop()
        .ok_or_else(|| invalid("empty encryption value stack"))
}
