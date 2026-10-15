//! Private PSChaCha security handler primitives.
//!
//! Randomness belongs to the writer. This module needs only no_std + alloc.

use alloc::sync::Arc;
use alloc::vec::Vec;

use argon2::{Algorithm, Argon2, Block, Params, Version};
use chacha20poly1305::aead::{AeadInOut, KeyInit};
use chacha20poly1305::{Tag, XChaCha20Poly1305, XNonce};

use crate::error::{malformed, DocError};
use crate::limits::{check, LimitKind, Limits};
use crate::object::{DictRef, Object, Value};
use crate::work::ObjectRef;

const CHECK_AAD: &[u8] = b"PSChaCha/Check/v1";
const MAX_MEMORY_BYTES: u64 = 256 << 20;
const MAX_ITERATIONS: u64 = 10;
const MAX_PARALLELISM: u64 = 16;

/// Argon2id version 0x13 parameters, with a 32-byte output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KdfParams {
    pub memory_kib: u32,
    pub iterations: u32,
    pub parallelism: u32,
}

impl Default for KdfParams {
    fn default() -> Self {
        Self {
            memory_kib: 65536,
            iterations: 3,
            parallelism: 1,
        }
    }
}

impl KdfParams {
    fn checked(self, limits: &Limits) -> Result<Params, DocError> {
        check(
            LimitKind::KdfMemoryBytes,
            u64::from(self.memory_kib) * 1024,
            limits
                .max_kdf_memory_bytes
                .min(limits.max_decoded_bytes)
                .min(MAX_MEMORY_BYTES),
        )?;
        check(
            LimitKind::CodecWork,
            u64::from(self.iterations),
            MAX_ITERATIONS,
        )?;
        check(
            LimitKind::CodecWork,
            u64::from(self.parallelism),
            MAX_PARALLELISM,
        )?;
        Params::new(self.memory_kib, self.iterations, self.parallelism, Some(32))
            .map_err(|_| malformed(0, "PSChaCha KDF parameters"))
    }
}

/// An opaque file key. Possession of this value permits reading and writing.
/// Share it through Arc; it intentionally has no Debug or raw-key accessor.
pub struct ChaChaKey {
    cipher: XChaCha20Poly1305,
}

impl ChaChaKey {
    pub fn derive(
        password: &[u8],
        salt: &[u8; 16],
        costs: KdfParams,
        limits: &Limits,
    ) -> Result<Self, DocError> {
        check(
            LimitKind::ObjectBytes,
            password.len() as u64,
            limits.max_object_bytes.min(1 << 20),
        )?;
        let params = costs.checked(limits)?;
        let count = params.block_count();
        let mut memory = Vec::new();
        memory
            .try_reserve_exact(count)
            .map_err(|_| malformed(0, "PSChaCha KDF allocation"))?;
        memory.resize_with(count, Block::default);
        let mut key = [0u8; 32];
        let result = Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
            .hash_password_into_with_memory(password, salt, &mut key, &mut memory);
        memory.fill(Block::default());
        result.map_err(|_| malformed(0, "PSChaCha KDF"))?;
        let cipher = XChaCha20Poly1305::new((&key).into());
        key.fill(0);
        Ok(Self { cipher })
    }

    /// Nonce zero is reserved exclusively for Check.
    pub fn seal(&self, id: ObjectRef, nonce: [u8; 24], plain: &[u8]) -> Result<Vec<u8>, DocError> {
        if id.number() == 0 || nonce == [0; 24] {
            return Err(malformed(0, "PSChaCha object nonce or ID"));
        }
        let length = plain
            .len()
            .checked_add(40)
            .ok_or_else(|| malformed(0, "PSChaCha length"))?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(length)
            .map_err(|_| malformed(0, "PSChaCha allocation"))?;
        bytes.extend_from_slice(&nonce);
        bytes.extend_from_slice(plain);
        let nonce = XNonce::try_from(&nonce[..]).map_err(|_| malformed(0, "PSChaCha nonce"))?;
        let tag = self
            .cipher
            .encrypt_inout_detached(&nonce, &object_aad(id), (&mut bytes[24..]).into())
            .map_err(|_| malformed(0, "PSChaCha encryption"))?;
        bytes.extend_from_slice(&tag);
        Ok(bytes)
    }

    pub fn open(&self, id: ObjectRef, bytes: &[u8], maximum: u64) -> Result<Vec<u8>, DocError> {
        if id.number() == 0 || bytes.len() < 40 {
            return Err(malformed(0, "PSChaCha authentication"));
        }
        let nonce = &bytes[..24];
        if nonce.iter().all(|byte| *byte == 0) {
            return Err(malformed(0, "PSChaCha authentication"));
        }
        let end = bytes.len() - 16;
        let length = end - 24;
        check(LimitKind::DecodedBytes, length as u64, maximum)?;
        let mut plain = Vec::new();
        plain
            .try_reserve_exact(length)
            .map_err(|_| malformed(0, "PSChaCha allocation"))?;
        plain.extend_from_slice(&bytes[24..end]);
        let nonce = XNonce::try_from(nonce).map_err(|_| malformed(0, "PSChaCha authentication"))?;
        let tag =
            Tag::try_from(&bytes[end..]).map_err(|_| malformed(0, "PSChaCha authentication"))?;
        self.cipher
            .decrypt_inout_detached(&nonce, &object_aad(id), plain.as_mut_slice().into(), &tag)
            .map_err(|_| malformed(0, "PSChaCha authentication"))?;
        Ok(plain)
    }

    /// A detached authentication tag over empty plaintext and handler metadata.
    pub fn password_check(&self, salt: &[u8; 16], costs: KdfParams) -> Result<[u8; 16], DocError> {
        let mut empty: [u8; 0] = [];
        let tag = self
            .cipher
            .encrypt_inout_detached(
                &XNonce::default(),
                &check_aad(salt, costs),
                empty.as_mut_slice().into(),
            )
            .map_err(|_| malformed(0, "PSChaCha password check"))?;
        let mut result = [0u8; 16];
        result.copy_from_slice(&tag);
        Ok(result)
    }

    fn authenticate_password(
        &self,
        salt: &[u8; 16],
        costs: KdfParams,
        token: &[u8; 16],
    ) -> Result<(), DocError> {
        let mut empty: [u8; 0] = [];
        self.cipher
            .decrypt_inout_detached(
                &XNonce::default(),
                &check_aad(salt, costs),
                empty.as_mut_slice().into(),
                &Tag::from(*token),
            )
            .map_err(|_| malformed(0, "PSChaCha password authentication"))
    }
}

fn object_aad(id: ObjectRef) -> [u8; 8] {
    let mut bytes = [0u8; 8];
    bytes[..4].copy_from_slice(&id.number().to_le_bytes());
    bytes[4..].copy_from_slice(&u32::from(id.generation()).to_le_bytes());
    bytes
}

fn check_aad(salt: &[u8; 16], costs: KdfParams) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(CHECK_AAD.len() + 28);
    bytes.extend_from_slice(CHECK_AAD);
    bytes.extend_from_slice(salt);
    bytes.extend_from_slice(&costs.memory_kib.to_le_bytes());
    bytes.extend_from_slice(&costs.iterations.to_le_bytes());
    bytes.extend_from_slice(&costs.parallelism.to_le_bytes());
    bytes
}

pub(crate) struct PrivateCrypt {
    pub(crate) key: Arc<ChaChaKey>,
    encrypt_id: Option<ObjectRef>,
}

impl PrivateCrypt {
    pub(crate) fn recognizes(value: &Value) -> bool {
        value
            .get()
            .as_dict()
            .and_then(|dict| dict.get(b"Filter"))
            .and_then(|value| value.as_name())
            .is_some_and(|name| name.eq_bytes(b"PSChaCha"))
    }

    pub(crate) fn open(
        value: &Value,
        password: Option<&[u8]>,
        limits: &Limits,
    ) -> Result<Self, DocError> {
        let password = password.ok_or(DocError::Encrypted)?;
        let dict = value
            .get()
            .as_dict()
            .ok_or_else(|| malformed(0, "PSChaCha security dictionary"))?;
        if integer(dict, b"V")? != 1 || integer(dict, b"Length")? != 256 {
            return Err(DocError::Unsupported {
                feature: "PSChaCha version",
            });
        }
        if !dict
            .get(b"KDF")
            .and_then(|value| value.as_name())
            .is_some_and(|name| name.eq_bytes(b"Argon2id"))
        {
            return Err(DocError::Unsupported {
                feature: "PSChaCha KDF",
            });
        }
        let costs = KdfParams {
            memory_kib: integer(dict, b"M")?,
            iterations: integer(dict, b"T")?,
            parallelism: integer(dict, b"P")?,
        };
        let salt = fixed::<16>(dict, b"Salt")?;
        let token = fixed::<16>(dict, b"Check")?;
        let key = ChaChaKey::derive(password, &salt, costs, limits)?;
        key.authenticate_password(&salt, costs, &token)?;
        Ok(Self {
            key: Arc::new(key),
            encrypt_id: value.object().id(),
        })
    }

    pub(crate) fn excluded(&self, object: &Object) -> bool {
        (self.encrypt_id.is_some() && self.encrypt_id == object.id()) || object.is_xref_stream()
    }

    pub(crate) fn check_xref_claim(&self, object: &Object) -> Result<(), DocError> {
        if object.is_stream()
            && !object.is_xref_stream()
            && object
                .dict()
                .and_then(|dict| dict.get(b"Type"))
                .and_then(|value| value.as_name())
                .is_some_and(|name| name.eq_bytes(b"XRef"))
        {
            return Err(malformed(0, "PSChaCha authentication"));
        }
        Ok(())
    }

    pub(crate) fn prepare(
        &self,
        object: &mut Object,
        expected: ObjectRef,
        limits: &Limits,
    ) -> Result<(), DocError> {
        if object.id() != Some(expected) {
            return Err(malformed(0, "PSChaCha object generation"));
        }
        self.check_xref_claim(object)?;
        if !self.excluded(object) {
            object.decrypt_private_strings(&self.key, limits)?;
        }
        Ok(())
    }
}

fn integer(dict: DictRef<'_>, key: &[u8]) -> Result<u32, DocError> {
    dict.get(key)
        .and_then(|value| value.as_i64())
        .and_then(|number| u32::try_from(number).ok())
        .ok_or_else(|| malformed(0, "PSChaCha security integer"))
}

fn fixed<const N: usize>(dict: DictRef<'_>, key: &[u8]) -> Result<[u8; N], DocError> {
    let text = dict
        .get(key)
        .and_then(|value| value.as_str())
        .ok_or_else(|| malformed(0, "PSChaCha security string"))?;
    check(
        LimitKind::ObjectBytes,
        text.raw().len() as u64,
        (N * 4 + 2) as u64,
    )?;
    let bytes = text.decode();
    bytes
        .as_slice()
        .try_into()
        .map_err(|_| malformed(0, "PSChaCha security string length"))
}
