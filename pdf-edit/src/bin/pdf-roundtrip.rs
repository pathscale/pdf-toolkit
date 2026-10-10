//! Deterministic writer and incremental-editor round trips.
//!
//! Usage: pdf-roundtrip OUT_DIR [HOUSE_DIR | CLASSIC_PDF XREF_PDF]
//! HOUSE_DIR defaults to fixtures/house relative to the current directory.
//! CLASSIC_PDF XREF_PDF runs that pair without the house fixture pass.
//! A missing house directory yields one failure that names that path.

use std::env;
use std::fmt::Debug;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use pdf_core::pages::{Page, PageWalk};
use pdf_core::{Document, Limits, PdfSource, XrefKind};
use pdf_toolkit_edit::Editor;
use pdf_raster::Raster;
use pdf_render::RenderOptions;
use pdf_text::images::{ColorSpace, ImageData};
use pdf_text::semantic::{Link, LinkTarget, OutlineItem, SemanticDoc};
use pdf_write::doc::{Color, DocumentBuilder, StandardFont, WriteOptions};

#[path = "../roundtrip_edits.rs"]
mod editing;

type CheckResult<T> = Result<T, String>;

// Library defaults from Limits::default: max_file_bytes and max_total_decoded_bytes.
const MAX_FILE_BYTES: u64 = 1 << 30;
const MAX_TOTAL_BYTES: u64 = 4 << 30;
/// Pages compared per snapshot (first, last, and evenly spaced between).
/// Read limits come from Limits::default; this bounds only comparisons.
const MAX_PAGES: u32 = 16;
const MAX_STARTXREF_DIGITS: usize = 20;
const WIDTH: f32 = 360.0;
const HEIGHT: f32 = 300.0;
const TITLE: &str = "Round-trip café";
const AUTHOR: &str = "House author";
const SUBJECT: &str = "Writer round trips";
const URI: &str = "https://example.invalid/roundtrip";
const SECOND_URI: &str = "https://example.invalid/second";
const EDIT_URI: &str = "https://example.invalid/incremental";
const STANDARD_TEXT: &str = "Standard (escaped) \\ text: café €";
const FIRST_TEXT: &str = "Standard (escaped) \\ text: café € ABO";
const SECOND_TEXT: &str = "Helvetica Helvetica Bold Helvetica Oblique Times Roman \
    Times Bold Times Italic Courier Courier Bold OBA";
const CLASSIC_TEXT: &[&str] = &["Hello house PDF Second run"];
const STREAM_TEXT: &[&str] = &[
    "Compressed house page 1",
    "Compressed house page 2",
    "Compressed house page 3",
];
const RGBA: [u8; 16] = [
    255, 0, 0, 255,
    0, 255, 0, 128,
    0, 0, 255, 0,
    17, 34, 51, 64,
];
const BLACK: Color = Color { r: 0.0, g: 0.0, b: 0.0 };
const RED: Color = Color { r: 1.0, g: 0.0, b: 0.0 };
const GREEN: Color = Color { r: 0.0, g: 1.0, b: 0.0 };
const BLUE: Color = Color { r: 0.0, g: 0.0, b: 1.0 };
const CYAN: Color = Color { r: 0.0, g: 1.0, b: 1.0 };
const ORANGE: Color = Color { r: 1.0, g: 0.5, b: 0.0 };

#[derive(Default)]
struct Report {
    passed: u64,
    failed: u64,
    output_failed: bool,
}

impl Report {
    fn check(&mut self, name: &str, check: &str, passed: bool, detail: &str) {
        let status = if passed {
            self.passed = self.passed.saturating_add(1);
            "PASS"
        } else {
            self.failed = self.failed.saturating_add(1);
            "FAIL"
        };
        let result = if passed || detail.is_empty() {
            writeln!(io::stdout().lock(), "{status} {name} {check}")
        } else {
            let detail = detail.replace('\n', " ").replace('\r', " ");
            writeln!(io::stdout().lock(), "{status} {name} {check}: {detail}")
        };
        self.output_failed |= result.is_err();
    }

    fn equal<T: Debug + PartialEq>(
        &mut self,
        name: &str,
        check: &str,
        actual: &T,
        expected: &T,
    ) {
        let passed = actual == expected;
        let detail = if passed {
            String::new()
        } else {
            format!("expected {expected:?}, got {actual:?}")
        };
        self.check(name, check, passed, &detail);
    }

    fn obtain<T>(&mut self, name: &str, check: &str, result: CheckResult<T>) -> Option<T> {
        match result {
            Ok(value) => {
                self.check(name, check, true, "");
                Some(value)
            }
            Err(error) => {
                self.check(name, check, false, &error);
                None
            }
        }
    }

    fn finish(mut self) -> ExitCode {
        self.output_failed |= writeln!(
            io::stdout().lock(),
            "TOTAL pass={} fail={}",
            self.passed,
            self.failed,
        ).is_err();
        if self.failed != 0 || self.output_failed {
            ExitCode::FAILURE
        } else {
            ExitCode::SUCCESS
        }
    }
}

fn describe(error: impl Debug) -> String {
    format!("{error:?}")
}

fn bounded(kind: &str, requested: u64, maximum: u64) -> CheckResult<()> {
    if requested > maximum {
        Err(format!("{kind} requested {requested}, maximum {maximum}"))
    } else {
        Ok(())
    }
}

fn limits() -> Limits {
    // File bytes, page count, nesting, and every other read budget stay on the
    // library default. No field here had a documented reason to be tighter.
    Limits::default()
}

struct Config {
    output: PathBuf,
    classic: PathBuf,
    stream: PathBuf,
}

const DEFAULT_HOUSE_DIR: &str = "fixtures/house";

fn config() -> CheckResult<Config> {
    // Four arguments suffice to detect every unsupported argument count.
    let arguments: Vec<_> = env::args_os().skip(1).take(4).collect();
    let (output, classic, stream) = match arguments.as_slice() {
        [output] => {
            let house = PathBuf::from(DEFAULT_HOUSE_DIR);
            (
                PathBuf::from(output.as_os_str()),
                house.join("classic-one-page.pdf"),
                house.join("xref-stream-objstm.pdf"),
            )
        }
        [output, house] => {
            let house = PathBuf::from(house.as_os_str());
            (
                PathBuf::from(output.as_os_str()),
                house.join("classic-one-page.pdf"),
                house.join("xref-stream-objstm.pdf"),
            )
        }
        [output, classic, stream] => (
            PathBuf::from(output.as_os_str()),
            PathBuf::from(classic.as_os_str()),
            PathBuf::from(stream.as_os_str()),
        ),
        _ => return Err(
            "usage: pdf-roundtrip OUT_DIR [HOUSE_DIR | CLASSIC_PDF XREF_PDF]".to_owned(),
        ),
    };
    Ok(Config { output, classic, stream })
}

/// House directory for the argument split `config` accepts.
/// Pair mode (classic PDF plus xref PDF) returns `None`.
fn active_house_dir() -> Option<PathBuf> {
    let arguments: Vec<_> = env::args_os().skip(1).take(4).collect();
    match arguments.as_slice() {
        [_] => Some(PathBuf::from(DEFAULT_HOUSE_DIR)),
        [_, house] => Some(PathBuf::from(house.as_os_str())),
        _ => None,
    }
}

struct FixturePlan {
    /// Scripted house-fixture edits. Pair mode leaves this false.
    house_pass: bool,
    /// Classic and xref inputs. False when the house directory is missing.
    open_inputs: bool,
}

fn fixture_plan(report: &mut Report) -> FixturePlan {
    let Some(house) = active_house_dir() else {
        return FixturePlan { house_pass: false, open_inputs: true };
    };
    if house.is_dir() {
        return FixturePlan { house_pass: true, open_inputs: true };
    }
    report.check(
        "setup",
        "house-directory",
        false,
        &format!("missing fixture directory: {}", house.display()),
    );
    FixturePlan { house_pass: false, open_inputs: false }
}

fn read_bounded(path: &Path) -> CheckResult<Arc<[u8]>> {
    let file = File::open(path).map_err(describe)?;
    let metadata = file.metadata().map_err(describe)?;
    if !metadata.is_file() {
        return Err(format!("regular file required: {}", path.display()));
    }
    bounded("file_bytes", metadata.len(), MAX_FILE_BYTES)?;
    let capacity = usize::try_from(metadata.len()).map_err(describe)?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(capacity).map_err(describe)?;
    let read_limit = MAX_FILE_BYTES.checked_add(1)
        .ok_or_else(|| "file byte limit overflow".to_owned())?;
    file.take(read_limit).read_to_end(&mut bytes).map_err(describe)?;
    let requested = u64::try_from(bytes.len()).map_err(describe)?;
    bounded("file_bytes", requested, MAX_FILE_BYTES)?;
    Ok(Arc::from(bytes))
}

fn persist(
    directory: &Path,
    name: &str,
    bytes: &[u8],
    inputs: &[&Path],
) -> CheckResult<Arc<[u8]>> {
    bounded("file_bytes", u64::try_from(bytes.len()).map_err(describe)?, MAX_FILE_BYTES)?;
    let path = directory.join(format!("{name}.pdf"));
    if path.exists() {
        let destination = fs::canonicalize(&path).map_err(describe)?;
        for input in inputs.iter().take(2) {
            if destination == fs::canonicalize(input).map_err(describe)? {
                return Err("output would overwrite an input fixture".to_owned());
            }
        }
    }
    fs::write(&path, bytes).map_err(describe)?;
    let saved = read_bounded(&path)?;
    if saved.as_ref() != bytes {
        return Err("saved output differs from serialized bytes".to_owned());
    }
    Ok(saved)
}

/// One baseline grayscale MCU: zero DC and EOB yield an 8 by 8 gray image.
/// These JPEG bytes are constructed here, without an encoder or external asset.
fn jpeg_program() -> Arc<[u8]> {
    let mut bytes = Vec::with_capacity(160);
    bytes.extend_from_slice(&[0xff, 0xd8]);
    bytes.extend_from_slice(&[0xff, 0xdb, 0x00, 0x43, 0x00]);
    bytes.extend_from_slice(&[1; 64]);
    bytes.extend_from_slice(&[
        0xff, 0xc0, 0x00, 0x0b, 8,
        0x00, 0x08, 0x00, 0x08, 1, 1, 0x11, 0,
    ]);
    bytes.extend_from_slice(&[0xff, 0xc4, 0x00, 0x26]);
    for table in [0x00, 0x10] {
        bytes.push(table);
        bytes.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        bytes.push(0);
    }
    bytes.extend_from_slice(&[
        0xff, 0xda, 0x00, 0x08, 1, 1, 0x00, 0, 63, 0,
        0x3f, 0xff, 0xd9,
    ]);
    Arc::from(bytes)
}

fn document(font: Arc<[u8]>, jpeg: Arc<[u8]>) -> CheckResult<DocumentBuilder> {
    let mut document = DocumentBuilder::new();
    document.info(Some(TITLE), Some(AUTHOR), Some(SUBJECT));
    let standard = document.standard_font(StandardFont::Helvetica);
    let embedded = document.truetype_font(font).map_err(describe)?;
    let fonts: Vec<_> = [
        (StandardFont::Helvetica, "Helvetica"),
        (StandardFont::HelveticaBold, "Helvetica Bold"),
        (StandardFont::HelveticaOblique, "Helvetica Oblique"),
        (StandardFont::TimesRoman, "Times Roman"),
        (StandardFont::TimesBold, "Times Bold"),
        (StandardFont::TimesItalic, "Times Italic"),
        (StandardFont::Courier, "Courier"),
        (StandardFont::CourierBold, "Courier Bold"),
    ].into_iter().map(|(font, label)| (document.standard_font(font), label)).collect();
    let rgba = document.rgba(2, 2, &RGBA);
    let jpeg = document.jpeg(jpeg, 8, 8, 1);
    {
        let mut page = document.page(WIDTH, HEIGHT);
        page.text(standard, 14.0, 20.0, 260.0, STANDARD_TEXT, BLACK);
        page.text(embedded, 18.0, 20.0, 210.0, "ABO", BLACK);
        page.fill_rect(20.0, 130.0, 40.0, 30.0, RED);
        page.fill_rect(80.0, 130.0, 40.0, 30.0, GREEN);
        page.fill_rect(140.0, 130.0, 40.0, 30.0, BLUE);
        page.stroke_rect(200.0, 130.0, 40.0, 30.0, 4.0, BLACK);
        {
            let mut path = page.path();
            path.move_to(240.0, 40.0);
            path.line_to(280.0, 40.0);
            path.line_to(260.0, 80.0);
            path.close();
            path.fill(ORANGE, false);
        }
        {
            let mut path = page.path();
            path.move_to(220.0, 90.0);
            path.cubic_to(240.0, 120.0, 280.0, 60.0, 310.0, 90.0);
            path.stroke(3.0, BLUE);
        }
        page.image(rgba, 20.0, 40.0, 60.0, 60.0);
        page.image(jpeg, 100.0, 40.0, 48.0, 48.0);
        page.link_uri(20.0, 260.0, 280.0, 20.0, URI);
        page.link_page(20.0, 5.0, 120.0, 20.0, 1, 270.0);
    }
    {
        let mut page = document.page(WIDTH, HEIGHT);
        for (index, (font, label)) in fonts.into_iter().enumerate() {
            // Exactly eight fixed entries, with baselines from 270 to 95.
            let y = 270.0 - index as f32 * 25.0;
            page.text(font, 12.0, 20.0, y, label, BLACK);
        }
        page.text(embedded, 18.0, 20.0, 70.0, "OBA", BLACK);
        page.fill_rect(20.0, 20.0, 40.0, 40.0, CYAN);
        page.link_uri(20.0, 5.0, 140.0, 20.0, SECOND_URI);
    }
    document.outline("First page", 0, 0, 280.0);
    document.outline("Images and ABO", 1, 0, 210.0);
    document.outline("Second page", 0, 1, 280.0);
    Ok(document)
}

fn build(
    options: &WriteOptions,
    font: Arc<[u8]>,
    jpeg: Arc<[u8]>,
    sink: bool,
) -> CheckResult<Vec<u8>> {
    let document = document(font, jpeg)?;
    let bytes = if sink {
        let mut bytes = Vec::new();
        let count = document.finish_to(options, &mut bytes).map_err(describe)?;
        let actual = u64::try_from(bytes.len()).map_err(describe)?;
        if count != actual {
            return Err(format!("finish_to length: reported {count}, wrote {actual}"));
        }
        bytes
    } else {
        document.finish(options).map_err(describe)?
    };
    bounded("file_bytes", u64::try_from(bytes.len()).map_err(describe)?, MAX_FILE_BYTES)?;
    Ok(bytes)
}

struct Snapshot {
    document: Document,
    pages: Vec<Page>,
    text: Vec<String>,
    unmapped: Vec<u32>,
    semantic: SemanticDoc,
    rasters: Vec<Raster>,
}

/// Comparison page indexes, at most MAX_PAGES.
///
/// The first and last pages are included, with the other indexes spaced evenly
/// between them. A shorter document returns every page in order, which keeps
/// the house and writer checks on their existing pages.
fn sample_indexes(page_count: usize) -> Vec<usize> {
    let cap = MAX_PAGES as usize;
    if page_count <= cap {
        return (0..page_count).collect();
    }
    if cap <= 1 {
        return vec![0];
    }
    let last = page_count - 1;
    let span = cap - 1;
    let mut indexes = Vec::with_capacity(cap);
    for slot in 0..cap {
        let index = slot as u128 * last as u128 / span as u128;
        indexes.push(usize::try_from(index).unwrap_or(last));
    }
    indexes
}

fn snapshot(bytes: Arc<[u8]>, font: Arc<[u8]>) -> CheckResult<Snapshot> {
    bounded("file_bytes", u64::try_from(bytes.len()).map_err(describe)?, MAX_FILE_BYTES)?;
    let source = PdfSource::new(bytes).map_err(describe)?;
    let document = Document::open(source.clone(), limits()).map_err(describe)?;
    if document.encrypted() {
        return Err("encrypted document".to_owned());
    }
    let mut resolver = document.resolver();
    let mut pages = Vec::new();
    for page in PageWalk::new(&mut resolver).map_err(describe)? {
        pages.push(page.map_err(describe)?);
    }
    let options = RenderOptions { scale: 1.0, fallback_font: Some(font) };
    let mut text = Vec::new();
    let mut unmapped = Vec::new();
    let mut rasters = Vec::new();
    let mut retained = 0u64;
    // Sample order matches document order when there are at most MAX_PAGES pages.
    for index in sample_indexes(pages.len()) {
        let page = pages.get(index).ok_or_else(|| format!("missing sampled page {index}"))?;
        let interpreted = pdf_text::page_text(&mut resolver, page).map_err(describe)?;
        let plain = pdf_text::layout(&interpreted).plain_text();
        let raster = pdf_raster::render_page(
            &mut resolver, page, &options, [255; 4],
        ).map_err(describe)?;
        for bytes in [plain.len(), raster.rgba.len()] {
            retained = retained.checked_add(u64::try_from(bytes).map_err(describe)?)
                .ok_or_else(|| format!(
                    "total_decoded_bytes requested {}, maximum {MAX_TOTAL_BYTES}",
                    u64::MAX,
                ))?;
            bounded("total_decoded_bytes", retained, MAX_TOTAL_BYTES)?;
        }
        text.push(plain);
        unmapped.push(interpreted.unmapped);
        rasters.push(raster);
    }
    drop(resolver);
    let semantic = pdf_text::semantic::semantic_document(source, limits()).map_err(describe)?;
    for index in sample_indexes(semantic.pages.len()) {
        let page = semantic.pages.get(index)
            .ok_or_else(|| format!("missing sampled semantic page {index}"))?;
        if let Some(refusal) = page.refusal {
            return Err(format!("semantic page {} refused: {refusal}", page.index));
        }
    }
    Ok(Snapshot { document, pages, text, unmapped, semantic, rasters })
}

fn collapsed_equal(left: &str, right: &str) -> bool {
    left.split_whitespace().eq(right.split_whitespace())
}

fn check_text(report: &mut Report, name: &str, snapshot: &Snapshot, expected: &[&str]) {
    report.equal(name, "page-count", &snapshot.pages.len(), &expected.len());
    report.equal(name, "text-page-count", &snapshot.text.len(), &expected.len());
    report.equal(name, "semantic-page-count", &snapshot.semantic.pages.len(), &expected.len());
    for (index, expected) in expected.iter().take(MAX_PAGES as usize).enumerate() {
        let check = format!("page.{index}.text");
        match snapshot.text.get(index) {
            Some(actual) => {
                let passed = collapsed_equal(actual, expected);
                let preview: String = actual.chars().take(256).collect();
                report.check(
                    name, &check, passed,
                    &format!("expected {expected:?}, got {preview:?}"),
                );
            }
            None => report.check(name, &check, false, "missing page"),
        }
        report.equal(
            name, &format!("page.{index}.unmapped"),
            &snapshot.unmapped.get(index).copied(), &Some(0),
        );
    }
}

fn pixel(raster: &Raster, x: u32, pdf_y: u32) -> CheckResult<[u8; 4]> {
    if x >= raster.width {
        return Err("pixel outside raster width".to_owned());
    }
    let y = raster.height.checked_sub(pdf_y).and_then(|value| value.checked_sub(1))
        .ok_or_else(|| "pixel outside raster height".to_owned())?;
    let offset = u64::from(y).checked_mul(u64::from(raster.width))
        .and_then(|value| value.checked_add(u64::from(x)))
        .and_then(|value| value.checked_mul(4))
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| "pixel offset overflow".to_owned())?;
    let end = offset.checked_add(4).ok_or_else(|| "pixel end overflow".to_owned())?;
    let bytes = raster.rgba.get(offset..end)
        .ok_or_else(|| "pixel outside RGBA buffer".to_owned())?;
    let mut value = [0; 4];
    value.copy_from_slice(bytes);
    Ok(value)
}

fn check_pixels(
    report: &mut Report,
    name: &str,
    snapshot: &Snapshot,
    page: usize,
    probes: &[(u32, u32, [u8; 4])],
) {
    let Some(raster) = snapshot.rasters.get(page) else {
        report.check(name, &format!("page.{page}.pixels"), false, "missing raster");
        return;
    };
    report.equal(name, &format!("page.{page}.raster-size"), &(raster.width, raster.height), &(360, 300));
    for (x, y, expected) in probes.iter().take(32) {
        let check = format!("page.{page}.pixel.{x}.{y}");
        match pixel(raster, *x, *y) {
            // Same tolerance as the fixture manifests: anti-aliasing moves edge pixels a little.
            Ok(actual) => {
                let close = actual.iter().zip(expected).all(|(a, e)| a.abs_diff(*e) <= 8);
                report.check(name, &check, close, &format!("expected {expected:?}, got {actual:?}"));
            }
            Err(error) => report.check(name, &check, false, &error),
        }
    }
}

fn uri_link(bbox: [f32; 4], uri: &str) -> Link {
    Link { bbox, target: LinkTarget::Uri(uri.to_owned()) }
}

fn check_links(report: &mut Report, name: &str, snapshot: &Snapshot, page: usize, expected: &[Link]) {
    match snapshot.semantic.pages.get(page) {
        Some(page) => report.equal(
            name, &format!("page.{}.links", page.index),
            &page.links.as_slice(), &expected,
        ),
        None => report.check(name, &format!("page.{page}.links"), false, "missing page"),
    }
}

/// The writer takes level 0 for top items; the reader reports top items as level 1.
fn expected_outline() -> Vec<OutlineItem> {
    vec![
        OutlineItem {
            title: "First page".to_owned(),
            level: 1,
            target: Some(LinkTarget::Page { index: 0, y: Some(20.0) }),
        },
        OutlineItem {
            title: "Images and ABO".to_owned(),
            level: 2,
            target: Some(LinkTarget::Page { index: 0, y: Some(90.0) }),
        },
        OutlineItem {
            title: "Second page".to_owned(),
            level: 1,
            target: Some(LinkTarget::Page { index: 1, y: Some(20.0) }),
        },
    ]
}

fn check_images(report: &mut Report, name: &str, snapshot: &Snapshot, jpeg: &[u8]) {
    let Some(page) = snapshot.semantic.pages.first() else {
        report.check(name, "images", false, "missing first page");
        return;
    };
    report.equal(name, "page.0.image-count", &page.images.len(), &2);
    if let Some(page) = snapshot.semantic.pages.get(1) {
        report.equal(name, "page.1.image-count", &page.images.len(), &0);
    }
    let mut resolver = snapshot.document.resolver();
    for (label, width, height, bbox) in [
        ("rgba", 2, 2, [20.0, 200.0, 80.0, 260.0]),
        ("jpeg", 8, 8, [100.0, 212.0, 148.0, 260.0]),
    ] {
        let Some(reference) = page.images.iter().find(|image| {
            image.width == width && image.height == height
        }) else {
            report.check(name, &format!("image.{label}"), false, "missing image");
            continue;
        };
        report.equal(name, &format!("image.{label}.bbox"), &reference.bbox, &bbox);
        report.equal(name, &format!("image.{label}.inline"), &reference.inline, &false);
        let Some(id) = reference.object else {
            report.check(name, &format!("image.{label}.object"), false, "missing image object");
            continue;
        };
        let result = pdf_text::images::load_image(&mut resolver, id, 64).map_err(describe);
        let Some(image) = report.obtain(name, &format!("image.{label}.decode"), result) else {
            continue;
        };
        report.equal(name, &format!("image.{label}.size"), &(image.width, image.height), &(width, height));
        report.equal(name, &format!("image.{label}.bits"), &image.bits_per_component, &8);
        if label == "rgba" {
            report.equal(name, "image.rgba.color-space", &image.color_space, &ColorSpace::Rgb);
            report.equal(name, "image.rgba.soft-mask", &image.has_soft_mask, &true);
            match image.data {
                ImageData::Pixels { rgba } => report.check(
                    name, "image.rgba.equality", rgba.as_slice() == RGBA.as_slice(),
                    "decoded RGBA differs from the authored samples",
                ),
                _ => report.check(name, "image.rgba.equality", false, "expected decoded pixels"),
            }
        } else {
            report.equal(name, "image.jpeg.color-space", &image.color_space, &ColorSpace::Gray);
            report.equal(name, "image.jpeg.soft-mask", &image.has_soft_mask, &false);
            match image.data {
                ImageData::Jpeg(bytes) => report.check(
                    name, "image.jpeg.passthrough", bytes.as_slice() == jpeg,
                    "JPEG payload was changed",
                ),
                _ => report.check(name, "image.jpeg.passthrough", false, "expected encoded JPEG"),
            }
        }
    }
}

fn run_generated(
    report: &mut Report,
    config: &Config,
    name: &str,
    options: &WriteOptions,
    font: Arc<[u8]>,
    jpeg: Arc<[u8]>,
) {
    let Some(bytes) = report.obtain(
        name, "build", build(options, font.clone(), jpeg.clone(), false),
    ) else {
        return;
    };
    if let Some(second) = report.obtain(
        name, "second-build-to-sink", build(options, font.clone(), jpeg.clone(), true),
    ) {
        report.check(name, "determinism", bytes == second, "second build differs");
    }
    let Some(saved) = report.obtain(
        name, "save-and-read",
        persist(&config.output, name, &bytes, &[&config.classic, &config.stream]),
    ) else {
        return;
    };
    let Some(snapshot) = report.obtain(name, "read-back", snapshot(saved, font)) else {
        return;
    };
    check_text(report, name, &snapshot, &[FIRST_TEXT, SECOND_TEXT]);
    report.equal(name, "info.title", &snapshot.semantic.info.title.as_deref(), &Some(TITLE));
    report.equal(name, "info.author", &snapshot.semantic.info.author.as_deref(), &Some(AUTHOR));
    report.equal(name, "info.subject", &snapshot.semantic.info.subject.as_deref(), &Some(SUBJECT));
    report.equal(name, "outline", &snapshot.semantic.outline, &expected_outline());
    let expected_kind = if options.object_streams { XrefKind::Stream } else { XrefKind::Table };
    report.equal(name, "xref-kind", &snapshot.document.xref().kind(), &expected_kind);
    report.equal(name, "xref-sections", &snapshot.document.xref().sections(), &1);
    report.check(
        name, "object-streams",
        (snapshot.document.xref().counts().compressed != 0) == options.object_streams,
        "compressed-object presence differs from WriteOptions",
    );
    check_links(report, name, &snapshot, 0, &[
        uri_link([20.0, 20.0, 300.0, 40.0], URI),
        Link {
            bbox: [20.0, 275.0, 140.0, 295.0],
            target: LinkTarget::Page { index: 1, y: Some(30.0) },
        },
    ]);
    check_links(report, name, &snapshot, 1, &[
        uri_link([20.0, 275.0, 160.0, 295.0], SECOND_URI),
    ]);
    check_images(report, name, &snapshot, &jpeg);
    check_pixels(report, name, &snapshot, 0, &[
        (30, 140, [255, 0, 0, 255]),
        (50, 150, [255, 0, 0, 255]),
        (90, 140, [0, 255, 0, 255]),
        (110, 150, [0, 255, 0, 255]),
        (150, 140, [0, 0, 255, 255]),
        (170, 150, [0, 0, 255, 255]),
        (5, 145, [255; 4]),
        (220, 145, [255; 4]),
        (200, 145, [0, 0, 0, 255]),
        (260, 50, [255, 128, 0, 255]),
        (26, 216, [0, 0, 0, 255]),
        (59, 216, [0, 0, 0, 255]),
        (64, 218, [255; 4]),
        (35, 85, [255, 0, 0, 255]),
        (124, 64, [128, 128, 128, 255]),
    ]);
    check_pixels(report, name, &snapshot, 1, &[
        (40, 40, [0, 255, 255, 255]),
        (10, 40, [255; 4]),
    ]);
}

fn pdf_space(byte: u8) -> bool {
    matches!(byte, 0 | 9 | 10 | 12 | 13 | 32)
}

fn startxref(bytes: &[u8]) -> CheckResult<u64> {
    let tail = bytes.get(bytes.len().saturating_sub(1024)..)
        .ok_or_else(|| "missing PDF tail".to_owned())?;
    let marker = tail.windows(9).rposition(|window| window == b"startxref")
        .ok_or_else(|| "missing startxref".to_owned())?;
    let begin = marker.checked_add(9).ok_or_else(|| "startxref position overflow".to_owned())?;
    let rest = tail.get(begin..).ok_or_else(|| "missing startxref value".to_owned())?;
    let mut value = 0u64;
    let mut digits = 0usize;
    for byte in rest.iter().copied().skip_while(|byte| pdf_space(*byte)) {
        if !byte.is_ascii_digit() {
            if !pdf_space(byte) {
                return Err("invalid startxref delimiter".to_owned());
            }
            break;
        }
        digits = digits.saturating_add(1);
        bounded("startxref_digits", digits as u64, MAX_STARTXREF_DIGITS as u64)?;
        value = value.checked_mul(10).and_then(|value| value.checked_add(u64::from(byte - b'0')))
            .ok_or_else(|| "startxref integer overflow".to_owned())?;
    }
    if digits == 0 || value >= u64::try_from(bytes.len()).map_err(describe)? {
        return Err("startxref value outside source".to_owned());
    }
    Ok(value)
}

fn first_id(document: &Document) -> Option<Vec<u8>> {
    document.trailer().dict()?.get(b"ID")?.as_array()?.get(0)?.as_str()
        .map(|string| string.decode())
}

fn edit(bytes: Arc<[u8]>, title: &str) -> CheckResult<Editor> {
    let mut editor = Editor::open(bytes, limits()).map_err(describe)?;
    editor.set_info("Title", title);
    editor.add_uri_link(0, [30.0, 30.0, 130.0, 50.0], EDIT_URI).map_err(describe)?;
    Ok(editor)
}

fn run_incremental(
    report: &mut Report,
    config: &Config,
    name: &str,
    path: &Path,
    title: &str,
    expected_text: &[&str],
    kind: XrefKind,
    font: Arc<[u8]>,
) {
    let Some(original) = report.obtain(name, "read-input", read_bounded(path)) else {
        return;
    };
    let Some(before) = report.obtain(name, "read-original", snapshot(original.clone(), font.clone())) else {
        return;
    };
    // A house input has exactly the fixture pages; any other file (a real book) is checked only for what an
    // incremental edit must preserve, never against the fixture text, links or layout.
    let fixture = before.pages.len() == expected_text.len();
    let kind = if fixture { kind } else { before.document.xref().kind() };
    if fixture {
        check_text(report, &format!("{name}.original"), &before, expected_text);
        report.equal(name, "original.xref-sections", &before.document.xref().sections(), &1);
    }
    report.equal(name, "original.xref-kind", &before.document.xref().kind(), &kind);
    let Some(previous) = report.obtain(name, "original.startxref", startxref(&original)) else {
        return;
    };
    let Some(editor) = report.obtain(name, "edit", edit(original.clone(), title)) else {
        return;
    };
    let Some(bytes) = report.obtain(name, "incremental-save", editor.save().map_err(describe)) else {
        return;
    };
    report.check(
        name, "original-prefix", bytes.starts_with(&original),
        "incremental save changed the original byte prefix",
    );
    if let Some(second) = report.obtain(name, "repeat-save", editor.save().map_err(describe)) {
        report.check(name, "repeat-save-determinism", bytes == second, "repeat save differs");
    }
    if let Some(second_editor) = report.obtain(name, "second-edit", edit(original.clone(), title)) {
        if let Some(second) = report.obtain(name, "second-save", second_editor.save().map_err(describe)) {
            report.check(name, "independent-edit-determinism", bytes == second, "second edit differs");
        }
    }
    let Some(saved) = report.obtain(
        name, "save-and-read",
        persist(&config.output, name, &bytes, &[&config.classic, &config.stream]),
    ) else {
        return;
    };
    if let Some(newest) = report.obtain(name, "new.startxref", startxref(&saved)) {
        report.check(
            name, "appended-xref-offset",
            newest >= original.len() as u64,
            "new startxref points inside the original prefix",
        );
    }
    let Some(after) = report.obtain(name, "read-back", snapshot(saved, font)) else {
        return;
    };
    if fixture {
        check_text(report, name, &after, expected_text);
    }
    report.equal(name, "xref-kind", &after.document.xref().kind(), &kind);
    let sections = before.document.xref().sections().checked_add(1);
    report.equal(name, "xref-sections", &Some(after.document.xref().sections()), &sections);
    let prev = after.document.trailer().dict()
        .and_then(|dict| dict.get(b"Prev"))
        .and_then(|value| value.as_i64())
        .and_then(|value| u64::try_from(value).ok());
    report.equal(name, "prev", &prev, &Some(previous));
    report.equal(name, "root-preserved", &after.document.root_ref(), &before.document.root_ref());
    // A file without /ID gains one on save; only an existing first element must survive.
    match first_id(&before.document) {
        Some(original) => report.equal(name, "first-id-preserved", &first_id(&after.document), &Some(original)),
        None => report.check(name, "first-id-added", first_id(&after.document).is_some(), "no /ID after save"),
    }
    report.equal(name, "info.title", &after.semantic.info.title.as_deref(), &Some(title));
    report.equal(name, "info.author-preserved", &after.semantic.info.author, &before.semantic.info.author);
    report.equal(name, "info.subject-preserved", &after.semantic.info.subject, &before.semantic.info.subject);
    report.equal(name, "outline-preserved", &after.semantic.outline, &before.semantic.outline);
    report.check(
        name, "render-preserved", after.rasters == before.rasters,
        "incremental metadata or annotation edit changed rendered pixels",
    );
    if fixture {
        check_links(report, name, &after, 0, &[
            uri_link([30.0, 742.0, 130.0, 762.0], EDIT_URI),
        ]);
        for page in 1..expected_text.len().min(MAX_PAGES as usize) {
            check_links(report, name, &after, page, &[]);
        }
    }
    let added = before.pages.first().map(|page| page.annots + 1);
    report.equal(name, "page.0.annotations", &after.pages.first().map(|page| page.annots), &added);
}

fn main() -> ExitCode {
    let mut report = Report::default();
    let Some(config) = report.obtain("setup", "arguments", config()) else {
        return report.finish();
    };
    if report.obtain(
        "setup", "output-directory", fs::create_dir_all(&config.output).map_err(describe),
    ).is_none() {
        return report.finish();
    }
    let Some(font) = report.obtain(
        "setup", "house-font", house_font::program().map(Arc::<[u8]>::from).map_err(describe),
    ) else {
        return report.finish();
    };
    let jpeg = jpeg_program();
    let plan = fixture_plan(&mut report);
    if plan.house_pass {
        editing::run(&mut report, &config, font.clone());
    }
    for (name, compress, object_streams) in [
        ("writer-table-plain", false, false),
        ("writer-table-flate", true, false),
        ("writer-objstm-plain", false, true),
        ("writer-objstm-flate", true, true),
    ] {
        let options = WriteOptions {
            compress,
            object_streams,
            version: if object_streams { (1, 7) } else { (1, 4) },
        };
        run_generated(&mut report, &config, name, &options, font.clone(), jpeg.clone());
    }
    if plan.open_inputs {
        run_incremental(
            &mut report, &config, "edited-classic", &config.classic,
            "Edited classic café", CLASSIC_TEXT, XrefKind::Table, font.clone(),
        );
        run_incremental(
            &mut report, &config, "edited-xref-stream", &config.stream,
            "Edited stream café", STREAM_TEXT, XrefKind::Stream, font,
        );
    }
    report.finish()
}

/// Equivalent to pdf-write's private render-fixture TrueType generator.
/// The font maps A, B and O to a square, triangle and square with a hole.
mod house_font {
    use std::io;

    const MAX_FONT_BYTES: usize = 4096;
    const MAX_CONTOURS: usize = 2;
    const MAX_POINTS: usize = 8;
    const MAX_NAME_BYTES: usize = 64;

    const SQUARE: [[i16; 2]; 4] = [
        [100, 100], [100, 800], [800, 800], [800, 100],
    ];
    const TRIANGLE: [[i16; 2]; 3] = [
        [100, 100], [450, 800], [800, 100],
    ];
    const HOLE: [[i16; 2]; 4] = [
        [300, 300], [600, 300], [600, 600], [300, 600],
    ];

    fn invalid(message: &str) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidInput, message)
    }

    fn bounded(requested: usize, maximum: usize, kind: &str) -> io::Result<()> {
        if requested > maximum {
            return Err(invalid(&format!("{kind} requested {requested}, maximum {maximum}")));
        }
        Ok(())
    }

    fn at(base: usize, offset: usize) -> io::Result<usize> {
        base.checked_add(offset).ok_or_else(|| invalid("font offset overflow"))
    }

    fn size16(size: usize) -> io::Result<u16> {
        u16::try_from(size).map_err(|_| invalid(&format!(
            "font_u16 requested {size}, maximum {}", u16::MAX,
        )))
    }

    fn size32(size: usize) -> io::Result<u32> {
        u32::try_from(size).map_err(|_| invalid(&format!(
            "font_u32 requested {size}, maximum {}", u32::MAX,
        )))
    }

    fn append(out: &mut Vec<u8>, bytes: &[u8]) -> io::Result<()> {
        let length = at(out.len(), bytes.len())?;
        bounded(length, MAX_FONT_BYTES, "font_bytes")?;
        out.try_reserve(bytes.len()).map_err(|_| invalid(&format!(
            "font_allocation requested {length}, maximum {MAX_FONT_BYTES}",
        )))?;
        out.extend_from_slice(bytes);
        Ok(())
    }

    fn patch(out: &mut [u8], offset: usize, bytes: &[u8]) -> io::Result<()> {
        let end = at(offset, bytes.len())?;
        let target = out.get_mut(offset..end)
            .ok_or_else(|| invalid("font patch outside table"))?;
        target.copy_from_slice(bytes);
        Ok(())
    }

    fn put16(out: &mut Vec<u8>, value: u16) -> io::Result<()> {
        append(out, &value.to_be_bytes())
    }

    fn put_signed(out: &mut Vec<u8>, value: i16) -> io::Result<()> {
        append(out, &value.to_be_bytes())
    }

    fn put32(out: &mut Vec<u8>, value: u32) -> io::Result<()> {
        append(out, &value.to_be_bytes())
    }

    fn set16(out: &mut [u8], offset: usize, value: u16) -> io::Result<()> {
        patch(out, offset, &value.to_be_bytes())
    }

    fn set_signed(out: &mut [u8], offset: usize, value: i16) -> io::Result<()> {
        patch(out, offset, &value.to_be_bytes())
    }

    fn set32(out: &mut [u8], offset: usize, value: u32) -> io::Result<()> {
        patch(out, offset, &value.to_be_bytes())
    }

    fn align(out: &mut Vec<u8>) -> io::Result<()> {
        let padding = (4 - out.len() % 4) % 4;
        let zeros = [0u8; 3];
        append(out, zeros.get(..padding).ok_or_else(|| invalid("font alignment"))?)
    }

    fn checksum(bytes: &[u8]) -> u32 {
        // All callers supply font buffers capped at MAX_FONT_BYTES.
        let mut sum = 0u32;
        for chunk in bytes.chunks(4) {
            let mut word = [0u8; 4];
            for (slot, byte) in word.iter_mut().zip(chunk) {
                *slot = *byte;
            }
            sum = sum.wrapping_add(u32::from_be_bytes(word));
        }
        sum
    }

    fn simple_glyph(out: &mut Vec<u8>, contours: &[&[[i16; 2]]]) -> io::Result<()> {
        bounded(contours.len(), MAX_CONTOURS, "font_contours")?;
        let points = contours.iter().try_fold(
            0usize, |count, contour| at(count, contour.len()),
        )?;
        bounded(points, MAX_POINTS, "font_points")?;
        if contours.is_empty() || contours.iter().any(|contour| contour.is_empty()) {
            return Err(invalid("empty simple glyph contour"));
        }
        let mut bounds = [i16::MAX, i16::MAX, i16::MIN, i16::MIN];
        for [x, y] in contours.iter().flat_map(|contour| contour.iter()).copied() {
            bounds[0] = bounds[0].min(x);
            bounds[1] = bounds[1].min(y);
            bounds[2] = bounds[2].max(x);
            bounds[3] = bounds[3].max(y);
        }
        let count = i16::try_from(contours.len())
            .map_err(|_| invalid("font contour count"))?;
        put_signed(out, count)?;
        for value in bounds {
            put_signed(out, value)?;
        }
        let mut end = 0usize;
        for contour in contours {
            end = at(end, contour.len())?;
            let last = end.checked_sub(1)
                .ok_or_else(|| invalid("font contour endpoint"))?;
            put16(out, size16(last)?)?;
        }
        put16(out, 0)?;
        for _ in 0..points {
            append(out, &[1])?;
        }
        for axis in 0..2 {
            let mut previous = 0i16;
            for point in contours.iter().flat_map(|contour| contour.iter()) {
                let coordinate = point.get(axis).copied()
                    .ok_or_else(|| invalid("font axis"))?;
                let delta = coordinate.checked_sub(previous)
                    .ok_or_else(|| invalid("font coordinate delta overflow"))?;
                put_signed(out, delta)?;
                previous = coordinate;
            }
        }
        align(out)
    }

    fn head() -> io::Result<Vec<u8>> {
        let mut out = vec![0; 54];
        set32(&mut out, 0, 0x0001_0000)?;
        set32(&mut out, 4, 0x0001_0000)?;
        set32(&mut out, 12, 0x5f0f_3cf5)?;
        set16(&mut out, 16, 3)?;
        set16(&mut out, 18, 1000)?;
        for (offset, value) in [
            (36, 100), (38, 100), (40, 800), (42, 800), (48, 2), (50, 1),
        ] {
            set_signed(&mut out, offset, value)?;
        }
        set16(&mut out, 46, 8)?;
        Ok(out)
    }

    fn hhea() -> io::Result<Vec<u8>> {
        let mut out = vec![0; 36];
        set32(&mut out, 0, 0x0001_0000)?;
        set_signed(&mut out, 4, 800)?;
        set16(&mut out, 10, 1000)?;
        set_signed(&mut out, 14, 200)?;
        set_signed(&mut out, 16, 800)?;
        set_signed(&mut out, 18, 1)?;
        set16(&mut out, 34, 4)?;
        Ok(out)
    }

    fn maxp() -> io::Result<Vec<u8>> {
        let mut out = vec![0; 32];
        set32(&mut out, 0, 0x0001_0000)?;
        set16(&mut out, 4, 4)?;
        set16(&mut out, 6, 8)?;
        set16(&mut out, 8, 2)?;
        set16(&mut out, 14, 1)?;
        Ok(out)
    }

    fn hmtx() -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        for bearing in [0, 100, 100, 100] {
            put16(&mut out, 1000)?;
            put_signed(&mut out, bearing)?;
        }
        Ok(out)
    }

    fn cmap() -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        for value in [0, 1, 3, 1] {
            put16(&mut out, value)?;
        }
        put32(&mut out, 12)?;
        for value in [4, 48, 0, 8, 8, 2, 0] {
            put16(&mut out, value)?;
        }
        for value in [65, 66, 79, 0xffff] {
            put16(&mut out, value)?;
        }
        put16(&mut out, 0)?;
        for value in [65, 66, 79, 0xffff] {
            put16(&mut out, value)?;
        }
        for value in [-64, -64, -76, 1] {
            put_signed(&mut out, value)?;
        }
        for _ in 0..4 {
            put16(&mut out, 0)?;
        }
        Ok(out)
    }

    fn post() -> io::Result<Vec<u8>> {
        let mut out = vec![0; 32];
        set32(&mut out, 0, 0x0003_0000)?;
        set_signed(&mut out, 8, -100)?;
        set_signed(&mut out, 10, 50)?;
        set32(&mut out, 12, 1)?;
        Ok(out)
    }

    fn names() -> io::Result<Vec<u8>> {
        const STRINGS: [(u16, &str); 6] = [
            (1, "House Shapes"),
            (2, "Regular"),
            (3, "HouseShapes-Regular-1"),
            (4, "House Shapes Regular"),
            (5, "Version 1.000"),
            (6, "HouseShapes-Regular"),
        ];
        let mut out = vec![0; 78];
        set16(&mut out, 2, 6)?;
        set16(&mut out, 4, 78)?;
        for (index, (id, text)) in STRINGS.into_iter().enumerate() {
            bounded(text.len(), MAX_NAME_BYTES, "font_name_bytes")?;
            let offset = out.len().checked_sub(78)
                .ok_or_else(|| invalid("font name offset"))?;
            let length = text.encode_utf16().count().checked_mul(2)
                .ok_or_else(|| invalid("font name length overflow"))?;
            let record = index.checked_mul(12).and_then(|value| value.checked_add(6))
                .ok_or_else(|| invalid("font name record overflow"))?;
            for (field, value) in [3, 1, 0x0409, id, size16(length)?, size16(offset)?]
                .into_iter().enumerate()
            {
                let delta = field.checked_mul(2)
                    .ok_or_else(|| invalid("font name field overflow"))?;
                set16(&mut out, at(record, delta)?, value)?;
            }
            for unit in text.encode_utf16() {
                put16(&mut out, unit)?;
            }
        }
        Ok(out)
    }

    fn os2() -> io::Result<Vec<u8>> {
        let mut out = vec![0; 78];
        set_signed(&mut out, 2, 1000)?;
        for (offset, value) in [
            (4, 400), (6, 5), (10, 650), (12, 600), (16, 75),
            (18, 650), (20, 600), (24, 350), (26, 50), (28, 300),
            (62, 64), (64, 65), (66, 79), (74, 800),
        ] {
            set16(&mut out, offset, value)?;
        }
        set32(&mut out, 42, 1)?;
        patch(&mut out, 58, b"HOME")?;
        set_signed(&mut out, 68, 800)?;
        Ok(out)
    }

    pub(super) fn program() -> io::Result<Vec<u8>> {
        let mut glyf = Vec::new();
        let mut loca = Vec::new();
        put32(&mut loca, 0)?;
        let a: &[&[[i16; 2]]] = &[&SQUARE];
        let b: &[&[[i16; 2]]] = &[&TRIANGLE];
        let o: &[&[[i16; 2]]] = &[&SQUARE, &HOLE];
        for contours in [a, b, o] {
            put32(&mut loca, size32(glyf.len())?)?;
            simple_glyph(&mut glyf, contours)?;
        }
        put32(&mut loca, size32(glyf.len())?)?;
        let tables: [([u8; 4], Vec<u8>); 10] = [
            (*b"OS/2", os2()?),
            (*b"cmap", cmap()?),
            (*b"glyf", glyf),
            (*b"head", head()?),
            (*b"hhea", hhea()?),
            (*b"hmtx", hmtx()?),
            (*b"loca", loca),
            (*b"maxp", maxp()?),
            (*b"name", names()?),
            (*b"post", post()?),
        ];
        let mut out = vec![0; 172];
        set32(&mut out, 0, 0x0001_0000)?;
        set16(&mut out, 4, 10)?;
        set16(&mut out, 6, 128)?;
        set16(&mut out, 8, 3)?;
        set16(&mut out, 10, 32)?;
        let mut head_offset = None;
        for (index, (tag, table)) in tables.into_iter().enumerate() {
            align(&mut out)?;
            let offset = out.len();
            let record = index.checked_mul(16).and_then(|value| value.checked_add(12))
                .ok_or_else(|| invalid("font directory overflow"))?;
            patch(&mut out, record, &tag)?;
            set32(&mut out, at(record, 4)?, checksum(&table))?;
            set32(&mut out, at(record, 8)?, size32(offset)?)?;
            set32(&mut out, at(record, 12)?, size32(table.len())?)?;
            if tag == *b"head" {
                head_offset = Some(offset);
            }
            append(&mut out, &table)?;
            align(&mut out)?;
        }
        let offset = head_offset.ok_or_else(|| invalid("font head table missing"))?;
        let adjustment = 0xb1b0_afba_u32.wrapping_sub(checksum(&out));
        set32(&mut out, at(offset, 8)?, adjustment)?;
        Ok(out)
    }
}
