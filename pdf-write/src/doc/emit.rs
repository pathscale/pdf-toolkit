use std::io;

use super::bounds::*;
use super::image::Pixels;
use super::{DocumentBuilder, Font, Outline, Target, WriteOptions};
use crate::{encode, serialize_value, Dict, Value, Writer, XrefCompression};

pub(super) fn name(value: &str) -> Value {
    Value::Name(value.to_owned())
}

pub(super) fn reference(id: u32) -> Value {
    Value::Ref(id, 0)
}

pub(super) fn dict(entries: Vec<(&str, Value)>) -> Value {
    Value::Dict(entries.into_iter().map(|(key, value)| (key.to_owned(), value)).collect())
}

fn real(value: f32) -> Value {
    Value::Real(f64::from(value))
}

fn page_ref(ids: &[u32], index: usize) -> io::Result<Value> {
    ids.get(index).copied().map(reference).ok_or_else(|| invalid("destination page is missing"))
}

fn destination(ids: &[u32], page: usize, top: f32) -> io::Result<Value> {
    Ok(Value::Array(vec![
        page_ref(ids, page)?, name("XYZ"), Value::Null, real(top), Value::Null,
    ]))
}

pub(super) struct Output {
    writer: Writer,
    pending: Vec<(u32, Value)>,
    pending_bytes: usize,
    allocated: usize,
    object_streams: bool,
    pub compress: bool,
}

impl Output {
    fn new(options: &WriteOptions) -> io::Result<Self> {
        Ok(Self {
            writer: Writer::new(&format!("{}.{}", options.version.0, options.version.1))?,
            pending: Vec::new(), pending_bytes: 0, allocated: 0,
            object_streams: options.object_streams, compress: options.compress,
        })
    }

    pub fn alloc(&mut self) -> io::Result<u32> {
        let count = add(self.allocated, 1)?;
        check("objects", count, MAX_OBJECTS - 1)?;
        let id = self.writer.alloc()?;
        self.allocated = count;
        Ok(id)
    }

    fn space(&self, extra: usize) -> io::Result<()> {
        let requested = add(add(self.writer.bytes().len(), self.pending_bytes)?, extra)?;
        check("output_bytes", requested, MAX_OUTPUT)
    }

    pub fn object(&mut self, id: u32, value: Value) -> io::Result<()> {
        let length = serialize_value(&value)?.len();
        check("object_value_bytes", length, MAX_VALUE_BYTES)?;
        let estimate = add(length, 64)?;
        if self.object_streams {
            if self.pending.len() >= 128 || add(self.pending_bytes, estimate)? > MAX_VALUE_BYTES {
                self.flush()?;
            }
            self.space(estimate)?;
            self.pending_bytes = add(self.pending_bytes, estimate)?;
            push(&mut self.pending, (id, value), "object_stream_members", 128)?;
        } else {
            self.space(estimate)?;
            self.writer.object(id, &value)?;
        }
        Ok(())
    }

    pub fn stream(&mut self, id: u32, mut dict: Dict, data: &[u8], compress: bool) -> io::Result<()> {
        check("stream_bytes", data.len(), MAX_BYTES)?;
        let encoded = if compress { Some(encode::flate(data)) } else { None };
        let data = encoded.as_deref().unwrap_or(data);
        if compress {
            dict.push(("Filter".to_owned(), name("FlateDecode")));
        }
        let dictionary_bytes = serialize_value(&Value::Dict(dict.clone()))?.len();
        check("stream_dictionary_bytes", dictionary_bytes, MAX_VALUE_BYTES)?;
        self.space(add(add(data.len(), dictionary_bytes)?, 128)?)?;
        self.writer.stream(id, dict, data)?;
        check("output_bytes", self.writer.bytes().len(), MAX_OUTPUT)?;
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let members = std::mem::take(&mut self.pending);
        let estimated = self.pending_bytes;
        self.pending_bytes = 0;
        self.space(add(estimated, add(estimated / 10, 1024)?)?)?;
        let id = self.alloc()?;
        self.writer.object_stream(id, &members, self.compress)?;
        check("output_bytes", self.writer.bytes().len(), MAX_OUTPUT)?;
        Ok(())
    }

    fn finish(mut self, root: u32, extra: Dict) -> io::Result<Vec<u8>> {
        self.flush()?;
        self.space(add(mul(add(self.allocated, 2)?, 40)?, 4096)?)?;
        if self.object_streams {
            let compression = if self.compress { XrefCompression::Flate } else { XrefCompression::None };
            self.writer.finish_stream(root, compression, extra)?;
        } else {
            self.writer.finish_table(root, extra)?;
        }
        check("output_bytes", self.writer.bytes().len(), MAX_OUTPUT)?;
        Ok(self.writer.into_bytes())
    }
}

fn validate(doc: &DocumentBuilder, options: &WriteOptions) -> io::Result<()> {
    let mut minimum = (1, 0);
    if options.compress {
        minimum = minimum.max((1, 2));
    }
    if options.object_streams {
        minimum = minimum.max((1, 5));
    }
    if doc.fonts.iter().any(|font| matches!(font, Font::TrueType(_))) {
        minimum = minimum.max((1, 4));
    }
    for image in &doc.images {
        minimum = minimum.max(match &image.pixels {
            Pixels::Jpeg { .. } => (1, 2),
            Pixels::Rgb { alpha: Some(_), .. } => (1, 4),
            Pixels::Rgb { alpha: None, .. } => (1, 2),
        });
    }
    if doc.links != 0 {
        minimum = minimum.max((1, 1));
    }
    if doc.annotations != 0 {
        minimum = minimum.max((1, 4));
    }
    if options.version < minimum {
        return Err(invalid("PDF version is too old for selected document features"));
    }
    for page in &doc.pages {
        for link in &page.links {
            if let Target::Page { page, .. } = &link.target {
                if doc.pages.get(*page).is_none() {
                    return Err(invalid("link destination page is missing"));
                }
            }
        }
    }
    for item in &doc.outlines {
        if doc.pages.get(item.page).is_none() {
            return Err(invalid("outline destination page is missing"));
        }
    }
    Ok(())
}

pub(super) fn finish(mut doc: DocumentBuilder, options: &WriteOptions) -> io::Result<Vec<u8>> {
    if let Some(error) = doc.error.take() {
        return Err(error);
    }
    validate(&doc, options)?;
    let mut out = Output::new(options)?;
    let root = out.alloc()?;
    let pages_root = out.alloc()?;
    let mut page_ids = Vec::new();
    for _ in &doc.pages {
        push(&mut page_ids, out.alloc()?, "pages", MAX_PAGES)?;
    }
    let mut font_ids = Vec::new();
    for font in &doc.fonts {
        let id = match font {
            Font::Standard(font) => {
                let id = out.alloc()?;
                out.object(id, dict(vec![
                    ("Type", name("Font")), ("Subtype", name("Type1")),
                    ("BaseFont", name(font.name())), ("Encoding", name("WinAnsiEncoding")),
                ]))?;
                id
            }
            Font::TrueType(font) => font.write(&mut out)?,
        };
        push(&mut font_ids, id, "fonts", MAX_FONTS)?;
    }
    let mut image_ids = Vec::new();
    for image in &doc.images {
        let id = out.alloc()?;
        let mut entries = vec![
            ("Type".to_owned(), name("XObject")),
            ("Subtype".to_owned(), name("Image")),
            ("Width".to_owned(), Value::Int(i64::from(image.width))),
            ("Height".to_owned(), Value::Int(i64::from(image.height))),
            ("BitsPerComponent".to_owned(), Value::Int(8)),
        ];
        match &image.pixels {
            Pixels::Jpeg { data, components, invert } => {
                let space = match components {
                    1 => "DeviceGray",
                    3 => "DeviceRGB",
                    4 => "DeviceCMYK",
                    _ => return Err(invalid("image component count is invalid")),
                };
                entries.push(("ColorSpace".to_owned(), name(space)));
                entries.push(("Filter".to_owned(), name("DCTDecode")));
                if *invert {
                    entries.push(("Decode".to_owned(), Value::Array(vec![
                        Value::Int(1), Value::Int(0), Value::Int(1), Value::Int(0),
                        Value::Int(1), Value::Int(0), Value::Int(1), Value::Int(0),
                    ])));
                }
                out.stream(id, entries, data, false)?;
            }
            Pixels::Rgb { data, alpha } => {
                entries.push(("ColorSpace".to_owned(), name("DeviceRGB")));
                if let Some(alpha) = alpha {
                    let mask = out.alloc()?;
                    out.stream(mask, vec![
                        ("Type".to_owned(), name("XObject")),
                        ("Subtype".to_owned(), name("Image")),
                        ("Width".to_owned(), Value::Int(i64::from(image.width))),
                        ("Height".to_owned(), Value::Int(i64::from(image.height))),
                        ("BitsPerComponent".to_owned(), Value::Int(8)),
                        ("ColorSpace".to_owned(), name("DeviceGray")),
                    ], alpha, true)?;
                    entries.push(("SMask".to_owned(), reference(mask)));
                }
                out.stream(id, entries, data, true)?;
            }
        }
        push(&mut image_ids, id, "images", MAX_IMAGES)?;
    }
    for (index, page) in doc.pages.iter().enumerate() {
        let content = out.alloc()?;
        out.stream(content, Vec::new(), &page.content, options.compress)?;
        let mut fonts = Vec::new();
        for id in &page.fonts {
            let index = usize::try_from(*id).map_err(|_| invalid("font index overflow"))?;
            let object = *font_ids.get(index).ok_or_else(|| invalid("page font is missing"))?;
            fonts.push((format!("F{id}"), reference(object)));
        }
        let mut images = Vec::new();
        for id in &page.images {
            let index = usize::try_from(*id).map_err(|_| invalid("image index overflow"))?;
            let object = *image_ids.get(index).ok_or_else(|| invalid("page image is missing"))?;
            images.push((format!("I{id}"), reference(object)));
        }
        let resources = dict(vec![
            ("Font", Value::Dict(fonts)), ("XObject", Value::Dict(images)),
        ]);
        let mut annotations = Vec::new();
        for link in &page.links {
            let id = out.alloc()?;
            let mut entries = vec![
                ("Type", name("Annot")), ("Subtype", name("Link")),
                ("Rect", Value::Array(link.rect.iter().copied().map(real).collect())),
                ("Border", Value::Array(vec![Value::Int(0), Value::Int(0), Value::Int(0)])),
                ("P", page_ref(&page_ids, index)?),
            ];
            match &link.target {
                Target::Uri(uri) => entries.push(("A", dict(vec![
                    ("S", name("URI")), ("URI", Value::Str(uri.clone())),
                ]))),
                Target::Page { page, top } => {
                    entries.push(("Dest", destination(&page_ids, *page, *top)?));
                }
            }
            out.object(id, dict(entries))?;
            push(&mut annotations, reference(id), "links", MAX_LINKS)?;
        }
        for parts in &page.annotations {
            let appearance = out.alloc()?;
            out.stream(
                appearance, parts.ap_dict.clone(), &parts.ap_content, options.compress,
            )?;
            let id = out.alloc()?;
            let mut entries = parts.dict.clone();
            entries.push(("P".to_owned(), page_ref(&page_ids, index)?));
            entries.push(("AP".to_owned(), dict(vec![("N", reference(appearance))])));
            out.object(id, Value::Dict(entries))?;
            push(&mut annotations, reference(id), "annotations", MAX_LINKS)?;
        }
        let mut entries = vec![
            ("Type", name("Page")), ("Parent", reference(pages_root)),
            ("MediaBox", Value::Array(vec![
                Value::Int(0), Value::Int(0), real(page.width), real(page.height),
            ])),
            ("Resources", resources), ("Contents", reference(content)),
        ];
        if !annotations.is_empty() {
            entries.push(("Annots", Value::Array(annotations)));
        }
        let id = *page_ids.get(index).ok_or_else(|| invalid("page object is missing"))?;
        out.object(id, dict(entries))?;
    }
    out.object(pages_root, dict(vec![
        ("Type", name("Pages")), ("Count", Value::Int(page_ids.len() as i64)),
        ("Kids", Value::Array(page_ids.iter().copied().map(reference).collect())),
    ]))?;
    let mut catalog = vec![("Type", name("Catalog")), ("Pages", reference(pages_root))];
    if !doc.outlines.is_empty() {
        let outline = write_outlines(&mut out, &doc.outlines, &page_ids)?;
        catalog.push(("Outlines", reference(outline)));
        catalog.push(("PageMode", name("UseOutlines")));
    }
    out.object(root, dict(catalog))?;
    let mut trailer = Vec::new();
    if !doc.info.is_empty() {
        let id = out.alloc()?;
        let entries = doc.info.into_iter().map(|(key, bytes)| {
            (key.to_owned(), Value::HexStr(bytes))
        }).collect();
        out.object(id, Value::Dict(entries))?;
        trailer.push(("Info".to_owned(), reference(id)));
    }
    out.finish(root, trailer)
}

#[derive(Default)]
struct Node {
    parent: Option<usize>,
    previous: Option<usize>,
    next: Option<usize>,
    first: Option<usize>,
    last: Option<usize>,
    descendants: usize,
}

fn write_outlines(out: &mut Output, items: &[Outline], pages: &[u32]) -> io::Result<u32> {
    let root = out.alloc()?;
    let mut ids = Vec::new();
    let mut nodes = Vec::new();
    for _ in items {
        push(&mut ids, out.alloc()?, "outline_items", MAX_OUTLINES)?;
        push(&mut nodes, Node::default(), "outline_items", MAX_OUTLINES)?;
    }
    let base = items.first().ok_or_else(|| invalid("outline is empty"))?.level;
    let mut stack = Vec::<usize>::new();
    let mut root_first = None;
    let mut root_last = None;
    for (index, item) in items.iter().enumerate() {
        let depth = usize::from(item.level.checked_sub(base).ok_or_else(|| invalid("outline level"))?);
        if depth > stack.len() {
            return Err(invalid("outline parent is missing"));
        }
        stack.truncate(depth);
        let parent = stack.last().copied();
        let previous = match parent {
            Some(parent) => nodes.get(parent).ok_or_else(|| invalid("outline parent"))?.last,
            None => root_last,
        };
        let node = nodes.get_mut(index).ok_or_else(|| invalid("outline node"))?;
        node.parent = parent;
        node.previous = previous;
        if let Some(previous) = previous {
            nodes.get_mut(previous).ok_or_else(|| invalid("outline sibling"))?.next = Some(index);
        }
        match parent {
            Some(parent) => {
                let node = nodes.get_mut(parent).ok_or_else(|| invalid("outline parent"))?;
                if node.first.is_none() {
                    node.first = Some(index);
                }
                node.last = Some(index);
            }
            None => {
                if root_first.is_none() {
                    root_first = Some(index);
                }
                root_last = Some(index);
            }
        }
        for ancestor in &stack {
            let node = nodes.get_mut(*ancestor).ok_or_else(|| invalid("outline ancestor"))?;
            node.descendants = add(node.descendants, 1)?;
        }
        push(&mut stack, index, "outline_depth", MAX_DEPTH)?;
    }
    let resolve = |index: usize| -> io::Result<Value> {
        ids.get(index).copied().map(reference).ok_or_else(|| invalid("outline object is missing"))
    };
    for (index, (item, node)) in items.iter().zip(&nodes).enumerate() {
        let mut entries = vec![
            ("Title", Value::HexStr(item.title.clone())),
            ("Parent", match node.parent { Some(parent) => resolve(parent)?, None => reference(root) }),
            ("Dest", destination(pages, item.page, item.y)?),
        ];
        for (key, target) in [
            ("Prev", node.previous), ("Next", node.next),
            ("First", node.first), ("Last", node.last),
        ] {
            if let Some(target) = target {
                entries.push((key, resolve(target)?));
            }
        }
        if node.first.is_some() {
            entries.push(("Count", Value::Int(node.descendants as i64)));
        }
        let id = *ids.get(index).ok_or_else(|| invalid("outline object is missing"))?;
        out.object(id, dict(entries))?;
    }
    let first = root_first.ok_or_else(|| invalid("outline first item is missing"))?;
    let last = root_last.ok_or_else(|| invalid("outline last item is missing"))?;
    out.object(root, dict(vec![
        ("Type", name("Outlines")), ("First", resolve(first)?),
        ("Last", resolve(last)?), ("Count", Value::Int(items.len() as i64)),
    ]))?;
    Ok(root)
}

