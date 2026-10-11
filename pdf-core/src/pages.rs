use alloc::vec::Vec;
use core::convert::TryFrom;

use crate::document::Resolver;
use crate::error::{DocError, Warning};
use crate::limits::{check, LimitKind, Refusal};
use crate::object::{DictRef, Object, Value, ValueRef};
use crate::work::ObjectRef;

const DEFAULT_MEDIA_BOX: [f64; 4] = [0.0, 0.0, 612.0, 792.0];

#[derive(Clone, Debug)]
pub struct Page {
    pub index: u32,
    pub id: ObjectRef,
    pub media_box: [f64; 4],
    pub crop_box: [f64; 4],
    pub rotate: u16,
    pub user_unit: f64,
    pub resources: Option<Value>,
    pub contents: Vec<ObjectRef>,
    pub annots: u32,
}

pub struct PageWalk<'r, 'd> {
    resolver: &'r mut Resolver<'d>,
    root: Option<PendingNode>,
    stack: Vec<Frame>,
    visited: Vec<ObjectRef>,
    page_count: u64,
    warnings: Vec<Warning>,
    finished: bool,
    tree_damaged: bool,
    repair_scan: Option<u32>,
    repair_error: Option<DocError>,
    repair_streams: Vec<u32>,
    repair_decoded: u64,
}

struct PendingNode {
    id: ObjectRef,
    depth: u64,
    inherited: Inherited,
    is_root: bool,
}

struct Frame {
    node: ObjectRef,
    kids: Value,
    next: usize,
    inherited: Inherited,
    depth: u64,
}

struct Child {
    id: Option<ObjectRef>,
    parent: ObjectRef,
    inherited: Inherited,
    depth: u64,
}

#[derive(Clone, Default)]
struct Inherited {
    resources: Option<Value>,
    resources_error: Option<DocError>,
    media_box: Option<[f64; 4]>,
    media_box_error: Option<DocError>,
    crop_box: Option<[f64; 4]>,
    crop_box_error: Option<DocError>,
    rotate: Option<u16>,
    rotate_error: Option<DocError>,
}

#[derive(Clone, Copy)]
enum NodeKind {
    Interior,
    Leaf,
}

impl<'r, 'd> PageWalk<'r, 'd> {
    pub fn new(resolver: &'r mut Resolver<'d>) -> Result<Self, DocError> {
        let root = (|| {
            let catalog_id = resolver
                .document()
                .root_ref()
                .ok_or_else(|| malformed(0, "catalog"))?;
            let catalog = resolver
                .get(catalog_id)
                .map_err(|error| map_missing(error, "catalog"))?;
            let catalog_dict = catalog
                .dict()
                .ok_or_else(|| malformed(0, "catalog"))?;
            let pages_id = catalog_dict
                .get(b"Pages")
                .and_then(|value| value.as_ref())
                .ok_or_else(|| malformed(0, "pages"))?;
            Ok(PendingNode {
                id: pages_id,
                depth: 0,
                inherited: Inherited::default(),
                is_root: true,
            })
        })();
        let (root, repair_error) = match root {
            Ok(root) => (Some(root), None),
            Err(error) if resolver.document().repair_mode()
                && !crate::repair::is_hard_error(&error) => (None, Some(error)),
            Err(error) => return Err(error),
        };
        let scan = repair_error.is_some();
        let mut walk = Self {
            resolver,
            root,
            stack: Vec::new(),
            visited: Vec::new(),
            page_count: 0,
            warnings: Vec::new(),
            finished: false,
            tree_damaged: scan,
            repair_scan: None,
            repair_error,
            repair_streams: Vec::new(),
            repair_decoded: 0,
        };
        if scan { walk.start_repair_scan(); }
        Ok(walk)
    }

    pub fn take_warnings(&mut self) -> Vec<Warning> {
        core::mem::take(&mut self.warnings)
    }

    fn insert_visited(&mut self, id: ObjectRef) -> bool {
        match self
            .visited
            .binary_search_by(|candidate| compare_refs(*candidate, id))
        {
            Ok(_) => false,
            Err(index) => {
                self.visited.insert(index, id);
                true
            }
        }
    }

    fn next_child(&mut self) -> Option<Child> {
        loop {
            let exhausted = match self.stack.last() {
                Some(frame) => match frame.kids.get().as_array() {
                    Some(kids) => frame.next >= kids.len(),
                    None => true,
                },
                None => return None,
            };

            if exhausted {
                self.stack.pop();
                continue;
            }

            let frame = match self.stack.last_mut() {
                Some(frame) => frame,
                None => return None,
            };
            let index = frame.next;
            frame.next = frame.next.saturating_add(1);
            let id = frame
                .kids
                .get()
                .as_array()
                .and_then(|kids| kids.get(index))
                .and_then(|value| value.as_ref());

            return Some(Child {
                id,
                parent: frame.node,
                inherited: frame.inherited.clone(),
                depth: frame.depth.saturating_add(1),
            });
        }
    }

    fn push_warning(&mut self, object: Option<ObjectRef>, what: &'static str) {
        if matches!(what, "page_tree_bad_kid" | "page_tree_cycle") {
            self.tree_damaged = true;
        }
        append_warning(&mut self.warnings, object, what);
    }

    fn start_repair_scan(&mut self) {
        self.root = None;
        self.stack.clear();
        self.finished = false;
        self.repair_scan = Some(1);
        self.push_warning(None, "page_list_rebuilt");
    }

    fn repair_get(&mut self, id: ObjectRef) -> Result<Object, DocError> {
        let object = self.resolver.get(id)?;
        if let Some(crate::document::XrefEntry::Compressed { stream, .. }) =
            self.resolver.document().xref().get(id.number())
        {
            if !self.repair_streams.contains(&stream) {
                self.repair_decoded = self.repair_decoded.saturating_add(object.buffer().len() as u64);
                check(
                    LimitKind::TotalDecodedBytes, self.repair_decoded,
                    self.resolver.document().limits().max_total_decoded_bytes,
                )?;
                self.repair_streams.push(stream);
            }
        }
        Ok(object)
    }

    fn repair_inherited(&mut self, object: &Object, id: ObjectRef) -> Result<Inherited, DocError> {
        let mut ancestors = Vec::new();
        let mut seen = Vec::new();
        ancestors.push(object.clone());
        seen.push(id);
        loop {
            let parent = ancestors.last().and_then(|object| object.dict())
                .and_then(|dict| dict.get(b"Parent")).and_then(|value| value.as_ref());
            let Some(parent) = parent else { break; };
            if seen.contains(&parent) {
                self.push_warning(Some(parent), "page_tree_cycle");
                break;
            }
            check(
                LimitKind::PageTreeDepth,
                ancestors.len() as u64,
                u64::from(self.resolver.document().limits().max_page_tree_depth),
            )?;
            let ancestor = match self.repair_get(parent) {
                Ok(ancestor) if ancestor.dict().is_some() => ancestor,
                Err(error) if crate::repair::is_hard_error(&error) => return Err(error),
                _ => {
                    self.push_warning(Some(parent), "repair_page_parent_unresolved");
                    break;
                }
            };
            seen.push(parent);
            ancestors.push(ancestor);
        }
        let mut inherited = Inherited::default();
        for ancestor in ancestors.iter().rev() {
            if let Some(dict) = ancestor.dict() {
                inherited = inherit_attrs(
                    self.resolver, dict, inherited, ancestor.id().unwrap_or(id), &mut self.warnings,
                );
            }
        }
        Ok(inherited)
    }

    fn scan_step(&mut self) -> Option<Result<Page, DocError>> {
        if self.finished { return None; }
        let size = self.resolver.document().xref().size();
        if let Err(error) = check(
            LimitKind::Objects, u64::from(size),
            u64::from(self.resolver.document().limits().max_objects),
        ) {
            self.finished = true;
            return Some(Err(error));
        }
        while let Some(number) = self.repair_scan.filter(|number| *number < size) {
            self.repair_scan = Some(number.saturating_add(1));
            let generation = match self.resolver.document().xref().get(number) {
                Some(crate::document::XrefEntry::InUse { generation, .. }) => generation,
                Some(crate::document::XrefEntry::Compressed { .. }) => 0,
                _ => continue,
            };
            let id = ObjectRef::new(number, generation);
            if self.visited.binary_search_by(|candidate| compare_refs(*candidate, id)).is_ok() {
                continue;
            }
            let object = match self.repair_get(id) {
                Ok(object) => object,
                Err(error) if crate::repair::is_hard_error(&error) => {
                    self.finished = true;
                    return Some(Err(error));
                }
                Err(_) => continue,
            };
            let Some(dict) = object.dict() else { continue; };
            let Some(kind) = dict.get(b"Type") else { continue; };
            let kind = match self.resolver.resolve(kind) {
                Ok(kind) => kind,
                Err(error) if crate::repair::is_hard_error(&error) => {
                    self.finished = true;
                    return Some(Err(error));
                }
                Err(_) => continue,
            };
            if !kind.get().as_name().is_some_and(|name| name.eq_bytes(b"Page")) { continue; }
            self.insert_visited(id);
            let requested = self.page_count.saturating_add(1);
            if let Err(error) = check(
                LimitKind::Pages, requested,
                u64::from(self.resolver.document().limits().max_pages),
            ) {
                self.finished = true;
                return Some(Err(error));
            }
            let inherited = match self.repair_inherited(&object, id) {
                Ok(inherited) => inherited,
                Err(error) => {
                    self.finished = true;
                    return Some(Err(error));
                }
            };
            let index = u32::try_from(self.page_count).unwrap_or(u32::MAX);
            self.page_count = requested;
            self.push_warning(Some(id), "page_recovered");
            return Some(self.build_page(id, index, inherited, dict));
        }
        self.finished = true;
        if self.page_count == 0 {
            self.repair_error.take().map(Err)
        } else {
            None
        }
    }

    fn build_page(
        &mut self,
        id: ObjectRef,
        index: u32,
        inherited: Inherited,
        dict: DictRef<'_>,
    ) -> Result<Page, DocError> {
        let Inherited {
            resources,
            resources_error,
            media_box,
            media_box_error,
            crop_box,
            crop_box_error,
            rotate,
            rotate_error,
        } = inherited;

        if let Some(error) = resources_error {
            return Err(error);
        }
        if let Some(error) = media_box_error {
            return Err(error);
        }
        if let Some(error) = crop_box_error {
            return Err(error);
        }
        if let Some(error) = rotate_error {
            return Err(error);
        }

        let media_box = match media_box {
            Some(media_box) => media_box,
            None => {
                self.push_warning(Some(id), "media_box_default");
                DEFAULT_MEDIA_BOX
            }
        };
        let crop_box = intersect_boxes(crop_box.unwrap_or(media_box), media_box);
        let rotate = rotate.unwrap_or(0);
        let user_unit = read_user_unit(self.resolver, dict)?;
        let contents = read_contents(self.resolver, dict, id, &mut self.warnings)?;
        let annots = read_annots(self.resolver, dict)?;

        Ok(Page {
            index,
            id,
            media_box,
            crop_box,
            rotate,
            user_unit,
            resources,
            contents,
            annots,
        })
    }
}

impl Iterator for PageWalk<'_, '_> {
    type Item = Result<Page, DocError>;

    /// Walk warnings are handed to the Resolver after every step, so callers read
    /// them all through `Resolver::take_warnings`.
    fn next(&mut self) -> Option<Self::Item> {
        let item = self.step();
        for warning in core::mem::take(&mut self.warnings) {
            self.resolver.warn(warning);
        }
        item
    }
}

impl PageWalk<'_, '_> {
    fn step(&mut self) -> Option<Result<Page, DocError>> {
        if self.repair_scan.is_some() { return self.scan_step(); }
        let item = self.tree_step();
        if !self.resolver.document().repair_mode() { return item; }
        match item {
            Some(Err(error)) if self.finished && !crate::repair::is_hard_error(&error) => {
                self.repair_error = Some(error);
                self.start_repair_scan();
                self.scan_step()
            }
            None if !self.finished && (self.tree_damaged || self.page_count == 0) => {
                self.start_repair_scan();
                self.scan_step()
            }
            item => item,
        }
    }

    fn tree_step(&mut self) -> Option<Result<Page, DocError>> {
        if self.finished {
            return None;
        }

        loop {
            let pending = if let Some(root) = self.root.take() {
                root
            } else {
                let child = self.next_child()?;
                let id = match child.id {
                    Some(id) => id,
                    None => {
                        self.push_warning(Some(child.parent), "page_tree_bad_kid");
                        continue;
                    }
                };
                PendingNode {
                    id,
                    depth: child.depth,
                    inherited: child.inherited,
                    is_root: false,
                }
            };

            if !self.insert_visited(pending.id) {
                self.push_warning(Some(pending.id), "page_tree_cycle");
                continue;
            }

            let maximum_depth = u64::from(self.resolver.document().limits().max_page_tree_depth);
            if pending.depth > maximum_depth {
                self.finished = true;
                return Some(Err(DocError::Refused(Refusal {
                    limit: LimitKind::PageTreeDepth,
                    requested: pending.depth,
                    maximum: maximum_depth,
                })));
            }

            let object = match self.resolver.get(pending.id) {
                Ok(object) => object,
                Err(error) if pending.is_root => {
                    self.finished = true;
                    return Some(Err(map_missing(error, "pages")));
                }
                // Repair can omit an unreadable kid and recover other leaves.
                // Keep its error if no page can be recovered at all.
                Err(error @ DocError::Malformed { .. }) if self.resolver.document().repair_mode() => {
                    if self.repair_error.is_none() { self.repair_error = Some(error); }
                    self.push_warning(Some(pending.id), "page_tree_bad_kid");
                    continue;
                }
                // A kid that exists but cannot be parsed (limit or malformed) is a
                // page error the caller can count; a dangling ref is only a warning.
                Err(error @ (DocError::Refused(_) | DocError::Malformed { .. })) => {
                    return Some(Err(error));
                }
                Err(_) => {
                    self.push_warning(Some(pending.id), "page_tree_bad_kid");
                    continue;
                }
            };

            let dict = match object.dict() {
                Some(dict) => dict,
                None if pending.is_root => {
                    self.finished = true;
                    return Some(Err(malformed(0, "pages")));
                }
                None => {
                    self.push_warning(Some(pending.id), "page_tree_bad_kid");
                    continue;
                }
            };

            let kind = match classify_node(self.resolver, dict) {
                Ok(kind) => kind,
                Err(error) if pending.is_root => {
                    self.finished = true;
                    return Some(Err(error));
                }
                Err(error) if self.resolver.document().repair_mode()
                    && crate::repair::is_hard_error(&error) => {
                    self.finished = true;
                    return Some(Err(error));
                }
                Err(_) => {
                    self.push_warning(Some(pending.id), "page_tree_bad_kid");
                    continue;
                }
            };

            let inherited = inherit_attrs(
                self.resolver,
                dict,
                pending.inherited,
                pending.id,
                &mut self.warnings,
            );

            match kind {
                NodeKind::Interior => {
                    if let Some(kids_ref) = dict.get(b"Kids") {
                        match self.resolver.resolve(kids_ref) {
                            Ok(kids) if kids.get().as_array().is_some() => {
                                self.stack.push(Frame {
                                    node: pending.id,
                                    kids,
                                    next: 0,
                                    inherited,
                                    depth: pending.depth,
                                });
                            }
                            Err(error) if self.resolver.document().repair_mode()
                                && crate::repair::is_hard_error(&error) => {
                                self.finished = true;
                                return Some(Err(error));
                            }
                            Ok(_) | Err(_) => {
                                self.push_warning(Some(pending.id), "page_tree_bad_kid");
                            }
                        }
                    } else if self.resolver.document().repair_mode() {
                        self.push_warning(Some(pending.id), "page_tree_bad_kid");
                    }
                }
                NodeKind::Leaf => {
                    let requested = self.page_count.saturating_add(1);
                    let maximum = u64::from(self.resolver.document().limits().max_pages);
                    if requested > maximum {
                        self.finished = true;
                        return Some(Err(DocError::Refused(Refusal {
                            limit: LimitKind::Pages,
                            requested,
                            maximum,
                        })));
                    }

                    let index = match u32::try_from(self.page_count) {
                        Ok(index) => index,
                        Err(_) => u32::MAX,
                    };
                    self.page_count = requested;
                    return Some(self.build_page(pending.id, index, inherited, dict));
                }
            }
        }
    }
}

pub fn page_count_hint(resolver: &mut Resolver<'_>) -> Result<u32, DocError> {
    if !resolver.document().repair_mode() { return page_count_hint_strict(resolver); }
    let declared = match page_count_hint_strict(resolver) {
        Ok(count) => Some(count),
        Err(error) if crate::repair::is_hard_error(&error) => return Err(error),
        Err(_) => None,
    };
    let count = {
        let mut walk = PageWalk::new(resolver)?;
        while let Some(page) = walk.next() {
            if let Err(error) = page {
                if crate::repair::is_hard_error(&error) || walk.page_count == 0 { return Err(error); }
            }
        }
        u32::try_from(walk.page_count).unwrap_or(u32::MAX)
    };
    if declared != Some(count) {
        resolver.warn(Warning {
            offset: None,
            object: resolver.document().root_ref(),
            what: "page_count_recovered",
        });
    }
    Ok(count)
}

fn page_count_hint_strict(resolver: &mut Resolver<'_>) -> Result<u32, DocError> {
    let catalog_id = resolver
        .document()
        .root_ref()
        .ok_or_else(|| malformed(0, "catalog"))?;
    let catalog = resolver
        .get(catalog_id)
        .map_err(|error| map_missing(error, "catalog"))?;
    let catalog_dict = catalog
        .dict()
        .ok_or_else(|| malformed(0, "catalog"))?;
    let pages_id = catalog_dict
        .get(b"Pages")
        .and_then(|value| value.as_ref())
        .ok_or_else(|| malformed(0, "pages"))?;
    let pages = resolver
        .get(pages_id)
        .map_err(|error| map_missing(error, "pages"))?;
    let pages_dict = pages.dict().ok_or_else(|| malformed(0, "pages"))?;

    let Some(count_ref) = pages_dict.get(b"Count") else {
        return Ok(0);
    };
    let count = resolver.resolve(count_ref)?;
    let Some(count) = count.get().as_i64() else {
        return Ok(0);
    };
    if count <= 0 {
        return Ok(0);
    }

    match u32::try_from(count) {
        Ok(count) => Ok(count),
        Err(_) => Ok(0),
    }
}

fn classify_node(
    resolver: &mut Resolver<'_>,
    dict: DictRef<'_>,
) -> Result<NodeKind, DocError> {
    if let Some(type_ref) = dict.get(b"Type") {
        let resolved = resolver.resolve(type_ref);
        match &resolved {
            Ok(value) if value.get().as_name().is_some_and(|name| name.eq_bytes(b"Pages")) => {
                return Ok(NodeKind::Interior);
            }
            Ok(value) if value.get().as_name().is_some_and(|name| name.eq_bytes(b"Page")) => {
                return Ok(NodeKind::Leaf);
            }
            _ => {}
        }
        if resolver.document().repair_mode() {
            if let Err(error) = &resolved {
                if crate::repair::is_hard_error(error) { return Err(error.clone()); }
            }
            let kind = if dict.get(b"Kids").is_some() || dict.get(b"Count").is_some() {
                Some(NodeKind::Interior)
            } else if dict.get(b"Parent").and_then(|value| value.as_ref()).is_some()
                && (dict.get(b"Contents").is_some() || dict.get(b"MediaBox").is_some())
            {
                Some(NodeKind::Leaf)
            } else {
                None
            };
            if let Some(kind) = kind {
                resolver.warn(Warning {
                    offset: Some(type_ref.span().start as u64),
                    object: None,
                    what: "page_tree_type_recovered",
                });
                return Ok(kind);
            }
        }
        return match resolved {
            Ok(value) => Err(malformed_at(value.get(), "page_tree_type")),
            Err(error) => Err(error),
        };
    }

    if dict.get(b"Kids").is_some()
        || (resolver.document().repair_mode() && dict.get(b"Count").is_some())
    {
        Ok(NodeKind::Interior)
    } else {
        Ok(NodeKind::Leaf)
    }
}

fn inherit_attrs(
    resolver: &mut Resolver<'_>,
    dict: DictRef<'_>,
    mut inherited: Inherited,
    id: ObjectRef,
    warnings: &mut Vec<Warning>,
) -> Inherited {
    if let Some(value) = dict.get(b"Resources") {
        inherited.resources = None;
        inherited.resources_error = match read_resources(resolver, value) {
            Ok(resources) => {
                inherited.resources = Some(resources);
                None
            }
            Err(error) => Some(error),
        };
    }

    if let Some(value) = dict.get(b"MediaBox") {
        inherited.media_box = None;
        inherited.media_box_error = match read_box(resolver, value, "media_box") {
            Ok(media_box) => Some({
                inherited.media_box = Some(media_box);
                return_after_box_ok()
            }),
            Err(error) => Some(error),
        };
        if inherited.media_box_error.as_ref().is_some_and(is_box_placeholder) {
            inherited.media_box_error = None;
        }
    }

    if let Some(value) = dict.get(b"CropBox") {
        inherited.crop_box = None;
        inherited.crop_box_error = match read_box(resolver, value, "crop_box") {
            Ok(crop_box) => {
                inherited.crop_box = Some(crop_box);
                None
            }
            Err(error) => Some(error),
        };
    }

    if let Some(value) = dict.get(b"Rotate") {
        inherited.rotate = None;
        inherited.rotate_error = match read_rotate(resolver, value) {
            Ok(rotate) => {
                if rotate.1 {
                    append_warning(warnings, Some(id), "rotate_rounded");
                }
                inherited.rotate = Some(rotate.0);
                None
            }
            Err(error) => Some(error),
        };
    }

    inherited
}

fn return_after_box_ok() -> DocError {
    malformed(0, "unused")
}

fn is_box_placeholder(error: &DocError) -> bool {
    matches!(
        error,
        DocError::Malformed {
            what: "unused",
            ..
        }
    )
}

fn read_resources(resolver: &mut Resolver<'_>, value: ValueRef<'_>) -> Result<Value, DocError> {
    let resources = resolver.resolve(value)?;
    if resources.get().as_dict().is_some() {
        Ok(resources)
    } else {
        Err(malformed_at(resources.get(), "resources"))
    }
}

fn read_box(
    resolver: &mut Resolver<'_>,
    value: ValueRef<'_>,
    what: &'static str,
) -> Result<[f64; 4], DocError> {
    let resolved = resolver.resolve(value)?;
    let resolved_ref = resolved.get();
    let Some(array) = resolved_ref.as_array() else {
        return Err(malformed_at(resolved_ref, what));
    };
    if array.len() != 4 {
        return Err(malformed_at(resolved_ref, what));
    }

    let mut coordinates = [0.0; 4];
    let mut index = 0usize;
    while index < 4 {
        let Some(component) = array.get(index) else {
            return Err(malformed_at(resolved_ref, what));
        };
        let component = resolver.resolve(component)?;
        let component_ref = component.get();
        let Some(number) = component_ref.as_f64() else {
            return Err(malformed_at(component_ref, what));
        };
        if !number.is_finite() {
            return Err(malformed_at(component_ref, what));
        }
        let Some(slot) = coordinates.get_mut(index) else {
            return Err(malformed_at(component_ref, what));
        };
        *slot = number;
        index = index.saturating_add(1);
    }

    Ok(normalize_box(coordinates))
}

fn read_rotate(
    resolver: &mut Resolver<'_>,
    value: ValueRef<'_>,
) -> Result<(u16, bool), DocError> {
    let resolved = resolver.resolve(value)?;
    let resolved_ref = resolved.get();
    let Some(degrees) = resolved_ref.as_f64() else {
        return Err(malformed_at(resolved_ref, "rotate"));
    };
    if !degrees.is_finite() {
        return Err(malformed_at(resolved_ref, "rotate"));
    }

    // no_std has no f64::round; Rotate is bounded so integer math is exact enough.
    let clamped = degrees.clamp(-1.0e9, 1.0e9);
    let quarter = clamped / 90.0;
    let nearest = if quarter >= 0.0 { (quarter + 0.5) as i64 } else { (quarter - 0.5) as i64 };
    let rounded = (nearest as f64) * 90.0 != clamped;
    let rotation = match nearest.rem_euclid(4) {
        1 => 90,
        2 => 180,
        3 => 270,
        _ => 0,
    };

    Ok((rotation, rounded))
}

fn read_user_unit(resolver: &mut Resolver<'_>, dict: DictRef<'_>) -> Result<f64, DocError> {
    let Some(value) = dict.get(b"UserUnit") else {
        return Ok(1.0);
    };
    let resolved = resolver.resolve(value)?;
    let resolved_ref = resolved.get();
    let Some(user_unit) = resolved_ref.as_f64() else {
        return Err(malformed_at(resolved_ref, "user_unit"));
    };
    if !user_unit.is_finite() || user_unit <= 0.0 {
        return Err(malformed_at(resolved_ref, "user_unit"));
    }
    Ok(user_unit)
}

fn read_contents(
    resolver: &mut Resolver<'_>,
    dict: DictRef<'_>,
    id: ObjectRef,
    warnings: &mut Vec<Warning>,
) -> Result<Vec<ObjectRef>, DocError> {
    let Some(contents_ref) = dict.get(b"Contents") else {
        return Ok(Vec::new());
    };

    if let Some(stream_id) = contents_ref.as_ref() {
        let mut contents = Vec::new();
        contents.push(stream_id);
        return Ok(contents);
    }

    let contents = resolver.resolve(contents_ref)?;
    let Some(array) = contents.get().as_array() else {
        append_warning(warnings, Some(id), "contents_not_ref");
        return Ok(Vec::new());
    };

    let mut stream_refs = Vec::new();
    let mut index = 0usize;
    while index < array.len() {
        match array.get(index).and_then(|value| value.as_ref()) {
            Some(stream_id) => stream_refs.push(stream_id),
            None => append_warning(warnings, Some(id), "contents_not_ref"),
        }
        index = index.saturating_add(1);
    }

    Ok(stream_refs)
}

fn read_annots(resolver: &mut Resolver<'_>, dict: DictRef<'_>) -> Result<u32, DocError> {
    let Some(annots_ref) = dict.get(b"Annots") else {
        return Ok(0);
    };
    let annots = resolver.resolve(annots_ref)?;
    let annots_ref = annots.get();
    let Some(array) = annots_ref.as_array() else {
        return Err(malformed_at(annots_ref, "annots"));
    };

    match u32::try_from(array.len()) {
        Ok(count) => Ok(count),
        Err(_) => Ok(u32::MAX),
    }
}

fn normalize_box(bounds: [f64; 4]) -> [f64; 4] {
    let [x0, y0, x1, y1] = bounds;
    [
        if x0 <= x1 { x0 } else { x1 },
        if y0 <= y1 { y0 } else { y1 },
        if x0 <= x1 { x1 } else { x0 },
        if y0 <= y1 { y1 } else { y0 },
    ]
}

fn intersect_boxes(crop: [f64; 4], media: [f64; 4]) -> [f64; 4] {
    [
        if crop[0] >= media[0] {
            crop[0]
        } else {
            media[0]
        },
        if crop[1] >= media[1] {
            crop[1]
        } else {
            media[1]
        },
        if crop[2] <= media[2] {
            crop[2]
        } else {
            media[2]
        },
        if crop[3] <= media[3] {
            crop[3]
        } else {
            media[3]
        },
    ]
}

fn compare_refs(left: ObjectRef, right: ObjectRef) -> core::cmp::Ordering {
    match left.number().cmp(&right.number()) {
        core::cmp::Ordering::Equal => left.generation().cmp(&right.generation()),
        ordering => ordering,
    }
}

fn append_warning(warnings: &mut Vec<Warning>, object: Option<ObjectRef>, what: &'static str) {
    warnings.push(Warning {
        offset: None,
        object,
        what,
    });
}

fn malformed(offset: u64, what: &'static str) -> DocError {
    DocError::Malformed { offset, what }
}

fn malformed_at(value: ValueRef<'_>, what: &'static str) -> DocError {
    let offset = match u64::try_from(value.span().start) {
        Ok(offset) => offset,
        Err(_) => u64::MAX,
    };
    malformed(offset, what)
}

fn map_missing(error: DocError, what: &'static str) -> DocError {
    match error {
        DocError::MissingObject { .. } => malformed(0, what),
        other => other,
    }
}

