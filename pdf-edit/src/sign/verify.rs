//! Offline signature checks: byte range digest, ECDSA validity, signer
//! subject and what was appended after each signed revision.

use std::collections::BTreeSet;
use std::sync::Arc;

use cms::cert::CertificateChoices;
use cms::content_info::ContentInfo;
use cms::signed_data::{SignedData, SignerIdentifier, SignerInfo};
use der::asn1::OctetString;
use der::oid::ObjectIdentifier;
use der::{Decode, Encode};
use pdf_core::{DocError, Document, Limits, ObjectRef, PdfSource, Value};
use x509_cert::time::Time;
use x509_cert::Certificate;

use super::cms::{
    digest, verify_prehash, ECDSA_SHA256, ECDSA_SHA384, ECDSA_SHA512, ID_EC_PUBLIC_KEY,
    ID_MESSAGE_DIGEST, ID_SHA256, ID_SHA384, ID_SHA512, ID_SIGNED_DATA, ID_SIGNING_TIME, SECP256R1,
    SECP384R1,
};
use super::{unsupported, Curve};

const MAX_FIELDS: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DigestCheck {
    /// The byte range digest equals the signed `messageDigest`.
    Match,
    Mismatch,
    UnsupportedAlgorithm,
    Malformed,
}

impl DigestCheck {
    pub fn name(self) -> &'static str {
        match self {
            Self::Match => "match",
            Self::Mismatch => "mismatch",
            Self::UnsupportedAlgorithm => "unsupported_algorithm",
            Self::Malformed => "malformed",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SignatureCheck {
    /// The ECDSA signature verifies with the signer certificate's key.
    Valid,
    Invalid,
    /// RSA, Ed25519 or anything else this crate does not verify.
    UnsupportedAlgorithm,
    Malformed,
}

impl SignatureCheck {
    pub fn name(self) -> &'static str {
        match self {
            Self::Valid => "valid",
            Self::Invalid => "invalid",
            Self::UnsupportedAlgorithm => "unsupported_algorithm",
            Self::Malformed => "malformed",
        }
    }
}

#[derive(Clone, Debug)]
pub struct SignatureReport {
    /// Fully qualified field name.
    pub field: String,
    pub sub_filter: String,
    /// For example `ecdsa-p256-sha256`, `rsa-sha256` or a dotted OID.
    pub algorithm: String,
    pub digest: DigestCheck,
    pub signature: SignatureCheck,
    /// Signer certificate subject, RFC 4514 style.
    pub signer: Option<String>,
    /// CMS `signingTime` when present, otherwise the `/M` entry.
    pub signing_time: Option<String>,
    pub byte_range: [u64; 4],
    /// The byte range starts at 0, skips exactly `/Contents` and ends at an
    /// `%%EOF` marker: it covers one whole revision.
    pub covers_revision: bool,
    /// File bytes after the signed revision.
    pub bytes_after: u64,
    /// Cross-reference sections (revisions) added after the signed revision.
    pub later_revisions: u32,
}

impl SignatureReport {
    /// Digest and signature both check out.
    pub fn intact(&self) -> bool {
        self.digest == DigestCheck::Match && self.signature == SignatureCheck::Valid
    }

    pub fn modified_after(&self) -> bool {
        self.bytes_after > 0
    }
}

fn text(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xfe, 0xff]) {
        let units = rest.chunks(2).map(|pair| {
            u16::from_be_bytes([
                pair.first().copied().unwrap_or(0),
                pair.get(1).copied().unwrap_or(0),
            ])
        });
        return char::decode_utf16(units)
            .map(|unit| unit.unwrap_or('\u{fffd}'))
            .collect();
    }
    bytes.iter().map(|byte| char::from(*byte)).collect()
}

fn resolve_dict(
    resolver: &mut pdf_core::Resolver<'_>,
    value: Option<pdf_core::ValueRef<'_>>,
) -> Result<Option<Value>, DocError> {
    let Some(value) = value else { return Ok(None) };
    let value = resolver.resolve(value)?;
    Ok(value.get().as_dict().is_some().then_some(value))
}

/// Every signature field with a value, in field tree order.
fn signature_fields(document: &Document) -> Result<Vec<(String, Value)>, DocError> {
    let mut resolver = document.resolver();
    let root = document.root_ref().ok_or(DocError::Malformed {
        offset: 0,
        what: "catalog",
    })?;
    let catalog = resolver.get(root)?;
    let Some(catalog) = catalog.dict() else {
        return Ok(Vec::new());
    };
    let Some(form) = resolve_dict(&mut resolver, catalog.get(b"AcroForm"))? else {
        return Ok(Vec::new());
    };
    let Some(fields) = form.get().as_dict().and_then(|dict| dict.get(b"Fields")) else {
        return Ok(Vec::new());
    };
    let fields = resolver.resolve(fields)?;
    let mut stack: Vec<(ObjectRef, String, bool)> = Vec::new();
    if let Some(array) = fields.get().as_array() {
        for index in (0..array.len()).rev() {
            if let Some(id) = array.get(index).and_then(|item| item.as_ref()) {
                stack.push((id, String::new(), false));
            }
        }
    }
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    while let Some((id, prefix, inherited_sig)) = stack.pop() {
        if !seen.insert((id.number(), id.generation())) || seen.len() > MAX_FIELDS {
            continue;
        }
        let object = resolver.get(id)?;
        let Some(dict) = object.dict() else { continue };
        let partial = match dict.get(b"T").and_then(|value| value.as_str()) {
            Some(name) => text(&resolver.decode_string(id, name)),
            None => String::new(),
        };
        let name = match (prefix.is_empty(), partial.is_empty()) {
            (true, _) => partial,
            (false, true) => prefix.clone(),
            (false, false) => format!("{prefix}.{partial}"),
        };
        let is_sig = match dict.get(b"FT").and_then(|value| value.as_name()) {
            Some(kind) => kind.eq_bytes(b"Sig"),
            None => inherited_sig,
        };
        if let Some(kids) = dict.get(b"Kids") {
            let kids = resolver.resolve(kids)?;
            if let Some(array) = kids.get().as_array() {
                for index in (0..array.len()).rev() {
                    if let Some(kid) = array.get(index).and_then(|item| item.as_ref()) {
                        stack.push((kid, name.clone(), is_sig));
                    }
                }
            }
        }
        if !is_sig {
            continue;
        }
        if let Some(signature) = resolve_dict(&mut resolver, dict.get(b"V"))? {
            out.push((name, signature));
        }
    }
    Ok(out)
}

fn byte_range(dict: pdf_core::DictRef<'_>) -> Option<[u64; 4]> {
    let array = dict.get(b"ByteRange")?.as_array()?;
    if array.len() != 4 {
        return None;
    }
    let mut out = [0u64; 4];
    for (index, slot) in out.iter_mut().enumerate() {
        *slot = u64::try_from(array.get(index)?.as_i64()?).ok()?;
    }
    Some(out)
}

/// The ranges as slices, when they lie inside the file in order.
fn ranges(bytes: &[u8], range: [u64; 4]) -> Option<[&[u8]; 2]> {
    let [a, b, c, d] = range.map(|value| usize::try_from(value).ok());
    let (a, b, c, d) = (a?, b?, c?, d?);
    let first_end = a.checked_add(b)?;
    let second_end = c.checked_add(d)?;
    if first_end > c {
        return None;
    }
    Some([bytes.get(a..first_end)?, bytes.get(c..second_end)?])
}

fn der_prefix(bytes: &[u8]) -> Option<&[u8]> {
    let first = *bytes.get(1)?;
    let (header, length) = if first < 0x80 {
        (2usize, usize::from(first))
    } else {
        let count = usize::from(first & 0x7f);
        if count == 0 || count > 4 {
            return None;
        }
        let mut length = 0usize;
        for byte in bytes.get(2..2 + count)? {
            length = (length << 8) | usize::from(*byte);
        }
        (2 + count, length)
    };
    bytes.get(..header.checked_add(length)?)
}

fn digest_name(oid: ObjectIdentifier) -> &'static str {
    match oid {
        ID_SHA256 => "sha256",
        ID_SHA384 => "sha384",
        ID_SHA512 => "sha512",
        _ => "unknown",
    }
}

fn format_time(time: &Time) -> String {
    let date = time.to_date_time();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        date.year(),
        date.month(),
        date.day(),
        date.hour(),
        date.minutes(),
        date.seconds(),
    )
}

fn signer_certificate<'a>(signed: &'a SignedData, info: &SignerInfo) -> Option<&'a Certificate> {
    let certificates = signed.certificates.as_ref()?;
    let mut first = None;
    for choice in certificates.0.iter() {
        let CertificateChoices::Certificate(certificate) = choice else {
            continue;
        };
        first.get_or_insert(certificate);
        if let SignerIdentifier::IssuerAndSerialNumber(id) = &info.sid {
            if certificate.tbs_certificate().issuer() == &id.issuer
                && certificate.tbs_certificate().serial_number() == &id.serial_number
            {
                return Some(certificate);
            }
        }
    }
    first
}

struct CmsCheck {
    algorithm: String,
    digest: DigestCheck,
    signature: SignatureCheck,
    signer: Option<String>,
    signing_time: Option<String>,
}

fn check_cms(contents: &[u8], parts: Option<[&[u8]; 2]>) -> CmsCheck {
    let mut check = CmsCheck {
        algorithm: "unknown".to_owned(),
        digest: DigestCheck::Malformed,
        signature: SignatureCheck::Malformed,
        signer: None,
        signing_time: None,
    };
    let Some(der) = der_prefix(contents) else {
        return check;
    };
    let Ok(info) = ContentInfo::from_der(der) else {
        return check;
    };
    if info.content_type != ID_SIGNED_DATA {
        return check;
    }
    let Ok(signed) = info.content.decode_as::<SignedData>() else {
        return check;
    };
    let Some(signer) = signed.signer_infos.0.iter().next() else {
        return check;
    };
    let certificate = signer_certificate(&signed, signer);
    check.signer =
        certificate.map(|certificate| certificate.tbs_certificate().subject().to_string());

    let digest_oid = signer.digest_alg.oid;
    let spki =
        certificate.map(|certificate| certificate.tbs_certificate().subject_public_key_info());
    let curve = spki.and_then(|spki| {
        if spki.algorithm.oid != ID_EC_PUBLIC_KEY {
            return None;
        }
        match spki
            .algorithm
            .parameters
            .as_ref()?
            .decode_as::<ObjectIdentifier>()
            .ok()?
        {
            SECP256R1 => Some(Curve::P256),
            SECP384R1 => Some(Curve::P384),
            _ => None,
        }
    });
    let signature_oid = signer.signature_algorithm.oid;
    let ecdsa = matches!(signature_oid, ECDSA_SHA256 | ECDSA_SHA384 | ECDSA_SHA512)
        || signature_oid == ID_EC_PUBLIC_KEY;
    check.algorithm = if ecdsa {
        let curve = match curve {
            Some(Curve::P256) => "p256",
            Some(Curve::P384) => "p384",
            None => "unknown-curve",
        };
        format!("ecdsa-{curve}-{}", digest_name(digest_oid))
    } else if signature_oid.to_string().starts_with("1.2.840.113549.1.1.") {
        format!("rsa-{}", digest_name(digest_oid))
    } else {
        signature_oid.to_string()
    };

    let Some(parts) = parts else { return check };
    let Some(content_digest) = digest(digest_oid, &parts) else {
        check.digest = DigestCheck::UnsupportedAlgorithm;
        check.signature = SignatureCheck::UnsupportedAlgorithm;
        return check;
    };
    let prehash = match &signer.signed_attrs {
        Some(attributes) => {
            let mut message = None;
            for attribute in attributes.iter() {
                let value = attribute.values.iter().next();
                if attribute.oid == ID_MESSAGE_DIGEST {
                    message = value.and_then(|any| any.decode_as::<OctetString>().ok());
                } else if attribute.oid == ID_SIGNING_TIME {
                    check.signing_time = value
                        .and_then(|any| any.to_der().ok())
                        .and_then(|bytes| Time::from_der(&bytes).ok())
                        .map(|time| format_time(&time));
                }
            }
            check.digest = match message {
                Some(message) if message.as_bytes() == content_digest.as_slice() => {
                    DigestCheck::Match
                }
                Some(_) => DigestCheck::Mismatch,
                None => DigestCheck::Malformed,
            };
            let Ok(encoded) = attributes.to_der() else {
                return check;
            };
            match digest(digest_oid, &[&encoded]) {
                Some(prehash) => prehash,
                None => return check,
            }
        }
        None => {
            check.digest = DigestCheck::Match;
            content_digest
        }
    };
    if !ecdsa {
        check.signature = SignatureCheck::UnsupportedAlgorithm;
        return check;
    }
    let (Some(curve), Some(spki)) = (curve, spki) else {
        check.signature = SignatureCheck::UnsupportedAlgorithm;
        return check;
    };
    check.signature = match verify_prehash(
        curve,
        spki.subject_public_key.raw_bytes(),
        &prehash,
        signer.signature.as_bytes(),
    ) {
        Some(true) => SignatureCheck::Valid,
        Some(false) => SignatureCheck::Invalid,
        None => SignatureCheck::Malformed,
    };
    check
}

fn ends_at_eof(bytes: &[u8]) -> bool {
    let mut end = bytes.len();
    while end > 0 && matches!(bytes.get(end - 1), Some(b'\r' | b'\n')) {
        end -= 1;
    }
    bytes
        .get(..end)
        .is_some_and(|prefix| prefix.ends_with(b"%%EOF"))
}

/// Checks every signature field in the file. Encrypted files are refused.
pub fn verify(bytes: Arc<[u8]>, limits: Limits) -> Result<Vec<SignatureReport>, DocError> {
    let source = PdfSource::new(bytes.clone())?;
    let document = Document::open(source, limits)?;
    if document.encrypted() {
        return Err(unsupported("verify:encrypted document"));
    }
    let total_sections = document.xref().sections();
    let mut reports = Vec::new();
    for (field, signature) in signature_fields(&document)? {
        let Some(dict) = signature.get().as_dict() else {
            continue;
        };
        let sub_filter = dict
            .get(b"SubFilter")
            .and_then(|value| value.as_name())
            .map(|name| String::from_utf8_lossy(&name.decoded()).into_owned())
            .unwrap_or_default();
        let contents = dict
            .get(b"Contents")
            .and_then(|value| value.as_str())
            .map(|string| string.decode())
            .unwrap_or_default();
        let modified = dict
            .get(b"M")
            .and_then(|value| value.as_str())
            .map(|string| text(&string.decode()));
        let range = byte_range(dict);
        let parts = range.and_then(|range| ranges(&bytes, range));
        let mut check = check_cms(&contents, parts);
        if parts.is_none() {
            check.digest = DigestCheck::Malformed;
        }
        let range = range.unwrap_or_default();
        let end = range[2].saturating_add(range[3]);
        let signed_end = usize::try_from(end).ok().filter(|end| *end <= bytes.len());
        let gap_is_contents = usize::try_from(range[1])
            .ok()
            .and_then(|at| bytes.get(at))
            .is_some_and(|byte| *byte == b'<')
            && usize::try_from(range[2])
                .ok()
                .and_then(|at| at.checked_sub(1))
                .and_then(|at| bytes.get(at))
                .is_some_and(|byte| *byte == b'>');
        let prefix = signed_end.and_then(|end| bytes.get(..end));
        let covers_revision = range[0] == 0 && gap_is_contents && prefix.is_some_and(ends_at_eof);
        let later_revisions = match prefix {
            Some(prefix) if covers_revision => PdfSource::new(Arc::<[u8]>::from(prefix))
                .ok()
                .and_then(|source| Document::open(source, limits).ok())
                .map(|signed| total_sections.saturating_sub(signed.xref().sections()))
                .unwrap_or(0),
            _ => 0,
        };
        reports.push(SignatureReport {
            field,
            sub_filter,
            algorithm: check.algorithm,
            digest: check.digest,
            signature: check.signature,
            signer: check.signer,
            signing_time: check.signing_time.or(modified),
            byte_range: range,
            covers_revision,
            bytes_after: (bytes.len() as u64).saturating_sub(end),
            later_revisions,
        });
    }
    Ok(reports)
}
