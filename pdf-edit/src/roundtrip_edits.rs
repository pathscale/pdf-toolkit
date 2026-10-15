use super::*;

use pdf_core::{DocError, LimitKind, Object, Refusal, Resolver, Value};
use pdf_text::annots::page_annotations;
use pdf_write::annot::{Annot, AnnotKind};

const CONTENTS: &str = "Wave 11 café";
const SUBTYPES: [&str; 6] = ["Highlight", "Text", "Ink", "FreeText", "Square", "Circle"];

fn edit_limits() -> Limits {
    // Reader default depth: deep-nesting is built to open at the default limit.
    Limits {
        max_pages: 4096,
        max_depth: Limits::default().max_depth,
        ..limits()
    }
}

fn open(bytes: Arc<[u8]>) -> CheckResult<Document> {
    Document::open(PdfSource::new(bytes).map_err(describe)?, edit_limits()).map_err(describe)
}

fn pages(document: &Document) -> CheckResult<Vec<Page>> {
    let mut resolver = document.resolver();
    PageWalk::new(&mut resolver)
        .map_err(describe)?
        .map(|page| page.map_err(describe))
        .collect()
}

fn page_object(resolver: &mut Resolver<'_>, page: &Page) -> CheckResult<Object> {
    resolver.get(page.id).map_err(describe)
}

/// Hash encoded content bytes in stream order, without inflating hostile fixtures.
fn content_hash(document: &Document, page: &Page) -> CheckResult<[u8; 32]> {
    let mut resolver = document.resolver();
    let object = page_object(&mut resolver, page)?;
    let contents = object.dict().and_then(|dict| dict.get(b"Contents"));
    let mut stack = Vec::new();
    if let Some(contents) = contents {
        stack.push((resolver.resolve(contents).map_err(describe)?, 0usize));
    }
    let mut bytes = Vec::new();
    let mut steps = 0u64;
    while let Some((value, depth)) = stack.pop() {
        bounded("depth", depth as u64, 32)?;
        steps = steps.saturating_add(1);
        bounded("content_ops", steps, edit_limits().max_content_ops)?;
        if value.object().is_stream() {
            let raw = value
                .object()
                .stream_raw()
                .ok_or_else(|| "missing content bytes".to_owned())?;
            bounded(
                "object_bytes",
                (bytes.len() as u64)
                    .saturating_add(raw.len() as u64)
                    .saturating_add(8),
                edit_limits().max_object_bytes,
            )?;
            bytes
                .try_reserve(raw.len().saturating_add(8))
                .map_err(describe)?;
            bytes.extend_from_slice(&(raw.len() as u64).to_be_bytes());
            bytes.extend_from_slice(raw);
        } else if let Some(array) = value.get().as_array() {
            bounded(
                "tokens",
                array.len() as u64,
                u64::from(edit_limits().max_tokens_per_object),
            )?;
            let children: Vec<Value> = array
                .iter()
                .map(|child| resolver.resolve(child).map_err(describe))
                .collect::<CheckResult<_>>()?;
            for child in children.into_iter().rev() {
                stack.push((child, depth.saturating_add(1)));
            }
        } else if !value.get().is_null() {
            return Err("invalid page Contents".to_owned());
        }
    }
    Ok(pdf_write::sha256::digest(&bytes))
}

fn annotations(rect: [f32; 4]) -> Vec<Annot> {
    let [x0, y0, x1, y1] = rect;
    let kinds = [
        AnnotKind::Highlight {
            quads: vec![[x0, y1, x1, y1, x0, y0, x1, y0]],
        },
        AnnotKind::Note,
        AnnotKind::Ink {
            paths: vec![vec![[x0 + 2.0, y0 + 2.0], [x1 - 2.0, y1 - 2.0]]],
            width: 2.0,
        },
        AnnotKind::FreeText {
            text: "HOUSE".to_owned(),
            size: 10.0,
        },
        AnnotKind::Square { width: 2.0 },
        AnnotKind::Circle { width: 2.0 },
    ];
    kinds
        .into_iter()
        .map(|kind| Annot {
            kind,
            rect,
            color: [0.9, 0.2, 0.7],
            opacity: 0.75,
            contents: Some(CONTENTS.to_owned()),
            author: Some("House author".to_owned()),
        })
        .collect()
}

fn render(document: &Document, page: &Page, font: Arc<[u8]>) -> CheckResult<Raster> {
    pdf_raster::render_page(
        &mut document.resolver(),
        page,
        &RenderOptions {
            fallback_fonts: Default::default(),
            scale: 1.0,
            fallback_font: Some(font),
        },
        [255; 4],
    )
    .map_err(describe)
}

fn compare_pixels(
    report: &mut Report,
    name: &str,
    before: &Raster,
    after: &Raster,
    page: &Page,
    rect: [f32; 4],
) {
    report.equal(
        name,
        "annotation.raster-size",
        &(after.width, after.height),
        &(before.width, before.height),
    );
    if before.width != after.width || before.height != after.height || before.width == 0 {
        return;
    }
    let transform = pdf_text::interp::display_transform(page);
    let [x0, y0, x1, y1] = rect;
    let mut bounds = [
        f32::INFINITY,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::NEG_INFINITY,
    ];
    for (x, y) in [(x0, y0), (x0, y1), (x1, y0), (x1, y1)] {
        let (x, y) = transform(x, y);
        bounds[0] = bounds[0].min(x);
        bounds[1] = bounds[1].min(y);
        bounds[2] = bounds[2].max(x);
        bounds[3] = bounds[3].max(y);
    }
    let mut inside = 0usize;
    let mut outside = 0usize;
    for (index, (a, b)) in before
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .zip(after.rgba.as_chunks::<4>().0)
        .enumerate()
    {
        if a == b {
            continue;
        }
        let x = (index % before.width as usize) as f32 + 0.5;
        let y = (index / before.width as usize) as f32 + 0.5;
        if x >= bounds[0] && x <= bounds[2] && y >= bounds[1] && y <= bounds[3] {
            inside = inside.saturating_add(1);
        } else if x < bounds[0] - 3.0
            || x > bounds[2] + 3.0
            || y < bounds[1] - 3.0
            || y > bounds[3] + 3.0
        {
            outside = outside.saturating_add(1);
        }
    }
    report.check(
        name,
        "annotation.pixels-inside",
        inside != 0,
        "appearance changed no pixels inside Rect",
    );
    report.equal(name, "annotation.pixels-far-outside", &outside, &0);
}

fn verify_annotations(
    report: &mut Report,
    name: &str,
    document: &Document,
    page: &Page,
    first: usize,
    rect: [f32; 4],
) -> CheckResult<()> {
    let mut resolver = document.resolver();
    let object = page_object(&mut resolver, page)?;
    let listed = page_annotations(&mut resolver, &object);
    for (offset, subtype) in SUBTYPES.iter().enumerate() {
        let index = first.saturating_add(offset);
        let label = format!("annotation.{offset}");
        let Some(annotation) = listed.iter().find(|annotation| annotation.index == index) else {
            report.check(
                name,
                &label,
                false,
                "missing annotation at original array index",
            );
            continue;
        };
        report.equal(
            name,
            &format!("{label}.subtype"),
            &annotation.subtype.as_str(),
            subtype,
        );
        // A note keeps a fixed 20x20 icon anchored at the requested top-left corner.
        let expected = if *subtype == "Text" {
            let left = rect[0].min(rect[2]);
            let top = rect[1].max(rect[3]);
            [left, top - 20.0, left + 20.0, top]
        } else {
            rect
        };
        report.check(
            name,
            &format!("{label}.rect"),
            annotation
                .rect
                .iter()
                .zip(expected)
                .all(|(a, b)| (*a - b).abs() <= 0.001),
            "annotation rectangle changed",
        );
        report.equal(
            name,
            &format!("{label}.contents"),
            &annotation.contents.as_deref(),
            &Some(CONTENTS),
        );
        report.equal(name, &format!("{label}.flags"), &annotation.flags, &4);
        report.check(
            name,
            &format!("{label}.appearance"),
            annotation.has_appearance,
            "missing normal appearance",
        );
        report.check(
            name,
            &format!("{label}.color"),
            annotation.color.is_some_and(|color| {
                color
                    .iter()
                    .zip([0.9, 0.2, 0.7])
                    .all(|(a, b)| (*a - b).abs() <= 0.001)
            }),
            "annotation color changed",
        );
    }
    Ok(())
}

fn scripted(
    report: &mut Report,
    config: &Config,
    path: &Path,
    name: &str,
    original: Arc<[u8]>,
    mut editor: Editor,
    font: Arc<[u8]>,
) -> CheckResult<()> {
    let before = open(original.clone())?;
    let original_pages = pages(&before)?;
    let Some(first_page) = original_pages.first() else {
        report.check(name, "script.no-pages", true, "");
        return Ok(());
    };
    report.equal(
        name,
        "initial-page-count",
        &editor.page_count(),
        &(original_pages.len() as u32),
    );
    let mut expected = original_pages
        .iter()
        .map(|page| content_hash(&before, page).map(|hash| (hash, page.clone())))
        .collect::<CheckResult<Vec<_>>>()?;
    let [left, bottom, right, top] = first_page.crop_box;
    let rect = [
        (left + (right - left) * 0.1) as f32,
        (bottom + (top - bottom) * 0.1) as f32,
        (left + (right - left) * 0.4) as f32,
        (bottom + (top - bottom) * 0.3) as f32,
    ];
    let first_index = first_page.annots as usize;
    let before_pixels = render(&before, first_page, font.clone());
    for annot in annotations(rect) {
        editor.add_annotation(0, &annot).map_err(describe)?;
    }
    editor.add_uri_link(0, rect, EDIT_URI).map_err(describe)?;
    let temporary_index = first_index.saturating_add(SUBTYPES.len());
    let temporary = open(Arc::from(editor.save().map_err(describe)?))?;
    let mut resolver = temporary.resolver();
    let object = page_object(&mut resolver, first_page)?;
    let listed = page_annotations(&mut resolver, &object);
    report.check(
        name,
        "remove.link-index",
        listed
            .iter()
            .any(|annotation| annotation.index == temporary_index && annotation.subtype == "Link"),
        "temporary Link did not retain its /Annots index",
    );
    editor
        .remove_annotation(0, temporary_index)
        .map_err(describe)?;
    let annotated_bytes = editor.save().map_err(describe)?;
    report.check(
        name,
        "annotation.original-prefix",
        annotated_bytes.starts_with(&original),
        "original prefix changed",
    );
    let annotated = open(Arc::from(annotated_bytes))?;
    let annotated_pages = pages(&annotated)?;
    let annotated_page = annotated_pages
        .first()
        .ok_or_else(|| "missing annotated page".to_owned())?;
    verify_annotations(report, name, &annotated, annotated_page, first_index, rect)?;
    let after_pixels = render(&annotated, annotated_page, font.clone());
    match (before_pixels, after_pixels) {
        (Ok(before), Ok(after)) => compare_pixels(report, name, &before, &after, first_page, rect),
        (Err(before), Err(after)) => {
            report.equal(
                name,
                "annotation.renderer-refusal-preserved",
                &after,
                &before,
            );
        }
        (before, after) => report.check(
            name,
            "annotation.render",
            false,
            &format!("before={before:?}, after={after:?}"),
        ),
    }
    let stable = editor.save().map_err(describe)?;
    report.check(
        name,
        "invalid-remove-refused",
        editor.remove_annotation(0, usize::MAX).is_err(),
        "out-of-range annotation index accepted",
    );
    report.check(
        name,
        "invalid-import-refused",
        editor
            .import_pages(original.clone(), &[u32::MAX], 0)
            .is_err(),
        "out-of-range source page accepted",
    );
    report.equal(
        name,
        "failed-edits-atomic",
        &editor.save().map_err(describe)?,
        &stable,
    );
    if editor.page_count() == 1 {
        report.check(
            name,
            "last-page-delete-refused",
            editor.delete_page(0).is_err(),
            "last page deleted",
        );
    }
    editor.rotate_page(0, 1).map_err(describe)?;
    if let Some((_, page)) = expected.first_mut() {
        page.rotate = (page.rotate + 90) % 360;
    }
    editor
        .move_page(editor.page_count().saturating_sub(1), 0)
        .map_err(describe)?;
    expected.rotate_right(1);
    if editor.page_count() >= 3 {
        editor
            .delete_page(editor.page_count().saturating_sub(1))
            .map_err(describe)?;
        expected.pop();
    }
    let source_path = if path.file_name() == config.classic.file_name() {
        &config.stream
    } else {
        &config.classic
    };
    let source_bytes = read_bounded(source_path)?;
    let source = open(source_bytes.clone())?;
    let source_pages = pages(&source)?;
    let source_page = source_pages
        .first()
        .ok_or_else(|| "import fixture has no pages".to_owned())?;
    expected.push((content_hash(&source, source_page)?, source_page.clone()));
    editor
        .import_pages(source_bytes, &[0], editor.page_count())
        .map_err(describe)?;
    editor.set_info("Title", CONTENTS);
    report.equal(
        name,
        "pending-page-count",
        &editor.page_count(),
        &(expected.len() as u32),
    );
    let saved_bytes = editor.save().map_err(describe)?;
    report.check(
        name,
        "script.original-prefix",
        saved_bytes.starts_with(&original),
        "original prefix changed",
    );
    report.equal(
        name,
        "script.repeat-save",
        &editor.save().map_err(describe)?,
        &saved_bytes,
    );
    let saved = persist(&config.output, name, &saved_bytes, &[path, source_path])?;
    let after = open(saved)?;
    let after_pages = pages(&after)?;
    report.equal(
        name,
        "script.page-count",
        &after_pages.len(),
        &expected.len(),
    );
    for (index, (page, (hash, original_page))) in after_pages.iter().zip(&expected).enumerate() {
        let label = format!("page.{index}");
        report.equal(
            name,
            &format!("{label}.content-hash"),
            &content_hash(&after, page)?,
            hash,
        );
        report.equal(
            name,
            &format!("{label}.rotate"),
            &page.rotate,
            &original_page.rotate,
        );
        report.equal(
            name,
            &format!("{label}.media-box"),
            &page.media_box,
            &original_page.media_box,
        );
        report.equal(
            name,
            &format!("{label}.crop-box"),
            &page.crop_box,
            &original_page.crop_box,
        );
        report.equal(
            name,
            &format!("{label}.user-unit"),
            &page.user_unit,
            &original_page.user_unit,
        );
        let mut resolver = after.resolver();
        let object = page_object(&mut resolver, page)?;
        let parent = object
            .dict()
            .and_then(|dict| dict.get(b"Parent"))
            .and_then(|value| value.as_ref());
        let catalog = resolver
            .get(
                after
                    .root_ref()
                    .ok_or_else(|| "missing catalog".to_owned())?,
            )
            .map_err(describe)?;
        let root = catalog
            .dict()
            .and_then(|dict| dict.get(b"Pages"))
            .and_then(|value| value.as_ref());
        report.equal(name, &format!("{label}.flat-parent"), &parent, &root);
        report.check(
            name,
            &format!("{label}.explicit-attributes"),
            object.dict().is_some_and(|dict| {
                [b"Resources".as_slice(), b"MediaBox", b"CropBox", b"Rotate"]
                    .iter()
                    .all(|key| dict.get(key).is_some())
            }),
            "inherited page attribute missing",
        );
    }
    let annotation_page = after_pages
        .iter()
        .find(|page| page.id == first_page.id)
        .ok_or_else(|| "script deleted the annotated page".to_owned())?;
    verify_annotations(report, name, &after, annotation_page, first_index, rect)?;
    let mut resolver = after.resolver();
    let info = resolver
        .get(after.info_ref().ok_or_else(|| "missing Info".to_owned())?)
        .map_err(describe)?;
    let title = info
        .dict()
        .and_then(|dict| dict.get(b"Title"))
        .ok_or_else(|| "missing Title".to_owned())?;
    let title = resolver.resolve(title).map_err(describe)?;
    let raw = title
        .get()
        .as_str()
        .ok_or_else(|| "Title is not a string".to_owned())?
        .decode();
    let expected_title: Vec<_> = [0xfe, 0xff]
        .into_iter()
        .chain(CONTENTS.encode_utf16().flat_map(u16::to_be_bytes))
        .collect();
    report.equal(name, "script.title", &raw, &expected_title);
    Ok(())
}

fn expected_refusal(error: &DocError, manifest: &str) -> bool {
    match error {
        DocError::Encrypted => manifest.lines().any(|line| line == "encrypted=true"),
        DocError::Unsupported {
            feature: "edit:recovered_xref_offsets",
        } => manifest
            .lines()
            .any(|line| line == "warning=offsets_shifted" || line == "warning=startxref_adjusted"),
        DocError::Malformed {
            what: "xref Prev cycle",
            ..
        } => manifest.lines().any(|line| line == "warning=prev_cycle"),
        // Depth-probe fixtures sit exactly at the reader limit; the editor may refuse at it.
        DocError::Refused(Refusal {
            limit: LimitKind::Depth,
            ..
        }) => manifest
            .lines()
            .any(|line| line.starts_with("limit.max_depth=")),
        _ => manifest.lines().any(|line| {
            line.strip_prefix("refusal=")
                .is_some_and(|code| code != "none" && code == error.code())
        }),
    }
}

pub(super) fn run(report: &mut Report, config: &Config, font: Arc<[u8]>) {
    let Some(directory) = config.classic.parent() else {
        report.check(
            "wave11",
            "house-directory",
            false,
            "fixture directory missing",
        );
        return;
    };
    let result = (|| -> CheckResult<Vec<PathBuf>> {
        let mut paths = Vec::new();
        for entry in fs::read_dir(directory).map_err(describe)? {
            let path = entry.map_err(describe)?.path();
            if path.extension().is_some_and(|extension| extension == "pdf") {
                bounded("house-fixtures", paths.len().saturating_add(1) as u64, 4096)?;
                paths.push(path);
            }
        }
        paths.sort();
        Ok(paths)
    })();
    let Some(paths) = report.obtain("wave11", "house-fixtures", result) else {
        return;
    };
    for path in paths {
        let stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("fixture");
        let name = format!("wave11-{stem}");
        let Some(original) = report.obtain(&name, "read-original", read_bounded(&path)) else {
            continue;
        };
        let editor = match Editor::open(original.clone(), edit_limits()) {
            Ok(editor) => editor,
            Err(error) => {
                let manifest = read_bounded(&path.with_extension("expect"))
                    .ok()
                    .and_then(|bytes| String::from_utf8(bytes.to_vec()).ok())
                    .unwrap_or_default();
                report.check(
                    &name,
                    "script.expected-refusal",
                    expected_refusal(&error, &manifest),
                    &describe(error),
                );
                continue;
            }
        };
        let result = scripted(report, config, &path, &name, original, editor, font.clone());
        report.obtain(&name, "script.completed", result);
    }
}
