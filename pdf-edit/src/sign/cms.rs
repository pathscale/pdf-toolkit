//! Detached CMS `SignedData` for PDF signatures, built from `cms` and
//! `x509-cert` types. The SignerInfo is assembled here rather than with the
//! `cms` builder so the signing time comes from the caller, not the clock.

use cms::cert::{CertificateChoices, IssuerAndSerialNumber};
use cms::content_info::{CmsVersion, ContentInfo};
use cms::signed_data::{
    CertificateSet, EncapsulatedContentInfo, SignedData, SignerIdentifier, SignerInfo, SignerInfos,
};
use der::asn1::{OctetString, SetOfVec, UintRef};
use der::oid::ObjectIdentifier;
use der::{Any, DateTime, Decode, Encode};
use p256::ecdsa::signature::hazmat::{PrehashSigner, PrehashVerifier};
use pdf_core::DocError;
use sha2::Digest;
use x509_cert::Certificate;
use x509_cert::attr::Attribute;
use x509_cert::spki::AlgorithmIdentifierOwned;
use x509_cert::time::Time;

use super::{Curve, SignOptions, Signer, SubFilter, unsupported};
use crate::malformed;

pub(super) const ID_DATA: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.7.1");
pub(super) const ID_SIGNED_DATA: ObjectIdentifier =
    ObjectIdentifier::new_unwrap("1.2.840.113549.1.7.2");
pub(super) const ID_CONTENT_TYPE: ObjectIdentifier =
    ObjectIdentifier::new_unwrap("1.2.840.113549.1.9.3");
pub(super) const ID_MESSAGE_DIGEST: ObjectIdentifier =
    ObjectIdentifier::new_unwrap("1.2.840.113549.1.9.4");
pub(super) const ID_SIGNING_TIME: ObjectIdentifier =
    ObjectIdentifier::new_unwrap("1.2.840.113549.1.9.5");
pub(super) const ID_SIGNING_CERTIFICATE_V2: ObjectIdentifier =
    ObjectIdentifier::new_unwrap("1.2.840.113549.1.9.16.2.47");
pub(super) const ID_SHA256: ObjectIdentifier =
    ObjectIdentifier::new_unwrap("2.16.840.1.101.3.4.2.1");
pub(super) const ID_SHA384: ObjectIdentifier =
    ObjectIdentifier::new_unwrap("2.16.840.1.101.3.4.2.2");
pub(super) const ID_SHA512: ObjectIdentifier =
    ObjectIdentifier::new_unwrap("2.16.840.1.101.3.4.2.3");
pub(super) const ECDSA_SHA256: ObjectIdentifier =
    ObjectIdentifier::new_unwrap("1.2.840.10045.4.3.2");
pub(super) const ECDSA_SHA384: ObjectIdentifier =
    ObjectIdentifier::new_unwrap("1.2.840.10045.4.3.3");
pub(super) const ECDSA_SHA512: ObjectIdentifier =
    ObjectIdentifier::new_unwrap("1.2.840.10045.4.3.4");
pub(super) const ID_EC_PUBLIC_KEY: ObjectIdentifier =
    ObjectIdentifier::new_unwrap("1.2.840.10045.2.1");
pub(super) const SECP256R1: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.10045.3.1.7");
pub(super) const SECP384R1: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.132.0.34");

fn der_error(_: der::Error) -> DocError {
    malformed("signature DER")
}

fn key_error<E>(_: E) -> DocError {
    malformed("signature key")
}

/// The digest algorithm paired with each curve.
pub(super) fn digest_oid(curve: Curve) -> ObjectIdentifier {
    match curve {
        Curve::P256 => ID_SHA256,
        Curve::P384 => ID_SHA384,
    }
}

pub(super) fn digest(oid: ObjectIdentifier, parts: &[&[u8]]) -> Option<Vec<u8>> {
    fn run<D: Digest>(parts: &[&[u8]]) -> Vec<u8> {
        let mut hasher = D::new();
        for part in parts {
            hasher.update(part);
        }
        hasher.finalize().to_vec()
    }
    match oid {
        ID_SHA256 => Some(run::<sha2::Sha256>(parts)),
        ID_SHA384 => Some(run::<sha2::Sha384>(parts)),
        ID_SHA512 => Some(run::<sha2::Sha512>(parts)),
        _ => None,
    }
}

/// SEC1 uncompressed public key for the signer's scalar.
fn public_key(signer: &Signer) -> Result<Vec<u8>, DocError> {
    Ok(match signer.curve {
        Curve::P256 => p256::ecdsa::SigningKey::from_slice(&signer.scalar)
            .map_err(key_error)?
            .verifying_key()
            .to_sec1_point(false)
            .as_bytes()
            .to_vec(),
        Curve::P384 => p384::ecdsa::SigningKey::from_slice(&signer.scalar)
            .map_err(key_error)?
            .verifying_key()
            .to_sec1_point(false)
            .as_bytes()
            .to_vec(),
    })
}

/// Refuses a scalar that is not a valid key or does not match the certificate.
pub(super) fn check_signer(signer: &Signer) -> Result<(), DocError> {
    let point = public_key(signer)?;
    let certificate = Certificate::from_der(&signer.certificate).map_err(der_error)?;
    let info = certificate.tbs_certificate().subject_public_key_info();
    let curve = match signer.curve {
        Curve::P256 => SECP256R1,
        Curve::P384 => SECP384R1,
    };
    let parameter = info
        .algorithm
        .parameters
        .as_ref()
        .and_then(|any| any.decode_as::<ObjectIdentifier>().ok());
    if info.algorithm.oid != ID_EC_PUBLIC_KEY || parameter != Some(curve) {
        return Err(unsupported(
            "sign:certificate key is not on the signing curve",
        ));
    }
    if info.subject_public_key.raw_bytes() != point.as_slice() {
        return Err(malformed("signing key does not match the certificate"));
    }
    for extra in &signer.chain {
        Certificate::from_der(extra).map_err(der_error)?;
    }
    Ok(())
}

/// ECDSA over a prehash, returning the DER `ECDSA-Sig-Value`.
pub(crate) fn sign_prehash(
    curve: Curve,
    scalar: &[u8],
    prehash: &[u8],
) -> Result<Vec<u8>, DocError> {
    let raw = match curve {
        Curve::P256 => {
            let key = p256::ecdsa::SigningKey::from_slice(scalar).map_err(key_error)?;
            let signature: p256::ecdsa::Signature = key.sign_prehash(prehash).map_err(key_error)?;
            signature.to_bytes().to_vec()
        }
        Curve::P384 => {
            let key = p384::ecdsa::SigningKey::from_slice(scalar).map_err(key_error)?;
            let signature: p384::ecdsa::Signature = key.sign_prehash(prehash).map_err(key_error)?;
            signature.to_bytes().to_vec()
        }
    };
    let (r, s) = raw.split_at(raw.len() / 2);
    vec![
        UintRef::new(r).map_err(der_error)?,
        UintRef::new(s).map_err(der_error)?,
    ]
    .to_der()
    .map_err(der_error)
}

/// Verifies a DER `ECDSA-Sig-Value` over a prehash with a SEC1 public key.
/// `None` means the key or signature could not be parsed.
pub(super) fn verify_prehash(
    curve: Curve,
    point: &[u8],
    prehash: &[u8],
    der: &[u8],
) -> Option<bool> {
    let parts = Vec::<UintRef<'_>>::from_der(der).ok()?;
    let size = match curve {
        Curve::P256 => 32,
        Curve::P384 => 48,
    };
    let [r, s] = parts.as_slice() else {
        return None;
    };
    let mut raw = vec![0u8; size * 2];
    for (value, slot) in [(r, 0), (s, size)] {
        let bytes = value.as_bytes();
        if bytes.len() > size {
            return None;
        }
        raw.get_mut(slot + size - bytes.len()..slot + size)?
            .copy_from_slice(bytes);
    }
    Some(match curve {
        Curve::P256 => {
            let key = p256::ecdsa::VerifyingKey::from_sec1_bytes(point).ok()?;
            let signature = p256::ecdsa::Signature::from_slice(&raw).ok()?;
            key.verify_prehash(prehash, &signature).is_ok()
        }
        Curve::P384 => {
            let key = p384::ecdsa::VerifyingKey::from_sec1_bytes(point).ok()?;
            let signature = p384::ecdsa::Signature::from_slice(&raw).ok()?;
            key.verify_prehash(prehash, &signature).is_ok()
        }
    })
}

fn algorithm(oid: ObjectIdentifier) -> AlgorithmIdentifierOwned {
    AlgorithmIdentifierOwned {
        oid,
        parameters: None,
    }
}

fn attribute(oid: ObjectIdentifier, value: Any) -> Result<Attribute, DocError> {
    let mut values = SetOfVec::new();
    values.insert(value).map_err(der_error)?;
    Ok(Attribute { oid, values })
}

/// `SigningCertificateV2 ::= SEQUENCE { certs SEQUENCE OF ESSCertIDv2 }` with
/// one `ESSCertIDv2 { certHash }` (SHA-256 is the default hash algorithm).
fn signing_certificate_v2(certificate: &[u8]) -> Result<Any, DocError> {
    let hash = digest(ID_SHA256, &[certificate]).ok_or_else(|| malformed("certificate hash"))?;
    let cert_id =
        vec![Any::encode_from(&OctetString::new(hash).map_err(der_error)?).map_err(der_error)?];
    let certs = vec![Any::encode_from(&cert_id).map_err(der_error)?];
    let outer = vec![Any::encode_from(&certs).map_err(der_error)?];
    Any::encode_from(&outer).map_err(der_error)
}

/// Converts a PDF date to a UTC `Time` (UTCTime through 2049, as RFC 5652 asks).
pub(super) fn utc_time(date: &pdf_write::doc::PdfDate) -> Result<Time, DocError> {
    let local = DateTime::new(
        date.year,
        date.month,
        date.day,
        date.hour,
        date.minute,
        date.second,
    )
    .map_err(der_error)?;
    let offset = i64::from(date.offset_minutes) * 60;
    let seconds = i64::try_from(local.unix_duration().as_secs())
        .map_err(|_| malformed("signing time"))?
        - offset;
    let seconds = u64::try_from(seconds).map_err(|_| malformed("signing time"))?;
    let utc = DateTime::from_unix_duration(core::time::Duration::from_secs(seconds))
        .map_err(der_error)?;
    if utc.year() < 2050 {
        Ok(Time::UtcTime(
            der::asn1::UtcTime::from_date_time(utc).map_err(der_error)?,
        ))
    } else {
        Ok(Time::GeneralTime(
            der::asn1::GeneralizedTime::from_date_time(utc),
        ))
    }
}

/// A DER `SEQUENCE` around fields that are already encoded.
fn sequence(fields: &[Vec<u8>]) -> Result<Vec<u8>, DocError> {
    let body = fields.concat();
    let length = der::Length::try_from(body.len()).map_err(der_error)?;
    let mut out = der::Header::new(der::Tag::Sequence, length)
        .to_der()
        .map_err(der_error)?;
    out.extend_from_slice(&body);
    Ok(out)
}

/// Builds the DER `ContentInfo` holding a detached `SignedData` over `parts`.
pub(super) fn signed_data(
    signer: &Signer,
    options: &SignOptions,
    parts: &[&[u8]],
) -> Result<Vec<u8>, DocError> {
    let digest_algorithm = digest_oid(signer.curve);
    let message = digest(digest_algorithm, parts).ok_or_else(|| malformed("digest"))?;
    let certificate = Certificate::from_der(&signer.certificate).map_err(der_error)?;

    let mut attributes = SetOfVec::new();
    let content_type = Any::encode_from(&ID_DATA).map_err(der_error)?;
    attributes
        .insert(attribute(ID_CONTENT_TYPE, content_type)?)
        .map_err(der_error)?;
    let message_digest =
        Any::encode_from(&OctetString::new(message).map_err(der_error)?).map_err(der_error)?;
    attributes
        .insert(attribute(ID_MESSAGE_DIGEST, message_digest)?)
        .map_err(der_error)?;
    match options.sub_filter {
        SubFilter::Pkcs7Detached => {
            let time = Any::encode_from(&utc_time(&options.time)?).map_err(der_error)?;
            attributes
                .insert(attribute(ID_SIGNING_TIME, time)?)
                .map_err(der_error)?;
        }
        SubFilter::CadesDetached => {
            let value = signing_certificate_v2(&signer.certificate)?;
            attributes
                .insert(attribute(ID_SIGNING_CERTIFICATE_V2, value)?)
                .map_err(der_error)?;
        }
    }
    // The signature covers the DER of the attributes as a SET OF.
    let encoded = attributes.to_der().map_err(der_error)?;
    let prehash = digest(digest_algorithm, &[&encoded]).ok_or_else(|| malformed("digest"))?;
    let signature = sign_prehash(signer.curve, &signer.scalar, &prehash)?;
    let signature_algorithm = match signer.curve {
        Curve::P256 => ECDSA_SHA256,
        Curve::P384 => ECDSA_SHA384,
    };

    let signer_info = SignerInfo {
        version: CmsVersion::V1,
        sid: SignerIdentifier::IssuerAndSerialNumber(IssuerAndSerialNumber {
            issuer: certificate.tbs_certificate().issuer().clone(),
            serial_number: certificate.tbs_certificate().serial_number().clone(),
        }),
        digest_alg: algorithm(digest_algorithm),
        signed_attrs: Some(attributes),
        signature_algorithm: algorithm(signature_algorithm),
        signature: OctetString::new(signature).map_err(der_error)?,
        unsigned_attrs: None,
    };
    let mut certificates = SetOfVec::new();
    certificates
        .insert(CertificateChoices::Certificate(certificate))
        .map_err(der_error)?;
    for extra in &signer.chain {
        let extra = Certificate::from_der(extra).map_err(der_error)?;
        certificates
            .insert(CertificateChoices::Certificate(extra))
            .map_err(der_error)?;
    }
    let mut digest_algorithms = SetOfVec::new();
    digest_algorithms
        .insert(algorithm(digest_algorithm))
        .map_err(der_error)?;
    let mut signer_infos = SetOfVec::new();
    signer_infos.insert(signer_info).map_err(der_error)?;
    let signed = SignedData {
        version: CmsVersion::V1,
        digest_algorithms,
        encap_content_info: EncapsulatedContentInfo {
            econtent_type: ID_DATA,
            econtent: None,
        },
        certificates: Some(CertificateSet(certificates)),
        crls: None,
        signer_infos: SignerInfos(signer_infos),
    };
    let content = Any::encode_from(&signed).map_err(der_error)?;
    ContentInfo {
        content_type: ID_SIGNED_DATA,
        content,
    }
    .to_der()
    .map_err(der_error)
}

/// A self-signed X.509 v3 certificate for `scalar`, for test identities and
/// fixtures. `subject` is an RFC 4514 name such as `CN=Example,O=House`.
/// Deterministic: the signature uses RFC 6979 nonces.
pub fn self_signed_certificate(
    curve: Curve,
    scalar: &[u8],
    subject: &str,
    serial: u64,
    not_before: &pdf_write::doc::PdfDate,
    not_after: &pdf_write::doc::PdfDate,
) -> Result<Vec<u8>, DocError> {
    use core::str::FromStr;
    use der::asn1::ContextSpecific;
    use der::{TagMode, TagNumber};
    use x509_cert::certificate::{Rfc5280, TbsCertificate, Version};
    use x509_cert::name::Name;
    use x509_cert::serial_number::SerialNumber;
    use x509_cert::spki::SubjectPublicKeyInfoOwned;
    use x509_cert::time::Validity;

    let signer = Signer {
        curve,
        scalar: scalar.to_vec(),
        certificate: Vec::new(),
        chain: Vec::new(),
    };
    let point = public_key(&signer)?;
    let (curve_oid, signature_oid) = match curve {
        Curve::P256 => (SECP256R1, ECDSA_SHA256),
        Curve::P384 => (SECP384R1, ECDSA_SHA384),
    };
    let name = Name::from_str(subject).map_err(der_error)?;
    let serial = serial.to_be_bytes();
    let first = serial
        .iter()
        .position(|byte| *byte != 0)
        .unwrap_or(serial.len() - 1);
    // x509-cert 0.3 builds certificates only through its profile builder,
    // which adds extensions; the TBSCertificate is encoded field by field
    // instead and checked by decoding it back.
    let version = ContextSpecific {
        tag_number: TagNumber(0),
        tag_mode: TagMode::Explicit,
        value: Version::V3,
    };
    let key = SubjectPublicKeyInfoOwned {
        algorithm: AlgorithmIdentifierOwned {
            oid: ID_EC_PUBLIC_KEY,
            parameters: Some(Any::encode_from(&curve_oid).map_err(der_error)?),
        },
        subject_public_key: der::asn1::BitString::from_bytes(&point).map_err(der_error)?,
    };
    let validity = Validity::<Rfc5280>::new(utc_time(not_before)?, utc_time(not_after)?);
    let encoded = sequence(&[
        version.to_der().map_err(der_error)?,
        SerialNumber::<Rfc5280>::new(serial.get(first..).unwrap_or(&[1]))
            .map_err(der_error)?
            .to_der()
            .map_err(der_error)?,
        algorithm(signature_oid).to_der().map_err(der_error)?,
        name.to_der().map_err(der_error)?,
        validity.to_der().map_err(der_error)?,
        name.to_der().map_err(der_error)?,
        key.to_der().map_err(der_error)?,
    ])?;
    TbsCertificate::from_der(&encoded).map_err(der_error)?;
    let prehash = digest(digest_oid(curve), &[&encoded]).ok_or_else(|| malformed("digest"))?;
    let signature = sign_prehash(curve, scalar, &prehash)?;
    let certificate = sequence(&[
        encoded,
        algorithm(signature_oid).to_der().map_err(der_error)?,
        der::asn1::BitString::from_bytes(&signature)
            .map_err(der_error)?
            .to_der()
            .map_err(der_error)?,
    ])?;
    Certificate::from_der(&certificate).map_err(der_error)?;
    Ok(certificate)
}
