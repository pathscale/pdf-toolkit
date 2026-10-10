use alloc::string::String;
use alloc::vec::Vec;

use pdf_core::pages::{Page, PageWalk};
use pdf_core::{DocError, Document, LimitKind, Limits, ObjectRef, PdfSource, Resolver, Warning};

use crate::content::{check, refusal};
use crate::interp::{display_dimensions, display_transform, page_text, PageText, PositionedGlyph};
use crate::layout::{direction, layout, BlockKind, Line, PageLayout, SPACE_GAP};
use crate::links;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SemanticKind {
    Paragraph,
    Heading(u8),
    ListItem,
    Caption,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub monospace: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Span {
    pub text: String,
    pub style: Style,
    pub size: f32,
    pub link: Option<u32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SemanticBlock {
    pub kind: SemanticKind,
    pub bbox: [f32; 4],
    pub spans: Vec<Span>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum LinkTarget {
    Uri(String),
    Page { index: u32, y: Option<f32> },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Link {
    pub bbox: [f32; 4],
    pub target: LinkTarget,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ImageRef {
    pub bbox: [f32; 4],
    pub object: Option<ObjectRef>,
    pub width: u32,
    pub height: u32,
    pub inline: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SemanticPage {
    pub index: u32,
    pub width: f32,
    pub height: f32,
    pub blocks: Vec<SemanticBlock>,
    pub images: Vec<ImageRef>,
    pub links: Vec<Link>,
    pub refusal: Option<&'static str>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OutlineItem {
    pub title: String,
    pub level: u8,
    pub target: Option<LinkTarget>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DocInfo {
    pub title: Option<String>,
    pub author: Option<String>,
    pub subject: Option<String>,
    pub pages: u32,
    pub encrypted: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SemanticDoc {
    pub info: DocInfo,
    pub outline: Vec<OutlineItem>,
    pub pages: Vec<SemanticPage>,
}

/// Owns the document and walked page descriptors, never a resolver borrowing it.
pub struct SemanticReader {
    document: Document,
    pages: Vec<Page>,
    page_ids: Vec<ObjectRef>,
    page_lookup: Vec<(u32, u16, u32)>,
    info: DocInfo,
    next: usize,
    warnings: Vec<Warning>,
}

impl SemanticReader {
    pub fn open(source: PdfSource, limits: Limits) -> Result<Self, DocError> {
        let document = Document::open(source, limits)?;
        let mut budget = Budget::new(limits.max_total_decoded_bytes);
        let mut resolver = document.resolver();
        let mut pages = Vec::new();
        let mut page_ids = Vec::new();
        for page in PageWalk::new(&mut resolver)? {
            let page = page?;
            budget.charge((page.contents.len() as u64)
                .saturating_mul(core::mem::size_of::<ObjectRef>() as u64))?;
            budget.push(&mut page_ids, page.id)?;
            budget.push(&mut pages, page)?;
        }
        let count = u32::try_from(pages.len()).map_err(|_| {
            refusal(LimitKind::Pages, pages.len() as u64, u64::from(u32::MAX))
        })?;
        let mut page_lookup = Vec::new();
        for page in &pages {
            budget.push(&mut page_lookup, (page.id.number(), page.id.generation(), page.index))?;
        }
        page_lookup.sort_unstable();
        let mut info = DocInfo {
            title: None,
            author: None,
            subject: None,
            pages: count,
            encrypted: document.encrypted(),
        };
        // Encrypted metadata is ciphertext, even if the page spine is readable.
        if !document.encrypted() {
            if let Some(dict) = document.trailer().dict() {
                if let Some(value) = links::read_key(&mut resolver, dict, b"Info")? {
                    if let Some(dict) = value.get().as_dict() {
                        info.title = info_string(&mut resolver, dict, b"Title", &mut budget)?;
                        info.author = info_string(&mut resolver, dict, b"Author", &mut budget)?;
                        info.subject = info_string(&mut resolver, dict, b"Subject", &mut budget)?;
                    } else {
                        links::warn(&mut resolver, value.object().id(), "info_invalid");
                    }
                }
            }
        }
        let mut warnings = Vec::new();
        for warning in document.warnings().iter().cloned() {
            budget.push(&mut warnings, warning)?;
        }
        for warning in resolver.take_warnings() {
            budget.push(&mut warnings, warning)?;
        }
        drop(resolver);
        Ok(Self { document, pages, page_ids, page_lookup, info, next: 0, warnings })
    }

    pub fn info(&self) -> &DocInfo {
        &self.info
    }

    pub fn outline(&mut self) -> Vec<OutlineItem> {
        let mut resolver = self.document.resolver();
        let result = links::outline_with_pages(&mut resolver, &self.page_ids, &self.pages, &self.page_lookup);
        let items = match result {
            Ok(items) => items,
            Err(error) => {
                links::warn(&mut resolver, None, error.code());
                Vec::new()
            }
        };
        let warnings = resolver.take_warnings();
        drop(resolver);
        if let Err(error) = self.keep_warnings(warnings) {
            // The Vec-returning API reports refusals through warnings.
            self.warnings.clear();
            let mut budget = Budget::new(self.document.limits().max_total_decoded_bytes);
            let _ = budget.push(&mut self.warnings,
                Warning { offset: None, object: None, what: error.code() });
            return Vec::new();
        }
        items
    }

    pub fn next_page(&mut self) -> Option<SemanticPage> {
        let page = self.pages.get(self.next)?;
        self.next = self.next.saturating_add(1);
        let (width, height) = display_dimensions(page);
        let mut out = SemanticPage {
            index: page.index,
            width,
            height,
            blocks: Vec::new(),
            images: Vec::new(),
            links: Vec::new(),
            refusal: None,
        };
        let mut resolver = self.document.resolver();
        let result = build_page(&mut resolver, page, &self.page_ids, &self.pages, &self.page_lookup);
        let warnings = resolver.take_warnings();
        drop(resolver);
        match result {
            Ok(value) => out = value,
            Err(error) => out.refusal = Some(error.code()),
        }
        if let Err(error) = self.keep_warnings(warnings) {
            out.blocks.clear();
            out.images.clear();
            out.links.clear();
            out.refusal = Some(error.code());
        }
        Some(out)
    }

    /// Includes walk, metadata, action, destination and interpreter warnings.
    pub fn take_warnings(&mut self) -> Vec<Warning> {
        core::mem::take(&mut self.warnings)
    }

    fn keep_warnings(&mut self, warnings: Vec<Warning>) -> Result<(), DocError> {
        let mut budget = Budget::new(self.document.limits().max_total_decoded_bytes);
        budget.charge((self.warnings.len() as u64)
            .saturating_mul(core::mem::size_of::<Warning>() as u64))?;
        for warning in warnings {
            budget.push(&mut self.warnings, warning)?;
        }
        Ok(())
    }
}

/// Whole-document output has a cumulative byte cap; the reader caps each page.
pub fn semantic_document(source: PdfSource, limits: Limits) -> Result<SemanticDoc, DocError> {
    let mut reader = SemanticReader::open(source, limits)?;
    let info = reader.info().clone();
    let mut resolver = reader.document.resolver();
    let outline = links::outline_with_pages(&mut resolver, &reader.page_ids, &reader.pages, &reader.page_lookup)?;
    drop(resolver);
    let mut budget = Budget::new(limits.max_total_decoded_bytes);
    for text in [&info.title, &info.author, &info.subject].into_iter().flatten() {
        budget.charge(text.len() as u64)?;
    }
    for item in &outline {
        budget.charge(core::mem::size_of::<OutlineItem>() as u64)?;
        budget.charge(item.title.len() as u64)?;
        budget.charge(target_bytes(item.target.as_ref()))?;
    }
    let mut pages = Vec::new();
    while let Some(page) = reader.next_page() {
        budget.charge(page_bytes(&page))?;
        budget.push(&mut pages, page)?;
        // Whole-document output has no warnings field.
        reader.take_warnings();
    }
    Ok(SemanticDoc { info, outline, pages })
}

fn info_string(
    resolver: &mut Resolver<'_>,
    dict: pdf_core::DictRef<'_>,
    key: &[u8],
    budget: &mut Budget,
) -> Result<Option<String>, DocError> {
    let Some(value) = links::read_key(resolver, dict, key)? else { return Ok(None) };
    let result = links::text_string(&value, *resolver.document().limits());
    match links::recover(resolver, result, value.object().id(), "info_string_invalid")? {
        Some(Some(text)) => {
            budget.charge(text.len() as u64)?;
            Ok(Some(text))
        }
        _ => Ok(None),
    }
}

fn build_page(
    resolver: &mut Resolver<'_>,
    page: &Page,
    page_ids: &[ObjectRef],
    pages: &[Page],
    page_lookup: &[(u32, u16, u32)],
) -> Result<SemanticPage, DocError> {
    let text = page_text(resolver, page)?;
    let layout = layout(&text);
    let mut budget = Budget::new(resolver.document().limits().max_total_decoded_bytes);
    let links = links::page_links_with_pages(
        resolver, page, page_ids, pages, page_lookup, display_transform(page),
    )?;
    for link in &links {
        budget.charge(core::mem::size_of::<Link>() as u64)?;
        budget.charge(target_bytes(Some(&link.target)))?;
    }
    // Keep annotation order in the public Vec; sort only lookup indices once.
    let mut lookup = Vec::new();
    for (index, link) in links.iter().enumerate() {
        budget.push(&mut lookup, (link.bbox[1], index))?;
    }
    lookup.sort_unstable_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let mut styles = Vec::new();
    for font in &text.fonts {
        let name = font.name.as_bytes();
        budget.push(&mut styles, Style {
            bold: font.flags.bold || hints(name, &[b"bold", b"black", b"heavy", b"semibold"]),
            italic: font.flags.italic || hints(name, &[b"italic", b"oblique"]),
            monospace: font.flags.fixed_pitch || hints(name, &[b"courier", b"mono", b"code"]),
        })?;
    }
    let mut blocks = Vec::new();
    for block in &layout.blocks {
        let kind = match block.kind {
            BlockKind::Paragraph => SemanticKind::Paragraph,
            BlockKind::Heading(level) => SemanticKind::Heading(level),
            BlockKind::ListItem => SemanticKind::ListItem,
            BlockKind::Caption => SemanticKind::Caption,
        };
        let mut spans: Vec<Span> = Vec::new();
        let lines = layout.lines.get(block.lines.start as usize..block.lines.end as usize)
            .unwrap_or(&[]);
        for line in lines {
            let line_spans = line_spans(line, &layout, &text, &styles, &links, &lookup, &mut budget)?;
            if line_spans.is_empty() {
                continue;
            }
            if !spans.is_empty() {
                let lowercase = line_spans.first().and_then(|span| span.text.chars().next())
                    .is_some_and(char::is_lowercase);
                let hyphen = spans.last().is_some_and(|span| span.text.ends_with('-'));
                if kind == SemanticKind::Paragraph && hyphen && lowercase {
                    if let Some(last) = spans.last_mut() { last.text.pop(); }
                    if spans.last().is_some_and(|span| span.text.is_empty()) { spans.pop(); }
                } else {
                    separator(&mut spans, if kind == SemanticKind::Paragraph { ' ' } else { '\n' },
                        &mut budget)?;
                }
            }
            for span in line_spans {
                if let Some(last) = spans.last_mut().filter(|last| compatible(last, &span)) {
                    budget.append(&mut last.text, &span.text)?;
                } else {
                    budget.push(&mut spans, span)?;
                }
            }
        }
        budget.push(&mut blocks, SemanticBlock { kind, bbox: block.bbox, spans })?;
    }
    let mut images = Vec::new();
    for image in text.images {
        budget.push(&mut images, ImageRef {
            bbox: image.bbox,
            object: image.object,
            width: image.width,
            height: image.height,
            inline: image.inline,
        })?;
    }
    Ok(SemanticPage {
        index: page.index, width: text.width, height: text.height,
        blocks, images, links, refusal: None,
    })
}

fn hints(name: &[u8], hints: &[&[u8]]) -> bool {
    hints.iter().any(|hint| name.windows(hint.len()).any(|part| part.eq_ignore_ascii_case(hint)))
}

fn compatible(left: &Span, right: &Span) -> bool {
    left.style == right.style && left.link == right.link
        && (left.size - right.size).abs() <= 0.05 * left.size.min(right.size)
}

fn glyph_direction(glyph: &PositionedGlyph) -> (f32, f32) {
    direction(i32::from(glyph.angle).rem_euclid(360) as i16)
}

fn glyph_link(glyph: &PositionedGlyph, links: &[Link], lookup: &[(f32, usize)]) -> Option<u32> {
    let (dx, dy) = glyph_direction(glyph);
    let size = glyph.size.abs().max(0.01);
    let x = glyph.x + dx * (glyph.width * 0.5) + dy * (size * 0.5);
    let y = glyph.y + dy * (glyph.width * 0.5) - dx * (size * 0.5);
    if !x.is_finite() || !y.is_finite() { return None; }
    let mut found: Option<usize> = None;
    for (top, index) in lookup {
        if *top > y { break; }
        let Some(link) = links.get(*index) else { continue };
        let [x0, y0, x1, y1] = link.bbox;
        if x >= x0 && x <= x1 && y >= y0 && y <= y1 {
            found = Some(found.map(|last| last.min(*index)).unwrap_or(*index));
        }
    }
    found.and_then(|index| u32::try_from(index).ok())
}

fn line_spans(
    line: &Line,
    layout: &PageLayout,
    text: &PageText,
    styles: &[Style],
    links: &[Link],
    lookup: &[(f32, usize)],
    budget: &mut Budget,
) -> Result<Vec<Span>, DocError> {
    let ids = layout.order.get(line.glyphs.start as usize..line.glyphs.end as usize)
        .unwrap_or(&[]);
    let mut spans = Vec::new();
    let mut previous: Option<(f32, f32, f32)> = None;
    for glyph in ids.iter().filter_map(|id| text.glyphs.get(*id as usize)) {
        let (dx, dy) = glyph_direction(glyph);
        let along = dx * glyph.x + dy * glyph.y;
        let size = glyph.size.abs().max(0.01);
        if let Some((last_along, width, last_size)) = previous {
            if along - last_along - width > SPACE_GAP * size.max(last_size) {
                if spans.last().is_some_and(|span: &Span| !span.text.ends_with(' ')) {
                    separator(&mut spans, ' ', budget)?;
                }
            }
        }
        let style = styles.get(usize::from(glyph.font)).copied().unwrap_or_default();
        let link = glyph_link(glyph, links, lookup);
        let chars = glyph.text.as_slice();
        if chars.is_empty() && !glyph.mapped {
            emit(&mut spans, '\u{fffd}', style, glyph.size, link, budget)?;
        } else {
            for character in chars {
                if character.is_whitespace() {
                    if spans.last().is_some_and(|span| !span.text.ends_with(' ')) {
                        emit(&mut spans, ' ', style, glyph.size, link, budget)?;
                    }
                } else {
                    emit(&mut spans, *character, style, glyph.size, link, budget)?;
                }
            }
        }
        previous = Some((along, glyph.width, size));
    }
    loop {
        let Some(last) = spans.last_mut() else { break };
        while last.text.ends_with(' ') { last.text.pop(); }
        if last.text.is_empty() { spans.pop(); } else { break; }
    }
    Ok(spans)
}

fn emit(
    spans: &mut Vec<Span>, character: char, style: Style, size: f32,
    link: Option<u32>, budget: &mut Budget,
) -> Result<(), DocError> {
    let candidate = Span { text: String::new(), style, size, link };
    if !spans.last().is_some_and(|last| compatible(last, &candidate)) {
        budget.push(spans, candidate)?;
    }
    if let Some(last) = spans.last_mut() {
        let mut bytes = [0u8; 4];
        budget.append(&mut last.text, character.encode_utf8(&mut bytes))?;
    }
    Ok(())
}

fn separator(spans: &mut [Span], character: char, budget: &mut Budget) -> Result<(), DocError> {
    if let Some(last) = spans.last_mut() {
        let mut bytes = [0u8; 4];
        budget.append(&mut last.text, character.encode_utf8(&mut bytes))?;
    }
    Ok(())
}

fn target_bytes(target: Option<&LinkTarget>) -> u64 {
    match target { Some(LinkTarget::Uri(uri)) => uri.len() as u64, _ => 0 }
}

fn page_bytes(page: &SemanticPage) -> u64 {
    let mut bytes = (page.images.len() as u64)
        .saturating_mul(core::mem::size_of::<ImageRef>() as u64);
    for link in &page.links {
        bytes = bytes.saturating_add(core::mem::size_of::<Link>() as u64)
            .saturating_add(target_bytes(Some(&link.target)));
    }
    for block in &page.blocks {
        bytes = bytes.saturating_add(core::mem::size_of::<SemanticBlock>() as u64);
        for span in &block.spans {
            bytes = bytes.saturating_add(core::mem::size_of::<Span>() as u64)
                .saturating_add(span.text.len() as u64);
        }
    }
    bytes
}

/// Conservative allocation accounting shared by the semantic and link readers.
pub(crate) struct Budget {
    used: u64,
    maximum: u64,
}

impl Budget {
    pub(crate) fn new(maximum: u64) -> Self { Self { used: 0, maximum } }

    pub(crate) fn charge(&mut self, bytes: u64) -> Result<(), DocError> {
        let requested = self.used.saturating_add(bytes);
        check(LimitKind::TotalDecodedBytes, requested, self.maximum)?;
        self.used = requested;
        Ok(())
    }

    pub(crate) fn push<T>(&mut self, out: &mut Vec<T>, value: T) -> Result<(), DocError> {
        self.charge(core::mem::size_of::<T>() as u64)?;
        out.try_reserve(1).map_err(|_| {
            refusal(LimitKind::TotalDecodedBytes, self.maximum.saturating_add(1), self.maximum)
        })?;
        out.push(value);
        Ok(())
    }

    pub(crate) fn append(&mut self, out: &mut String, text: &str) -> Result<(), DocError> {
        self.charge(text.len() as u64)?;
        out.try_reserve(text.len()).map_err(|_| {
            refusal(LimitKind::TotalDecodedBytes, self.maximum.saturating_add(1), self.maximum)
        })?;
        out.push_str(text);
        Ok(())
    }
}
