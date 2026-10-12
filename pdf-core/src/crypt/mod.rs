use alloc::borrow::Cow;
use alloc::vec::Vec;

use crate::document::Resolver;
use crate::error::{malformed, DocError, Warning};
use crate::filter::{Decoded, FilterKind, FilterStep};
use crate::limits::{check, LimitKind, Refusal};
use crate::object::{DictRef, Object, Value};
use crate::work::ObjectRef;

mod aes;
mod md5;
mod rc4;
mod sha2;

const SECURITY_STRING_BYTES: u64 = 4096;
const SECURITY_NAME_BYTES: u64 = 256;
const CRYPT_FILTERS: u64 = 64;
const R6_ROUNDS: usize = 287;
const PASSWORD_PAD: [u8; 32] = [
    0x28, 0xbf, 0x4e, 0x5e, 0x4e, 0x75, 0x8a, 0x41,
    0x64, 0x00, 0x4e, 0x56, 0xff, 0xfa, 0x01, 0x08,
    0x2e, 0x2e, 0x00, 0xb6, 0xd0, 0x68, 0x3e, 0x80,
    0x2f, 0x0c, 0xa9, 0xfe, 0x64, 0x53, 0x69, 0x7a,
];

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum Method {
    Identity,
    Rc4,
    Aes128,
    Aes256,
}

pub(crate) struct Crypt {
    key: [u8; 32],
    key_len: usize,
    revision: i64,
    encrypt_id: Option<ObjectRef>,
    metadata: bool,
    strings: Method,
    streams: Method,
    embedded: Method,
    filters: Vec<(Vec<u8>, Method)>,
}

impl Crypt {
    pub(crate) fn open(
        resolver: &mut Resolver<'_>,
        value: &Value,
    ) -> Result<Option<Self>, DocError> {
        let dict = value.get().as_dict().ok_or_else(bad_dictionary)?;
        if name(resolver, dict, b"Filter")?.as_deref() != Some(&b"Standard"[..]) {
            return Err(unsupported());
        }
        let version = integer(resolver, dict, b"V")?.unwrap_or(0);
        let revision = integer(resolver, dict, b"R")?.ok_or_else(bad_dictionary)?;
        let modern = version == 5 && (revision == 5 || revision == 6);
        if !modern
            && !((version == 1 || version == 2) && (2..=4).contains(&revision))
            && !(version == 4 && revision == 4)
        {
            return Err(unsupported());
        }
        let bits = integer(resolver, dict, b"Length")?
            .unwrap_or(if modern { 256 } else { 40 });
        let key_len = if modern {
            if bits != 256 {
                return Err(bad_dictionary());
            }
            32
        } else {
            if !(40..=128).contains(&bits) || bits % 8 != 0
                || ((version == 1 || revision == 2) && bits != 40)
            {
                return Err(bad_dictionary());
            }
            usize::try_from(bits / 8).map_err(|_| bad_dictionary())?
        };
        let permissions = integer(resolver, dict, b"P")?
            .and_then(|number| i32::try_from(number).ok())
            .ok_or_else(bad_dictionary)?;
        let metadata = match resolver.resolve_dict_key(dict, b"EncryptMetadata")? {
            Some(value) if !value.get().is_null() => {
                value.get().as_bool().ok_or_else(bad_dictionary)?
            }
            _ => true,
        };
        let mut crypt = Self {
            key: [0; 32],
            key_len,
            revision,
            encrypt_id: value.object().id(),
            metadata,
            strings: Method::Rc4,
            streams: Method::Rc4,
            embedded: Method::Rc4,
            filters: Vec::new(),
        };
        if version >= 4 {
            crypt.read_filters(resolver, dict, modern)?;
            crypt.strings = crypt.selected(resolver, dict, b"StrF", Method::Identity)?;
            crypt.streams = crypt.selected(resolver, dict, b"StmF", Method::Identity)?;
            crypt.embedded = crypt.selected(resolver, dict, b"EFF", crypt.streams)?;
        }
        if modern {
            let user = fixed::<48>(resolver, dict, b"U")?;
            let _owner = fixed::<48>(resolver, dict, b"O")?;
            let mut salt = [0u8; 8];
            salt.copy_from_slice(&user[32..40]);
            let hash = |salt: &[u8; 8]| if revision == 5 { Ok(sha2::sha256(salt)) } else { r6_hash(salt) };
            if !equal(&hash(&salt)?, &user[..32]) {
                return Ok(None);
            }
            salt.copy_from_slice(&user[40..48]);
            let wrapping_key = hash(&salt)?;
            let mut file_key = fixed::<32>(resolver, dict, b"UE")?;
            if !aes::decrypt_cbc(&wrapping_key, [0; 16], &mut file_key) {
                return Err(bad_dictionary());
            }
            let mut perms = fixed::<16>(resolver, dict, b"Perms")?;
            let cipher = aes::Aes::new(&file_key).ok_or_else(bad_dictionary)?;
            cipher.decrypt(&mut perms);
            if !equal(&perms[..4], &permissions.to_le_bytes())
                || perms[4..8] != [255; 4]
                || perms[8] != if metadata { b'T' } else { b'F' }
                || &perms[9..12] != b"adb"
            {
                return Ok(None);
            }
            crypt.key = file_key;
        } else {
            let owner = fixed::<32>(resolver, dict, b"O")?;
            let user = fixed::<32>(resolver, dict, b"U")?;
            let trailer = resolver.document().trailer().clone();
            let trailer_dict = trailer.dict().ok_or_else(bad_dictionary)?;
            let ids = resolver.resolve_dict_key(trailer_dict, b"ID")?
                .ok_or_else(bad_dictionary)?;
            let first = ids.get().as_array().and_then(|array| array.get(0))
                .ok_or_else(bad_dictionary)?;
            let first = resolver.resolve(first)?;
            let id = binary_value(resolver, &first, SECURITY_STRING_BYTES)?;
            let mut digest = md5::Md5::new();
            digest.update(&PASSWORD_PAD);
            digest.update(&owner);
            digest.update(&permissions.to_le_bytes());
            digest.update(&id);
            if revision >= 4 && !metadata {
                digest.update(&[255; 4]);
            }
            let mut key = digest.finish();
            if revision >= 3 {
                for _ in 0..50 {
                    key = md5::digest(&key[..key_len]);
                }
            }
            crypt.key[..key_len].copy_from_slice(&key[..key_len]);
            let mut expected = PASSWORD_PAD;
            if revision == 2 {
                if !rc4::apply(&crypt.key[..key_len], &mut expected) {
                    return Err(bad_dictionary());
                }
                if !equal(&expected, &user) {
                    return Ok(None);
                }
            } else {
                let mut digest = md5::Md5::new();
                digest.update(&PASSWORD_PAD);
                digest.update(&id);
                let mut expected = digest.finish();
                if !rc4::apply(&crypt.key[..key_len], &mut expected) {
                    return Err(bad_dictionary());
                }
                let mut iteration_key = [0u8; 16];
                for iteration in 1..20u8 {
                    for (slot, byte) in iteration_key[..key_len].iter_mut()
                        .zip(&crypt.key[..key_len])
                    {
                        *slot = *byte ^ iteration;
                    }
                    if !rc4::apply(&iteration_key[..key_len], &mut expected) {
                        return Err(bad_dictionary());
                    }
                }
                if !equal(&expected, &user[..16]) {
                    return Ok(None);
                }
            }
        }
        Ok(Some(crypt))
    }

    fn read_filters(
        &mut self,
        resolver: &mut Resolver<'_>,
        dict: DictRef<'_>,
        modern: bool,
    ) -> Result<(), DocError> {
        let Some(value) = resolver.resolve_dict_key(dict, b"CF")? else {
            return Ok(());
        };
        if value.get().is_null() {
            return Ok(());
        }
        let dict = value.get().as_dict().ok_or_else(bad_dictionary)?;
        let maximum = CRYPT_FILTERS.min(u64::from(resolver.document().limits().max_objects));
        check(LimitKind::Objects, dict.len() as u64, maximum)?;
        self.filters.try_reserve_exact(dict.len()).map_err(|_| refusal(
            LimitKind::Objects, maximum.saturating_add(1), maximum,
        ))?;
        for (filter_name, value) in dict.iter() {
            let maximum = SECURITY_NAME_BYTES.min(resolver.document().limits().max_object_bytes);
            check(LimitKind::ObjectBytes, filter_name.encoded().len() as u64, maximum)?;
            let filter_name = filter_name.decoded().into_owned();
            let value = resolver.resolve(value)?;
            let dict = value.get().as_dict().ok_or_else(bad_dictionary)?;
            let method = match name(resolver, dict, b"CFM")?.as_deref() {
                None | Some(b"None") => Method::Identity,
                Some(b"V2") if !modern => Method::Rc4,
                Some(b"AESV2") if !modern && self.key_len == 16 => Method::Aes128,
                Some(b"AESV3") if modern => Method::Aes256,
                _ => return Err(unsupported()),
            };
            if method != Method::Identity {
                if let Some(length) = integer(resolver, dict, b"Length")? {
                    if usize::try_from(length).ok() != Some(self.key_len) {
                        return Err(unsupported());
                    }
                }
            }
            self.filters.push((filter_name, method));
        }
        Ok(())
    }

    fn selected(
        &self,
        resolver: &mut Resolver<'_>,
        dict: DictRef<'_>,
        key: &[u8],
        default: Method,
    ) -> Result<Method, DocError> {
        match name(resolver, dict, key)? {
            Some(name) => self.method(&name),
            None => Ok(default),
        }
    }

    fn method(&self, name: &[u8]) -> Result<Method, DocError> {
        if name == b"Identity" {
            return Ok(Method::Identity);
        }
        self.filters.iter().find(|(candidate, _)| candidate.as_slice() == name)
            .map(|(_, method)| *method).ok_or_else(unsupported)
    }

    pub(crate) fn strings(&self) -> Method {
        self.strings
    }

    pub(crate) fn excluded(&self, id: Option<ObjectRef>) -> bool {
        self.encrypt_id.is_some() && self.encrypt_id == id
    }

    fn object_key(&self, id: ObjectRef, method: Method) -> ([u8; 32], usize) {
        if self.revision >= 5 {
            return (self.key, 32);
        }
        let number = id.number().to_le_bytes();
        let generation = id.generation().to_le_bytes();
        let mut digest = md5::Md5::new();
        digest.update(&self.key[..self.key_len]);
        digest.update(&number[..3]);
        digest.update(&generation);
        if method == Method::Aes128 {
            digest.update(b"sAlT");
        }
        let digest = digest.finish();
        let mut key = [0u8; 32];
        key[..16].copy_from_slice(&digest);
        (key, (self.key_len + 5).min(16))
    }

    pub(crate) fn decrypt(
        &self,
        id: ObjectRef,
        method: Method,
        bytes: &[u8],
        maximum: u64,
    ) -> Result<(Vec<u8>, bool), DocError> {
        let allowance = if matches!(method, Method::Aes128 | Method::Aes256) { 32 } else { 0 };
        let bytes = copy(bytes, maximum.saturating_add(allowance))?;
        self.decrypt_owned(id, method, bytes, maximum)
    }

    pub(crate) fn decrypt_owned(
        &self,
        id: ObjectRef,
        method: Method,
        mut bytes: Vec<u8>,
        maximum: u64,
    ) -> Result<(Vec<u8>, bool), DocError> {
        let allowance = if matches!(method, Method::Aes128 | Method::Aes256) { 32 } else { 0 };
        check(LimitKind::DecodedBytes, bytes.len() as u64, maximum.saturating_add(allowance))?;
        let mut bad_padding = false;
        if method != Method::Identity {
            let (key, length) = self.object_key(id, method);
            if method == Method::Rc4 {
                if !rc4::apply(&key[..length], &mut bytes) {
                    return Err(malformed(0, "crypt RC4 key"));
                }
            } else {
                let mut iv = [0u8; 16];
                iv.copy_from_slice(bytes.get(..16).ok_or_else(|| malformed(0, "crypt IV"))?);
                bytes.drain(..16);
                if !aes::decrypt_cbc(&key[..length], iv, &mut bytes) {
                    return Err(malformed(0, "crypt AES blocks"));
                }
                let padding = usize::from(bytes.last().copied().unwrap_or(0));
                let valid = (1..=16).contains(&padding)
                    && bytes.get(bytes.len().saturating_sub(padding)..)
                        .is_some_and(|tail| tail.len() == padding
                            && tail.iter().all(|byte| usize::from(*byte) == padding));
                if valid {
                    bytes.truncate(bytes.len().saturating_sub(padding));
                } else {
                    bad_padding = true;
                }
            }
        }
        check(LimitKind::DecodedBytes, bytes.len() as u64, maximum)?;
        Ok((bytes, bad_padding))
    }
}

fn integer(
    resolver: &mut Resolver<'_>, dict: DictRef<'_>, key: &[u8],
) -> Result<Option<i64>, DocError> {
    match resolver.resolve_dict_key(dict, key)? {
        Some(value) if !value.get().is_null() => {
            value.get().as_i64().map(Some).ok_or_else(bad_dictionary)
        }
        _ => Ok(None),
    }
}

fn name(
    resolver: &mut Resolver<'_>, dict: DictRef<'_>, key: &[u8],
) -> Result<Option<Vec<u8>>, DocError> {
    let Some(value) = resolver.resolve_dict_key(dict, key)? else {
        return Ok(None);
    };
    if value.get().is_null() {
        return Ok(None);
    }
    let name = value.get().as_name().ok_or_else(bad_dictionary)?;
    let maximum = SECURITY_NAME_BYTES.min(resolver.document().limits().max_object_bytes);
    check(LimitKind::ObjectBytes, name.encoded().len() as u64, maximum)?;
    Ok(Some(name.decoded().into_owned()))
}

fn binary_value(
    resolver: &Resolver<'_>, value: &Value, maximum: u64,
) -> Result<Vec<u8>, DocError> {
    let text = value.get().as_str().ok_or_else(bad_dictionary)?;
    let raw_maximum = SECURITY_STRING_BYTES.min(resolver.document().limits().max_object_bytes);
    check(LimitKind::ObjectBytes, text.raw().len() as u64, raw_maximum)?;
    let bytes = text.decode();
    check(LimitKind::ObjectBytes, bytes.len() as u64, maximum.min(raw_maximum))?;
    Ok(bytes)
}

fn fixed<const N: usize>(
    resolver: &mut Resolver<'_>, dict: DictRef<'_>, key: &[u8],
) -> Result<[u8; N], DocError> {
    let value = resolver.resolve_dict_key(dict, key)?.ok_or_else(bad_dictionary)?;
    // Some R6 writers pad U and O with zero bytes after their 48-byte payload.
    // Keep the security-string bound and authenticate the original payload.
    let padded = resolver.document().repair_mode()
        && N == 48 && matches!(key, b"U" | b"O");
    let maximum = if padded { SECURITY_STRING_BYTES } else { N as u64 };
    let bytes = binary_value(resolver, &value, maximum)?;
    if bytes.len() > N {
        if !padded || !bytes[N..].iter().all(|byte| *byte == 0) {
            check(LimitKind::ObjectBytes, bytes.len() as u64, N as u64)?;
        }
        resolver.warn(Warning {
            offset: Some(value.get().span().start as u64),
            object: value.object().id(),
            what: "crypt_string_padding_recovered",
        });
    } else if bytes.len() != N {
        return Err(bad_dictionary());
    }
    let mut result = [0u8; N];
    result.copy_from_slice(&bytes[..N]);
    Ok(result)
}

fn equal(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).fold(0u8, |difference, (a, b)| difference | (*a ^ *b)) == 0
}

fn r6_hash(salt: &[u8; 8]) -> Result<[u8; 32], DocError> {
    // Algorithm 2.B specialized to the empty user password. No owner-user-key
    // suffix is used when verifying or deriving the empty user's key.
    let mut key = [0u8; 64];
    key[..32].copy_from_slice(&sha2::sha256(salt));
    let mut length = 32usize;
    let mut buffer = [0u8; 4096];
    for round in 0..R6_ROUNDS {
        let size = length * 64;
        for block in buffer[..size].chunks_exact_mut(length) {
            block.copy_from_slice(&key[..length]);
        }
        let mut iv = [0u8; 16];
        iv.copy_from_slice(&key[16..32]);
        if !aes::encrypt_cbc(&key[..16], iv, &mut buffer[..size]) {
            return Err(bad_dictionary());
        }
        let last = buffer[size - 1];
        let selector = buffer[..16].iter().fold(0usize, |sum, byte| sum + usize::from(*byte)) % 3;
        match selector {
            0 => {
                key[..32].copy_from_slice(&sha2::sha256(&buffer[..size]));
                length = 32;
            }
            1 => {
                key = sha2::sha512(&buffer[..size], true);
                length = 48;
            }
            _ => {
                key = sha2::sha512(&buffer[..size], false);
                length = 64;
            }
        }
        let completed = round + 1;
        if completed >= 64 && usize::from(last) <= completed - 32 {
            let mut result = [0u8; 32];
            result.copy_from_slice(&key[..32]);
            return Ok(result);
        }
    }
    Err(refusal(LimitKind::CodecWork, R6_ROUNDS as u64 + 1, R6_ROUNDS as u64))
}

pub(crate) fn decode_stream(
    resolver: &mut Resolver<'_>,
    stream: &Object,
    raw: &[u8],
    steps: &[FilterStep],
) -> Result<Decoded, DocError> {
    let maximum = resolver.document().limits().max_decoded_bytes;
    let crypt = resolver.document().crypt();
    let exempt = is_type(stream, b"XRef")
        || crypt.as_ref().is_some_and(|crypt| crypt.excluded(stream.id())
            || (!crypt.metadata && is_type(stream, b"Metadata")));
    let explicit = steps.iter().any(|step| step.kind == FilterKind::Crypt);
    let mut data = Cow::Borrowed(raw);
    let mut truncated = false;
    let mut image = None;
    if !explicit && !exempt {
        if let Some(crypt) = &crypt {
            let method = if is_type(stream, b"EmbeddedFile") { crypt.embedded } else { crypt.streams };
            if method != Method::Identity {
                data = Cow::Owned(decrypt_stream(resolver, stream, crypt, method, raw, maximum)?);
            }
        }
    }
    let params = if explicit {
        let dict = stream.dict().ok_or_else(bad_dictionary)?;
        resolver.resolve_dict_key(dict, b"DecodeParms")?
    } else {
        None
    };
    for (index, step) in steps.iter().enumerate() {
        if step.kind == FilterKind::Crypt {
            let param = if let Some(value) = &params {
                if let Some(array) = value.get().as_array() {
                    match array.get(index) {
                        Some(value) => Some(resolver.resolve(value)?),
                        None => None,
                    }
                } else if index == 0 {
                    Some(value.clone())
                } else {
                    None
                }
            } else {
                None
            };
            let filter_name = match param.as_ref() {
                Some(value) if !value.get().is_null() => {
                    let dict = value.get().as_dict().ok_or_else(bad_dictionary)?;
                    name(resolver, dict, b"Name")?
                }
                _ => None,
            };
            let method = match filter_name.as_deref() {
                None | Some(b"Identity") => Method::Identity,
                Some(name) => crypt.as_ref().ok_or_else(unsupported)?.method(name)?,
            };
            if !exempt && method != Method::Identity {
                let crypt = crypt.as_ref().ok_or_else(unsupported)?;
                data = Cow::Owned(decrypt_stream(
                    resolver, stream, crypt, method, data.as_ref(), maximum,
                )?);
            }
        } else {
            let decoded = crate::filter::decode(data.as_ref(), core::slice::from_ref(step), maximum)?;
            truncated |= decoded.truncated;
            image = decoded.image;
            data = Cow::Owned(decoded.bytes);
            if image.is_some() {
                break;
            }
        }
    }
    check(LimitKind::DecodedBytes, data.len() as u64, maximum)?;
    let bytes = match data {
        Cow::Owned(bytes) => bytes,
        Cow::Borrowed(bytes) => copy(bytes, maximum)?,
    };
    Ok(Decoded { bytes, image, truncated })
}

fn decrypt_stream(
    resolver: &mut Resolver<'_>,
    stream: &Object,
    crypt: &Crypt,
    method: Method,
    raw: &[u8],
    maximum: u64,
) -> Result<Vec<u8>, DocError> {
    let id = stream.id().ok_or(DocError::WrongType {
        id: None, expected: "indirect encrypted stream",
    })?;
    let (bytes, padding) = crypt.decrypt(id, method, raw, maximum)?;
    if padding {
        resolver.warn(Warning { offset: None, object: Some(id), what: "crypt_padding" });
    }
    Ok(bytes)
}

fn is_type(object: &Object, kind: &[u8]) -> bool {
    object.dict().and_then(|dict| dict.get(b"Type")).and_then(|value| value.as_name())
        .is_some_and(|name| name.eq_bytes(kind))
}

fn copy(bytes: &[u8], maximum: u64) -> Result<Vec<u8>, DocError> {
    let maximum = maximum.min(isize::MAX as u64);
    check(LimitKind::DecodedBytes, bytes.len() as u64, maximum)?;
    let mut result = Vec::new();
    result.try_reserve_exact(bytes.len()).map_err(|_| refusal(
        LimitKind::DecodedBytes, maximum.saturating_add(1), maximum,
    ))?;
    result.extend_from_slice(bytes);
    Ok(result)
}

fn refusal(limit: LimitKind, requested: u64, maximum: u64) -> DocError {
    DocError::Refused(Refusal { limit, requested, maximum })
}

fn bad_dictionary() -> DocError {
    malformed(0, "Standard security dictionary")
}

fn unsupported() -> DocError {
    DocError::Unsupported { feature: "Standard security handler" }
}
