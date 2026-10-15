use rusty_alloc::RustyAlloc as System;
use std::alloc::{GlobalAlloc, Layout};
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use pdf_core::document::{Document, Resolver, XrefEntry, XrefKind};
use pdf_core::error::{DocError, Warning};
mod file {
    pub use pdf_core::file::FileOpenError;
    pub fn open_repair(
        path: &std::path::Path,
        maximum: u64,
    ) -> Result<pdf_core::source::PdfSource, FileOpenError> {
        crate::app_caps::stage("repair PDF source");
        if let Ok(metadata) = std::fs::metadata(path) {
            crate::app_caps::check("input-bytes", metadata.len());
        }
        let result =
            pdf_core::file::open_repair(path, maximum.min(crate::app_caps::hard("input-bytes")));
        if let Err(FileOpenError::TooLarge { observed, .. }) = &result {
            crate::app_caps::check("input-bytes", *observed);
        }
        result
    }
    pub fn open(
        path: &std::path::Path,
        maximum: u64,
    ) -> Result<pdf_core::source::PdfSource, pdf_core::error::DocError> {
        crate::app_caps::stage(std::sync::Arc::<str>::from(format!(
            "read PDF {}",
            path.display()
        )));
        if let Ok(metadata) = std::fs::metadata(path) {
            crate::app_caps::check("input-bytes", metadata.len());
        }
        pdf_core::file::open(path, maximum.min(crate::app_caps::hard("input-bytes"))).map_err(
            |error| match error {
                FileOpenError::Source(error) => pdf_core::error::DocError::Source(error),
                FileOpenError::TooLarge { limit, observed } => {
                    crate::app_caps::check("input-bytes", observed);
                    pdf_core::error::DocError::Refused(pdf_core::limits::Refusal {
                        limit: pdf_core::limits::LimitKind::FileBytes,
                        requested: observed,
                        maximum: limit,
                    })
                }
                FileOpenError::Io(error) => {
                    eprintln!("pdf-inspect: {}: {error}", path.display());
                    pdf_core::error::DocError::Malformed {
                        offset: 0,
                        what: "PDF source I/O",
                    }
                }
            },
        )
    }
}
use pdf_core::limits::Limits;
use pdf_core::object::{DictRef, Object};
use pdf_core::pages::{Page, PageWalk, page_count_hint};
use pdf_core::source::PdfSource;
use pdf_core::work::ObjectRef;
use pdf_raster::{encode_png, render_page as rasterize_page};
use pdf_render::RenderOptions;
use pdf_toolkit_inspect::{JsonWriter, Sha256, digest_hex, format_number};

/// println! that ends the process quietly once stdout is closed, for example by `head`.
macro_rules! out {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        if writeln!(std::io::stdout().lock(), $($arg)*).is_err() {
            std::process::exit(0);
        }
    }};
}

mod metadata;
mod signatures;
mod text;

struct CountingAllocator;

static CURRENT_BYTES: AtomicUsize = AtomicUsize::new(0);
static PEAK_BYTES: AtomicUsize = AtomicUsize::new(0);

#[global_allocator]
static GLOBAL_ALLOCATOR: CountingAllocator = CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            record_allocation(layout.size());
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            record_allocation(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
        record_free(layout.size());
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let result = unsafe { System.realloc(pointer, layout, new_size) };
        if !result.is_null() {
            if new_size >= layout.size() {
                record_allocation(new_size.saturating_sub(layout.size()));
            } else {
                record_free(layout.size().saturating_sub(new_size));
            }
        }
        result
    }
}

fn record_allocation(size: usize) {
    let current = CURRENT_BYTES
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            Some(value.saturating_add(size))
        })
        .map(|previous| previous.saturating_add(size))
        .unwrap_or(usize::MAX);

    let mut peak = PEAK_BYTES.load(Ordering::Relaxed);
    while current > peak {
        match PEAK_BYTES.compare_exchange_weak(peak, current, Ordering::Relaxed, Ordering::Relaxed)
        {
            Ok(_) => break,
            Err(observed) => peak = observed,
        }
    }
}

fn record_free(size: usize) {
    let _ = CURRENT_BYTES.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        Some(value.saturating_sub(size))
    });
}

fn reset_peak_bytes() {
    PEAK_BYTES.store(CURRENT_BYTES.load(Ordering::Relaxed), Ordering::Relaxed);
}

fn peak_bytes() -> u64 {
    PEAK_BYTES.load(Ordering::Relaxed) as u64
}

#[path = "app_caps.rs"]
mod app_caps;

fn main() -> std::process::ExitCode {
    let _caps = app_caps::start([4294967296, 600, 900, 512, 2, 32, 268435456, 10000, 10000]);
    app_caps::stage("pdf-inspect/src/main.rs: running");
    std::process::ExitCode::from(run())
}

fn run() -> u8 {
    let args: Vec<OsString> = app_caps::args_os().skip(1).collect();
    app_caps::stage(Arc::<str>::from(format!("pdf-inspect {args:?}")));
    let Some(command_arg) = args.first() else {
        print_usage();
        return 1;
    };
    let Some(command) = command_arg.to_str() else {
        print_usage();
        return 1;
    };

    match command {
        "summary" => match parse_summary_args(&args) {
            Some((path, limits, repair)) => run_summary(&path, limits, repair),
            None => {
                print_usage();
                1
            }
        },
        "text" => match parse_path_repair(&args) {
            Some((path, repair)) => text::run_text(&path, Limits::default(), repair),
            None => {
                print_usage();
                1
            }
        },
        "receipt" => match parse_single_path(&args) {
            Some(path) => text::run_receipt(&path, Limits::default()),
            None => {
                print_usage();
                1
            }
        },
        "repair" => match parse_single_path(&args) {
            Some(path) => run_repair(&path, Limits::default()),
            None => {
                print_usage();
                1
            }
        },
        "semantic" => match parse_path_repair(&args) {
            Some((path, repair)) => run_semantic(&path, Limits::default(), repair),
            None => {
                print_usage();
                1
            }
        },
        "image" => match parse_image_args(&args) {
            Some((path, object_number, output)) => {
                run_image(&path, object_number, &output, Limits::default())
            }
            None => {
                print_usage();
                1
            }
        },
        "render" => match parse_render_args(&args) {
            Some((path, page, output, dpi, font, repair)) => run_render(
                &path,
                page,
                &output,
                dpi,
                font.as_deref(),
                repair,
                Limits::default(),
            ),
            None => {
                print_usage();
                1
            }
        },
        "streams" => match parse_single_path(&args) {
            Some(path) => run_streams(&path, Limits::default()),
            None => {
                print_usage();
                1
            }
        },
        "bench" => match parse_bench_args(&args) {
            Some((paths, repeat)) => run_bench(&paths, repeat, Limits::default()),
            None => {
                print_usage();
                1
            }
        },
        "mutate" => match parse_mutate_args(&args) {
            Some((path, seed, count)) => run_mutate(&path, seed, count),
            None => {
                print_usage();
                1
            }
        },
        "signatures" => match parse_single_path(&args) {
            Some(path) => signatures::run(&path, Limits::default()),
            None => {
                print_usage();
                1
            }
        },
        "check" => match parse_single_path(&args) {
            Some(path) => run_check(&path),
            None => {
                print_usage();
                1
            }
        },
        _ => {
            print_usage();
            1
        }
    }
}

fn print_usage() {
    eprintln!(
        "usage:\n  pdf-inspect summary FILE [--max-decoded N] [--max-depth N] \
         [--max-prev-chain N] [--max-pages N] [--repair]\n  pdf-inspect streams FILE\n  \
         pdf-inspect text FILE [--repair]\n  pdf-inspect receipt FILE\n  \
         pdf-inspect semantic FILE [--repair]\n  pdf-inspect image FILE OBJ_NUM OUT\n  \
         pdf-inspect render FILE PAGE OUT.png [--dpi N] [--font FILE] [--repair]\n  \
         pdf-inspect bench FILE... [--repeat N]\n  \
         pdf-inspect mutate FILE --seed N --count K\n  pdf-inspect signatures FILE\n  \
         pdf-inspect check DIR"
    );
}

fn parse_single_path(args: &[OsString]) -> Option<PathBuf> {
    if args.len() != 2 {
        return None;
    }
    args.get(1).map(PathBuf::from)
}

fn parse_path_repair(args: &[OsString]) -> Option<(PathBuf, bool)> {
    match args.len() {
        2 => Some((PathBuf::from(args.get(1)?), false)),
        3 if args.get(2)?.to_str()? == "--repair" => Some((PathBuf::from(args.get(1)?), true)),
        _ => None,
    }
}

fn parse_image_args(args: &[OsString]) -> Option<(PathBuf, u32, PathBuf)> {
    if args.len() != 4 {
        return None;
    }
    Some((
        PathBuf::from(args.get(1)?),
        parse_u32_argument(args.get(2)?)?,
        PathBuf::from(args.get(3)?),
    ))
}

fn parse_render_args(
    args: &[OsString],
) -> Option<(PathBuf, u32, PathBuf, u32, Option<PathBuf>, bool)> {
    if args.len() < 4 {
        return None;
    }

    let path = PathBuf::from(args.get(1)?);
    let page = parse_u32_argument(args.get(2)?)?;
    let output = PathBuf::from(args.get(3)?);
    let mut dpi = 72u32;
    let mut font = None;
    let mut repair = false;
    let mut saw_dpi = false;
    let mut saw_font = false;
    let mut saw_repair = false;
    let mut index = 4usize;

    while index < args.len() {
        match args.get(index)?.to_str()? {
            "--dpi" if !saw_dpi => {
                dpi = parse_u32_argument(args.get(index.saturating_add(1))?)?;
                if dpi == 0 {
                    return None;
                }
                saw_dpi = true;
                index = index.saturating_add(2);
            }
            "--font" if !saw_font => {
                font = Some(PathBuf::from(args.get(index.saturating_add(1))?));
                saw_font = true;
                index = index.saturating_add(2);
            }
            "--repair" if !saw_repair => {
                repair = true;
                saw_repair = true;
                index = index.saturating_add(1);
            }
            _ => return None,
        }
    }

    Some((path, page, output, dpi, font, repair))
}

fn parse_summary_args(args: &[OsString]) -> Option<(PathBuf, Limits, bool)> {
    let mut path = None;
    let mut limits = Limits::default();
    let mut repair = false;
    let mut saw_repair = false;
    let mut index = 1usize;

    while index < args.len() {
        let argument = args.get(index)?;
        let text = argument.to_str()?;
        match text {
            "--max-decoded" => {
                let value = parse_u64_argument(args.get(index.saturating_add(1))?)?;
                limits.max_decoded_bytes = value;
                index = index.saturating_add(2);
            }
            "--max-depth" => {
                let value = parse_u32_argument(args.get(index.saturating_add(1))?)?;
                limits.max_depth = value;
                index = index.saturating_add(2);
            }
            "--max-prev-chain" => {
                let value = parse_u32_argument(args.get(index.saturating_add(1))?)?;
                limits.max_prev_chain = value;
                index = index.saturating_add(2);
            }
            "--max-pages" => {
                let value = parse_u32_argument(args.get(index.saturating_add(1))?)?;
                limits.max_pages = value;
                index = index.saturating_add(2);
            }
            "--repair" if !saw_repair => {
                repair = true;
                saw_repair = true;
                index = index.saturating_add(1);
            }
            value if value.starts_with("--") => return None,
            _ => {
                if path.is_some() {
                    return None;
                }
                path = Some(PathBuf::from(argument));
                index = index.saturating_add(1);
            }
        }
    }

    path.map(|path| (path, limits, repair))
}

fn parse_bench_args(args: &[OsString]) -> Option<(Vec<PathBuf>, usize)> {
    let mut paths = Vec::new();
    let mut repeat = 3usize;
    let mut index = 1usize;

    while index < args.len() {
        let argument = args.get(index)?;
        let text = argument.to_str()?;
        if text == "--repeat" {
            repeat =
                usize::try_from(parse_u64_argument(args.get(index.saturating_add(1))?)?).ok()?;
            if repeat == 0 {
                return None;
            }
            index = index.saturating_add(2);
        } else if text.starts_with("--") {
            return None;
        } else {
            paths.push(PathBuf::from(argument));
            index = index.saturating_add(1);
        }
    }

    if paths.is_empty() {
        None
    } else {
        Some((paths, repeat))
    }
}

fn parse_mutate_args(args: &[OsString]) -> Option<(PathBuf, u64, usize)> {
    let mut path = None;
    let mut seed = None;
    let mut count = None;
    let mut index = 1usize;

    while index < args.len() {
        let argument = args.get(index)?;
        let text = argument.to_str()?;
        match text {
            "--seed" => {
                seed = Some(parse_u64_argument(args.get(index.saturating_add(1))?)?);
                index = index.saturating_add(2);
            }
            "--count" => {
                let parsed =
                    usize::try_from(parse_u64_argument(args.get(index.saturating_add(1))?)?)
                        .ok()?;
                if parsed > MAX_MUTATE_CASES {
                    return None;
                }
                count = Some(parsed);
                index = index.saturating_add(2);
            }
            value if value.starts_with("--") => return None,
            _ => {
                if path.is_some() {
                    return None;
                }
                path = Some(PathBuf::from(argument));
                index = index.saturating_add(1);
            }
        }
    }

    Some((path?, seed?, count?))
}

fn parse_u64_argument(argument: &OsStr) -> Option<u64> {
    argument.to_str()?.parse().ok()
}

fn parse_u32_argument(argument: &OsStr) -> Option<u32> {
    u32::try_from(parse_u64_argument(argument)?).ok()
}

fn open_source(
    path: &Path,
    limits: Limits,
    repair: bool,
) -> Result<PdfSource, file::FileOpenError> {
    app_caps::stage(Arc::<str>::from(format!("read PDF {}", path.display())));
    if let Ok(metadata) = std::fs::metadata(path) {
        app_caps::check("input-bytes", metadata.len());
    }
    let maximum = limits.max_file_bytes.min(app_caps::hard("input-bytes"));
    if repair {
        pdf_core::file::open_repair(path, maximum)
    } else {
        pdf_core::file::open(path, maximum)
    }
}

fn open_document(source: PdfSource, limits: Limits, repair: bool) -> Result<Document, DocError> {
    if repair {
        Document::open_with(source, limits, pdf_core::OpenMode::Repair)
    } else {
        Document::open(source, limits)
    }
}

#[derive(Clone)]
struct RefusalReport {
    code: String,
    message: String,
}

fn refusal_from_error(error: &DocError) -> RefusalReport {
    RefusalReport {
        code: error.code().to_string(),
        message: error.to_string(),
    }
}

fn remember_refusal(slot: &mut Option<RefusalReport>, error: &DocError) {
    if slot.is_none() {
        *slot = Some(refusal_from_error(error));
    }
}

fn object_ref_text(id: ObjectRef) -> String {
    format!("{} {}", id.number(), id.generation())
}

#[derive(Clone)]
struct WarningRecord {
    code: &'static str,
    offset: Option<u64>,
    object: Option<ObjectRef>,
}

fn warning_record(warning: &Warning) -> WarningRecord {
    WarningRecord {
        code: warning.what,
        offset: warning.offset,
        object: warning.object,
    }
}

fn append_resolver_warnings(resolver: &mut Resolver<'_>, warnings: &mut Vec<WarningRecord>) {
    for warning in resolver.take_warnings() {
        warnings.push(warning_record(&warning));
    }
}

fn sorted_dict_keys(dict: DictRef<'_>) -> Vec<Vec<u8>> {
    let mut keys = Vec::new();
    for (name, _) in dict.iter() {
        keys.push(name.decoded().into_owned());
    }
    keys.sort();
    keys.dedup();
    keys
}

fn page_resource_keys(page: &Page) -> Vec<Vec<u8>> {
    let Some(resources) = page.resources.as_ref() else {
        return Vec::new();
    };
    let Some(dict) = resources.get().as_dict() else {
        return Vec::new();
    };
    sorted_dict_keys(dict)
}

struct PageErrorRecord {
    index_hint: u32,
    code: String,
    message: String,
}

struct WalkReport {
    page_count_hint: Option<u32>,
    pages: Vec<Page>,
    errors: Vec<PageErrorRecord>,
    warnings: Vec<WarningRecord>,
    refusal: Option<RefusalReport>,
}

fn inspect_pages(document: &Document) -> WalkReport {
    let mut warnings = document
        .warnings()
        .iter()
        .map(warning_record)
        .collect::<Vec<_>>();
    let mut resolver = document.resolver();
    let page_count_hint = page_count_hint(&mut resolver).ok();
    append_resolver_warnings(&mut resolver, &mut warnings);

    let mut pages = Vec::new();
    let mut errors = Vec::new();
    let mut refusal = None;
    {
        match PageWalk::new(&mut resolver) {
            Ok(walk) => {
                let mut yielded = 0u32;
                for item in walk {
                    match item {
                        Ok(page) => pages.push(page),
                        Err(error) => {
                            errors.push(PageErrorRecord {
                                index_hint: yielded,
                                code: error.code().to_string(),
                                message: error.to_string(),
                            });
                            remember_refusal(&mut refusal, &error);
                        }
                    }
                    yielded = yielded.saturating_add(1);
                }
            }
            Err(error) => {
                errors.push(PageErrorRecord {
                    index_hint: 0,
                    code: error.code().to_string(),
                    message: error.to_string(),
                });
                remember_refusal(&mut refusal, &error);
            }
        }
    }
    append_resolver_warnings(&mut resolver, &mut warnings);

    WalkReport {
        page_count_hint,
        pages,
        errors,
        warnings,
        refusal,
    }
}

fn write_file_info(json: &mut JsonWriter, source: &PdfSource) {
    json.begin_object();
    json.member("bytes");
    json.number(source.bytes().len() as u64);
    json.member("sha256");
    json.string(&digest_hex(source.identity().digest().as_bytes()));
    json.member("version");
    json.string(&source.header().version().to_string());
    json.end_object();
}

fn write_refusal(json: &mut JsonWriter, refusal: Option<&RefusalReport>) {
    match refusal {
        Some(refusal) => {
            json.begin_object();
            json.member("code");
            json.string(&refusal.code);
            json.member("message");
            json.string(&refusal.message);
            json.end_object();
        }
        None => json.null(),
    }
}

fn write_warnings(json: &mut JsonWriter, warnings: &[WarningRecord]) {
    json.begin_array();
    for warning in warnings {
        json.begin_object();
        json.member("code");
        json.string(warning.code);
        json.member("offset");
        match warning.offset {
            Some(offset) => json.number(offset),
            None => json.null(),
        }
        json.member("object");
        match warning.object {
            Some(id) => json.string(&object_ref_text(id)),
            None => json.null(),
        }
        json.end_object();
    }
    json.end_array();
}

fn write_page_box(json: &mut JsonWriter, bounds: [f64; 4]) {
    json.begin_array();
    for value in bounds {
        json.float(value);
    }
    json.end_array();
}

fn write_page_summary(json: &mut JsonWriter, page: &Page) {
    json.begin_object();
    json.member("index");
    json.number(page.index as u64);
    json.member("id");
    json.string(&object_ref_text(page.id));
    json.member("media_box");
    write_page_box(json, page.media_box);
    json.member("crop_box");
    write_page_box(json, page.crop_box);
    json.member("rotate");
    json.number(page.rotate as u64);
    json.member("user_unit");
    json.float(page.user_unit);
    json.member("contents");
    json.number(page.contents.len() as u64);
    json.member("resource_keys");
    json.begin_array();
    for key in page_resource_keys(page) {
        json.bytes_string(&key);
    }
    json.end_array();
    json.member("annots");
    json.number(page.annots as u64);
    json.end_object();
}

fn write_page_errors(json: &mut JsonWriter, errors: &[PageErrorRecord]) {
    json.begin_array();
    for error in errors {
        json.begin_object();
        json.member("index_hint");
        json.number(error.index_hint as u64);
        json.member("code");
        json.string(&error.code);
        json.member("message");
        json.string(&error.message);
        json.end_object();
    }
    json.end_array();
}

fn xref_kind_name(kind: XrefKind) -> &'static str {
    match kind {
        XrefKind::Table => "table",
        XrefKind::Stream => "stream",
        XrefKind::Hybrid => "hybrid",
    }
}

fn write_semantic_bbox(json: &mut JsonWriter, bbox: [f32; 4]) {
    json.begin_array();
    for value in bbox {
        json.float(f64::from(value));
    }
    json.end_array();
}

fn write_semantic_target(json: &mut JsonWriter, target: Option<&pdf_text::semantic::LinkTarget>) {
    let Some(target) = target else {
        json.null();
        return;
    };

    json.begin_object();
    match target {
        pdf_text::semantic::LinkTarget::Uri(uri) => {
            json.member("uri");
            json.string(uri);
        }
        pdf_text::semantic::LinkTarget::Page { index, y } => {
            json.member("page");
            json.number(*index as u64);
            json.member("y");
            match y {
                Some(value) => json.float(f64::from(*value)),
                None => json.null(),
            }
        }
    }
    json.end_object();
}

fn semantic_kind_text(kind: pdf_text::semantic::SemanticKind) -> String {
    match kind {
        pdf_text::semantic::SemanticKind::Paragraph => "paragraph".to_string(),
        pdf_text::semantic::SemanticKind::Heading(level) => format!("heading:{level}"),
        pdf_text::semantic::SemanticKind::ListItem => "list_item".to_string(),
        pdf_text::semantic::SemanticKind::Caption => "caption".to_string(),
    }
}

fn write_semantic_document(json: &mut JsonWriter, document: &pdf_text::semantic::SemanticDoc) {
    json.begin_object();
    json.member("semantic_version");
    json.number(1);

    json.member("info");
    json.begin_object();
    json.member("title");
    match document.info.title.as_deref() {
        Some(title) => json.string(title),
        None => json.null(),
    }
    json.member("author");
    match document.info.author.as_deref() {
        Some(author) => json.string(author),
        None => json.null(),
    }
    json.member("subject");
    match document.info.subject.as_deref() {
        Some(subject) => json.string(subject),
        None => json.null(),
    }
    json.member("pages");
    json.number(document.info.pages as u64);
    json.member("encrypted");
    json.boolean(document.info.encrypted);
    json.end_object();

    json.member("outline");
    json.begin_array();
    for item in &document.outline {
        json.begin_object();
        json.member("title");
        json.string(&item.title);
        json.member("level");
        json.number(item.level as u64);
        json.member("target");
        write_semantic_target(json, item.target.as_ref());
        json.end_object();
    }
    json.end_array();

    json.member("pages");
    json.begin_array();
    for page in &document.pages {
        json.begin_object();
        json.member("index");
        json.number(page.index as u64);
        json.member("width");
        json.float(f64::from(page.width));
        json.member("height");
        json.float(f64::from(page.height));
        json.member("refusal");
        match page.refusal {
            Some(refusal) => json.string(refusal),
            None => json.null(),
        }

        json.member("blocks");
        json.begin_array();
        for block in &page.blocks {
            json.begin_object();
            json.member("kind");
            json.string(&semantic_kind_text(block.kind));
            json.member("bbox");
            write_semantic_bbox(json, block.bbox);
            json.member("spans");
            json.begin_array();
            for span in &block.spans {
                json.begin_object();
                json.member("text");
                json.string(&span.text);
                json.member("bold");
                json.boolean(span.style.bold);
                json.member("italic");
                json.boolean(span.style.italic);
                json.member("monospace");
                json.boolean(span.style.monospace);
                json.member("size");
                json.float(f64::from(span.size));
                json.member("link");
                match span.link {
                    Some(link) => json.number(u64::from(link)),
                    None => json.null(),
                }
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
            write_semantic_bbox(json, image.bbox);
            json.member("object");
            match image.object {
                Some(object) => json.string(&object_ref_text(object)),
                None => json.null(),
            }
            json.member("width");
            json.number(image.width as u64);
            json.member("height");
            json.number(image.height as u64);
            json.member("inline");
            json.boolean(image.inline);
            json.end_object();
        }
        json.end_array();

        json.member("links");
        json.begin_array();
        for link in &page.links {
            json.begin_object();
            json.member("bbox");
            write_semantic_bbox(json, link.bbox);
            match &link.target {
                pdf_text::semantic::LinkTarget::Uri(uri) => {
                    json.member("uri");
                    json.string(uri);
                }
                pdf_text::semantic::LinkTarget::Page { index, y } => {
                    json.member("page");
                    json.number(*index as u64);
                    json.member("y");
                    match y {
                        Some(value) => json.float(f64::from(*value)),
                        None => json.null(),
                    }
                }
            }
            json.end_object();
        }
        json.end_array();
        json.end_object();
    }
    json.end_array();
    json.end_object();
}

fn write_repaired_semantic_document(json: &mut JsonWriter, document: &Document) {
    let report = inspect_pages(document);
    let mut resolver = document.resolver();
    let mut fonts = pdf_text::font::FontCache::default();

    json.begin_object();
    json.member("semantic_version");
    json.number(1);

    json.member("info");
    json.begin_object();
    json.member("title");
    json.null();
    json.member("author");
    json.null();
    json.member("subject");
    json.null();
    json.member("pages");
    json.number(report.pages.len() as u64);
    json.member("encrypted");
    json.boolean(document.encrypted());
    json.end_object();

    json.member("outline");
    json.begin_array();
    json.end_array();

    json.member("pages");
    json.begin_array();
    for page in &report.pages {
        let receipt = pdf_text::extract::page_receipt_cached(&mut resolver, page, &mut fonts);
        json.begin_object();
        json.member("index");
        json.number(page.index as u64);
        json.member("width");
        json.float(f64::from(receipt.width));
        json.member("height");
        json.float(f64::from(receipt.height));
        json.member("refusal");
        match receipt.refusal.as_ref() {
            Some(error) => json.string(error.code()),
            None => json.null(),
        }

        json.member("blocks");
        json.begin_array();
        for block in &receipt.layout.blocks {
            let kind = match block.kind {
                pdf_text::layout::BlockKind::Paragraph => "paragraph".to_string(),
                pdf_text::layout::BlockKind::Heading(level) => format!("heading:{level}"),
                pdf_text::layout::BlockKind::ListItem => "list_item".to_string(),
                pdf_text::layout::BlockKind::Caption => "caption".to_string(),
            };
            let text = receipt.layout.block_text(block);
            json.begin_object();
            json.member("kind");
            json.string(&kind);
            json.member("bbox");
            write_semantic_bbox(json, block.bbox);
            json.member("spans");
            json.begin_array();
            json.begin_object();
            json.member("text");
            json.string(&text);
            json.member("bold");
            json.boolean(false);
            json.member("italic");
            json.boolean(false);
            json.member("monospace");
            json.boolean(false);
            json.member("size");
            json.float(f64::from(receipt.layout.body_size));
            json.member("link");
            json.null();
            json.end_object();
            json.end_array();
            json.end_object();
        }
        json.end_array();

        json.member("images");
        json.begin_array();
        for image in &receipt.images {
            json.begin_object();
            json.member("bbox");
            write_semantic_bbox(json, image.bbox);
            json.member("object");
            json.null();
            json.member("width");
            json.number(u64::from(image.width));
            json.member("height");
            json.number(u64::from(image.height));
            json.member("inline");
            json.boolean(false);
            json.end_object();
        }
        json.end_array();

        json.member("links");
        json.begin_array();
        json.end_array();
        json.end_object();
    }
    json.end_array();
    json.end_object();
}

fn run_semantic(path: &Path, limits: Limits, repair: bool) -> u8 {
    let source = match open_source(path, limits, repair) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("{error}");
            return 3;
        }
    };

    if repair {
        let repaired = match open_document(source.clone(), limits, true) {
            Ok(document) => document,
            Err(error) => {
                eprintln!("{error}");
                return 3;
            }
        };
        let mut json = JsonWriter::new();
        match pdf_text::semantic::semantic_document_with(source, limits, pdf_core::OpenMode::Repair)
        {
            Ok(document) => write_semantic_document(&mut json, &document),
            Err(_) => write_repaired_semantic_document(&mut json, &repaired),
        }
        out!("{}", json.finish());
        return 0;
    }

    let document = match pdf_text::semantic::semantic_document(source, limits) {
        Ok(document) => document,
        Err(error) => {
            eprintln!("{error}");
            return 3;
        }
    };

    let mut json = JsonWriter::new();
    write_semantic_document(&mut json, &document);
    out!("{}", json.finish());
    0
}

/// Opens with xref repair (only used when the strict open fails) and reports what
/// the rebuilt document yields: pages walked, extracted glyphs and every warning.
fn run_repair(path: &Path, limits: Limits) -> u8 {
    let source = match file::open_repair(path, limits.max_file_bytes) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("{error}");
            return 3;
        }
    };
    let strict = Document::open(source.clone(), limits)
        .err()
        .map(|error| error.code());
    let document = match Document::open_with(source, limits, pdf_core::OpenMode::Repair) {
        Ok(document) => document,
        Err(error) => {
            let mut json = JsonWriter::new();
            json.begin_object();
            json.member("strict");
            match strict {
                Some(code) => json.string(code),
                None => json.null(),
            }
            json.member("repair");
            json.string(error.code());
            json.end_object();
            out!("{}", json.finish());
            return 1;
        }
    };
    let mut resolver = document.resolver();
    let (mut pages, mut page_errors, mut glyphs) = (0u32, 0u32, 0usize);
    let walked: Vec<_> = match PageWalk::new(&mut resolver) {
        Ok(walk) => walk.collect(),
        Err(_) => Vec::new(),
    };
    for item in walked {
        match item {
            Ok(page) => {
                pages += 1;
                if let Ok(text) = pdf_text::interp::page_text(&mut resolver, &page) {
                    glyphs += text.glyphs.len();
                }
            }
            Err(_) => page_errors += 1,
        }
    }
    let mut warnings: Vec<&str> = document.warnings().iter().map(|w| w.what).collect();
    let resolver_warnings = resolver.take_warnings();
    warnings.extend(resolver_warnings.iter().map(|w| w.what));
    warnings.dedup();
    let mut json = JsonWriter::new();
    json.begin_object();
    json.member("strict");
    match strict {
        Some(code) => json.string(code),
        None => json.null(),
    }
    json.member("pages");
    json.number(u64::from(pages));
    json.member("page_errors");
    json.number(u64::from(page_errors));
    json.member("glyphs");
    json.number(glyphs as u64);
    json.member("warnings");
    json.begin_array();
    for warning in warnings {
        json.string(warning);
    }
    json.end_array();
    json.end_object();
    out!("{}", json.finish());
    0
}

fn image_kind(data: &pdf_text::images::ImageData) -> String {
    match data {
        pdf_text::images::ImageData::Jpeg(_) => "jpeg".to_string(),
        pdf_text::images::ImageData::Pixels { .. } => "pixels".to_string(),
        pdf_text::images::ImageData::Unsupported(why) => format!("unsupported:{why}"),
    }
}

fn image_output_path(base: &Path, suffix: &str) -> PathBuf {
    let mut output = base.as_os_str().to_os_string();
    output.push(suffix);
    PathBuf::from(output)
}

fn invalid_image_data() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "invalid decoded image data",
    )
}

fn write_image_output(base: &Path, image: &pdf_text::images::Image) -> Result<(), std::io::Error> {
    match &image.data {
        pdf_text::images::ImageData::Jpeg(bytes) => {
            std::fs::write(image_output_path(base, ".jpg"), bytes)
        }
        pdf_text::images::ImageData::Pixels { rgba } => {
            let pixel_count = u64::from(image.width)
                .checked_mul(u64::from(image.height))
                .ok_or_else(invalid_image_data)?;
            let pixel_count = usize::try_from(pixel_count).map_err(|_| invalid_image_data())?;
            let rgba_len = pixel_count.checked_mul(4).ok_or_else(invalid_image_data)?;
            if rgba.len() != rgba_len {
                return Err(invalid_image_data());
            }

            let soft_mask = image.soft_mask.as_deref();
            if soft_mask.is_some_and(|mask| mask.len() != pixel_count) {
                return Err(invalid_image_data());
            }
            let has_alpha = soft_mask.is_some()
                || rgba
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .any(|pixel| pixel.get(3).copied().unwrap_or(255) < 255);

            let (suffix, header) = if has_alpha {
                (
                    ".pam",
                    format!(
                        "P7\nWIDTH {}\nHEIGHT {}\nDEPTH 4\nMAXVAL 255\nTUPLTYPE RGB_ALPHA\nENDHDR\n",
                        image.width, image.height
                    ),
                )
            } else {
                (
                    ".ppm",
                    format!("P6\n{} {}\n255\n", image.width, image.height),
                )
            };
            let mut output = header.into_bytes();
            for (index, pixel) in rgba.as_chunks::<4>().0.iter().enumerate() {
                let rgb = pixel.get(..3).ok_or_else(invalid_image_data)?;
                output.extend_from_slice(rgb);
                if has_alpha {
                    let pixel_alpha = pixel.get(3).copied().unwrap_or(255);
                    let alpha = soft_mask
                        .and_then(|mask| mask.get(index))
                        .copied()
                        .map(|mask_alpha| {
                            (u16::from(pixel_alpha).saturating_mul(u16::from(mask_alpha)) / 255)
                                as u8
                        })
                        .unwrap_or(pixel_alpha);
                    output.push(alpha);
                }
            }
            std::fs::write(image_output_path(base, suffix), output)
        }
        pdf_text::images::ImageData::Unsupported(_) => Ok(()),
    }
}

fn run_image(path: &Path, object_number: u32, output: &Path, limits: Limits) -> u8 {
    let source = match file::open(path, limits.max_file_bytes) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("{error}");
            return 3;
        }
    };
    let document = match Document::open(source, limits) {
        Ok(document) => document,
        Err(error) => {
            eprintln!("{error}");
            return 3;
        }
    };
    let object = match document.xref().get(object_number) {
        Some(XrefEntry::InUse { generation, .. }) => ObjectRef::new(object_number, generation),
        Some(XrefEntry::Compressed { .. }) => ObjectRef::new(object_number, 0),
        _ => {
            eprintln!("missing object {object_number}");
            return 3;
        }
    };

    let mut resolver = document.resolver();
    let image =
        match pdf_text::images::load_image(&mut resolver, object, limits.max_decoded_bytes / 4) {
            Ok(image) => image,
            Err(error) => {
                eprintln!("{error}");
                return 3;
            }
        };
    if let Err(error) = write_image_output(output, &image) {
        eprintln!("{error}");
        return 3;
    }

    let kind = image_kind(&image.data);
    let mut json = JsonWriter::new();
    json.begin_object();
    json.member("width");
    json.number(image.width as u64);
    json.member("height");
    json.number(image.height as u64);
    json.member("kind");
    json.string(&kind);
    json.end_object();
    out!("{}", json.finish());
    0
}

fn read_bounded(path: &Path, maximum: u64) -> std::io::Result<Vec<u8>> {
    app_caps::stage(Arc::<str>::from(format!("read {}", path.display())));
    let maximum = maximum.min(app_caps::hard("input-bytes"));
    let file = std::fs::File::open(path)?;
    app_caps::read_limited(file, maximum)
}

fn run_render(
    path: &Path,
    page_index: u32,
    output: &Path,
    dpi: u32,
    font_path: Option<&Path>,
    repair: bool,
    limits: Limits,
) -> u8 {
    let source = match open_source(path, limits, repair) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("{error}");
            return 3;
        }
    };
    let document = match open_document(source, limits, repair) {
        Ok(document) => document,
        Err(error) => {
            eprintln!("{error}");
            return 3;
        }
    };

    let fallback_font = match font_path {
        Some(font_path) => match read_bounded(font_path, limits.max_decoded_bytes) {
            Ok(bytes) => Some(Arc::<[u8]>::from(bytes)),
            Err(error) => {
                eprintln!("{error}");
                return 3;
            }
        },
        None => None,
    };

    let mut resolver = document.resolver();
    let page = {
        let walk = match PageWalk::new(&mut resolver) {
            Ok(walk) => walk,
            Err(error) => {
                eprintln!("{error}");
                return 3;
            }
        };
        let mut found = None;
        for item in walk {
            match item {
                Ok(page) if page.index == page_index => {
                    found = Some(page);
                    break;
                }
                Ok(_) => {}
                Err(error) => {
                    eprintln!("{error}");
                    return 3;
                }
            }
        }
        found
    };
    let Some(page) = page else {
        eprintln!("missing page {page_index}");
        return 3;
    };

    let options = RenderOptions {
        scale: dpi as f32 / 72.0,
        fallback_font,
    };
    let raster = match rasterize_page(&mut resolver, &page, &options, [255, 255, 255, 255]) {
        Ok(raster) => raster,
        Err(error) => {
            eprintln!("{error}");
            return 3;
        }
    };
    let png = encode_png(&raster);
    if png.is_empty() {
        eprintln!("PNG encoding failed");
        return 3;
    }
    if let Err(error) = std::fs::write(output, png) {
        eprintln!("{error}");
        return 3;
    }
    0
}

fn run_summary(path: &Path, limits: Limits, repair: bool) -> u8 {
    let source = match open_source(path, limits, repair) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("{error}");
            return 3;
        }
    };

    let document = open_document(source.clone(), limits, repair);
    let mut json = JsonWriter::new();
    json.begin_object();
    json.member("file");
    write_file_info(&mut json, &source);

    match document {
        Err(error) => {
            json.member("xref");
            json.null();
            json.member("encrypted");
            json.null();
            json.member("trailer_keys");
            json.begin_array();
            json.end_array();
            json.member("root");
            json.null();
            json.member("info");
            json.null();
            json.member("page_count_hint");
            json.null();
            json.member("pages");
            json.begin_array();
            json.end_array();
            json.member("page_errors");
            json.begin_array();
            json.end_array();
            json.member("warnings");
            json.begin_array();
            json.end_array();
            json.member("refusal");
            write_refusal(&mut json, Some(&refusal_from_error(&error)));
        }
        Ok(document) => {
            json.member("xref");
            json.begin_object();
            json.member("kind");
            json.string(xref_kind_name(document.xref().kind()));
            json.member("size");
            json.number(document.xref().size() as u64);
            json.member("sections");
            json.number(document.xref().sections() as u64);
            let counts = document.xref().counts();
            json.member("free");
            json.number(counts.free as u64);
            json.member("in_use");
            json.number(counts.in_use as u64);
            json.member("compressed");
            json.number(counts.compressed as u64);
            json.member("object_streams");
            json.number(counts.object_streams as u64);
            json.end_object();

            json.member("encrypted");
            json.boolean(document.encrypted());

            json.member("trailer_keys");
            let trailer_keys = document
                .trailer()
                .dict()
                .map(sorted_dict_keys)
                .unwrap_or_default();
            json.begin_array();
            for key in trailer_keys {
                json.bytes_string(&key);
            }
            json.end_array();

            json.member("root");
            match document.root_ref() {
                Some(id) => json.string(&object_ref_text(id)),
                None => json.null(),
            }
            json.member("info");
            match document.info_ref() {
                Some(id) => json.string(&object_ref_text(id)),
                None => json.null(),
            }

            let report = inspect_pages(&document);
            json.member("page_count_hint");
            match report.page_count_hint {
                Some(count) => json.number(count as u64),
                None => json.null(),
            }
            json.member("pages");
            json.begin_array();
            for page in &report.pages {
                write_page_summary(&mut json, page);
            }
            json.end_array();
            json.member("page_errors");
            write_page_errors(&mut json, &report.errors);
            json.member("warnings");
            write_warnings(&mut json, &report.warnings);
            json.member("refusal");
            write_refusal(&mut json, report.refusal.as_ref());
        }
    }
    json.end_object();

    out!("{}", json.finish());
    0
}

struct StreamRecord {
    id: ObjectRef,
    filters: Vec<Vec<u8>>,
    raw_bytes: Option<u64>,
    decoded_bytes: Option<u64>,
    truncated: Option<bool>,
    image: Option<Vec<u8>>,
    refusal: Option<RefusalReport>,
}

fn stream_filter_names(resolver: &mut Resolver<'_>, object: &Object) -> Vec<Vec<u8>> {
    let Some(dict) = object.dict() else {
        return Vec::new();
    };
    let Some(filter_value) = dict.get(b"Filter") else {
        return Vec::new();
    };
    let Ok(resolved) = resolver.resolve(filter_value) else {
        return Vec::new();
    };

    let root = resolved.get();
    if let Some(name) = root.as_name() {
        return vec![name.decoded().into_owned()];
    }

    let Some(array) = root.as_array() else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for item in array.iter() {
        if let Ok(value) = resolver.resolve(item)
            && let Some(name) = value.get().as_name()
        {
            names.push(name.decoded().into_owned());
        }
    }
    names
}

fn write_stream_record(json: &mut JsonWriter, stream: &StreamRecord) {
    json.begin_object();
    json.member("id");
    json.string(&object_ref_text(stream.id));
    json.member("filters");
    json.begin_array();
    for name in &stream.filters {
        json.bytes_string(name);
    }
    json.end_array();
    json.member("raw_bytes");
    match stream.raw_bytes {
        Some(count) => json.number(count),
        None => json.null(),
    }
    json.member("decoded_bytes");
    match stream.decoded_bytes {
        Some(count) => json.number(count),
        None => json.null(),
    }
    json.member("truncated");
    match stream.truncated {
        Some(value) => json.boolean(value),
        None => json.null(),
    }
    json.member("image");
    match stream.image.as_ref() {
        Some(name) => json.bytes_string(name),
        None => json.null(),
    }
    json.member("refusal");
    write_refusal(json, stream.refusal.as_ref());
    json.end_object();
}

fn run_streams(path: &Path, limits: Limits) -> u8 {
    let source = match file::open(path, limits.max_file_bytes) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("{error}");
            return 3;
        }
    };
    let document = match Document::open(source, limits) {
        Ok(document) => document,
        Err(error) => {
            let refusal = refusal_from_error(&error);
            let mut json = JsonWriter::new();
            json.begin_object();
            json.member("pages");
            json.begin_array();
            json.end_array();
            json.member("warnings");
            json.begin_array();
            json.end_array();
            json.member("refusal");
            write_refusal(&mut json, Some(&refusal));
            json.end_object();
            out!("{}", json.finish());
            return 0;
        }
    };

    let mut report = inspect_pages(&document);
    let mut resolver = document.resolver();
    let mut page_streams: Vec<(Page, Vec<StreamRecord>)> = Vec::new();

    for page in &report.pages {
        let mut streams = Vec::new();
        for id in &page.contents {
            match resolver.get(*id) {
                Ok(object) => {
                    let raw_bytes = object.stream_raw().map(|bytes| bytes.len() as u64);
                    let filters = stream_filter_names(&mut resolver, &object);
                    match resolver.decode_stream(&object) {
                        Ok(decoded) => streams.push(StreamRecord {
                            id: *id,
                            filters,
                            raw_bytes,
                            decoded_bytes: Some(decoded.bytes.len() as u64),
                            truncated: Some(decoded.truncated),
                            image: decoded.image.map(|image| image.name().as_bytes().to_vec()),
                            refusal: None,
                        }),
                        Err(error) => {
                            remember_refusal(&mut report.refusal, &error);
                            streams.push(StreamRecord {
                                id: *id,
                                filters,
                                raw_bytes,
                                decoded_bytes: None,
                                truncated: None,
                                image: None,
                                refusal: Some(refusal_from_error(&error)),
                            });
                        }
                    }
                }
                Err(error) => {
                    remember_refusal(&mut report.refusal, &error);
                    streams.push(StreamRecord {
                        id: *id,
                        filters: Vec::new(),
                        raw_bytes: None,
                        decoded_bytes: None,
                        truncated: None,
                        image: None,
                        refusal: Some(refusal_from_error(&error)),
                    });
                }
            }
        }
        page_streams.push((page.clone(), streams));
    }
    append_resolver_warnings(&mut resolver, &mut report.warnings);

    let mut json = JsonWriter::new();
    json.begin_object();
    json.member("pages");
    json.begin_array();
    for (page, streams) in &page_streams {
        json.begin_object();
        json.member("index");
        json.number(page.index as u64);
        json.member("id");
        json.string(&object_ref_text(page.id));
        json.member("streams");
        json.begin_array();
        for stream in streams {
            write_stream_record(&mut json, stream);
        }
        json.end_array();
        json.end_object();
    }
    json.end_array();
    json.member("page_errors");
    write_page_errors(&mut json, &report.errors);
    json.member("warnings");
    write_warnings(&mut json, &report.warnings);
    json.member("refusal");
    write_refusal(&mut json, report.refusal.as_ref());
    json.end_object();
    out!("{}", json.finish());
    0
}

#[derive(Clone)]
struct BenchSample {
    bytes: u64,
    open_ms: f64,
    first_page_ms: f64,
    pages_ms: f64,
    decode_ms: f64,
    text_ms: f64,
    render_ms: f64,
    pages: u64,
    decoded_bytes: u64,
    glyphs: u64,
    refusal: Option<String>,
}

fn record_bench_page(
    result: Result<Page, DocError>,
    pages: &mut Vec<Page>,
    refusal: &mut Option<String>,
) {
    match result {
        Ok(page) => pages.push(page),
        Err(error) => {
            if refusal.is_none() {
                *refusal = Some(error.code().to_string());
            }
        }
    }
}

fn bench_sample(path: &Path, limits: Limits) -> Result<BenchSample, String> {
    let overall_start = Instant::now();
    let source = file::open(path, limits.max_file_bytes).map_err(|error| error.to_string())?;
    let bytes = source.bytes().len() as u64;
    let document = Document::open(source, limits);
    let open_ms = overall_start.elapsed().as_secs_f64() * 1000.0;
    let mut first_page_ms = open_ms;
    let mut pages_ms = 0.0;
    let mut decode_ms = 0.0;
    let mut text_ms = 0.0;
    let mut render_ms = 0.0;
    let mut pages = Vec::new();
    let mut decoded_bytes = 0u64;
    let mut glyphs = 0u64;
    let mut refusal = None;

    if let Err(error) = &document {
        refusal = Some(error.code().to_string());
    }

    if let Ok(document) = document {
        let mut resolver = document.resolver();
        let walk_start = Instant::now();
        {
            match PageWalk::new(&mut resolver) {
                Ok(walk) => {
                    let mut walk = walk;
                    if let Some(item) = walk.next() {
                        record_bench_page(item, &mut pages, &mut refusal)
                    }
                    first_page_ms = overall_start.elapsed().as_secs_f64() * 1000.0;
                    for item in walk {
                        record_bench_page(item, &mut pages, &mut refusal);
                    }
                }
                Err(error) => {
                    if refusal.is_none() {
                        refusal = Some(error.code().to_string());
                    }
                    first_page_ms = overall_start.elapsed().as_secs_f64() * 1000.0;
                }
            }
        }
        pages_ms = walk_start.elapsed().as_secs_f64() * 1000.0;

        let decode_start = Instant::now();
        for page in &pages {
            for id in &page.contents {
                match resolver.get(*id) {
                    Ok(object) => match resolver.decode_stream(&object) {
                        Ok(decoded) => {
                            decoded_bytes =
                                decoded_bytes.saturating_add(decoded.bytes.len() as u64);
                        }
                        Err(error) => {
                            if refusal.is_none() {
                                refusal = Some(error.code().to_string());
                            }
                        }
                    },
                    Err(error) => {
                        if refusal.is_none() {
                            refusal = Some(error.code().to_string());
                        }
                    }
                }
            }
        }
        decode_ms = decode_start.elapsed().as_secs_f64() * 1000.0;

        let text_start = Instant::now();
        glyphs = text::scan_text(&document, &pages, &mut refusal);
        text_ms = text_start.elapsed().as_secs_f64() * 1000.0;

        let render_start = Instant::now();
        if let Some(page) = pages.first() {
            let options = RenderOptions {
                scale: 1.0,
                fallback_font: None,
            };
            if let Err(error) = rasterize_page(&mut resolver, page, &options, [255, 255, 255, 255])
                && refusal.is_none()
            {
                refusal = Some(error.code().to_string());
            }
        }
        render_ms = render_start.elapsed().as_secs_f64() * 1000.0;
    }

    Ok(BenchSample {
        bytes,
        open_ms,
        first_page_ms,
        pages_ms,
        decode_ms,
        text_ms,
        render_ms,
        pages: pages.len() as u64,
        decoded_bytes,
        glyphs,
        refusal,
    })
}

fn median_f64(values: impl Iterator<Item = f64>) -> f64 {
    let mut values = values.collect::<Vec<_>>();
    values.sort_by(f64::total_cmp);
    let Some(middle) = values.len().checked_div(2) else {
        return 0.0;
    };
    if values.is_empty() {
        return 0.0;
    }
    if values.len() % 2 == 1 {
        values[middle]
    } else {
        let lower = values.get(middle.saturating_sub(1)).copied().unwrap_or(0.0);
        let upper = values.get(middle).copied().unwrap_or(lower);
        lower + (upper - lower) / 2.0
    }
}

fn median_u64(values: impl Iterator<Item = u64>) -> u64 {
    let mut values = values.collect::<Vec<_>>();
    values.sort_unstable();
    let Some(middle) = values.len().checked_div(2) else {
        return 0;
    };
    if values.is_empty() {
        return 0;
    }
    if values.len() % 2 == 1 {
        values[middle]
    } else {
        let lower = values.get(middle.saturating_sub(1)).copied().unwrap_or(0);
        let upper = values.get(middle).copied().unwrap_or(lower);
        lower.saturating_add(upper.saturating_sub(lower) / 2)
    }
}

fn run_bench(paths: &[PathBuf], repeat: usize, limits: Limits) -> u8 {
    app_caps::check(
        "iterations",
        (paths.len() as u64).saturating_mul(repeat as u64),
    );
    for path in paths {
        reset_peak_bytes();
        app_caps::check("cache-entries", repeat as u64);
        app_caps::reserve_bytes(
            (repeat as u64).saturating_mul(std::mem::size_of::<BenchSample>() as u64),
        );
        let mut samples = Vec::new();
        for _ in 0..repeat {
            match bench_sample(path, limits) {
                Ok(sample) => samples.push(sample),
                Err(error) => {
                    eprintln!("{error}");
                    return 3;
                }
            }
        }

        let peak = peak_bytes();
        let bytes = samples.first().map(|sample| sample.bytes).unwrap_or(0);
        let open_ms = median_f64(samples.iter().map(|sample| sample.open_ms));
        let first_page_ms = median_f64(samples.iter().map(|sample| sample.first_page_ms));
        let pages_ms = median_f64(samples.iter().map(|sample| sample.pages_ms));
        let decode_ms = median_f64(samples.iter().map(|sample| sample.decode_ms));
        let text_ms = median_f64(samples.iter().map(|sample| sample.text_ms));
        let render_ms = median_f64(samples.iter().map(|sample| sample.render_ms));
        let pages = median_u64(samples.iter().map(|sample| sample.pages));
        let glyphs = median_u64(samples.iter().map(|sample| sample.glyphs));
        let decoded_bytes = median_u64(samples.iter().map(|sample| sample.decoded_bytes));
        let elapsed_seconds = (open_ms + pages_ms) / 1000.0;
        let mb_per_s = if elapsed_seconds > 0.0 {
            bytes as f64 / 1_000_000.0 / elapsed_seconds
        } else {
            0.0
        };
        let pages_per_s = if elapsed_seconds > 0.0 {
            pages as f64 / elapsed_seconds
        } else {
            0.0
        };
        let refusal = samples.iter().find_map(|sample| sample.refusal.as_ref());

        let mut json = JsonWriter::new();
        json.begin_object();
        json.member("file");
        json.string(&path.to_string_lossy());
        json.member("bytes");
        json.number(bytes);
        json.member("open_ms");
        json.float(open_ms);
        json.member("first_page_ms");
        json.float(first_page_ms);
        json.member("pages_ms");
        json.float(pages_ms);
        json.member("decode_ms");
        json.float(decode_ms);
        json.member("text_ms");
        json.float(text_ms);
        json.member("render_ms");
        json.float(render_ms);
        json.member("glyphs");
        json.number(glyphs);
        json.member("pages");
        json.number(pages);
        json.member("decoded_bytes");
        json.number(decoded_bytes);
        json.member("mb_per_s");
        json.float(mb_per_s);
        json.member("pages_per_s");
        json.float(pages_per_s);
        json.member("peak_bytes");
        json.number(peak);
        json.member("refusal");
        match refusal {
            Some(code) => json.string(code),
            None => json.null(),
        }
        json.end_object();
        out!("{}", json.finish());
    }
    0
}

const MAX_MUTATE_CASES: usize = 10_000;

fn next_random(state: &mut u64) -> u64 {
    if *state == 0 {
        *state = 0x9e3779b97f4a7c15;
    }
    let mut value = *state;
    value ^= value << 13;
    value ^= value >> 7;
    value ^= value << 17;
    *state = value;
    value
}

fn random_below(state: &mut u64, upper: usize) -> usize {
    if upper == 0 {
        return 0;
    }
    (next_random(state) as usize) % upper
}

fn find_startxref_target(bytes: &[u8]) -> Option<usize> {
    let marker = b"startxref";
    if bytes.len() < marker.len() {
        return None;
    }
    let position = bytes
        .windows(marker.len())
        .rposition(|window| window == marker)?;
    let mut index = position.saturating_add(marker.len());
    while let Some(byte) = bytes.get(index) {
        if !byte.is_ascii_whitespace() {
            break;
        }
        index = index.saturating_add(1);
    }

    let mut value = 0u64;
    let mut found_digit = false;
    while let Some(byte) = bytes.get(index).copied() {
        if !byte.is_ascii_digit() {
            break;
        }
        found_digit = true;
        value = value
            .saturating_mul(10)
            .saturating_add((byte - b'0') as u64);
        index = index.saturating_add(1);
    }
    if !found_digit {
        None
    } else {
        usize::try_from(value).ok()
    }
}

fn mutate_digit_run(bytes: &mut [u8], state: &mut u64) -> bool {
    let tail_start = bytes.len().saturating_sub(4096);
    let startxref = find_startxref_target(bytes);
    let mut all = Vec::new();
    let mut tail = Vec::new();
    let mut near_startxref = Vec::new();
    let mut index = 0usize;

    while index < bytes.len() {
        let is_digit = bytes
            .get(index)
            .map(|byte| byte.is_ascii_digit())
            .unwrap_or(false);
        if !is_digit {
            index = index.saturating_add(1);
            continue;
        }

        let start = index;
        while bytes
            .get(index)
            .map(|byte| byte.is_ascii_digit())
            .unwrap_or(false)
        {
            index = index.saturating_add(1);
        }
        let range = (start, index);
        all.push(range);
        if start >= tail_start {
            tail.push(range);
        }
        if let Some(target) = startxref
            && start.abs_diff(target) <= 64
        {
            near_startxref.push(range);
        }
    }

    let candidates = if !near_startxref.is_empty() {
        &near_startxref
    } else if !tail.is_empty() {
        &tail
    } else {
        &all
    };
    if candidates.is_empty() {
        return false;
    }

    let candidate = candidates.get(random_below(state, candidates.len()));
    let Some((start, end)) = candidate.copied() else {
        return false;
    };
    for position in start..end {
        if let Some(byte) = bytes.get_mut(position) {
            *byte = b'0' + (next_random(state) % 10) as u8;
        }
    }
    true
}

fn mutate_bytes(bytes: &mut Vec<u8>, state: &mut u64) {
    if bytes.is_empty() {
        return;
    }
    match next_random(state) % 5 {
        0 => {
            let flips = 1usize.saturating_add((next_random(state) % 8) as usize);
            for _ in 0..flips {
                let index = random_below(state, bytes.len());
                if let Some(byte) = bytes.get_mut(index) {
                    let mask = 1u8.saturating_add((next_random(state) % 255) as u8);
                    *byte ^= mask;
                }
            }
        }
        1 => {
            let end = bytes.len().saturating_add(1);
            let new_len = random_below(state, end);
            bytes.truncate(new_len);
        }
        2 => {
            if !mutate_digit_run(bytes, state) {
                let index = random_below(state, bytes.len());
                if let Some(byte) = bytes.get_mut(index) {
                    *byte ^= 1;
                }
            }
        }
        3 => {
            let start = random_below(state, bytes.len());
            let available = bytes.len().saturating_sub(start).min(4096);
            if available > 0 {
                let length = 1usize.saturating_add(random_below(state, available));
                let insert_at = random_below(state, bytes.len().saturating_add(1));
                let end = start.saturating_add(length);
                if let Some(range) = bytes.get(start..end) {
                    let duplicate = range.to_vec();
                    let capacity = bytes.len().saturating_add(duplicate.len());
                    let mut expanded = Vec::with_capacity(capacity);
                    if let Some(prefix) = bytes.get(..insert_at) {
                        expanded.extend_from_slice(prefix);
                    }
                    expanded.extend_from_slice(&duplicate);
                    if let Some(suffix) = bytes.get(insert_at..) {
                        expanded.extend_from_slice(suffix);
                    }
                    *bytes = expanded;
                }
            }
        }
        _ => {
            let start = random_below(state, bytes.len());
            let available = bytes.len().saturating_sub(start).min(4096);
            if available > 0 {
                let length = 1usize.saturating_add(random_below(state, available));
                let end = start.saturating_add(length);
                if let Some(range) = bytes.get_mut(start..end) {
                    range.fill(0);
                }
            }
        }
    }
}

#[derive(Default)]
struct FuzzStageCounts {
    attempted: u64,
    ok: u64,
    refused: u64,
    panicked: u64,
    refusal_codes: BTreeMap<String, u64>,
}

impl FuzzStageCounts {
    fn refuse(&mut self, code: &str) {
        self.refused = self.refused.saturating_add(1);
        let occurrences = self.refusal_codes.entry(code.to_string()).or_insert(0);
        *occurrences = occurrences.saturating_add(1);
    }

    fn merge(&mut self, other: &Self) {
        self.attempted = self.attempted.saturating_add(other.attempted);
        self.ok = self.ok.saturating_add(other.ok);
        self.refused = self.refused.saturating_add(other.refused);
        self.panicked = self.panicked.saturating_add(other.panicked);
        for (code, occurrences) in &other.refusal_codes {
            let total = self.refusal_codes.entry(code.clone()).or_insert(0);
            *total = total.saturating_add(*occurrences);
        }
    }
}

#[derive(Default)]
struct FuzzStages {
    source: FuzzStageCounts,
    strict_open: FuzzStageCounts,
    repair_open: FuzzStageCounts,
    page_walk: FuzzStageCounts,
    content_streams: FuzzStageCounts,
    text: FuzzStageCounts,
    semantic_document: FuzzStageCounts,
    load_image: FuzzStageCounts,
    render_page_0: FuzzStageCounts,
}

impl FuzzStages {
    fn merge(&mut self, other: &Self) {
        self.source.merge(&other.source);
        self.strict_open.merge(&other.strict_open);
        self.repair_open.merge(&other.repair_open);
        self.page_walk.merge(&other.page_walk);
        self.content_streams.merge(&other.content_streams);
        self.text.merge(&other.text);
        self.semantic_document.merge(&other.semantic_document);
        self.load_image.merge(&other.load_image);
        self.render_page_0.merge(&other.render_page_0);
    }
}

enum FuzzStageOutcome<T> {
    Success(T),
    Refused(String),
    Panicked,
}

fn run_fuzz_stage<T, F>(counts: &mut FuzzStageCounts, action: F) -> FuzzStageOutcome<T>
where
    F: FnOnce() -> Result<T, String>,
{
    counts.attempted = counts.attempted.saturating_add(1);
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(action)) {
        Ok(Ok(value)) => {
            counts.ok = counts.ok.saturating_add(1);
            FuzzStageOutcome::Success(value)
        }
        Ok(Err(code)) => {
            counts.refuse(&code);
            FuzzStageOutcome::Refused(code)
        }
        Err(_) => {
            counts.panicked = counts.panicked.saturating_add(1);
            FuzzStageOutcome::Panicked
        }
    }
}

fn record_fuzz_refusal(counts: &mut FuzzStageCounts, code: &str) {
    counts.attempted = counts.attempted.saturating_add(1);
    counts.refuse(code);
}

fn remember_fuzz_outcome<T>(
    outcome: &FuzzStageOutcome<T>,
    refusal: &mut Option<String>,
    panicked: &mut bool,
) {
    match outcome {
        FuzzStageOutcome::Success(_) => {}
        FuzzStageOutcome::Refused(code) => {
            if refusal.is_none() {
                *refusal = Some(code.clone());
            }
        }
        FuzzStageOutcome::Panicked => *panicked = true,
    }
}

struct MutateCaseReport {
    stages: FuzzStages,
    refusal: Option<String>,
    panicked: bool,
}

fn mutation_render_scale(page: &Page) -> Option<f32> {
    const BASE_SCALE: f64 = 18.0 / 72.0;
    const MAX_DEVICE_PIXELS: u64 = 1_000_000;

    let [left, bottom, right, top] = page.crop_box;
    let width = (right - left).abs() * page.user_unit;
    let height = (top - bottom).abs() * page.user_unit;
    if !width.is_finite()
        || !height.is_finite()
        || !page.user_unit.is_finite()
        || page.user_unit <= 0.0
        || width <= 0.0
        || height <= 0.0
    {
        return None;
    }

    let base_width = (width * BASE_SCALE).ceil().max(1.0) as u64;
    let base_height = (height * BASE_SCALE).ceil().max(1.0) as u64;
    let base_pixels = base_width.saturating_mul(base_height);
    if base_pixels <= MAX_DEVICE_PIXELS {
        return Some(BASE_SCALE as f32);
    }

    let target_width = ((MAX_DEVICE_PIXELS as f64 * base_width as f64 / base_height as f64)
        .sqrt()
        .floor()
        .max(1.0) as u64)
        .min(MAX_DEVICE_PIXELS);
    let target_height = (MAX_DEVICE_PIXELS / target_width).max(1);
    let scale = (target_width as f64 / width)
        .min(target_height as f64 / height)
        .min(BASE_SCALE) as f32;
    if scale.is_finite() && scale > 0.0 {
        Some(scale)
    } else {
        None
    }
}

fn write_fuzz_stage(json: &mut JsonWriter, counts: &FuzzStageCounts) {
    json.begin_object();
    json.member("attempted");
    json.number(counts.attempted);
    json.member("ok");
    json.number(counts.ok);
    json.member("refused");
    json.number(counts.refused);
    json.member("refusal_codes");
    json.begin_object();
    for (code, occurrences) in &counts.refusal_codes {
        json.member(code);
        json.number(*occurrences);
    }
    json.end_object();
    json.member("panicked");
    json.number(counts.panicked);
    json.end_object();
}

fn write_fuzz_stages(json: &mut JsonWriter, stages: &FuzzStages) {
    json.begin_object();
    json.member("source");
    write_fuzz_stage(json, &stages.source);
    json.member("strict_open");
    write_fuzz_stage(json, &stages.strict_open);
    json.member("repair_open");
    write_fuzz_stage(json, &stages.repair_open);
    json.member("page_walk");
    write_fuzz_stage(json, &stages.page_walk);
    json.member("content_streams");
    write_fuzz_stage(json, &stages.content_streams);
    json.member("text");
    write_fuzz_stage(json, &stages.text);
    json.member("semantic_document");
    write_fuzz_stage(json, &stages.semantic_document);
    json.member("load_image");
    write_fuzz_stage(json, &stages.load_image);
    json.member("render_page_0");
    write_fuzz_stage(json, &stages.render_page_0);
    json.end_object();
}

fn run_mutated(bytes: Vec<u8>, limits: Limits) -> MutateCaseReport {
    let mut report = MutateCaseReport {
        stages: FuzzStages::default(),
        refusal: None,
        panicked: false,
    };

    let source_result = run_fuzz_stage(&mut report.stages.source, || {
        PdfSource::new(Arc::<[u8]>::from(bytes)).map_err(|_| "source".to_string())
    });
    remember_fuzz_outcome(&source_result, &mut report.refusal, &mut report.panicked);
    let source = match source_result {
        FuzzStageOutcome::Success(source) => Some(source),
        FuzzStageOutcome::Refused(_) | FuzzStageOutcome::Panicked => None,
    };

    let mut document = None;
    if let Some(source) = source.as_ref() {
        let strict_result = run_fuzz_stage(&mut report.stages.strict_open, || {
            Document::open(source.clone(), limits).map_err(|error| error.code().to_string())
        });
        remember_fuzz_outcome(&strict_result, &mut report.refusal, &mut report.panicked);
        match strict_result {
            // Later stages see what a robust consumer gets: the repair-mode document, whose
            // page walk rebuilds a broken page tree even when the xref opened strictly.
            FuzzStageOutcome::Success(opened) => {
                document = Some(
                    Document::open_with(source.clone(), limits, pdf_core::OpenMode::Repair)
                        .unwrap_or(opened),
                );
            }
            FuzzStageOutcome::Refused(_) | FuzzStageOutcome::Panicked => {
                let repair_result = run_fuzz_stage(&mut report.stages.repair_open, || {
                    Document::open_with(source.clone(), limits, pdf_core::OpenMode::Repair)
                        .map_err(|error| error.code().to_string())
                });
                remember_fuzz_outcome(&repair_result, &mut report.refusal, &mut report.panicked);
                if let FuzzStageOutcome::Success(opened) = repair_result {
                    document = Some(opened);
                }
            }
        }
    }

    let mut pages = None;
    if let Some(document) = document.as_ref() {
        let page_result = run_fuzz_stage(&mut report.stages.page_walk, || {
            let mut resolver = document.resolver();
            let walk = PageWalk::new(&mut resolver).map_err(|error| error.code().to_string())?;
            let mut pages = Vec::new();
            for item in walk {
                pages.push(item.map_err(|error| error.code().to_string())?);
            }
            Ok(pages)
        });
        remember_fuzz_outcome(&page_result, &mut report.refusal, &mut report.panicked);
        if let FuzzStageOutcome::Success(walked) = page_result {
            pages = Some(walked);
        }

        if let Some(pages) = pages.as_ref() {
            let stream_result = run_fuzz_stage(&mut report.stages.content_streams, || {
                let mut resolver = document.resolver();
                for page in pages {
                    for id in &page.contents {
                        let object = resolver
                            .get(*id)
                            .map_err(|error| error.code().to_string())?;
                        resolver
                            .decode_stream(&object)
                            .map_err(|error| error.code().to_string())?;
                    }
                }
                Ok(())
            });
            remember_fuzz_outcome(&stream_result, &mut report.refusal, &mut report.panicked);

            let text_result = run_fuzz_stage(&mut report.stages.text, || {
                let mut refusal = None;
                let _ = text::scan_text(document, pages, &mut refusal);
                match refusal {
                    Some(code) => Err(code),
                    None => Ok(()),
                }
            });
            remember_fuzz_outcome(&text_result, &mut report.refusal, &mut report.panicked);
        }
    }

    let mut semantic = None;
    if let Some(source) = source.as_ref() {
        let semantic_result = run_fuzz_stage(&mut report.stages.semantic_document, || {
            pdf_text::semantic::semantic_document_with(
                source.clone(),
                limits,
                pdf_core::OpenMode::Repair,
            )
            .map_err(|error| error.code().to_string())
        });
        remember_fuzz_outcome(&semantic_result, &mut report.refusal, &mut report.panicked);
        if let FuzzStageOutcome::Success(value) = semantic_result {
            semantic = Some(value);
        }
    }

    if let Some(semantic) = semantic.as_ref() {
        for page in &semantic.pages {
            for image in &page.images {
                let Some(object) = image.object else {
                    record_fuzz_refusal(&mut report.stages.load_image, "image_without_object");
                    if report.refusal.is_none() {
                        report.refusal = Some("image_without_object".to_string());
                    }
                    continue;
                };

                let Some(document) = document.as_ref() else {
                    record_fuzz_refusal(&mut report.stages.load_image, "document_unavailable");
                    if report.refusal.is_none() {
                        report.refusal = Some("document_unavailable".to_string());
                    }
                    continue;
                };

                let image_result = run_fuzz_stage(&mut report.stages.load_image, || {
                    let mut resolver = document.resolver();
                    pdf_text::images::load_image(
                        &mut resolver,
                        object,
                        limits.max_decoded_bytes / 4,
                    )
                    .map(|_| ())
                    .map_err(|error| error.code().to_string())
                });
                remember_fuzz_outcome(&image_result, &mut report.refusal, &mut report.panicked);
            }
        }
    }

    if let Some(document) = document.as_ref() {
        let page_zero = pages
            .as_ref()
            .and_then(|pages| pages.iter().find(|page| page.index == 0));
        if let Some(page) = page_zero {
            let render_result = run_fuzz_stage(&mut report.stages.render_page_0, || {
                let scale =
                    mutation_render_scale(page).ok_or_else(|| "invalid_page_bounds".to_string())?;
                let options = RenderOptions {
                    scale,
                    fallback_font: None,
                };
                let mut resolver = document.resolver();
                let raster = rasterize_page(&mut resolver, page, &options, [255, 255, 255, 255])
                    .map_err(|_| "render_failed".to_string())?;
                let _ = std::hint::black_box(raster);
                Ok(())
            });
            remember_fuzz_outcome(&render_result, &mut report.refusal, &mut report.panicked);
        } else {
            record_fuzz_refusal(&mut report.stages.render_page_0, "page_0_unavailable");
            if report.refusal.is_none() {
                report.refusal = Some("page_0_unavailable".to_string());
            }
        }
    }

    report
}

fn run_mutate(path: &Path, seed: u64, count: usize) -> u8 {
    let source = match file::open(path, Limits::default().max_file_bytes) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("{error}");
            return 3;
        }
    };
    let original = source.bytes().to_vec();
    let mut master_state = seed;
    let mut ok = 0u64;
    let mut refused: BTreeMap<String, u64> = BTreeMap::new();
    let mut panicked = 0u64;
    let mut panic_seeds = Vec::new();
    let mut stages = FuzzStages::default();

    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));

    app_caps::check("iterations", count as u64);
    for _ in 0..count.min(MAX_MUTATE_CASES) {
        let case_seed = next_random(&mut master_state);
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut mutated = original.clone();
            let mut case_state = case_seed;
            mutate_bytes(&mut mutated, &mut case_state);
            run_mutated(mutated, Limits::default())
        }));

        match outcome {
            Ok(case) => {
                stages.merge(&case.stages);
                if case.panicked {
                    panicked = panicked.saturating_add(1);
                    panic_seeds.push(case_seed);
                } else if let Some(code) = case.refusal {
                    let occurrences = refused.entry(code).or_insert(0);
                    *occurrences = occurrences.saturating_add(1);
                } else {
                    ok = ok.saturating_add(1);
                }
            }
            Err(_) => {
                panicked = panicked.saturating_add(1);
                panic_seeds.push(case_seed);
            }
        }
    }

    std::panic::set_hook(previous_hook);

    let mut json = JsonWriter::new();
    json.begin_object();
    json.member("cases");
    json.number(count.min(MAX_MUTATE_CASES) as u64);
    json.member("ok");
    json.number(ok);
    json.member("refused");
    json.begin_object();
    for (code, occurrences) in refused {
        json.member(&code);
        json.number(occurrences);
    }
    json.end_object();
    json.member("panicked");
    json.number(panicked);
    json.member("panic_seeds");
    json.begin_array();
    for seed in panic_seeds {
        json.number(seed);
    }
    json.end_array();
    json.member("stages");
    write_fuzz_stages(&mut json, &stages);
    json.end_object();
    out!("{}", json.finish());

    if panicked > 0 { 2 } else { 0 }
}

struct Manifest {
    expectations: Vec<(String, String)>,
    limit_errors: Vec<(String, String)>,
    limits: Limits,
}

fn parse_manifest(text: &str) -> Result<Manifest, String> {
    let mut manifest = Manifest {
        expectations: Vec::new(),
        limit_errors: Vec::new(),
        limits: Limits::default(),
    };

    for (line_number, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            return Err(format!("line {} has no '='", line_number.saturating_add(1)));
        };
        let key = key.trim().to_string();
        let value = value.trim().to_string();
        if key.is_empty() {
            return Err(format!(
                "line {} has an empty key",
                line_number.saturating_add(1)
            ));
        }

        if key.starts_with("limit.") {
            if !apply_limit_override(&mut manifest.limits, &key, &value) {
                manifest.limit_errors.push((key, value));
            }
        } else {
            manifest.expectations.push((key, value));
        }
    }
    Ok(manifest)
}

fn apply_limit_override(limits: &mut Limits, key: &str, value: &str) -> bool {
    let Ok(number) = value.parse::<u64>() else {
        return false;
    };
    match key {
        "limit.max_decoded_bytes" => limits.max_decoded_bytes = number,
        "limit.max_content_ops" => limits.max_content_ops = number,
        "limit.max_prev_chain" => {
            let Ok(number) = u32::try_from(number) else {
                return false;
            };
            limits.max_prev_chain = number;
        }
        "limit.max_depth" => {
            let Ok(number) = u32::try_from(number) else {
                return false;
            };
            limits.max_depth = number;
        }
        "limit.max_pages" => {
            let Ok(number) = u32::try_from(number) else {
                return false;
            };
            limits.max_pages = number;
        }
        "limit.max_page_tree_depth" => {
            let Ok(number) = u32::try_from(number) else {
                return false;
            };
            limits.max_page_tree_depth = number;
        }
        _ => return false,
    }
    true
}

fn box_manifest(bounds: [f64; 4]) -> String {
    let mut parts = Vec::new();
    for value in bounds {
        parts.push(format_number(value));
    }
    parts.join(" ")
}

fn resource_keys_manifest(page: &Page) -> String {
    let keys = page_resource_keys(page);
    let mut parts = Vec::new();
    for key in keys {
        parts.push(String::from_utf8_lossy(&key).into_owned());
    }
    parts.join(",")
}

fn semantic_spans_manifest(block: &pdf_text::semantic::SemanticBlock) -> String {
    let mut spans = Vec::new();
    for span in &block.spans {
        let mut value = span.text.clone();
        if span.style.bold {
            value.push_str("[b]");
        }
        if span.style.italic {
            value.push_str("[i]");
        }
        if span.style.monospace {
            value.push_str("[m]");
        }
        spans.push(value);
    }
    spans.join(" | ")
}

fn add_semantic_fixture_values(
    actuals: &mut BTreeMap<String, String>,
    semantic: &pdf_text::semantic::SemanticDoc,
    resolver: &mut Resolver<'_>,
    limits: Limits,
) {
    actuals.insert(
        "info.title".to_string(),
        semantic.info.title.clone().unwrap_or_default(),
    );

    let mut outline = Vec::new();
    for item in &semantic.outline {
        let prefix = ">".repeat(usize::from(item.level.saturating_sub(1)));
        outline.push(format!("{prefix}{}", item.title));
    }
    actuals.insert("outline".to_string(), outline.join(" | "));

    for page in &semantic.pages {
        let prefix = format!("page.{}", page.index);
        let mut links = Vec::new();
        for link in &page.links {
            match &link.target {
                pdf_text::semantic::LinkTarget::Uri(uri) => links.push(format!("uri:{uri}")),
                pdf_text::semantic::LinkTarget::Page { index, .. } => {
                    links.push(format!("page:{index}"));
                }
            }
        }
        actuals.insert(format!("{prefix}.links"), links.join(" | "));
        actuals.insert(format!("{prefix}.images"), page.images.len().to_string());
        actuals.insert(
            format!("{prefix}.spans"),
            page.blocks
                .first()
                .map(semantic_spans_manifest)
                .unwrap_or_default(),
        );
    }

    // Every image object, painted or not (a soft mask is never painted itself):
    // fixtures are small, so a scan over the xref is cheap and finds them all.
    let size = resolver.document().xref().size();
    for number in 1..size {
        let Some(pdf_core::XrefEntry::InUse { generation, .. }) =
            resolver.document().xref().get(number)
        else {
            continue;
        };
        let object = pdf_core::ObjectRef::new(number, generation);
        let is_image = resolver
            .get(object)
            .ok()
            .and_then(|found| {
                found
                    .dict()
                    .and_then(|dict| dict.get(b"Subtype"))
                    .and_then(|value| value.as_name())
                    .map(|name| name.eq_bytes(b"Image"))
            })
            .unwrap_or(false);
        if !is_image {
            continue;
        }
        if let Ok(image) =
            pdf_text::images::load_image(resolver, object, limits.max_decoded_bytes / 4)
        {
            let kind = image_kind(&image.data);
            actuals.insert(format!("image.{number}.kind"), kind);
            if let pdf_text::images::ImageData::Pixels { rgba } = &image.data {
                let mut hasher = Sha256::new();
                hasher.update(rgba);
                actuals.insert(
                    format!("image.{number}.rgba_sha256"),
                    digest_hex(&hasher.finalize()),
                );
            }
        }
    }
}

/// Builds the semantic document through SemanticReader so its warnings (ignored
/// actions, dropped link schemes) reach the fixture check.
fn semantic_with_warnings(
    source: PdfSource,
    limits: Limits,
) -> (Option<pdf_text::semantic::SemanticDoc>, Vec<String>) {
    let Ok(mut reader) = pdf_text::semantic::SemanticReader::open(source, limits) else {
        return (None, Vec::new());
    };
    let info = reader.info().clone();
    let outline = reader.outline();
    let mut pages = Vec::new();
    while let Some(page) = reader.next_page() {
        pages.push(page);
    }
    let warnings = reader
        .take_warnings()
        .iter()
        .map(|warning| warning.what.to_string())
        .collect();
    (
        Some(pdf_text::semantic::SemanticDoc {
            info,
            outline,
            pages,
        }),
        warnings,
    )
}

fn parse_render_page_key(key: &str) -> Option<u32> {
    key.strip_prefix("page.")?
        .strip_suffix(".render")?
        .parse()
        .ok()
}

fn parse_pixel_key(key: &str) -> Option<(u32, u32, u32)> {
    let value = key.strip_prefix("page.")?;
    let (page, coordinates) = value.split_once(".pixel.")?;
    let (x, y) = coordinates.split_once('.')?;
    if y.contains('.') {
        return None;
    }
    Some((page.parse().ok()?, x.parse().ok()?, y.parse().ok()?))
}

fn mark_render_failure(
    actuals: &mut BTreeMap<String, String>,
    expectations: &[(String, String)],
    page_index: u32,
    code: &str,
) {
    actuals.insert(format!("page.{page_index}.render"), code.to_string());
    for (key, _) in expectations {
        if parse_pixel_key(key).is_some_and(|(index, _, _)| index == page_index) {
            actuals.insert(key.clone(), "unavailable".to_string());
        }
    }
}

fn mark_requested_render_failures(
    actuals: &mut BTreeMap<String, String>,
    expectations: &[(String, String)],
    code: &str,
) {
    let mut indices = BTreeMap::new();
    for (key, _) in expectations {
        if let Some(index) =
            parse_render_page_key(key).or_else(|| parse_pixel_key(key).map(|(index, _, _)| index))
        {
            indices.insert(index, ());
        }
    }
    for index in indices.keys().copied() {
        mark_render_failure(actuals, expectations, index, code);
    }
}

fn pixel_rgb(raster: &pdf_raster::Raster, x: u32, y: u32) -> Option<String> {
    if x >= raster.width || y >= raster.height {
        return None;
    }
    let width = usize::try_from(raster.width).ok()?;
    let x = usize::try_from(x).ok()?;
    let y = usize::try_from(y).ok()?;
    let pixel = y.checked_mul(width)?.checked_add(x)?;
    let start = pixel.checked_mul(4)?;
    let end = start.checked_add(3)?;
    let rgb = raster.rgba.get(start..end)?;
    let r = rgb.first().copied()?;
    let g = rgb.get(1).copied()?;
    let b = rgb.get(2).copied()?;
    Some(format!("{r:02x}{g:02x}{b:02x}"))
}

fn parse_rgb_hex(value: &str) -> Option<[u8; 3]> {
    if value.len() != 6 {
        return None;
    }
    Some([
        u8::from_str_radix(value.get(0..2)?, 16).ok()?,
        u8::from_str_radix(value.get(2..4)?, 16).ok()?,
        u8::from_str_radix(value.get(4..6)?, 16).ok()?,
    ])
}

fn pixel_manifest_matches(expected: &str, actual: &str) -> bool {
    let (Some(expected), Some(actual)) = (parse_rgb_hex(expected), parse_rgb_hex(actual)) else {
        return false;
    };
    expected
        .iter()
        .zip(actual.iter())
        .all(|(expected, actual)| expected.abs_diff(*actual) <= 8)
}

fn render_fixture_expectations(
    actuals: &mut BTreeMap<String, String>,
    document: &Document,
    pages: &[Page],
    expectations: &[(String, String)],
    warnings: &mut Vec<WarningRecord>,
) {
    let mut indices = BTreeMap::new();
    for (key, _) in expectations {
        if let Some(index) =
            parse_render_page_key(key).or_else(|| parse_pixel_key(key).map(|(index, _, _)| index))
        {
            indices.insert(index, ());
        }
    }

    for index in indices.keys().copied() {
        let Some(page) = pages.iter().find(|page| page.index == index) else {
            mark_render_failure(actuals, expectations, index, "missing_page");
            continue;
        };
        let mut resolver = document.resolver();
        let options = RenderOptions {
            scale: 1.0,
            fallback_font: None,
        };
        match rasterize_page(&mut resolver, page, &options, [255, 255, 255, 255]) {
            Ok(raster) => {
                actuals.insert(format!("page.{index}.render"), "ok".to_string());
                for (key, _) in expectations {
                    if let Some((page_index, x, y)) = parse_pixel_key(key)
                        && page_index == index
                    {
                        actuals.insert(
                            key.clone(),
                            pixel_rgb(&raster, x, y).unwrap_or_else(|| "unavailable".to_string()),
                        );
                    }
                }
            }
            Err(error) => {
                mark_render_failure(actuals, expectations, index, error.code());
            }
        }
        append_resolver_warnings(&mut resolver, warnings);
    }
}

fn actual_fixture_values(
    path: &Path,
    limits: Limits,
    expectations: &[(String, String)],
) -> Result<(BTreeMap<String, String>, Vec<String>), String> {
    let source = file::open(path, limits.max_file_bytes).map_err(|error| error.to_string())?;
    let mut actuals = BTreeMap::new();
    actuals.insert("version".to_string(), source.header().version().to_string());

    let password = expectations
        .iter()
        .find(|(key, _)| key == "open.password")
        .map(|(_, value)| value.as_bytes());
    let (semantic, semantic_warnings) = if password.is_some() {
        (None, Vec::new())
    } else {
        semantic_with_warnings(source.clone(), limits)
    };
    let opened = if let Some(password) = password {
        actuals.insert(
            "open.password".to_string(),
            String::from_utf8_lossy(password).into_owned(),
        );
        let absent = match Document::open(source.clone(), limits) {
            Ok(_) => "none",
            Err(error) => error.code(),
        };
        actuals.insert("open.without_password".to_string(), absent.to_string());
        let mut wrong = password.to_vec();
        wrong.push(0);
        let wrong = match Document::open_with_password(source.clone(), limits, &wrong) {
            Ok(_) => "none",
            Err(error) => error.code(),
        };
        actuals.insert("open.wrong_password".to_string(), wrong.to_string());
        Document::open_with_password(source.clone(), limits, password)
    } else {
        Document::open(source.clone(), limits)
    };
    let document = match opened {
        Ok(document) => {
            actuals.insert("refusal".to_string(), "none".to_string());
            document
        }
        Err(error) => {
            actuals.insert("refusal".to_string(), error.code().to_string());
            mark_requested_render_failures(&mut actuals, expectations, error.code());
            return Ok((actuals, Vec::new()));
        }
    };

    actuals.insert(
        "xref_kind".to_string(),
        xref_kind_name(document.xref().kind()).to_string(),
    );
    actuals.insert(
        "xref_sections".to_string(),
        document.xref().sections().to_string(),
    );
    actuals.insert("encrypted".to_string(), document.encrypted().to_string());

    if let Some(password) = password {
        let mut resolver = document.resolver();
        let mut corruption = "unavailable".to_string();
        for number in 1..document.xref().size() {
            let Some(pdf_core::XrefEntry::InUse { generation, .. }) = document.xref().get(number)
            else {
                continue;
            };
            let object = resolver
                .get(pdf_core::ObjectRef::new(number, generation))
                .map_err(|error| error.to_string())?;
            let xref = object
                .dict()
                .and_then(|dict| dict.get(b"Type"))
                .and_then(|value| value.as_name())
                .is_some_and(|name| name.eq_bytes(b"XRef"));
            if xref || object.id() == document.encryption_ref() {
                continue;
            }
            if let Some(range) = object.stream_range()
                && range.len() >= 40
            {
                let mut damaged = source.bytes().to_vec();
                let position = range
                    .start
                    .checked_add(24)
                    .ok_or_else(|| "ciphertext position overflow".to_string())?;
                let byte = damaged
                    .get_mut(position)
                    .ok_or_else(|| "ciphertext position outside file".to_string())?;
                *byte ^= 1;
                let damaged = PdfSource::new(std::sync::Arc::<[u8]>::from(damaged))
                    .map_err(|error| error.to_string())?;
                corruption = match Document::open_with_password(damaged, limits, password) {
                    Ok(_) => "none".to_string(),
                    Err(error) => error.code().to_string(),
                };
                break;
            }
        }
        actuals.insert("open.corrupt_ciphertext".to_string(), corruption);
        if let Some(info) = document.info_ref() {
            let object = resolver.get(info).map_err(|error| error.to_string())?;
            if let Some(title) = object
                .dict()
                .and_then(|dict| dict.get(b"Title"))
                .and_then(|value| value.as_str())
            {
                let bytes = resolver.decode_string(info, title);
                actuals.insert("info.title".to_string(), metadata::text_string(&bytes));
            }
        }
    }

    let mut report = inspect_pages(&document);
    actuals.insert("pages".to_string(), report.pages.len().to_string());
    actuals.insert("page_errors".to_string(), report.errors.len().to_string());

    let mut resolver = document.resolver();
    let mut text_resolver = document.resolver();
    let mut image_resolver = document.resolver();
    if let Some(semantic) = semantic.as_ref() {
        add_semantic_fixture_values(&mut actuals, semantic, &mut image_resolver, limits);
    }
    metadata::info_values(&mut actuals, &document, &mut resolver, password.is_some());
    signatures::fixture_values(&mut actuals, source.bytes(), limits, expectations);
    for page in &report.pages {
        let prefix = format!("page.{}", page.index);
        actuals.insert(format!("{prefix}.media_box"), box_manifest(page.media_box));
        actuals.insert(format!("{prefix}.crop_box"), box_manifest(page.crop_box));
        actuals.insert(format!("{prefix}.rotate"), page.rotate.to_string());
        metadata::page_boxes(&mut actuals, &prefix, &mut resolver, page.id);
        actuals.insert(
            format!("{prefix}.contents"),
            page.contents.len().to_string(),
        );
        let annotation_count = match resolver.get(page.id) {
            Ok(object) => pdf_text::annots::page_annotations(&mut resolver, &object).len(),
            Err(_) => 0,
        };
        actuals.insert(
            format!("{prefix}.annotations"),
            annotation_count.to_string(),
        );
        actuals.insert(
            format!("{prefix}.resource_keys"),
            resource_keys_manifest(page),
        );
        match resolver.get(page.id) {
            Ok(object) => {
                let annotations = pdf_text::annots::page_annotations(&mut resolver, &object);
                actuals.insert(
                    format!("{prefix}.annotations"),
                    annotations.len().to_string(),
                );
            }
            Err(error) => {
                actuals.insert(format!("{prefix}.annotations"), error.code().to_string());
            }
        }

        let mut hasher = Sha256::new();
        let mut content_refusal = None;
        for id in &page.contents {
            match resolver.get(*id) {
                Ok(object) => match resolver.decode_stream(&object) {
                    Ok(decoded) => hasher.update(&decoded.bytes),
                    Err(error) => {
                        if content_refusal.is_none() {
                            content_refusal = Some(error.code().to_string());
                        }
                    }
                },
                Err(error) => {
                    if content_refusal.is_none() {
                        content_refusal = Some(error.code().to_string());
                    }
                }
            }
        }
        actuals.insert(
            format!("{prefix}.content_refusal"),
            content_refusal
                .clone()
                .unwrap_or_else(|| "none".to_string()),
        );
        if content_refusal.is_none() {
            actuals.insert(
                format!("{prefix}.content_sha256"),
                digest_hex(&hasher.finalize()),
            );
        }
        let receipt = pdf_text::extract::page_receipt(&mut text_resolver, page);
        text::fixture_values(&mut actuals, &prefix, &receipt);
    }

    render_fixture_expectations(
        &mut actuals,
        &document,
        &report.pages,
        expectations,
        &mut report.warnings,
    );
    append_resolver_warnings(&mut resolver, &mut report.warnings);
    append_resolver_warnings(&mut text_resolver, &mut report.warnings);
    append_resolver_warnings(&mut image_resolver, &mut report.warnings);
    let mut warnings = report
        .warnings
        .iter()
        .map(|warning| warning.code.to_string())
        .collect::<Vec<_>>();
    warnings.extend(semantic_warnings);
    Ok((actuals, warnings))
}

fn run_check(directory: &Path) -> u8 {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) => {
            eprintln!("{error}");
            return 3;
        }
    };

    let mut pdf_paths = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                eprintln!("{error}");
                return 3;
            }
        };
        let path = entry.path();
        if path.extension() == Some(OsStr::new("pdf")) {
            app_caps::check("cache-entries", pdf_paths.len() as u64 + 1);
            pdf_paths.push(path);
        }
    }
    pdf_paths.sort_by(|left, right| left.file_name().cmp(&right.file_name()));

    let mut passed_fixtures = 0u64;
    let mut failed_fixtures = 0u64;
    let mut checked_keys = 0u64;

    for pdf_path in pdf_paths {
        let Some(stem) = pdf_path.file_stem() else {
            continue;
        };
        let fixture_name = stem.to_string_lossy().into_owned();
        let mut expect_name = stem.to_os_string();
        expect_name.push(".expect");
        let expect_path = directory.join(expect_name);
        if !expect_path.is_file() {
            continue;
        }

        let manifest_text = match std::fs::read_to_string(&expect_path) {
            Ok(text) => text,
            Err(error) => {
                eprintln!("{error}");
                return 3;
            }
        };
        let manifest = match parse_manifest(&manifest_text) {
            Ok(manifest) => manifest,
            Err(error) => {
                out!("FAIL {fixture_name} manifest expected=valid got={error}");
                failed_fixtures = failed_fixtures.saturating_add(1);
                checked_keys = checked_keys.saturating_add(1);
                continue;
            }
        };

        let mut fixture_failed = false;
        for (key, expected) in &manifest.limit_errors {
            out!("FAIL {fixture_name} {key} expected={expected} got=invalid_limit");
            fixture_failed = true;
            checked_keys = checked_keys.saturating_add(1);
        }

        let (actuals, warnings) =
            match actual_fixture_values(&pdf_path, manifest.limits, &manifest.expectations) {
                Ok(values) => values,
                Err(error) => {
                    eprintln!("{error}");
                    return 3;
                }
            };

        for (key, expected) in &manifest.expectations {
            let actual = if key == "warning" {
                if warnings.iter().any(|warning| warning == expected) {
                    Some(expected.clone())
                } else {
                    None
                }
            } else {
                actuals.get(key).cloned()
            };
            let got = actual
                .as_ref()
                .cloned()
                .unwrap_or_else(|| "unavailable".to_string());

            let matches_expected = if key.starts_with("page.") && key.contains(".pixel.") {
                actual
                    .as_deref()
                    .map(|value| pixel_manifest_matches(expected, value))
                    .unwrap_or(false)
            } else {
                actual.as_deref() == Some(expected.as_str())
            };
            if matches_expected {
                out!("PASS {fixture_name} {key}");
            } else {
                out!("FAIL {fixture_name} {key} expected={expected} got={got}");
                fixture_failed = true;
            }
            checked_keys = checked_keys.saturating_add(1);
        }

        if fixture_failed {
            failed_fixtures = failed_fixtures.saturating_add(1);
        } else {
            passed_fixtures = passed_fixtures.saturating_add(1);
        }
    }

    out!("fixtures: {passed_fixtures} passed, {failed_fixtures} failed, {checked_keys} keys");
    if failed_fixtures > 0 { 1 } else { 0 }
}
