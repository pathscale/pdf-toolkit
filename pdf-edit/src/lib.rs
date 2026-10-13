//! Bounded, append-only PDF revisions.
//!
//! Page indices refer to the current pending page order. Changes to a page's
//! dictionary, annotation arrays and content arrays are read from pending objects.
//! Methods without a Result return retain their first error for save(). A failed
//! add_object returns object reference 0 0, which is never allocated.
//!
//! Direct values are limited to 128 levels, even when the supplied parser limits
//! allow greater depth. Existing encoded streams remain in the original bytes.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use pdf_core::{DocError, Document, LimitKind, Limits, ObjectRef, PdfSource, Refusal, XrefEntry};
use pdf_write::{Dict, Value};

mod annotations;
mod import;
mod pages;
mod section;
#[cfg(feature = "sign")]
pub mod sign;
mod value;

use import::StreamData;
use pages::PageEntry;

struct Pending {
    id: ObjectRef,
    value: Value,
    data: Option<StreamData>,
    cost: u64,
}

pub struct Editor {
    bytes: Arc<[u8]>,
    document: Document,
    pending: BTreeMap<u32, Pending>,
    pending_bytes: u64,
    next: u32,
    prev: u64,
    first_id: Option<Vec<u8>>,
    info_new: Option<ObjectRef>,
    error: Option<DocError>,
    pages_root: ObjectRef,
    pages: Vec<PageEntry>,
    order_changed: bool,
}

pub(crate) fn malformed(what: &'static str) -> DocError {
    DocError::Malformed { offset: 0, what }
}

pub(crate) fn refused(limit: LimitKind, requested: u64, maximum: u64) -> DocError {
    DocError::Refused(Refusal {
        limit,
        requested,
        maximum,
    })
}

pub(crate) fn check(limit: LimitKind, requested: u64, maximum: u64) -> Result<(), DocError> {
    if requested > maximum {
        Err(refused(limit, requested, maximum))
    } else {
        Ok(())
    }
}

fn wrong(id: Option<ObjectRef>, expected: &'static str) -> DocError {
    DocError::WrongType { id, expected }
}

fn set(dict: &mut Dict, key: &str, replacement: Value) {
    dict.retain(|(existing, _)| existing != key);
    dict.push((key.to_owned(), replacement));
}

fn get<'a>(dict: &'a Dict, key: &str) -> Option<&'a Value> {
    dict.iter()
        .rev()
        .find(|(existing, _)| existing == key)
        .map(|(_, value)| value)
}

fn first_id(document: &Document) -> Result<Option<Vec<u8>>, DocError> {
    let dict = document
        .trailer()
        .dict()
        .ok_or_else(|| malformed("trailer"))?;
    let Some(id) = dict.get(b"ID") else {
        return Ok(None);
    };
    if id.is_null() {
        return Ok(None);
    }
    let mut resolver = document.resolver();
    let resolved = resolver.resolve(id)?;
    let array = resolved
        .get()
        .as_array()
        .ok_or_else(|| wrong(None, "file ID array"))?;
    let Some(first) = array.get(0) else {
        return Ok(None);
    };
    let first = resolver.resolve(first)?;
    let string = first
        .get()
        .as_str()
        .ok_or_else(|| wrong(None, "first file ID string"))?;
    Ok(Some(string.decode()))
}

impl Editor {
    pub fn open(bytes: Arc<[u8]>, limits: Limits) -> Result<Editor, DocError> {
        Self::open_inner(bytes, limits, None)
    }

    /// Opens an authenticated PSChaCha file for encrypted incremental updates.
    pub fn open_with_password(
        bytes: Arc<[u8]>,
        limits: Limits,
        password: &[u8],
    ) -> Result<Editor, DocError> {
        Self::open_inner(bytes, limits, Some(password))
    }

    fn open_inner(
        bytes: Arc<[u8]>,
        limits: Limits,
        password: Option<&[u8]>,
    ) -> Result<Editor, DocError> {
        check(
            LimitKind::FileBytes,
            bytes.len() as u64,
            limits.max_file_bytes.min(u64::from(u32::MAX)),
        )?;
        let source = PdfSource::new(bytes.clone())?;
        let document = match password {
            Some(password) => Document::open_with_password(source, limits, password)?,
            None => Document::open(source, limits)?,
        };
        if document.encrypted() && document.chacha_key().is_none() {
            return Err(DocError::Encrypted);
        }
        for warning in document.warnings() {
            match warning.what {
                "offsets_shifted" | "startxref_adjusted" => {
                    return Err(DocError::Unsupported {
                        feature: "edit:recovered_xref_offsets",
                    });
                }
                "prev_cycle" => return Err(malformed("xref Prev cycle")),
                _ => {}
            }
        }
        let root = document.root_ref().ok_or_else(|| malformed("catalog"))?;
        let trailer = document
            .trailer()
            .dict()
            .ok_or_else(|| malformed("trailer"))?;
        if let Some(info) = trailer.get(b"Info") {
            if !info.is_null() && info.as_ref().is_none() {
                return Err(wrong(None, "Info reference"));
            }
        }
        let next = document.xref().size();
        if next == 0 {
            return Err(malformed("xref Size"));
        }
        let prev = section::startxref(&bytes)?;
        let first_id = first_id(&document)?;
        let (pages_root, pages) = pages::initial(&document)?;
        let editor = Self {
            bytes,
            document,
            pending: BTreeMap::new(),
            pending_bytes: 0,
            next,
            prev,
            first_id,
            info_new: None,
            error: None,
            pages_root,
            pages,
            order_changed: false,
        };
        editor.reference(root)?;
        editor.reference(pages_root)?;
        let catalog = editor.document.resolver().get(root)?;
        if catalog.is_stream() || catalog.dict().is_none() {
            return Err(malformed("catalog"));
        }
        Ok(editor)
    }

    fn clean(&self) -> Result<(), DocError> {
        match &self.error {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }

    fn remember(&mut self, error: DocError) {
        if self.error.is_none() {
            self.error = Some(error);
        }
    }

    fn fresh(&self, count: u32) -> Result<u32, DocError> {
        let after = value::add(
            u64::from(self.next),
            u64::from(count),
            LimitKind::Objects,
            u64::from(self.document.limits().max_objects),
        )?;
        u32::try_from(after).map_err(|_| refused(LimitKind::Objects, after, u64::from(u32::MAX)))
    }

    fn reference(&self, id: ObjectRef) -> Result<(), DocError> {
        if id.number() == 0 || id.generation() == u16::MAX {
            return Err(wrong(Some(id), "live object reference"));
        }
        if let Some(pending) = self.pending.get(&id.number()) {
            return if pending.id == id {
                Ok(())
            } else {
                Err(wrong(Some(id), "matching object generation"))
            };
        }
        match self.document.xref().get(id.number()) {
            Some(XrefEntry::InUse { generation, .. }) if generation == id.generation() => Ok(()),
            Some(XrefEntry::Compressed { .. }) if id.generation() == 0 => Ok(()),
            Some(XrefEntry::InUse { .. }) | Some(XrefEntry::Compressed { .. }) => {
                Err(wrong(Some(id), "matching object generation"))
            }
            _ => Err(DocError::MissingObject { id }),
        }
    }

    // None identifies a stream without copying or converting its dictionary.
    fn read_value(&self, id: ObjectRef) -> Result<Option<Value>, DocError> {
        self.reference(id)?;
        if let Some(pending) = self.pending.get(&id.number()) {
            return if pending.data.is_some() {
                Ok(None)
            } else {
                Ok(Some(pending.value.clone()))
            };
        }
        let object = self.document.resolver().get(id)?;
        if object.is_stream() {
            Ok(None)
        } else {
            value::checked_object(&object, self.document.limits()).map(Some)
        }
    }

    fn dict_at(&self, id: ObjectRef) -> Result<Dict, DocError> {
        match self.read_value(id)? {
            Some(Value::Dict(dict)) => Ok(dict),
            _ => Err(wrong(Some(id), "dictionary")),
        }
    }

    fn page_ref(&self, index: u32) -> Result<ObjectRef, DocError> {
        check(
            LimitKind::Pages,
            u64::from(index).saturating_add(1),
            u64::from(self.document.limits().max_pages),
        )?;
        self.pages
            .get(index as usize)
            .map(|page| page.id)
            .ok_or_else(|| malformed("page index"))
    }

    fn array_slot(&self, mut current: Value, contents: bool) -> Result<Vec<Value>, DocError> {
        let mut seen = BTreeSet::new();
        let maximum = u64::from(self.document.limits().max_ref_chain);
        loop {
            match current {
                Value::Null => return Ok(Vec::new()),
                Value::Array(values) => return Ok(values),
                Value::Ref(number, generation) => {
                    check(
                        LimitKind::RefChain,
                        (seen.len() as u64).saturating_add(1),
                        maximum,
                    )?;
                    if !seen.insert((number, generation)) {
                        return Err(refused(
                            LimitKind::RefChain,
                            maximum.saturating_add(1),
                            maximum,
                        ));
                    }
                    let id = ObjectRef::new(number, generation);
                    match self.read_value(id)? {
                        Some(value) => current = value,
                        None if contents => return Ok(vec![Value::Ref(number, generation)]),
                        None => return Err(wrong(Some(id), "annotation array")),
                    }
                }
                _ => {
                    return Err(wrong(
                        None,
                        if contents {
                            "page contents"
                        } else {
                            "annotation array"
                        },
                    ));
                }
            }
        }
    }

    fn cost(&self, id: ObjectRef, body: &Value, data: Option<&[u8]>) -> Result<u64, DocError> {
        let maximum = self.document.limits().max_object_bytes;
        let mut size = value::measure(body, self.document.limits())?;
        size = value::add(
            size,
            format!("{} {} obj\n", id.number(), id.generation()).len() as u64,
            LimitKind::ObjectBytes,
            maximum,
        )?;
        let tail = match data {
            Some(data) => {
                check(
                    LimitKind::DecodedBytes,
                    data.len() as u64,
                    self.document.limits().max_decoded_bytes,
                )?;
                size = value::add(size, data.len() as u64, LimitKind::ObjectBytes, maximum)?;
                b"\nstream\n"
                    .len()
                    .saturating_add(b"\nendstream\nendobj\n".len())
            }
            None => b"\nendobj\n".len(),
        };
        value::add(size, tail as u64, LimitKind::ObjectBytes, maximum)
    }

    fn changed_size(&self, changes: &[(ObjectRef, u64)]) -> Result<u64, DocError> {
        let maximum = self
            .document
            .limits()
            .max_file_bytes
            .min(u64::from(u32::MAX));
        let mut total = self.pending_bytes;
        for (id, cost) in changes {
            if let Some(old) = self.pending.get(&id.number()) {
                total = total
                    .checked_sub(old.cost)
                    .ok_or_else(|| malformed("pending byte accounting"))?;
            }
            total = value::add(total, *cost, LimitKind::FileBytes, maximum)?;
        }
        value::add(
            self.bytes.len() as u64,
            total,
            LimitKind::FileBytes,
            maximum,
        )?;
        Ok(total)
    }

    fn install(&mut self, pending: Pending) {
        self.pending.insert(pending.id.number(), pending);
    }

    pub fn replace_object(&mut self, id: ObjectRef, body: Value) {
        let result = self
            .clean()
            .and_then(|()| {
                if self.document.chacha_key().is_some()
                    && self.document.encryption_ref() == Some(id)
                {
                    Err(DocError::Unsupported {
                        feature: "edit:PSChaCha security dictionary",
                    })
                } else {
                    Ok(())
                }
            })
            .and_then(|()| self.reference(id))
            .and_then(|()| self.cost(id, &body, None))
            .and_then(|cost| self.changed_size(&[(id, cost)]).map(|total| (cost, total)));
        match result {
            Ok((cost, total)) => {
                self.install(Pending {
                    id,
                    value: body,
                    data: None,
                    cost,
                });
                self.pending_bytes = total;
            }
            Err(error) => {
                value::dispose(body);
                self.remember(error);
            }
        }
    }

    pub fn add_object(&mut self, body: Value) -> ObjectRef {
        let id = ObjectRef::new(self.next, 0);
        let result = self
            .clean()
            .and_then(|()| self.fresh(1))
            .and_then(|after| self.cost(id, &body, None).map(|cost| (after, cost)))
            .and_then(|(after, cost)| {
                self.changed_size(&[(id, cost)])
                    .map(|total| (after, cost, total))
            });
        match result {
            Ok((after, cost, total)) => {
                self.install(Pending {
                    id,
                    value: body,
                    data: None,
                    cost,
                });
                self.next = after;
                self.pending_bytes = total;
                id
            }
            Err(error) => {
                value::dispose(body);
                self.remember(error);
                ObjectRef::new(0, 0)
            }
        }
    }

    pub fn set_info(&mut self, key: &str, text: &str) {
        if let Err(error) = self.try_set_info(key, text) {
            self.remember(error);
        }
    }

    fn try_set_info(&mut self, key: &str, text: &str) -> Result<(), DocError> {
        self.clean()?;
        check(
            LimitKind::ObjectBytes,
            key.len() as u64,
            self.document.limits().max_object_bytes,
        )?;
        let text = value::text_string(text, self.document.limits())?;
        let (id, after, mut dict) = match self.info_new {
            Some(id) => (id, self.next, self.dict_at(id)?),
            None => {
                let after = self.fresh(1)?;
                let dict = match self.document.info_ref() {
                    Some(id) => self.dict_at(id)?,
                    None => Vec::new(),
                };
                (ObjectRef::new(self.next, 0), after, dict)
            }
        };
        set(&mut dict, key, Value::Str(text));
        let body = Value::Dict(dict);
        let cost = self.cost(id, &body, None)?;
        let total = self.changed_size(&[(id, cost)])?;
        self.install(Pending {
            id,
            value: body,
            data: None,
            cost,
        });
        self.next = after;
        self.pending_bytes = total;
        self.info_new = Some(id);
        Ok(())
    }

    pub fn add_uri_link(&mut self, page: u32, rect: [f32; 4], uri: &str) -> Result<(), DocError> {
        self.clean()?;
        if rect.iter().any(|number| !number.is_finite()) {
            return Err(malformed("link rectangle"));
        }
        check(
            LimitKind::ObjectBytes,
            uri.len() as u64,
            self.document.limits().max_object_bytes,
        )?;
        let page_id = self.page_ref(page)?;
        let mut dict = self.dict_at(page_id)?;
        let slot = match get(&dict, "Annots") {
            Some(value) => value.clone(),
            None => Value::Null,
        };
        let mut annots = self.array_slot(slot, false)?;
        let after = self.fresh(1)?;
        let id = ObjectRef::new(self.next, 0);
        let [x0, y0, x1, y1] = rect;
        let rectangle = [x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1)];
        let annotation = Value::Dict(vec![
            ("Type".to_owned(), Value::Name("Annot".to_owned())),
            ("Subtype".to_owned(), Value::Name("Link".to_owned())),
            (
                "P".to_owned(),
                Value::Ref(page_id.number(), page_id.generation()),
            ),
            (
                "Rect".to_owned(),
                Value::Array(
                    rectangle
                        .into_iter()
                        .map(|n| Value::Real(f64::from(n)))
                        .collect(),
                ),
            ),
            (
                "Border".to_owned(),
                Value::Array(vec![Value::Int(0), Value::Int(0), Value::Int(0)]),
            ),
            (
                "A".to_owned(),
                Value::Dict(vec![
                    ("S".to_owned(), Value::Name("URI".to_owned())),
                    ("URI".to_owned(), Value::Str(uri.as_bytes().to_vec())),
                ]),
            ),
        ]);
        annots.push(Value::Ref(id.number(), id.generation()));
        set(&mut dict, "Annots", Value::Array(annots));
        let page_value = Value::Dict(dict);
        let cost = self.cost(id, &annotation, None)?;
        let page_cost = self.cost(page_id, &page_value, None)?;
        let total = self.changed_size(&[(id, cost), (page_id, page_cost)])?;
        self.install(Pending {
            id,
            value: annotation,
            data: None,
            cost,
        });
        self.install(Pending {
            id: page_id,
            value: page_value,
            data: None,
            cost: page_cost,
        });
        self.pending_bytes = total;
        self.next = after;
        Ok(())
    }

    pub fn append_page_content(&mut self, page: u32, content: &[u8]) -> Result<(), DocError> {
        self.clean()?;
        check(
            LimitKind::DecodedBytes,
            content.len() as u64,
            self.document.limits().max_decoded_bytes,
        )?;
        check(
            LimitKind::ObjectBytes,
            content.len() as u64,
            self.document.limits().max_object_bytes,
        )?;
        let page_id = self.page_ref(page)?;
        let mut dict = self.dict_at(page_id)?;
        let slot = match get(&dict, "Contents") {
            Some(value) => value.clone(),
            None => Value::Null,
        };
        let mut contents = self.array_slot(slot, true)?;
        let after = self.fresh(1)?;
        let id = ObjectRef::new(self.next, 0);
        let length =
            i64::try_from(content.len()).map_err(|_| malformed("content stream Length"))?;
        let stream = Value::Dict(vec![("Length".to_owned(), Value::Int(length))]);
        contents.push(Value::Ref(id.number(), id.generation()));
        set(&mut dict, "Contents", Value::Array(contents));
        let page_value = Value::Dict(dict);
        let cost = self.cost(id, &stream, Some(content))?;
        let page_cost = self.cost(page_id, &page_value, None)?;
        let total = self.changed_size(&[(id, cost), (page_id, page_cost)])?;
        self.install(Pending {
            id,
            value: stream,
            data: Some(StreamData::owned(Arc::from(content))),
            cost,
        });
        self.install(Pending {
            id: page_id,
            value: page_value,
            data: None,
            cost: page_cost,
        });
        self.next = after;
        self.pending_bytes = total;
        Ok(())
    }

    pub fn save(&self) -> Result<Vec<u8>, DocError> {
        self.clean()?;
        section::save(self)
    }
}
