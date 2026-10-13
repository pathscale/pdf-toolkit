use std::collections::{BTreeMap, VecDeque};
use std::ops::{Deref, Range};
use std::sync::Arc;

use pdf_core::{DocError, Document, LimitKind, ObjectRef, PdfSource};
use pdf_write::Value;

use crate::pages::{self, PageEntry};
use crate::{check, get, malformed, refused, set, value, Editor, Pending};

pub(crate) struct StreamData {
    bytes: Arc<[u8]>,
    range: Range<usize>,
}

impl StreamData {
    pub(crate) fn owned(bytes: Arc<[u8]>) -> Self {
        let range = 0..bytes.len();
        Self { bytes, range }
    }
}

impl Deref for StreamData {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.bytes.get(self.range.clone()).unwrap_or(&[])
    }
}

type Key = (u32, u16);

fn key(id: ObjectRef) -> Key {
    (id.number(), id.generation())
}

impl Editor {
    fn import_id(&self, next: &mut u32) -> Result<ObjectRef, DocError> {
        let count = next
            .checked_sub(self.next)
            .and_then(|count| count.checked_add(1))
            .ok_or_else(|| malformed("import object count"))?;
        let after = self.fresh(count)?;
        let id = ObjectRef::new(*next, 0);
        *next = after;
        Ok(id)
    }

    /// Stages the entire import before changing this editor.
    pub fn import_pages(
        &mut self,
        source: Arc<[u8]>,
        requested: &[u32],
        at: u32,
    ) -> Result<(), DocError> {
        self.clean()?;
        if at > self.page_count() {
            return Err(malformed("page insertion index"));
        }
        let limits = *self.document.limits();
        check(
            LimitKind::Pages,
            (self.pages.len() as u64).saturating_add(requested.len() as u64),
            u64::from(limits.max_pages),
        )?;
        if requested.is_empty() {
            return Ok(());
        }
        check(
            LimitKind::FileBytes,
            source.len() as u64,
            limits.max_file_bytes,
        )?;
        let document = Document::open(PdfSource::new(source)?, limits)?;
        if document.encrypted() {
            return Err(DocError::Encrypted);
        }
        let (_, source_pages) = pages::initial(&document)?;
        for index in requested {
            if source_pages.get(*index as usize).is_none() {
                return Err(malformed("source page index"));
            }
        }
        let source_pages: BTreeMap<_, _> = source_pages
            .into_iter()
            .map(|page| (key(page.id), page))
            .collect();
        let mut resolver = document.resolver();
        let mut mapping = BTreeMap::new();
        let mut queue = VecDeque::new();
        let mut selected = Vec::new();
        let mut next = self.next;
        // Resolve the source page order independently of object-number order.
        let mut walk_resolver = document.resolver();
        let order: Vec<_> = pdf_core::pages::PageWalk::new(&mut walk_resolver)?
            .map(|page| page.map(|page| page.id))
            .collect::<Result<_, _>>()?;
        for index in requested {
            let old = *order
                .get(*index as usize)
                .ok_or_else(|| malformed("source page index"))?;
            let new = self.import_id(&mut next)?;
            if !mapping.contains_key(&key(old)) {
                mapping.insert(key(old), new);
                queue.push_back((old, 0u64));
            }
            selected.push((old, new));
        }
        let mut staged = BTreeMap::new();
        let mut staged_bytes = 0u64;
        let mut steps = 0u64;
        let maximum = limits.max_file_bytes.min(u64::from(u32::MAX));
        while let Some((old, depth)) = queue.pop_front() {
            check(LimitKind::Depth, depth, u64::from(limits.max_depth))?;
            steps = value::add(steps, 1, LimitKind::ContentOps, limits.max_content_ops)?;
            let id = *mapping
                .get(&key(old))
                .ok_or_else(|| malformed("import mapping"))?;
            let object = resolver.get(old)?;
            let mut body = value::checked_object(&object, &limits)?;
            let data = if object.is_stream() {
                let range = object
                    .stream_range()
                    .ok_or_else(|| malformed("source stream"))?;
                let raw = object
                    .stream_raw()
                    .ok_or_else(|| malformed("source stream"))?;
                let length = i64::try_from(raw.len()).map_err(|_| malformed("stream Length"))?;
                let Value::Dict(dict) = &mut body else {
                    return Err(malformed("stream dictionary"));
                };
                set(dict, "Length", Value::Int(length));
                Some(StreamData {
                    bytes: object.buffer().clone(),
                    range,
                })
            } else {
                None
            };
            if let Value::Dict(dict) = &mut body {
                if let Some(entry) = source_pages.get(&key(old)) {
                    pages::explicit(dict, entry);
                    dict.retain(|(name, _)| name != "Parent");
                } else if matches!(get(dict, "Type"), Some(Value::Name(name)) if name == "Page" || name == "Pages")
                {
                    dict.retain(|(name, _)| name != "Parent");
                }
            }
            let mut stack = vec![&mut body];
            while let Some(node) = stack.pop() {
                steps = value::add(steps, 1, LimitKind::ContentOps, limits.max_content_ops)?;
                match node {
                    Value::Ref(number, generation) => {
                        let reference = ObjectRef::new(*number, *generation);
                        let target = match mapping.get(&key(reference)) {
                            Some(target) => *target,
                            None => {
                                let child_depth = depth.saturating_add(1);
                                check(LimitKind::Depth, child_depth, u64::from(limits.max_depth))?;
                                let target = self.import_id(&mut next)?;
                                let metadata =
                                    (mapping.len() as u64).saturating_add(1).saturating_mul(128);
                                check(
                                    LimitKind::TotalDecodedBytes,
                                    metadata,
                                    limits.max_total_decoded_bytes,
                                )?;
                                mapping.insert(key(reference), target);
                                queue.push_back((reference, child_depth));
                                target
                            }
                        };
                        *node = Value::Ref(target.number(), target.generation());
                    }
                    Value::Array(children) => stack.extend(children.iter_mut()),
                    Value::Dict(entries) => {
                        for (_, child) in entries.iter_mut() {
                            stack.push(child);
                        }
                    }
                    _ => {}
                }
            }
            let cost = self.cost(id, &body, data.as_deref())?;
            staged_bytes = value::add(staged_bytes, cost, LimitKind::FileBytes, maximum)?;
            value::add(
                self.bytes.len() as u64,
                value::add(
                    self.pending_bytes,
                    staged_bytes,
                    LimitKind::FileBytes,
                    maximum,
                )?,
                LimitKind::FileBytes,
                maximum,
            )?;
            check(
                LimitKind::TotalDecodedBytes,
                staged_bytes.saturating_add((mapping.len() as u64).saturating_mul(128)),
                limits.max_total_decoded_bytes,
            )?;
            staged.insert(
                id.number(),
                Pending {
                    id,
                    value: body,
                    data,
                    cost,
                },
            );
        }
        let mut inserted = Vec::new();
        for (old, id) in selected {
            let first = *mapping
                .get(&key(old))
                .ok_or_else(|| malformed("import page mapping"))?;
            if first != id {
                let body = staged
                    .get(&first.number())
                    .ok_or_else(|| malformed("import page dictionary"))?
                    .value
                    .clone();
                let cost = self.cost(id, &body, None)?;
                staged_bytes = value::add(staged_bytes, cost, LimitKind::FileBytes, maximum)?;
                staged.insert(
                    id.number(),
                    Pending {
                        id,
                        value: body,
                        data: None,
                        cost,
                    },
                );
            }
            inserted.push(PageEntry {
                id,
                inherited: Vec::new(),
            });
        }
        let changes: Vec<_> = staged
            .values()
            .map(|pending| (pending.id, pending.cost))
            .collect();
        let total = self.changed_size(&changes)?;
        check(
            LimitKind::TotalDecodedBytes,
            staged_bytes.saturating_add((staged.len() as u64).saturating_mul(128)),
            limits.max_total_decoded_bytes,
        )?;
        self.pages.try_reserve(inserted.len()).map_err(|_| {
            refused(
                LimitKind::TotalDecodedBytes,
                limits.max_total_decoded_bytes.saturating_add(1),
                limits.max_total_decoded_bytes,
            )
        })?;
        for (_, pending) in staged {
            self.install(pending);
        }
        self.pages.splice(at as usize..at as usize, inserted);
        self.pending_bytes = total;
        self.next = next;
        self.order_changed = true;
        Ok(())
    }
}
