use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use pdf_core::pages::{Page, PageWalk};
use pdf_core::{DocError, Document, Limits, ObjectRef, PdfSource, Resolver};

use crate::font::FontCache;
use crate::interp::{page_text_cached, ImageInfo};
use crate::layout::{layout, PageLayout};

pub struct TextExtraction {
    pub text: String,
    pub pages: u32,
    pub empty_pages: Vec<u32>,
    pub refused_pages: Vec<(u32, &'static str)>,
    pub unmapped_glyphs: u64,
    pub note: String,
}

/// An owned page result shared by extraction and receipt consumers.
/// Interpreter failures retain page identity and display dimensions.
#[derive(Clone, Debug)]
pub struct PageReceipt {
    pub index: u32,
    pub id: ObjectRef,
    pub width: f32,
    pub height: f32,
    pub rotate: u16,
    pub layout: PageLayout,
    pub images: Vec<ImageInfo>,
    pub glyphs: u64,
    pub unmapped: u32,
    pub ops: u64,
    pub refusal: Option<DocError>,
}

fn display_dimensions(page: &Page) -> (f32, f32) {
    let width = ((page.crop_box[2] - page.crop_box[0]).max(0.0) * page.user_unit) as f32;
    let height = ((page.crop_box[3] - page.crop_box[1]).max(0.0) * page.user_unit) as f32;
    let width = if width.is_finite() { width } else { 0.0 };
    let height = if height.is_finite() { height } else { 0.0 };
    if matches!(page.rotate, 90 | 270) {
        (height, width)
    } else {
        (width, height)
    }
}

pub fn page_receipt(resolver: &mut Resolver<'_>, page: &Page) -> PageReceipt {
    page_receipt_cached(resolver, page, &mut FontCache::default())
}

pub fn page_receipt_cached(
    resolver: &mut Resolver<'_>,
    page: &Page,
    cache: &mut FontCache,
) -> PageReceipt {
    match page_text_cached(resolver, page, cache) {
        Ok(text) => {
            let page_layout = layout(&text);
            PageReceipt {
                index: page.index,
                id: page.id,
                width: text.width,
                height: text.height,
                rotate: page.rotate,
                layout: page_layout,
                glyphs: text.glyphs.len() as u64,
                unmapped: text.unmapped,
                ops: text.ops,
                images: text.images,
                refusal: None,
            }
        }
        Err(error) => {
            let (width, height) = display_dimensions(page);
            PageReceipt {
                index: page.index,
                id: page.id,
                width,
                height,
                rotate: page.rotate,
                layout: PageLayout {
                    lines: Vec::new(),
                    blocks: Vec::new(),
                    order: Vec::new(),
                    columns: 0,
                    body_size: 0.0,
                },
                images: Vec::new(),
                glyphs: 0,
                unmapped: 0,
                ops: 0,
                refusal: Some(error),
            }
        }
    }
}

pub fn extract_text(source: PdfSource, limits: Limits) -> Result<TextExtraction, DocError> {
    let document = Document::open(source, limits)?;
    let mut resolver = document.resolver();
    let pages = {
        let walk = PageWalk::new(&mut resolver)?;
        let mut pages = Vec::new();
        for page in walk {
            pages.push(page?);
        }
        pages
    };

    let mut text = String::new();
    let mut empty_pages = Vec::new();
    let mut refused_pages = Vec::new();
    let mut unmapped_glyphs = 0u64;
    let mut font_cache = FontCache::default();
    for (position, page) in pages.iter().enumerate() {
        if position != 0 {
            text.push('\u{0c}');
        }
        let receipt = page_receipt_cached(&mut resolver, page, &mut font_cache);
        if let Some(error) = receipt.refusal.as_ref() {
            refused_pages.push((page.index, error.code()));
            continue;
        }
        if receipt.glyphs == 0 && !receipt.images.is_empty() {
            empty_pages.push(page.index);
        }
        unmapped_glyphs = unmapped_glyphs.saturating_add(u64::from(receipt.unmapped));
        text.push_str(&receipt.layout.plain_text());
    }

    let pages = u32::try_from(pages.len()).unwrap_or(u32::MAX);
    let note = format!(
        "pdf-toolkit text layer: {} pages, {} refused, {} need OCR, {} unmapped glyphs; reading order is heuristic",
        pages,
        refused_pages.len(),
        empty_pages.len(),
        unmapped_glyphs,
    );
    Ok(TextExtraction {
        text,
        pages,
        empty_pages,
        refused_pages,
        unmapped_glyphs,
        note,
    })
}
