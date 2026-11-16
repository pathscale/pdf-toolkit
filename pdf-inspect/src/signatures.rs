//! Signature reports from `pdf_edit::sign::verify`: the `signatures`
//! subcommand and the `signature.*` check keys.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use crate::file;
use pdf_core::limits::Limits;
use pdf_edit::sign::{SignatureReport, verify};

fn values(index: usize, report: &SignatureReport) -> Vec<(String, String)> {
    let prefix = format!("signature.{index}");
    vec![
        (format!("{prefix}.field"), report.field.clone()),
        (format!("{prefix}.sub_filter"), report.sub_filter.clone()),
        (format!("{prefix}.algorithm"), report.algorithm.clone()),
        (format!("{prefix}.digest"), report.digest.name().to_string()),
        (
            format!("{prefix}.signature"),
            report.signature.name().to_string(),
        ),
        (
            format!("{prefix}.signer"),
            report.signer.clone().unwrap_or_default(),
        ),
        (
            format!("{prefix}.signing_time"),
            report.signing_time.clone().unwrap_or_default(),
        ),
        (
            format!("{prefix}.byte_range"),
            report.byte_range.map(|value| value.to_string()).join(" "),
        ),
        (
            format!("{prefix}.covers_revision"),
            report.covers_revision.to_string(),
        ),
        (
            format!("{prefix}.modified_after"),
            report.modified_after().to_string(),
        ),
        (
            format!("{prefix}.bytes_after"),
            report.bytes_after.to_string(),
        ),
        (
            format!("{prefix}.later_revisions"),
            report.later_revisions.to_string(),
        ),
    ]
}

/// Adds `signatures` and `signature.N.*` when the expectations ask for them.
pub(crate) fn fixture_values(
    actuals: &mut BTreeMap<String, String>,
    bytes: &[u8],
    limits: Limits,
    expectations: &[(String, String)],
) {
    if !expectations
        .iter()
        .any(|(key, _)| key == "signatures" || key.starts_with("signature."))
    {
        return;
    }
    match verify(Arc::from(bytes), limits) {
        Ok(reports) => {
            actuals.insert("signatures".to_string(), reports.len().to_string());
            for (index, report) in reports.iter().enumerate() {
                actuals.extend(values(index, report));
            }
        }
        Err(error) => {
            actuals.insert("signatures".to_string(), error.code().to_string());
        }
    }
}

pub(crate) fn run(path: &Path, limits: Limits) -> u8 {
    let source = match file::open(
        path,
        limits
            .max_file_bytes
            .min(crate::app_caps::hard("input-bytes")),
    ) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("pdf-inspect: {error}");
            return 2;
        }
    };
    let reports = match verify(Arc::from(source.bytes()), limits) {
        Ok(reports) => reports,
        Err(error) => {
            eprintln!("pdf-inspect: {}", error.code());
            return 2;
        }
    };
    println!("signatures={}", reports.len());
    // Every signature must check out over a whole revision, and every
    // revision from the earliest signed one to the final one must itself be
    // signed: an unsigned later update fails, even between two signatures.
    let mut accepted = !reports.is_empty();
    let mut final_revision_signed = false;
    let mut signed_revisions = BTreeSet::new();
    for (index, report) in reports.iter().enumerate() {
        accepted &= report.intact() && report.covers_revision;
        final_revision_signed |= report.strict_intact();
        signed_revisions.insert(report.later_revisions);
        for (key, value) in values(index, report) {
            println!("{key}={value}");
        }
    }
    // `later_revisions` is each signed revision's distance from the end, so
    // the signed revisions are unbroken up to the end exactly when the set
    // holds every distance from 0 to its largest.
    let unbroken = signed_revisions
        .last()
        .is_some_and(|&earliest| signed_revisions.len() as u64 == u64::from(earliest) + 1);
    if accepted && final_revision_signed && unbroken { 0 } else { 1 }
}
