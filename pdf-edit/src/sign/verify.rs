//! Offline signature checks: byte range digest, ECDSA validity, signer
//! subject and what was appended after each signed revision.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use cms::cert::CertificateChoices;
use cms::content_info::ContentInfo;
use cms::signed_data::{SignedData, SignerIdentifier, SignerInfo};
use der::asn1::OctetString;
use der::oid::ObjectIdentifier;
use der::{Decode, Encode};
use pdf_core::{
    DocError, Document, LimitKind, Limits, ObjectRef, PdfSource, Refusal, Value, XrefEntry,
    XrefKind,
};
use x509_cert::Certificate;
use x509_cert::time::Time;

use super::cms::{
    ECDSA_SHA256, ECDSA_SHA384, ECDSA_SHA512, ID_EC_PUBLIC_KEY, ID_MESSAGE_DIGEST, ID_SHA256,
    ID_SHA384, ID_SHA512, ID_SIGNED_DATA, ID_SIGNING_TIME, SECP256R1, SECP384R1, digest,
    verify_prehash,
};
use super::{Curve, unsupported};

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
    /// Untrusted signer certificate subject, RFC 4514 style. Certificate chains
    /// are not validated.
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

    /// Digest and signature check out, cover a whole revision, and have no later changes.
    pub fn strict_intact(&self) -> bool {
        self.intact() && self.covers_revision && !self.modified_after()
    }

    /// True when bytes follow the signed range or any later revision exists,
    /// including a legitimate countersignature.
    pub fn modified_after(&self) -> bool {
        self.bytes_after > 0 || self.later_revisions > 0
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
fn signature_fields(
    document: &Document,
    maximum_fields: u32,
) -> Result<Vec<(String, ObjectRef, Option<ObjectRef>, Value)>, DocError> {
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
    let maximum = u64::from(maximum_fields);
    let mut scheduled = 0u64;
    if let Some(array) = fields.get().as_array() {
        for index in (0..array.len()).rev() {
            if let Some(id) = array.get(index).and_then(|item| item.as_ref()) {
                enqueue_field(
                    &mut stack,
                    &mut scheduled,
                    id,
                    String::new(),
                    false,
                    maximum,
                )?;
            }
        }
    }
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    while let Some((id, prefix, inherited_sig)) = stack.pop() {
        if !seen.insert((id.number(), id.generation())) {
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
                        enqueue_field(
                            &mut stack,
                            &mut scheduled,
                            kid,
                            name.clone(),
                            is_sig,
                            maximum,
                        )?;
                    }
                }
            }
        }
        if !is_sig {
            continue;
        }
        let signature_id = dict.get(b"V").and_then(|value| value.as_ref());
        if let Some(signature) = resolve_dict(&mut resolver, dict.get(b"V"))? {
            out.push((name, id, signature_id, signature));
        }
    }
    Ok(out)
}

fn enqueue_field(
    stack: &mut Vec<(ObjectRef, String, bool)>,
    scheduled: &mut u64,
    id: ObjectRef,
    prefix: String,
    inherited_sig: bool,
    maximum: u64,
) -> Result<(), DocError> {
    let requested = scheduled.checked_add(1).unwrap_or(u64::MAX);
    if requested > maximum {
        return Err(DocError::Refused(Refusal {
            limit: LimitKind::SignatureFields,
            requested,
            maximum,
        }));
    }
    *scheduled = requested;
    stack.push((id, prefix, inherited_sig));
    Ok(())
}

fn charge_signature_work(used: &mut u64, bytes: usize, maximum: u64) -> Result<(), DocError> {
    let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
    let requested = used.checked_add(bytes).unwrap_or(u64::MAX);
    if requested > maximum {
        return Err(DocError::Refused(Refusal {
            limit: LimitKind::SignatureWorkBytes,
            requested,
            maximum,
        }));
    }
    *used = requested;
    Ok(())
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

/// A PDF date (D:YYYYMMDDHHmmSSOHH'mm', trailing fields optional) as ISO 8601, keeping its UTC offset.
fn pdf_date_iso(raw: &str) -> Option<String> {
    let body = raw.strip_prefix("D:").unwrap_or(raw).as_bytes();
    let digits = body.iter().take_while(|byte| byte.is_ascii_digit()).count();
    if digits < 4 || !digits.is_multiple_of(2) || digits > 14 {
        return None;
    }
    let field = |at: usize, default: &str| {
        body.get(at..at + 2)
            .filter(|_| at + 2 <= digits)
            .map_or(default.to_owned(), |pair| {
                String::from_utf8_lossy(pair).into_owned()
            })
    };
    let year = String::from_utf8_lossy(&body[..4]);
    let date = format!(
        "{year}-{}-{}T{}:{}:{}",
        field(4, "01"),
        field(6, "01"),
        field(8, "00"),
        field(10, "00"),
        field(12, "00"),
    );
    let zone = &body[digits..];
    let zone = match zone.first() {
        None => String::new(),
        Some(b'Z') => "Z".to_owned(),
        Some(&sign @ (b'+' | b'-')) => {
            let numbers: Vec<u8> = zone[1..]
                .iter()
                .copied()
                .filter(u8::is_ascii_digit)
                .collect();
            if numbers.len() != 2 && numbers.len() != 4 {
                return None;
            }
            let hours = String::from_utf8_lossy(&numbers[..2]);
            let minutes = numbers
                .get(2..4)
                .map_or("00".into(), String::from_utf8_lossy);
            if hours == "00" && minutes == "00" {
                "Z".to_owned()
            } else {
                format!("{}{hours}:{minutes}", sign as char)
            }
        }
        Some(_) => return None,
    };
    Some(date + &zone)
}

fn signer_certificate<'a>(signed: &'a SignedData, info: &SignerInfo) -> Option<&'a Certificate> {
    let certificates = signed.certificates.as_ref()?;
    let mut first = None;
    for choice in certificates.0.iter() {
        let CertificateChoices::Certificate(certificate) = choice else {
            continue;
        };
        first.get_or_insert(certificate);
        if let SignerIdentifier::IssuerAndSerialNumber(id) = &info.sid
            && certificate.tbs_certificate().issuer() == &id.issuer
            && certificate.tbs_certificate().serial_number() == &id.serial_number
        {
            return Some(certificate);
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
    bytes.get(..end).is_some_and(|prefix| {
        prefix.ends_with(b"%%EOF")
            && (prefix.len() == 5 || matches!(prefix.get(prefix.len() - 6), Some(b'\r' | b'\n')))
    })
}

fn pdf_ws(byte: u8) -> bool {
    matches!(byte, 0 | 9 | 10 | 12 | 13 | 32)
}

fn skip_trivia(bytes: &[u8], mut pos: usize) -> Option<usize> {
    loop {
        while bytes.get(pos).is_some_and(|byte| pdf_ws(*byte)) {
            pos += 1;
        }
        if bytes.get(pos) != Some(&b'%') {
            return Some(pos);
        }
        let end = bytes[pos..]
            .iter()
            .position(|byte| matches!(byte, b'\r' | b'\n'))
            .map(|relative| pos + relative)
            .unwrap_or(bytes.len());
        let comment = bytes.get(pos..end)?;
        if comment.starts_with(b"%%EOF") && comment[5..].iter().all(|byte| pdf_ws(*byte)) {
            // Leave EOF to the grammar state: only the terminal-marker state
            // accepts it; every earlier state requires a different token.
            return Some(pos);
        }
        pos = end;
    }
}

fn token_at(bytes: &[u8], pos: usize, expected: &[u8]) -> Option<usize> {
    let end = pos.checked_add(expected.len())?;
    if bytes.get(pos..end)? != expected {
        return None;
    }
    if bytes.get(end).is_some_and(|byte| {
        !pdf_ws(*byte)
            && !matches!(
                *byte,
                b'%' | b'<' | b'>' | b'[' | b']' | b'(' | b')' | b'/'
            )
    }) {
        return None;
    }
    Some(end)
}

/// Validate the terminal revision grammar after the primary trailer/stream object.
/// Comments are allowed as trivia, but a comment containing an EOF marker is not.
fn terminal_tail(bytes: &[u8], anchor: usize, is_stream: bool) -> Option<usize> {
    let window_start = bytes.len().saturating_sub(1024);
    let mut pos = anchor;
    if is_stream {
        pos = skip_trivia(bytes, pos)?;
        pos = token_at(bytes, pos, b"endstream")?;
        pos = skip_trivia(bytes, pos)?;
        pos = token_at(bytes, pos, b"endobj")?;
    }
    pos = skip_trivia(bytes, pos)?;
    if pos < window_start {
        return None;
    }
    pos = token_at(bytes, pos, b"startxref")?;
    pos = skip_trivia(bytes, pos)?;
    let number_start = pos;
    while bytes.get(pos).is_some_and(u8::is_ascii_digit) {
        pos += 1;
    }
    if pos == number_start {
        return None;
    }
    let pointer = core::str::from_utf8(bytes.get(number_start..pos)?)
        .ok()?
        .parse::<u64>()
        .ok()?;
    pos = skip_trivia(bytes, pos)?;
    let eof_end = token_at(bytes, pos, b"%%EOF")?;
    if pos != 0 && !matches!(bytes.get(pos - 1), Some(b'\r' | b'\n')) {
        return None;
    }
    let after_eof = match (bytes.get(eof_end), bytes.get(eof_end + 1)) {
        (Some(b'\r'), Some(b'\n')) => eof_end + 2,
        (Some(b'\r' | b'\n'), _) => eof_end + 1,
        (None, _) => eof_end,
        _ => return None,
    };
    let trailing = skip_trivia(bytes, after_eof)?;
    (trailing == bytes.len()).then_some(usize::try_from(pointer).ok()?)
}

fn same_live_mapping(live: &Document, signed: &Document, id: ObjectRef) -> bool {
    let expected = Some(XrefEntry::InUse {
        offset: match live.xref().get(id.number()) {
            Some(XrefEntry::InUse { offset, generation }) if generation == id.generation() => offset,
            _ => return false,
        },
        generation: id.generation(),
    });
    live.xref().get(id.number()) == expected && signed.xref().get(id.number()) == expected
}

fn file_backed(value: &Value, document: &Document) -> bool {
    Arc::ptr_eq(value.object().buffer(), &document.source().shared())
}

fn contents_span(value: &Value) -> Option<std::ops::Range<usize>> {
    value
        .get()
        .as_dict()?
        .get(b"Contents")
        .filter(|contents| contents.as_str().is_some())
        .map(|contents| contents.span())
}

fn same_signature_value(
    live: &Document,
    signed: &Document,
    field_id: ObjectRef,
    signature_id: Option<ObjectRef>,
    outer: &Value,
) -> bool {
    let mut resolver = signed.resolver();
    let signed_value = if let Some(signature_id) = signature_id {
        if !same_live_mapping(live, signed, signature_id)
            || outer.object().id() != Some(signature_id)
            || !file_backed(outer, live)
        {
            return false;
        }
        let Ok(object) = resolver.get(signature_id) else { return false };
        if object.id() != Some(signature_id) {
            return false;
        }
        object.root().to_owned()
    } else {
        if !same_live_mapping(live, signed, field_id)
            || outer.object().id() != Some(field_id)
            || !file_backed(outer, live)
        {
            return false;
        }
        let Ok(field) = resolver.get(field_id) else { return false };
        if field.id() != Some(field_id) {
            return false;
        }
        let Some(value) = field.dict().and_then(|dict| dict.get(b"V")) else { return false };
        if value.as_ref().is_some() {
            return false;
        }
        value.to_owned()
    };
    if !file_backed(&signed_value, signed) || signed_value.get().as_dict().is_none() {
        return false;
    }
    let outer_span = outer.get().span();
    let signed_span = signed_value.get().span();
    outer_span == signed_span && contents_span(outer) == contents_span(&signed_value)
}

fn valid_signed_revision(bytes: &[u8], signed: &Document) -> bool {
    let (anchor, is_stream) = match signed.xref().kind() {
        XrefKind::Stream => match signed.trailer().stream_range() {
            Some(range) => (range.end, true),
            None => return false,
        },
        XrefKind::Table | XrefKind::Hybrid => {
            (signed.trailer().root().span().end, false)
        }
    };
    terminal_tail(bytes, anchor, is_stream).is_some_and(|offset| {
        signed.xref().exact_primary_offset() == Some(offset)
    })
}

/// Checks every signature field in the file. Encrypted files are refused.
pub fn verify(bytes: Arc<[u8]>, limits: Limits) -> Result<Vec<SignatureReport>, DocError> {
    let source = PdfSource::new(bytes.clone())?;
    let document = Document::open(source, limits)?;
    if document.encrypted() {
        return Err(unsupported("verify:encrypted document"));
    }
    let total_sections = document.xref().sections();
    let mut reports: Vec<SignatureReport> = Vec::new();
    let mut verified: BTreeMap<(u32, u16), SignatureReport> = BTreeMap::new();
    let mut verification_work_bytes = 0u64;
    for (field, field_id, signature_id, signature) in
        signature_fields(&document, limits.max_signature_fields)?
    {
        let cache_key = signature_id.map(|id| (id.number(), id.generation()));
        if let Some(cached) = cache_key.and_then(|key| verified.get(&key)) {
            let mut report = cached.clone();
            report.field = field;
            reports.push(report);
            continue;
        }
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
            .map(|string| {
                let raw = text(&string.decode());
                pdf_date_iso(&raw).unwrap_or(raw)
            });
        let range = byte_range(dict);
        let parts = range.and_then(|range| ranges(&bytes, range));
        let range = range.unwrap_or_default();
        let end = range[2].checked_add(range[3]);
        let signed_end = end
            .and_then(|end| usize::try_from(end).ok())
            .filter(|end| *end <= bytes.len());
        let contents_span = dict
            .get(b"Contents")
            .filter(|value| value.as_str().is_some())
            .map(|value| value.span());
        let gap_is_contents = contents_span.is_some_and(|span| {
            span.start <= span.end
                && span.end <= bytes.len()
                && usize::try_from(range[1]).ok() == Some(span.start)
                && usize::try_from(range[2]).ok() == Some(span.end)
                && bytes.get(span).is_some_and(|contents| {
                    contents.first() == Some(&b'<') && contents.last() == Some(&b'>')
                })
        });
        let prefix = signed_end.and_then(|end| bytes.get(..end));
        let preliminary_coverage =
            parts.is_some() && range[0] == 0 && gap_is_contents && prefix.is_some_and(ends_at_eof);
        let ranged_bytes = match parts {
            Some([first, second]) => first.len().checked_add(second.len()).unwrap_or(usize::MAX),
            None => 0,
        };
        let work_bytes = ranged_bytes
            .checked_add(contents.len())
            .and_then(|bytes| {
                bytes.checked_add(if preliminary_coverage {
                    prefix.map(|prefix| prefix.len()).unwrap_or(0)
                } else {
                    0
                })
            })
            .unwrap_or(usize::MAX);
        charge_signature_work(
            &mut verification_work_bytes,
            work_bytes,
            limits.max_signature_work_bytes,
        )?;
        let mut check = check_cms(&contents, parts);
        if parts.is_none() {
            check.digest = DigestCheck::Malformed;
        }
        let (covers_revision, later_revisions) = if preliminary_coverage {
            let signed = prefix
                .and_then(|prefix| PdfSource::new(Arc::<[u8]>::from(prefix)).ok())
                .and_then(|source| Document::open(source, limits).ok());
            match signed {
                Some(signed)
                    if prefix.is_some_and(|prefix| valid_signed_revision(prefix, &signed))
                        && same_signature_value(
                            &document,
                            &signed,
                            field_id,
                            signature_id,
                            &signature,
                        ) =>
                {
                    (
                        true,
                        total_sections.saturating_sub(signed.xref().sections()),
                    )
                }
                None => (false, 0),
                Some(_) => (false, 0),
            }
        } else {
            (false, 0)
        };
        let report = SignatureReport {
            field,
            sub_filter,
            algorithm: check.algorithm,
            digest: check.digest,
            signature: check.signature,
            signer: check.signer,
            signing_time: check.signing_time.or(modified),
            byte_range: range,
            covers_revision,
            bytes_after: end
                .map(|end| (bytes.len() as u64).saturating_sub(end))
                .unwrap_or(0),
            later_revisions,
        };
        if let Some(key) = cache_key {
            verified.insert(key, report.clone());
        }
        reports.push(report);
    }
    Ok(reports)
}

#[cfg(test)]
mod tests {
    use super::{
        DigestCheck, SignatureCheck, ends_at_eof, terminal_tail, verify,
    };
    use pdf_core::Limits;
    use std::sync::Arc;

    #[test]
    fn terminal_tail_accepts_one_table_tail_and_rejects_extra_eof_or_tail() {
        let valid = b"xref\ntrailer\n<< /Size 1 >>\nstartxref\n0\n%%EOF\n";
        let anchor = valid.windows(2).position(|window| window == b">>").unwrap() + 2;
        assert_eq!(terminal_tail(valid, anchor, false), Some(0));

        let orphan = b"xref\ntrailer\n<< /Size 1 >>\n%%EOF\nstartxref\n0\n%%EOF\n";
        let anchor = orphan.windows(2).position(|window| window == b">>").unwrap() + 2;
        assert_eq!(terminal_tail(orphan, anchor, false), None);

        let duplicate = b"xref\ntrailer\n<< /Size 1 >>\nstartxref\n0\n%%EOF\nstartxref\n0\n%%EOF\n";
        let anchor = duplicate.windows(2).position(|window| window == b">>").unwrap() + 2;
        assert_eq!(terminal_tail(duplicate, anchor, false), None);
    }

    #[test]
    fn terminal_tail_requires_stream_terminators_and_standalone_pointer() {
        let valid = b"<< /Type /XRef >>\nstream\ndata\nendstream\nendobj\nstartxref\r\n0\r\n%%EOF\r\n";
        let anchor = valid.windows(4).position(|window| window == b"data").unwrap() + 4;
        assert_eq!(terminal_tail(valid, anchor, true), Some(0));

        let missing_endobj = b"data\nendstream\nstartxref\n0\n%%EOF\n";
        assert_eq!(terminal_tail(missing_endobj, 4, true), None);

        let payload = b"xref\ntrailer\n<<>>\npayload\nstartxref\n0\n%%EOF\n";
        let anchor = payload.windows(2).position(|window| window == b">>").unwrap() + 2;
        assert_eq!(terminal_tail(payload, anchor, false), None);
    }

    #[test]
    fn eof_marker_must_be_a_standalone_line_and_padded_markers_are_not_trivia() {
        assert!(ends_at_eof(b"body\n%%EOF\n"));
        assert!(!ends_at_eof(b"body 0%%EOF\n"));

        let crlf_comment = b"xref\ntrailer\n<<>>\n% ordinary note\r\nstartxref\r\n0\r\n%%EOF\r\n";
        let anchor = crlf_comment.windows(2).position(|window| window == b">>").unwrap() + 2;
        assert_eq!(terminal_tail(crlf_comment, anchor, false), Some(0));

        let padded = b"xref\ntrailer\n<<>>\n%%EOF \nstartxref\n0\n%%EOF\n";
        let anchor = padded.windows(2).position(|window| window == b">>").unwrap() + 2;
        assert_eq!(terminal_tail(padded, anchor, false), None);

        let same_line = b"xref\ntrailer\n<<>>\nstartxref\n0 %%EOF\n";
        let anchor = same_line.windows(2).position(|window| window == b">>").unwrap() + 2;
        assert_eq!(terminal_tail(same_line, anchor, false), None);

        let attached = b"xref\ntrailer\n<<>>\nstartxref\n0%%EOF\n";
        let anchor = attached.windows(2).position(|window| window == b">>").unwrap() + 2;
        assert_eq!(terminal_tail(attached, anchor, false), None);

        let intervening = b"xref\ntrailer\n<<>>\nstartxref\n0\n%%EOF \t\n%%EOF\n";
        let anchor = intervening.windows(2).position(|window| window == b">>").unwrap() + 2;
        assert_eq!(terminal_tail(intervening, anchor, false), None);
    }

    #[test]
    fn terminal_parser_accepts_comments_but_still_requires_one_pointer() {
        let bytes = b"xref\ntrailer\n<<>>\nstartxref\n% startxref 0\n0\n%%EOF\n";
        let anchor = bytes.windows(2).position(|window| window == b">>").unwrap() + 2;
        let strict = terminal_tail(bytes, anchor, false).unwrap();
        assert_eq!(strict, 0);

        let plus_in_comment = b"xref\ntrailer\n<<>>\nstartxref\n% startxref +12\n0\n%%EOF\n";
        let anchor = plus_in_comment.windows(2).position(|window| window == b">>").unwrap() + 2;
        assert_eq!(terminal_tail(plus_in_comment, anchor, false), Some(0));
    }

    #[test]
    fn public_verifier_keeps_single_and_incremental_signature_controls() {
        let single = include_bytes!("../../../fixtures/house/author-signed.pdf");
        let reports = verify(Arc::from(single.as_slice()), Limits::default()).unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].digest, DigestCheck::Match);
        assert_eq!(reports[0].signature, SignatureCheck::Valid);
        assert!(reports[0].strict_intact());

        let twice = include_bytes!("../../../fixtures/house/author-signed-twice.pdf");
        let reports = verify(Arc::from(twice.as_slice()), Limits::default()).unwrap();
        assert_eq!(reports.len(), 2);
        assert!(reports.iter().all(|report| {
            report.digest == DigestCheck::Match && report.signature == SignatureCheck::Valid
        }));
        assert!(reports[0].covers_revision);
        assert_eq!(reports[0].later_revisions, 1);
        assert!(!reports[0].strict_intact());
        assert!(reports[1].strict_intact());
    }
}
