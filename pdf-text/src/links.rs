use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;

use pdf_core::pages::{Page, PageWalk};
use pdf_core::{
    ArrayRef, DictRef, DocError, LimitKind, Limits, ObjectRef, Resolver, Value, Warning,
};

use crate::content::{check, decode_object_string, refusal};
use crate::data::encodings::PDF_DOC;
use crate::interp::display_transform;
use crate::semantic::{Budget, Link, LinkTarget, OutlineItem};

const MAX_ITEMS: u64 = 10_000;
const MAX_DEPTH: u64 = 16;
const MAX_STEPS: u64 = 100_000;
const MAX_TEXT_BYTES: u64 = 1 << 20;
const MAX_NAME_BYTES: u64 = 16 << 10;
const MAX_URI_BYTES: u64 = 16 << 10;

pub fn page_links(
    resolver: &mut Resolver<'_>,
    page: &Page,
    page_ids: &[ObjectRef],
    display: impl Fn(f32, f32) -> (f32, f32),
) -> Vec<Link> {
    let result = Context::new(resolver, page_ids, None, None).page_links(page, display);
    match result {
        Ok(links) => links,
        Err(error) => {
            warn(resolver, Some(page.id), error.code());
            Vec::new()
        }
    }
}

pub fn outline(resolver: &mut Resolver<'_>, page_ids: &[ObjectRef]) -> Vec<OutlineItem> {
    let result = Context::new(resolver, page_ids, None, None).outline();
    match result {
        Ok(items) => items,
        Err(error) => {
            warn(resolver, None, error.code());
            Vec::new()
        }
    }
}

pub(crate) fn page_links_with_pages(
    resolver: &mut Resolver<'_>,
    page: &Page,
    page_ids: &[ObjectRef],
    pages: &[Page],
    page_lookup: &[(u32, u16, u32)],
    display: impl Fn(f32, f32) -> (f32, f32),
) -> Result<Vec<Link>, DocError> {
    Context::new(resolver, page_ids, Some(pages), Some(page_lookup)).page_links(page, display)
}

pub(crate) fn outline_with_pages(
    resolver: &mut Resolver<'_>,
    page_ids: &[ObjectRef],
    pages: &[Page],
    page_lookup: &[(u32, u16, u32)],
) -> Result<Vec<OutlineItem>, DocError> {
    Context::new(resolver, page_ids, Some(pages), Some(page_lookup)).outline()
}

type Identity = (u32, u16, usize, usize);

fn identity(value: &Value) -> Identity {
    let id = value.object().id();
    let span = value.get().span();
    (
        id.map(|id| id.number()).unwrap_or(0),
        id.map(|id| id.generation()).unwrap_or(0),
        span.start,
        span.end,
    )
}

struct Context<'r, 'd, 'p> {
    resolver: &'r mut Resolver<'d>,
    page_ids: &'p [ObjectRef],
    pages: Option<&'p [Page]>,
    page_lookup: Option<&'p [(u32, u16, u32)]>,
    walked: Vec<Page>,
    did_walk: bool,
    budget: Budget,
    steps: u64,
}

impl<'r, 'd, 'p> Context<'r, 'd, 'p> {
    fn new(
        resolver: &'r mut Resolver<'d>,
        page_ids: &'p [ObjectRef],
        pages: Option<&'p [Page]>,
        page_lookup: Option<&'p [(u32, u16, u32)]>,
    ) -> Self {
        let maximum = resolver.document().limits().max_total_decoded_bytes;
        Self {
            resolver,
            page_ids,
            pages,
            page_lookup,
            walked: Vec::new(),
            did_walk: false,
            budget: Budget::new(maximum),
            steps: 0,
        }
    }

    fn step(&mut self) -> Result<(), DocError> {
        self.steps = self.steps.saturating_add(1);
        let maximum = self
            .resolver
            .document()
            .limits()
            .max_content_ops
            .min(MAX_STEPS);
        check(LimitKind::ContentOps, self.steps, maximum)
    }

    fn depth(&self, depth: u64) -> Result<(), DocError> {
        check(
            LimitKind::Depth,
            depth,
            u64::from(self.resolver.document().limits().max_depth).min(MAX_DEPTH),
        )
    }

    fn items(&self, count: u64) -> Result<(), DocError> {
        check(
            LimitKind::Objects,
            count,
            u64::from(self.resolver.document().limits().max_objects).min(MAX_ITEMS),
        )
    }

    fn key(&mut self, dict: DictRef<'_>, key: &[u8]) -> Result<Option<Value>, DocError> {
        self.step()?;
        read_key(self.resolver, dict, key)
    }

    fn resolve(&mut self, value: pdf_core::ValueRef<'_>) -> Result<Option<Value>, DocError> {
        self.step()?;
        let result = self.resolver.resolve(value);
        recover(self.resolver, result, value.as_ref(), "link_object_failed")
    }

    fn catalog(&mut self) -> Result<Option<Value>, DocError> {
        self.step()?;
        let Some(id) = self.resolver.document().root_ref() else {
            return Ok(None);
        };
        let result = self.resolver.get(id);
        Ok(
            recover(self.resolver, result, Some(id), "link_catalog_failed")?
                .map(|object| object.root().to_owned()),
        )
    }

    fn page_links(
        &mut self,
        page: &Page,
        display: impl Fn(f32, f32) -> (f32, f32),
    ) -> Result<Vec<Link>, DocError> {
        if !self.resolver.document().decryptable() {
            return Err(DocError::Encrypted);
        }
        let result = self.resolver.get(page.id);
        let Some(object) = recover(self.resolver, result, Some(page.id), "link_page_failed")?
        else {
            return Ok(Vec::new());
        };
        let Some(dict) = object.dict() else {
            return Ok(Vec::new());
        };
        let Some(annots) = self.key(dict, b"Annots")? else {
            return Ok(Vec::new());
        };
        let Some(array) = annots.get().as_array() else {
            warn(self.resolver, Some(page.id), "link_annots_invalid");
            return Ok(Vec::new());
        };
        self.items(array.len() as u64)?;
        let mut links = Vec::new();
        for annotation in array.iter() {
            let Some(value) = self.resolve(annotation)? else {
                continue;
            };
            let Some(dict) = value.get().as_dict() else {
                continue;
            };
            let subtype = self.key(dict, b"Subtype")?;
            if !subtype
                .as_ref()
                .and_then(|value| value.get().as_name())
                .is_some_and(|name| name.eq_bytes(b"Link"))
            {
                continue;
            }
            let Some(target) = self.target(dict)? else {
                continue;
            };
            let Some(rect) = self.key(dict, b"Rect")? else {
                warn(self.resolver, value.object().id(), "link_rect_invalid");
                continue;
            };
            let Some(rect) = self.rect(&rect)? else {
                continue;
            };
            let Some(bbox) = transformed_rect(rect, &display) else {
                warn(self.resolver, value.object().id(), "link_geometry_failed");
                continue;
            };
            self.budget.push(&mut links, Link { bbox, target })?;
        }
        Ok(links)
    }

    fn target(&mut self, dict: DictRef<'_>) -> Result<Option<LinkTarget>, DocError> {
        if dict.get(b"AA").is_some() {
            warn(self.resolver, None, "action_ignored");
        }
        if dict.get(b"A").is_some() {
            let Some(action) = self.key(dict, b"A")? else {
                return Ok(None);
            };
            let Some(dict) = action.get().as_dict() else {
                warn(self.resolver, action.object().id(), "action_ignored");
                return Ok(None);
            };
            if dict.get(b"Next").is_some() {
                warn(self.resolver, action.object().id(), "action_ignored");
            }
            let kind = self.key(dict, b"S")?;
            let name = kind.as_ref().and_then(|value| value.get().as_name());
            if name.is_some_and(|name| name.eq_bytes(b"URI")) {
                let Some(value) = self.key(dict, b"URI")? else {
                    return Ok(None);
                };
                let mut limits = *self.resolver.document().limits();
                limits.max_decoded_bytes = limits.max_decoded_bytes.min(MAX_URI_BYTES);
                let result = text_string(self.resolver, &value, limits);
                let Some(Some(uri)) = recover(
                    self.resolver,
                    result,
                    value.object().id(),
                    "link_uri_invalid",
                )?
                else {
                    return Ok(None);
                };
                let safe = uri.split_once(':').is_some_and(|(scheme, _)| {
                    scheme.eq_ignore_ascii_case("http")
                        || scheme.eq_ignore_ascii_case("https")
                        || scheme.eq_ignore_ascii_case("mailto")
                }) && !uri
                    .chars()
                    .any(|character| character.is_control() || character.is_whitespace());
                if !safe {
                    warn(self.resolver, value.object().id(), "link_scheme_dropped");
                    return Ok(None);
                }
                self.budget.charge(uri.len() as u64)?;
                return Ok(Some(LinkTarget::Uri(uri)));
            }
            if name.is_some_and(|name| name.eq_bytes(b"GoTo")) {
                return match self.key(dict, b"D")? {
                    Some(value) => self.destination(value),
                    None => Ok(None),
                };
            }
            warn(self.resolver, action.object().id(), "action_ignored");
            return Ok(None);
        }
        match self.key(dict, b"Dest")? {
            Some(value) => self.destination(value),
            None => Ok(None),
        }
    }

    fn destination(&mut self, mut value: Value) -> Result<Option<LinkTarget>, DocError> {
        let mut seen = Vec::new();
        for depth in 1..=MAX_DEPTH.saturating_add(1) {
            self.depth(depth)?;
            self.step()?;
            if let Some(array) = value.get().as_array() {
                return self.explicit_destination(array);
            }
            if let Some(dict) = value.get().as_dict() {
                let Some(next) = self.key(dict, b"D")? else {
                    break;
                };
                value = next;
                continue;
            }
            let Some(name) = name_bytes(self.resolver, &value)? else {
                break;
            };
            if seen.contains(&name) {
                warn(self.resolver, value.object().id(), "destination_cycle");
                return Ok(None);
            }
            self.budget.charge(name.len() as u64)?;
            let next = self.named_destination(&name)?;
            self.budget.push(&mut seen, name)?;
            let Some(next) = next else {
                warn(self.resolver, value.object().id(), "destination_missing");
                return Ok(None);
            };
            value = next;
        }
        warn(self.resolver, value.object().id(), "destination_invalid");
        Ok(None)
    }

    fn explicit_destination(
        &mut self,
        array: ArrayRef<'_>,
    ) -> Result<Option<LinkTarget>, DocError> {
        let Some(page) = array.get(0) else {
            return Ok(None);
        };
        let index = if let Some(id) = page.as_ref() {
            if let Some(lookup) = self.page_lookup {
                lookup
                    .binary_search_by(|entry| {
                        (entry.0, entry.1).cmp(&(id.number(), id.generation()))
                    })
                    .ok()
                    .and_then(|position| lookup.get(position))
                    .map(|entry| entry.2 as usize)
            } else {
                self.page_ids.iter().position(|candidate| *candidate == id)
            }
        } else {
            // Integer page indices are accepted only when they name a local page.
            page.as_i64()
                .and_then(|number| usize::try_from(number).ok())
                .filter(|index| *index < self.page_ids.len())
        };
        let Some(index) = index.and_then(|index| u32::try_from(index).ok()) else {
            warn(self.resolver, page.as_ref(), "destination_page_missing");
            return Ok(None);
        };
        let Some(kind) = array.get(1) else {
            return Ok(None);
        };
        let Some(kind) = self.resolve(kind)? else {
            return Ok(None);
        };
        let Some(name) = kind.get().as_name() else {
            return Ok(None);
        };
        let mode = if name.eq_bytes(b"XYZ") {
            0
        } else if name.eq_bytes(b"Fit") || name.eq_bytes(b"FitB") {
            1
        } else if name.eq_bytes(b"FitH") || name.eq_bytes(b"FitBH") {
            2
        } else if name.eq_bytes(b"FitV") || name.eq_bytes(b"FitBV") {
            3
        } else if name.eq_bytes(b"FitR") {
            4
        } else {
            warn(
                self.resolver,
                kind.object().id(),
                "destination_mode_ignored",
            );
            return Ok(None);
        };
        let y = if mode == 1 {
            None
        } else {
            self.destination_y(index, mode, array)?
        };
        Ok(Some(LinkTarget::Page { index, y }))
    }

    fn number(&mut self, array: ArrayRef<'_>, index: usize) -> Result<Option<f64>, DocError> {
        let Some(value) = array.get(index) else {
            return Ok(None);
        };
        let Some(value) = self.resolve(value)? else {
            return Ok(None);
        };
        if value.get().is_null() {
            return Ok(None);
        }
        let number = value.get().as_f64().filter(|number| number.is_finite());
        if number.is_none() {
            warn(self.resolver, value.object().id(), "link_number_invalid");
        }
        Ok(number)
    }

    fn rect(&mut self, value: &Value) -> Result<Option<[f64; 4]>, DocError> {
        let Some(array) = value.get().as_array().filter(|array| array.len() == 4) else {
            warn(self.resolver, value.object().id(), "link_rect_invalid");
            return Ok(None);
        };
        let mut rect = [0.0; 4];
        for (index, slot) in rect.iter_mut().enumerate() {
            let Some(number) = self.number(array, index)? else {
                warn(self.resolver, value.object().id(), "link_rect_invalid");
                return Ok(None);
            };
            *slot = number;
        }
        Ok(Some(rect))
    }

    fn destination_y(
        &mut self,
        index: u32,
        mode: u8,
        array: ArrayRef<'_>,
    ) -> Result<Option<f32>, DocError> {
        let left = if mode == 0 || mode == 3 || mode == 4 {
            self.number(array, 2)?
        } else {
            None
        };
        let top = match mode {
            0 => self.number(array, 3)?,
            2 => self.number(array, 2)?,
            4 => self.number(array, 5)?,
            _ => None,
        };
        if (mode == 0 || mode == 2) && top.is_none() {
            return Ok(None);
        }
        if mode == 3 && left.is_none() {
            return Ok(None);
        }
        let bottom = if mode == 4 {
            self.number(array, 3)?
        } else {
            None
        };
        let right = if mode == 4 {
            self.number(array, 4)?
        } else {
            None
        };
        if mode == 4 && (left.is_none() || top.is_none() || bottom.is_none() || right.is_none()) {
            return Ok(None);
        }
        if self.pages.is_none() && !self.did_walk {
            self.did_walk = true;
            // Only the standalone APIs need a walk. SemanticReader supplies its descriptors.
            let mut walked = Vec::new();
            for page in PageWalk::new(self.resolver)? {
                self.budget.push(&mut walked, page?)?;
            }
            self.walked = walked;
        }
        let Some(id) = self.page_ids.get(index as usize) else {
            return Ok(None);
        };
        let pages = self.pages.unwrap_or(&self.walked);
        let page = pages
            .get(index as usize)
            .filter(|page| page.id == *id)
            .or_else(|| pages.iter().find(|page| page.id == *id));
        let Some(page) = page else { return Ok(None) };
        let display = display_transform(page);
        let [x0, _, _, y1] = page.crop_box;
        let point = (left.unwrap_or(x0), top.unwrap_or(y1));
        let y = if mode == 4 {
            transformed_rect(
                [
                    left.unwrap_or(x0),
                    bottom.unwrap_or(y1),
                    right.unwrap_or(x0),
                    top.unwrap_or(y1),
                ],
                &display,
            )
            .map(|bbox| bbox[1])
        } else {
            coord(point.0)
                .zip(coord(point.1))
                .map(|(x, y)| display(x, y).1)
                .filter(|y| y.is_finite())
        };
        if y.is_none() {
            warn(self.resolver, Some(*id), "destination_geometry_failed");
        }
        Ok(y)
    }

    fn named_destination(&mut self, name: &[u8]) -> Result<Option<Value>, DocError> {
        let Some(catalog) = self.catalog()? else {
            return Ok(None);
        };
        let Some(dict) = catalog.get().as_dict() else {
            return Ok(None);
        };
        if let Some(dests) = self.key(dict, b"Dests")? {
            if let Some(dict) = dests.get().as_dict() {
                if let Some(value) = self.key(dict, name)? {
                    return Ok(Some(value));
                }
            }
        }
        let Some(names) = self.key(dict, b"Names")? else {
            return Ok(None);
        };
        let Some(dict) = names.get().as_dict() else {
            return Ok(None);
        };
        let Some(root) = self.key(dict, b"Dests")? else {
            return Ok(None);
        };
        let mut stack = Vec::new();
        self.budget.push(
            &mut stack,
            TreeFrame {
                value: root,
                depth: 1,
                next: 0,
                entered: false,
            },
        )?;
        let mut seen = BTreeSet::new();
        let mut nodes = 0u64;
        while let Some(mut frame) = stack.pop() {
            self.step()?;
            self.depth(frame.depth)?;
            if !frame.entered {
                if !seen.insert(identity(&frame.value)) {
                    warn(
                        self.resolver,
                        frame.value.object().id(),
                        "destination_tree_cycle",
                    );
                    continue;
                }
                self.budget
                    .charge(core::mem::size_of::<Identity>() as u64)?;
                nodes = nodes.saturating_add(1);
                self.items(nodes)?;
                let Some(dict) = frame.value.get().as_dict() else {
                    continue;
                };
                if let Some(names) = self.key(dict, b"Names")? {
                    if let Some(array) = names.get().as_array() {
                        let mut pairs = array.iter();
                        while let Some(key) = pairs.next() {
                            self.step()?;
                            let Some(destination) = pairs.next() else {
                                warn(
                                    self.resolver,
                                    names.object().id(),
                                    "destination_names_invalid",
                                );
                                break;
                            };
                            let Some(key) = self.resolve(key)? else {
                                continue;
                            };
                            if name_bytes(self.resolver, &key)?.is_some_and(|key| key == name) {
                                return self.resolve(destination);
                            }
                        }
                    }
                }
                let Some(kids) = self.key(dict, b"Kids")? else {
                    continue;
                };
                if kids.get().as_array().is_none() {
                    continue;
                }
                frame.value = kids;
                frame.entered = true;
            }
            let child = frame
                .value
                .get()
                .as_array()
                .and_then(|array| array.get(frame.next));
            let Some(child) = child else { continue };
            let child = self.resolve(child)?;
            frame.next = frame.next.saturating_add(1);
            let depth = frame.depth.saturating_add(1);
            self.budget.push(&mut stack, frame)?;
            if let Some(value) = child {
                self.budget.push(
                    &mut stack,
                    TreeFrame {
                        value,
                        depth,
                        next: 0,
                        entered: false,
                    },
                )?;
            }
        }
        Ok(None)
    }

    fn outline(&mut self) -> Result<Vec<OutlineItem>, DocError> {
        if !self.resolver.document().decryptable() {
            return Ok(Vec::new());
        }
        let Some(catalog) = self.catalog()? else {
            return Ok(Vec::new());
        };
        let Some(dict) = catalog.get().as_dict() else {
            return Ok(Vec::new());
        };
        let Some(root) = self.key(dict, b"Outlines")? else {
            return Ok(Vec::new());
        };
        let Some(dict) = root.get().as_dict() else {
            return Ok(Vec::new());
        };
        let Some(first) = self.key(dict, b"First")? else {
            return Ok(Vec::new());
        };
        let mut stack = Vec::new();
        self.budget.push(&mut stack, (first, 1u8))?;
        let mut seen = BTreeSet::new();
        let mut nodes = 0u64;
        let mut items = Vec::new();
        while let Some((value, level)) = stack.pop() {
            self.step()?;
            self.depth(u64::from(level))?;
            if !seen.insert(identity(&value)) {
                warn(self.resolver, value.object().id(), "outline_cycle");
                continue;
            }
            self.budget
                .charge(core::mem::size_of::<Identity>() as u64)?;
            nodes = nodes.saturating_add(1);
            self.items(nodes)?;
            let Some(dict) = value.get().as_dict() else {
                warn(self.resolver, value.object().id(), "outline_item_invalid");
                continue;
            };
            let title = match self.key(dict, b"Title")? {
                Some(title) => {
                    let limits = *self.resolver.document().limits();
                    let result = text_string(self.resolver, &title, limits);
                    recover(
                        self.resolver,
                        result,
                        title.object().id(),
                        "outline_title_invalid",
                    )?
                    .flatten()
                    .unwrap_or_default()
                }
                None => String::new(),
            };
            self.budget.charge(title.len() as u64)?;
            let target = self.target(dict)?;
            self.budget.push(
                &mut items,
                OutlineItem {
                    title,
                    level,
                    target,
                },
            )?;
            // Reverse push order gives pre-order traversal, including closed branches.
            if let Some(next) = self.key(dict, b"Next")? {
                self.budget.push(&mut stack, (next, level))?;
            }
            if let Some(first) = self.key(dict, b"First")? {
                let child_level = level.saturating_add(1);
                self.depth(u64::from(child_level))?;
                self.budget.push(&mut stack, (first, child_level))?;
            }
        }
        Ok(items)
    }
}

struct TreeFrame {
    value: Value,
    depth: u64,
    next: usize,
    entered: bool,
}

fn coord(number: f64) -> Option<f32> {
    let number = number as f32;
    number.is_finite().then_some(number)
}

fn transformed_rect(
    [x0, y0, x1, y1]: [f64; 4],
    display: &impl Fn(f32, f32) -> (f32, f32),
) -> Option<[f32; 4]> {
    let mut bbox = [
        f32::INFINITY,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::NEG_INFINITY,
    ];
    for (x, y) in [(x0, y0), (x0, y1), (x1, y0), (x1, y1)] {
        let (x, y) = display(coord(x)?, coord(y)?);
        if !x.is_finite() || !y.is_finite() {
            return None;
        }
        bbox[0] = bbox[0].min(x);
        bbox[1] = bbox[1].min(y);
        bbox[2] = bbox[2].max(x);
        bbox[3] = bbox[3].max(y);
    }
    Some(bbox)
}

fn name_bytes(resolver: &mut Resolver<'_>, value: &Value) -> Result<Option<Vec<u8>>, DocError> {
    let limits = *resolver.document().limits();
    let maximum = MAX_NAME_BYTES
        .min(limits.max_object_bytes)
        .min(limits.max_total_decoded_bytes);
    if let Some(name) = value.get().as_name() {
        check(LimitKind::ObjectBytes, name.encoded().len() as u64, maximum)?;
        let bytes = name.decoded();
        let mut out = Vec::new();
        out.try_reserve_exact(bytes.len())
            .map_err(|_| refusal(LimitKind::ObjectBytes, maximum.saturating_add(1), maximum))?;
        out.extend_from_slice(&bytes);
        return Ok(Some(out));
    }
    if value.get().as_str().is_some() {
        return decode_object_string(resolver, value, maximum).map(Some);
    }
    Ok(None)
}

/// Decode PDF text strings without assuming that their source bytes are UTF-8.
pub(crate) fn text_string(
    resolver: &mut Resolver<'_>,
    value: &Value,
    limits: Limits,
) -> Result<Option<String>, DocError> {
    let Some(text) = value.get().as_str() else {
        return Err(DocError::WrongType {
            id: value.object().id(),
            expected: "PDF text string",
        });
    };
    let raw_maximum = limits
        .max_object_bytes
        .min(MAX_TEXT_BYTES.saturating_mul(2).saturating_add(2));
    check(LimitKind::ObjectBytes, text.raw().len() as u64, raw_maximum)?;
    let buffer = decode_object_string(resolver, value, raw_maximum)?;
    let bytes = buffer.as_slice();
    let maximum = MAX_TEXT_BYTES
        .min(limits.max_decoded_bytes)
        .min(limits.max_total_decoded_bytes);
    check(LimitKind::DecodedBytes, bytes.len() as u64, maximum)?;
    let mut out = String::new();
    if bytes.starts_with(&[0xfe, 0xff]) || bytes.starts_with(&[0xff, 0xfe]) {
        let little = bytes.starts_with(&[0xff, 0xfe]);
        let body = bytes.get(2..).unwrap_or(&[]);
        let units = body.as_chunks::<2>().0.iter().map(|&bytes| {
            if little {
                u16::from_le_bytes(bytes)
            } else {
                u16::from_be_bytes(bytes)
            }
        });
        for character in core::char::decode_utf16(units) {
            text_char(&mut out, character.unwrap_or('\u{fffd}'), maximum)?;
        }
        if body.len() % 2 != 0 {
            text_char(&mut out, '\u{fffd}', maximum)?;
        }
    } else if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        let mut remaining = bytes.get(3..).unwrap_or(&[]);
        while !remaining.is_empty() {
            match core::str::from_utf8(remaining) {
                Ok(text) => {
                    text_append(&mut out, text, maximum)?;
                    break;
                }
                Err(error) => {
                    let valid = remaining.get(..error.valid_up_to()).unwrap_or(&[]);
                    if let Ok(text) = core::str::from_utf8(valid) {
                        text_append(&mut out, text, maximum)?;
                    }
                    text_char(&mut out, '\u{fffd}', maximum)?;
                    let skip = error
                        .valid_up_to()
                        .saturating_add(error.error_len().unwrap_or(remaining.len()));
                    remaining = remaining.get(skip..).unwrap_or(&[]);
                }
            }
        }
    } else {
        for byte in bytes {
            let unicode = PDF_DOC.get(usize::from(*byte)).copied().unwrap_or(0);
            let character = char::from_u32(u32::from(unicode))
                .filter(|_| unicode != 0 || *byte == 0)
                .unwrap_or('\u{fffd}');
            text_char(&mut out, character, maximum)?;
        }
    }
    Ok(Some(out))
}

fn text_char(out: &mut String, character: char, maximum: u64) -> Result<(), DocError> {
    let mut bytes = [0u8; 4];
    text_append(out, character.encode_utf8(&mut bytes), maximum)
}

fn text_append(out: &mut String, text: &str, maximum: u64) -> Result<(), DocError> {
    check(
        LimitKind::DecodedBytes,
        (out.len() as u64).saturating_add(text.len() as u64),
        maximum,
    )?;
    out.try_reserve(text.len())
        .map_err(|_| refusal(LimitKind::DecodedBytes, maximum.saturating_add(1), maximum))?;
    out.push_str(text);
    Ok(())
}

pub(crate) fn read_key(
    resolver: &mut Resolver<'_>,
    dict: DictRef<'_>,
    key: &[u8],
) -> Result<Option<Value>, DocError> {
    let result = resolver.resolve_dict_key(dict, key);
    Ok(recover(resolver, result, None, "semantic_object_failed")?
        .flatten()
        .filter(|value| !value.get().is_null()))
}

pub(crate) fn recover<T>(
    resolver: &mut Resolver<'_>,
    result: Result<T, DocError>,
    object: Option<ObjectRef>,
    what: &'static str,
) -> Result<Option<T>, DocError> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error @ (DocError::Encrypted | DocError::Refused(_))) => Err(error),
        Err(_) => {
            warn(resolver, object, what);
            Ok(None)
        }
    }
}

pub(crate) fn warn(resolver: &mut Resolver<'_>, object: Option<ObjectRef>, what: &'static str) {
    resolver.warn(Warning {
        offset: None,
        object,
        what,
    });
}
