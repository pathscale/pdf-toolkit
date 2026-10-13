//! Fixture keys read straight from the Info dictionary and page dictionaries:
//! `info.*` text strings and the page boxes other than MediaBox and CropBox.

use std::collections::BTreeMap;

use pdf_core::document::{Document, Resolver};
use pdf_core::work::ObjectRef;
use pdf_toolkit_inspect::format_number;

/// Decodes a PDF text string: UTF-16BE or UTF-8 with a byte order mark,
/// otherwise PDFDocEncoding, read as Latin-1 (identical for ASCII).
pub(crate) fn text_string(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xfe, 0xff]) {
        let units: Vec<u16> = rest
            .chunks(2)
            .map(|pair| {
                u16::from_be_bytes([
                    pair.first().copied().unwrap_or(0),
                    pair.get(1).copied().unwrap_or(0),
                ])
            })
            .collect();
        return char::decode_utf16(units)
            .map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER))
            .collect();
    }
    if let Some(rest) = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    bytes.iter().map(|byte| char::from(*byte)).collect()
}

/// Inserts `info.<key>` for each text entry present in the Info dictionary.
pub(crate) fn info_values(
    actuals: &mut BTreeMap<String, String>,
    document: &Document,
    resolver: &mut Resolver<'_>,
    title: bool,
) {
    let Some(info) = document.info_ref() else {
        return;
    };
    let Ok(object) = resolver.get(info) else {
        return;
    };
    let keys: [(&[u8], &str); 8] = [
        (b"Title", "title"),
        (b"Author", "author"),
        (b"Subject", "subject"),
        (b"Keywords", "keywords"),
        (b"Creator", "creator"),
        (b"Producer", "producer"),
        (b"CreationDate", "creation_date"),
        (b"ModDate", "mod_date"),
    ];
    for (key, label) in keys {
        if !title && label == "title" {
            continue;
        }
        let Some(dict) = object.dict() else { return };
        let Some(value) = dict.get(key) else { continue };
        let Ok(value) = resolver.resolve(value) else {
            continue;
        };
        let Some(text) = value.get().as_str() else {
            continue;
        };
        let bytes = resolver.decode_string(info, text);
        actuals.insert(format!("info.{label}"), text_string(&bytes));
    }
}

/// Inserts `page.N.bleed_box`, `trim_box` and `art_box` when the page sets them.
pub(crate) fn page_boxes(
    actuals: &mut BTreeMap<String, String>,
    prefix: &str,
    resolver: &mut Resolver<'_>,
    page: ObjectRef,
) {
    let Ok(object) = resolver.get(page) else {
        return;
    };
    let keys: [(&[u8], &str); 3] = [
        (b"BleedBox", "bleed_box"),
        (b"TrimBox", "trim_box"),
        (b"ArtBox", "art_box"),
    ];
    for (key, label) in keys {
        let Some(dict) = object.dict() else { return };
        let Some(value) = dict.get(key) else { continue };
        let Ok(value) = resolver.resolve(value) else {
            continue;
        };
        let Some(array) = value.get().as_array() else {
            continue;
        };
        let mut parts = Vec::new();
        for item in array.iter() {
            let number = item.as_f64();
            parts.push(number.map_or_else(|| "?".to_string(), format_number));
        }
        actuals.insert(format!("{prefix}.{label}"), parts.join(" "));
    }
}
