//! Signature reports from `pdf_edit::sign::verify`: the `signatures`
//! subcommand and the `signature.*` check keys.

use std::collections::BTreeMap;
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
    let mut intact = true;
    for (index, report) in reports.iter().enumerate() {
        intact &= report.intact();
        for (key, value) in values(index, report) {
            println!("{key}={value}");
        }
    }
    if intact { 0 } else { 1 }
}
