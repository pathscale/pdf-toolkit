//! Signature proofs run by pdf-roundtrip, without unit tests: reproducible
//! signed fixtures, tamper and corruption detection, unsupported algorithms,
//! signatures surviving later incremental edits, and signing every house
//! fixture the editor accepts.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cms::content_info::ContentInfo;
use cms::signed_data::SignedData;
use der::oid::ObjectIdentifier;
use der::{Any, Decode, Encode};
use pdf_toolkit_edit::sign::{
    self, Curve, DigestCheck, SignOptions, SignatureCheck, SignatureReport,
};
use pdf_write::doc::PdfDate;

use super::sign_fixtures;
use super::{CheckResult, Config, Editor, Report, describe, read_bounded};

const SHA256_WITH_RSA: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.11");

fn require(value: bool, what: &str) -> CheckResult<()> {
    if value { Ok(()) } else { Err(what.to_owned()) }
}

fn verify(bytes: &[u8]) -> CheckResult<Vec<SignatureReport>> {
    sign::verify(Arc::from(bytes), sign_fixtures::limits()).map_err(describe)
}

/// Byte offsets of the hex digits inside signature `index`'s `/Contents`.
fn contents_span(bytes: &[u8], report: &SignatureReport) -> CheckResult<(usize, usize)> {
    let start = usize::try_from(report.byte_range[1]).map_err(describe)? + 1;
    let end = usize::try_from(report.byte_range[2]).map_err(describe)? - 1;
    require(
        bytes.get(start - 1) == Some(&b'<') && bytes.get(end) == Some(&b'>'),
        "contents span",
    )?;
    Ok((start, end))
}

fn hex_digits(bytes: &[u8]) -> CheckResult<Vec<u8>> {
    let mut out = Vec::new();
    for pair in bytes.chunks(2) {
        let text = std::str::from_utf8(pair).map_err(describe)?;
        out.push(u8::from_str_radix(text, 16).map_err(describe)?);
    }
    Ok(out)
}

fn der_length(der: &[u8]) -> CheckResult<usize> {
    let first = *der.get(1).ok_or("short DER")?;
    if first < 0x80 {
        return Ok(2 + usize::from(first));
    }
    let count = usize::from(first & 0x7f);
    let mut length = 0usize;
    for byte in der.get(2..2 + count).ok_or("short DER")? {
        length = (length << 8) | usize::from(*byte);
    }
    Ok(2 + count + length)
}

fn splice(bytes: &[u8], span: (usize, usize), der: &[u8]) -> CheckResult<Vec<u8>> {
    let mut hex = String::new();
    for byte in der {
        hex.push_str(&format!("{byte:02x}"));
    }
    require(hex.len() <= span.1 - span.0, "replacement CMS fits")?;
    while hex.len() < span.1 - span.0 {
        hex.push('0');
    }
    let mut out = bytes.to_vec();
    out.get_mut(span.0..span.1)
        .ok_or("contents span")?
        .copy_from_slice(hex.as_bytes());
    Ok(out)
}

fn reproducible(directory: &Path) -> CheckResult<()> {
    for fixture in sign_fixtures::build()? {
        let path = directory.join(format!("{}.pdf", fixture.name));
        let committed = read_bounded(&path)?;
        require(
            committed.as_ref() == fixture.pdf.as_slice(),
            "signed fixture bytes reproduce",
        )?;
        let expect = fs::read_to_string(path.with_extension("expect")).map_err(describe)?;
        require(
            expect == fixture.expected,
            "signed fixture expectations reproduce",
        )?;
    }
    Ok(())
}

fn twice(directory: &Path) -> CheckResult<()> {
    let bytes = read_bounded(&directory.join("author-signed-twice.pdf"))?;
    let reports = verify(&bytes)?;
    require(reports.len() == 2, "two signatures")?;
    require(
        reports.iter().all(SignatureReport::intact),
        "both signatures intact",
    )?;
    require(
        reports[0].later_revisions == 1 && !reports[1].modified_after(),
        "revision accounting",
    )
}

fn tampered(directory: &Path) -> CheckResult<()> {
    let bytes = read_bounded(&directory.join("author-signed.pdf"))?;
    let report = verify(&bytes)?.into_iter().next().ok_or("signature")?;
    // Flip one byte of the page content area, inside the first signed range.
    let mut changed = bytes.to_vec();
    let at = usize::try_from(report.byte_range[1] / 2).map_err(describe)?;
    let byte = changed.get_mut(at).ok_or("tamper offset")?;
    *byte ^= 0x01;
    let after = verify(&changed);
    match after {
        Ok(reports) => {
            let report = reports.first().ok_or("signature after tamper")?;
            require(
                report.digest == DigestCheck::Mismatch,
                "tampered digest mismatches",
            )
        }
        // A flipped byte may also break parsing; that is a refusal, not a pass.
        Err(error) => Err(format!("tampered file did not open: {error}")),
    }
}

fn corrupted(directory: &Path) -> CheckResult<()> {
    let bytes = read_bounded(&directory.join("author-signed.pdf"))?;
    let report = verify(&bytes)?.into_iter().next().ok_or("signature")?;
    let span = contents_span(&bytes, &report)?;
    let der = hex_digits(bytes.get(span.0..span.1).ok_or("contents")?)?;
    let length = der_length(&der)?;
    // The ECDSA value is the last field of the DER; flip its final byte.
    let mut changed = bytes.to_vec();
    let last = span.0 + length * 2 - 1;
    let digit = changed.get_mut(last).ok_or("signature digit")?;
    *digit = if *digit == b'0' { b'1' } else { b'0' };
    let report = verify(&changed)?.into_iter().next().ok_or("signature")?;
    require(
        report.digest == DigestCheck::Match,
        "corruption keeps the digest",
    )?;
    require(
        report.signature == SignatureCheck::Invalid,
        "corrupted signature is invalid",
    )
}

fn rsa_oid(directory: &Path) -> CheckResult<()> {
    let bytes = read_bounded(&directory.join("author-signed.pdf"))?;
    let report = verify(&bytes)?.into_iter().next().ok_or("signature")?;
    let span = contents_span(&bytes, &report)?;
    let der = hex_digits(bytes.get(span.0..span.1).ok_or("contents")?)?;
    let der = der.get(..der_length(&der)?).ok_or("DER length")?;
    let info = ContentInfo::from_der(der).map_err(describe)?;
    let mut signed = info.content.decode_as::<SignedData>().map_err(describe)?;
    let mut signers: Vec<_> = signed.signer_infos.0.iter().cloned().collect();
    let signer = signers.first_mut().ok_or("signer info")?;
    signer.signature_algorithm.oid = SHA256_WITH_RSA;
    signed.signer_infos.0 = signers.try_into().map_err(describe)?;
    let content = Any::encode_from(&signed).map_err(describe)?;
    let rebuilt = ContentInfo {
        content_type: info.content_type,
        content,
    }
    .to_der()
    .map_err(describe)?;
    let changed = splice(&bytes, span, &rebuilt)?;
    let report = verify(&changed)?.into_iter().next().ok_or("signature")?;
    require(report.algorithm == "rsa-sha256", "RSA algorithm named")?;
    require(
        report.signature == SignatureCheck::UnsupportedAlgorithm,
        "RSA is unsupported",
    )
}

fn survives_edits(signed: &[PathBuf]) -> CheckResult<()> {
    for path in signed {
        let bytes = read_bounded(path)?;
        let before = verify(&bytes)?;
        let mut editor = Editor::open(bytes, sign_fixtures::limits()).map_err(describe)?;
        editor.set_info("Subject", "Edited by pdf-roundtrip");
        editor
            .add_uri_link(0, [10.0, 10.0, 60.0, 30.0], "https://example.invalid/after")
            .map_err(describe)?;
        let edited = editor.save().map_err(describe)?;
        let after = verify(&edited)?;
        require(after.len() == before.len(), "signature count after edit")?;
        require(
            after.iter().all(SignatureReport::intact),
            "signatures intact after edit",
        )?;
        require(
            after.iter().all(SignatureReport::modified_after),
            "edit reported after signing",
        )?;
    }
    Ok(())
}

/// Signs one house fixture with each curve in turn and checks both.
fn sign_fixture(bytes: Arc<[u8]>) -> CheckResult<()> {
    let first = sign_fixtures::identity(Curve::P256)?;
    let second = sign_fixtures::identity(Curve::P384)?;
    let time = PdfDate::utc(2026, 5, 1, 10, 0, 0);
    let once = sign::sign(
        bytes,
        sign_fixtures::limits(),
        &first,
        &SignOptions::new("RoundTripA", time),
    )
    .map_err(describe)?;
    let twice = sign::sign(
        Arc::from(once),
        sign_fixtures::limits(),
        &second,
        &SignOptions::new("RoundTripB", time),
    )
    .map_err(describe)?;
    let reports = verify(&twice)?;
    let ours: Vec<_> = reports
        .iter()
        .filter(|report| report.field.starts_with("RoundTrip"))
        .collect();
    require(ours.len() == 2, "two new signatures")?;
    require(
        ours.iter()
            .all(|report| report.intact() && report.covers_revision),
        "new signatures intact",
    )?;
    require(
        reports.iter().all(SignatureReport::intact),
        "earlier signatures intact",
    )
}

/// PSChaCha files are never signed: the editor would encrypt the placeholders.
fn encrypted_refused(directory: &Path) -> CheckResult<()> {
    let bytes = read_bounded(&directory.join("author-encrypted.pdf"))?;
    let signer = sign_fixtures::identity(Curve::P256)?;
    let options = SignOptions::new("Refused", PdfDate::utc(2026, 5, 1, 10, 0, 0));
    match sign::sign(bytes, sign_fixtures::limits(), &signer, &options) {
        Err(pdf_core::DocError::Encrypted) => Ok(()),
        Err(error) => Err(format!("unexpected refusal: {error:?}")),
        Ok(_) => Err("encrypted file was signed".to_owned()),
    }
}

pub(super) fn run(report: &mut Report, config: &Config) {
    let Some(directory) = config.classic.parent().map(Path::to_path_buf) else {
        report.check(
            "sign",
            "house-directory",
            false,
            "fixture directory missing",
        );
        return;
    };
    report.obtain("sign", "fixtures-reproduce", reproducible(&directory));
    report.obtain("sign", "double-signature", twice(&directory));
    report.obtain("sign", "tamper-digest-mismatch", tampered(&directory));
    report.obtain("sign", "corrupt-signature-invalid", corrupted(&directory));
    report.obtain("sign", "rsa-unsupported", rsa_oid(&directory));
    report.obtain("sign", "encrypted-refused", encrypted_refused(&directory));

    let mut paths = Vec::new();
    if let Ok(entries) = fs::read_dir(&directory) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|extension| extension == "pdf") {
                paths.push(path);
            }
        }
    }
    paths.sort();
    let signed: Vec<_> = paths
        .iter()
        .filter(|path| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(|stem| stem.starts_with("author-signed"))
        })
        .cloned()
        .collect();
    report.obtain("sign", "signatures-survive-edits", survives_edits(&signed));

    for path in &paths {
        let stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("fixture");
        let name = format!("sign-{stem}");
        let Some(bytes) = report.obtain(&name, "read", read_bounded(path)) else {
            continue;
        };
        // Files the editor cannot open (encrypted without a password, broken
        // cross references) are not signable; wave11 checks those refusals.
        if Editor::open(bytes.clone(), sign_fixtures::limits()).is_err() {
            report.check(&name, "editor-refuses", true, "");
            continue;
        }
        report.obtain(&name, "sign-twice-verify", sign_fixture(bytes));
    }
}
