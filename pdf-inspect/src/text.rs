use std::collections::BTreeMap;
use std::io::{self, Write as _};
use std::path::Path;

use pdf_core::file;
use pdf_core::pages::Page;
use pdf_core::{Document, Limits};
use pdf_inspect::JsonWriter;
use pdf_text::extract::{extract_text, page_receipt, PageReceipt};
use pdf_text::layout::BlockKind;

use super::{
    append_resolver_warnings, inspect_pages, object_ref_text, refusal_from_error,
    write_file_info, write_refusal, write_warnings,
};

pub(super) fn run_text(path: &Path, limits: Limits) -> u8 {
    let source = match file::open(path, limits.max_file_bytes) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("{error}");
            return 3;
        }
    };
    let extraction = match extract_text(source, limits) {
        Ok(extraction) => extraction,
        Err(error) => {
            eprintln!("{error}");
            return 1;
        }
    };
    {
        let mut output = io::stdout().lock();
        if let Err(error) = output.write_all(extraction.text.as_bytes()) {
            eprintln!("{error}");
            return 3;
        }
        if let Err(error) = output.flush() {
            eprintln!("{error}");
            return 3;
        }
    }
    eprintln!("{}", extraction.note);
    0
}

fn write_box(json: &mut JsonWriter, bounds: [f32; 4]) {
    json.begin_array();
    for value in bounds {
        json.float(f64::from(value));
    }
    json.end_array();
}

fn kind_name(kind: BlockKind) -> &'static str {
    match kind {
        BlockKind::Paragraph => "paragraph",
        BlockKind::Heading(1) => "heading1",
        BlockKind::Heading(2) => "heading2",
        BlockKind::Heading(_) => "heading3",
        BlockKind::ListItem => "list_item",
        BlockKind::Caption => "caption",
    }
}

fn write_page(json: &mut JsonWriter, page: &PageReceipt) {
    json.begin_object();
    json.member("index");
    json.number(u64::from(page.index));
    json.member("id");
    json.string(&object_ref_text(page.id));
    json.member("width");
    json.float(f64::from(page.width));
    json.member("height");
    json.float(f64::from(page.height));
    json.member("rotate");
    json.number(u64::from(page.rotate));
    json.member("columns");
    json.number(u64::from(page.layout.columns));
    json.member("body_size");
    json.float(f64::from(page.layout.body_size));

    json.member("blocks");
    json.begin_array();
    for (index, block) in page.layout.blocks.iter().enumerate() {
        json.begin_object();
        json.member("id");
        json.string(&format!("p{}b{}", page.index, index));
        json.member("kind");
        json.string(kind_name(block.kind));
        json.member("bbox");
        write_box(json, block.bbox);
        json.member("text");
        json.string(&page.layout.block_text(block));
        json.member("lines");
        json.begin_array();
        let lines = page.layout.lines
            .get(block.lines.start as usize..block.lines.end as usize)
            .unwrap_or(&[]);
        for line in lines {
            json.begin_object();
            json.member("bbox");
            write_box(json, line.bbox);
            json.member("size");
            json.float(f64::from(line.size));
            json.member("text");
            json.string(&line.text);
            json.end_object();
        }
        json.end_array();
        json.end_object();
    }
    json.end_array();

    json.member("images");
    json.begin_array();
    for image in &page.images {
        json.begin_object();
        json.member("bbox");
        write_box(json, image.bbox);
        json.member("width");
        json.number(u64::from(image.width));
        json.member("height");
        json.number(u64::from(image.height));
        json.end_object();
    }
    json.end_array();

    json.member("coverage");
    json.begin_object();
    json.member("glyphs");
    json.number(page.glyphs);
    json.member("unmapped");
    json.number(u64::from(page.unmapped));
    json.member("ops");
    json.number(page.ops);
    json.end_object();
    json.member("refusal");
    let refusal = page.refusal.as_ref().map(refusal_from_error);
    write_refusal(json, refusal.as_ref());
    json.end_object();
}

pub(super) fn run_receipt(path: &Path, limits: Limits) -> u8 {
    let source = match file::open(path, limits.max_file_bytes) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("{error}");
            return 3;
        }
    };
    let mut json = JsonWriter::new();
    json.begin_object();
    json.member("receipt_version");
    json.number(1);
    json.member("source");
    write_file_info(&mut json, &source);

    match Document::open(source, limits) {
        Ok(document) => {
            let mut report = inspect_pages(&document);
            let mut resolver = document.resolver();
            json.member("pages");
            json.begin_array();
            for page in &report.pages {
                let receipt = page_receipt(&mut resolver, page);
                write_page(&mut json, &receipt);
            }
            json.end_array();
            append_resolver_warnings(&mut resolver, &mut report.warnings);
            json.member("warnings");
            write_warnings(&mut json, &report.warnings);
            json.member("refusal");
            write_refusal(&mut json, report.refusal.as_ref());
        }
        Err(error) => {
            json.member("pages");
            json.begin_array();
            json.end_array();
            json.member("warnings");
            write_warnings(&mut json, &[]);
            json.member("refusal");
            write_refusal(&mut json, Some(&refusal_from_error(&error)));
        }
    }
    json.end_object();
    println!("{}", json.finish());
    0
}

/// Materialize every layout, including during mutation checks.
/// A separate resolver keeps this pass independent of stream-only decoding.
pub(super) fn scan_text(
    document: &Document,
    pages: &[Page],
    refusal: &mut Option<String>,
) -> u64 {
    let mut resolver = document.resolver();
    let mut glyphs = 0u64;
    for page in pages {
        let receipt = page_receipt(&mut resolver, page);
        if let Some(error) = receipt.refusal.as_ref() {
            if refusal.is_none() {
                *refusal = Some(error.code().to_string());
            }
        }
        glyphs = glyphs.saturating_add(receipt.glyphs);
        let _ = std::hint::black_box(receipt);
    }
    glyphs
}

fn normalized(text: &str) -> String {
    let mut out = String::new();
    for word in text.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

pub(super) fn fixture_values(
    actuals: &mut BTreeMap<String, String>,
    prefix: &str,
    page: &PageReceipt,
) {
    actuals.insert(
        format!("{prefix}.text_refusal"),
        page.refusal.as_ref().map(|error| error.code())
            .unwrap_or("none").to_string(),
    );
    if page.refusal.is_some() {
        return;
    }
    actuals.insert(
        format!("{prefix}.text_norm"),
        normalized(&page.layout.plain_text()),
    );
    let headings = page.layout.blocks.iter()
        .filter(|block| matches!(block.kind, BlockKind::Heading(_)))
        .map(|block| normalized(&page.layout.block_text(block)))
        .collect::<Vec<_>>();
    actuals.insert(format!("{prefix}.headings"), headings.join(" | "));
    actuals.insert(
        format!("{prefix}.columns"),
        page.layout.columns.to_string(),
    );
    actuals.insert(
        format!("{prefix}.unmapped"),
        page.unmapped.to_string(),
    );
    actuals.insert(
        format!("{prefix}.ocr_required"),
        (page.glyphs == 0 && !page.images.is_empty()).to_string(),
    );
}
