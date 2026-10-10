use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::error::{malformed, DocError, Warning};
use crate::filter::{DecodeParams, Decoded, FilterKind, FilterStep};
use crate::limits::{check, LimitKind, Limits, Refusal};
use crate::object::{parse_indirect_pending, DictRef, Object, Value, ValueRef};
use crate::objstm::ObjectStream;
use crate::source::PdfSource;
use crate::work::ObjectRef;

pub use crate::xref::{Xref, XrefCounts, XrefEntry, XrefKind};

pub struct Document {
    source: PdfSource,
    xref: Xref,
    trailer: Object,
    limits: Limits,
    warnings: Vec<Warning>,
    encrypted: bool,
}

impl Document {
    pub fn open(source: PdfSource, limits: Limits) -> Result<Document, DocError> {
        let (xref, trailer, warnings) = crate::xref::read(&source, &limits)?;
        let encrypted = trailer.dict().and_then(|dict| dict.get(b"Encrypt"))
            .map(|value| !value.is_null()).unwrap_or(false);
        Ok(Self { source, xref, trailer, limits, warnings, encrypted })
    }
    pub fn source(&self) -> &PdfSource { &self.source }
    pub fn limits(&self) -> &Limits { &self.limits }
    pub fn xref(&self) -> &Xref { &self.xref }
    pub fn trailer(&self) -> &Object { &self.trailer }
    pub fn root_ref(&self) -> Option<ObjectRef> {
        self.trailer.dict().and_then(|dict| dict.get(b"Root")).and_then(|value| value.as_ref())
    }
    pub fn info_ref(&self) -> Option<ObjectRef> {
        self.trailer.dict().and_then(|dict| dict.get(b"Info")).and_then(|value| value.as_ref())
    }
    pub fn encrypted(&self) -> bool { self.encrypted }
    pub fn warnings(&self) -> &[Warning] { &self.warnings }
    pub fn resolver(&self) -> Resolver<'_> {
        Resolver { document: self, cache: Vec::new(), cache_bytes: 0, warnings: Vec::new(), nesting: 0 }
    }
}

struct CacheEntry {
    number: u32,
    stream: ObjectStream,
    cost: u64,
}

pub struct Resolver<'d> {
    document: &'d Document,
    cache: Vec<CacheEntry>,
    cache_bytes: u64,
    warnings: Vec<Warning>,
    /// Nested `get` calls in flight: an indirect /Length or an object stream's
    /// indirect filter parameters re-enter `get`; this bounds that recursion.
    nesting: u32,
}

enum GetTask {
    Object(ObjectRef),
    FinishObjectStream { id: ObjectRef, index: u32, number: u32 },
}

impl<'d> Resolver<'d> {
    pub fn document(&self) -> &'d Document { self.document }

    pub fn get(&mut self, id: ObjectRef) -> Result<Object, DocError> {
        if self.nesting >= self.document.limits.max_ref_chain {
            return Err(self.ref_cycle());
        }
        self.nesting += 1;
        let result = self.get_with_tasks(id);
        self.nesting -= 1;
        result
    }

    fn get_with_tasks(&mut self, id: ObjectRef) -> Result<Object, DocError> {
        let mut tasks = Vec::new();
        let mut objects = Vec::new();
        let mut active = Vec::new();
        tasks.push(GetTask::Object(id));
        while let Some(task) = tasks.pop() {
            match task {
                GetTask::Object(id) => {
                    if active.contains(&id.number()) {
                        return Err(self.ref_cycle());
                    }
                    check(LimitKind::RefChain, active.len() as u64, u64::from(self.document.limits.max_ref_chain))?;
                    active.push(id.number());
                    match self.document.xref.get(id.number()) {
                        Some(XrefEntry::InUse { offset, generation }) => {
                            let offset = usize::try_from(offset).map_err(|_| malformed(0, "object offset"))?;
                            let mut object = parse_indirect_pending(
                                &self.document.source.shared(), offset, id, &self.document.limits,
                            )?;
                            if object.id().map(|found| found.generation() != generation || found.generation() != id.generation()).unwrap_or(false) {
                                self.warn(Warning { offset: Some(offset as u64), object: Some(id), what: "generation_mismatch" });
                            }
                            if object.is_stream() && object.stream_raw().is_none() {
                                let length = object.dict().and_then(|dict| dict.get(b"Length")).and_then(|value| value.as_ref());
                                // An unresolvable or non-integer indirect /Length falls back
                                // to the bounded endstream scan inside finish_stream.
                                let resolved = match length {
                                    Some(length) => self
                                        .get(length)
                                        .ok()
                                        .and_then(|target| target.root().as_i64())
                                        .and_then(|n| u64::try_from(n).ok()),
                                    None => None,
                                };
                                object.finish_stream(resolved, &self.document.limits)?;
                            }
                            self.record_recovery(&object);
                            active.pop();
                            objects.push(object);
                        }
                        Some(XrefEntry::Compressed { stream, index }) => {
                            if self.document.encrypted { return Err(DocError::Encrypted); }
                            if id.generation() != 0 {
                                self.warn(Warning { offset: None, object: Some(id), what: "generation_mismatch" });
                            }
                            if let Some(cached) = self.cache.iter().find(|entry| entry.number == stream) {
                                let object = cached.stream.member(index, id, &self.document.limits)?;
                                active.pop();
                                objects.push(object);
                                continue;
                            }
                            let generation = match self.document.xref.get(stream) {
                                Some(XrefEntry::InUse { generation, .. }) => generation,
                                _ => return Err(malformed(0, "object stream xref")),
                            };
                            tasks.push(GetTask::FinishObjectStream { id, index, number: stream });
                            tasks.push(GetTask::Object(ObjectRef::new(stream, generation)));
                        }
                        _ => return Err(DocError::MissingObject { id }),
                    }
                }
                GetTask::FinishObjectStream { id, index, number } => {
                    let container = objects.pop().ok_or_else(|| malformed(0, "resolver state"))?;
                    let decoded = self.decode_stream(&container)?;
                    if decoded.image.is_some() || decoded.truncated {
                        return Err(malformed(0, "object stream decode"));
                    }
                    let stream = ObjectStream::parse(
                        &container, Arc::<[u8]>::from(decoded.bytes), &self.document.limits,
                    )?;
                    let object = stream.member(index, id, &self.document.limits)?;
                    self.cache_stream(number, stream);
                    active.pop();
                    objects.push(object);
                }
            }
        }
        objects.pop().ok_or_else(|| malformed(0, "resolver state"))
    }

    fn ref_cycle(&self) -> DocError {
        let maximum = u64::from(self.document.limits.max_ref_chain);
        DocError::Refused(Refusal { limit: LimitKind::RefChain, requested: maximum.saturating_add(1), maximum })
    }

    fn record_recovery(&mut self, object: &Object) {
        if object.stream_recovered() {
            self.warn(Warning {
                offset: object.stream_range().map(|range| range.start as u64),
                object: object.id(),
                what: "stream_length_recovered",
            });
        }
    }

    fn cache_stream(&mut self, number: u32, stream: ObjectStream) {
        let cost = stream.cost();
        let maximum = self.document.limits.max_objstm_cache_bytes;
        if cost > maximum { return; }
        if let Some(index) = self.cache.iter().position(|entry| entry.number == number) {
            let removed = self.cache.remove(index);
            self.cache_bytes = self.cache_bytes.saturating_sub(removed.cost);
        }
        while self.cache_bytes.saturating_add(cost) > maximum && !self.cache.is_empty() {
            let removed = self.cache.remove(0);
            self.cache_bytes = self.cache_bytes.saturating_sub(removed.cost);
        }
        self.cache_bytes = self.cache_bytes.saturating_add(cost);
        self.cache.push(CacheEntry { number, stream, cost });
    }

    pub fn resolve(&mut self, value: ValueRef<'_>) -> Result<Value, DocError> {
        let mut value = value.to_owned();
        let mut seen = Vec::new();
        while let Some(id) = value.get().as_ref() {
            if seen.contains(&id) { return Err(self.ref_cycle()); }
            check(LimitKind::RefChain, seen.len().saturating_add(1) as u64, u64::from(self.document.limits.max_ref_chain))?;
            seen.push(id);
            let object = self.get(id)?;
            value = object.root().to_owned();
        }
        Ok(value)
    }

    pub fn resolve_dict_key(&mut self, dict: DictRef<'_>, key: &[u8]) -> Result<Option<Value>, DocError> {
        match dict.get(key) {
            Some(value) => self.resolve(value).map(Some),
            None => Ok(None),
        }
    }

    pub fn decode_stream(&mut self, stream: &Object) -> Result<Decoded, DocError> {
        if self.document.encrypted { return Err(DocError::Encrypted); }
        let raw = stream.stream_raw().ok_or(DocError::WrongType { id: stream.id(), expected: "stream" })?;
        let steps = filter_steps(stream, |value| self.resolve(value))?;
        crate::filter::decode(raw, &steps, self.document.limits.max_decoded_bytes).map_err(DocError::from)
    }

    pub fn take_warnings(&mut self) -> Vec<Warning> { core::mem::take(&mut self.warnings) }

    pub fn warn(&mut self, warning: Warning) { self.warnings.push(warning); }
}

pub(crate) fn filter_steps<F>(
    stream: &Object,
    mut resolve: F,
) -> Result<Vec<FilterStep>, DocError>
where
    F: for<'o> FnMut(ValueRef<'o>) -> Result<Value, DocError>,
{
    let wrong = || DocError::WrongType { id: stream.id(), expected: "stream filters" };
    let dict = stream.dict().ok_or_else(wrong)?;
    let Some(filters) = dict.get(b"Filter") else { return Ok(Vec::new()) };
    let filters = resolve(filters)?;
    if filters.get().is_null() { return Ok(Vec::new()); }
    let values: Vec<Value> = if let Some(array) = filters.get().as_array() {
        array.iter().map(|value| value.to_owned()).collect()
    } else {
        alloc::vec![filters]
    };
    let params = match dict.get(b"DecodeParms") {
        Some(value) => Some(resolve(value)?),
        None => None,
    };
    let mut steps = Vec::new();
    for (index, value) in values.iter().enumerate() {
        let value = resolve(value.get())?;
        let name = value.get().as_name().ok_or_else(wrong)?;
        let kind = FilterKind::from_name(name.decoded().as_ref())
            .ok_or(DocError::Unsupported { feature: "filter:unknown" })?;
        let param = if let Some(params) = &params {
            if let Some(array) = params.get().as_array() {
                match array.get(index) {
                    Some(value) => Some(resolve(value)?),
                    None => None,
                }
            } else if index == 0 {
                Some(params.clone())
            } else {
                None
            }
        } else {
            None
        };
        let decoded_params = parameters(param.as_ref(), kind, &mut resolve, stream.id())?;
        steps.push(FilterStep { kind, params: decoded_params });
    }
    Ok(steps)
}

fn parameters<F>(
    value: Option<&Value>,
    kind: FilterKind,
    resolve: &mut F,
    id: Option<ObjectRef>,
) -> Result<DecodeParams, DocError>
where
    F: for<'o> FnMut(ValueRef<'o>) -> Result<Value, DocError>,
{
    let mut params = DecodeParams::default();
    let Some(value) = value else { return Ok(params) };
    if value.get().is_null() { return Ok(params); }
    let wrong = || DocError::WrongType { id, expected: "decode parameters" };
    let dict = value.get().as_dict().ok_or_else(wrong)?;
    if kind == FilterKind::Crypt {
        if let Some(name) = dict.get(b"Name") {
            let name = resolve(name)?;
            if !name.get().as_name().map(|name| name.eq_bytes(b"Identity")).unwrap_or(false) {
                return Err(DocError::Unsupported { feature: "filter:Crypt" });
            }
        }
        return Ok(params);
    }
    if let Some(n) = parameter(dict, b"Predictor", resolve, id)? {
        params.predictor = u8::try_from(n).map_err(|_| wrong())?;
    }
    if let Some(n) = parameter(dict, b"Colors", resolve, id)? {
        params.colors = u8::try_from(n).map_err(|_| wrong())?;
    }
    if let Some(n) = parameter(dict, b"BitsPerComponent", resolve, id)? {
        params.bits_per_component = u8::try_from(n).map_err(|_| wrong())?;
    }
    if let Some(n) = parameter(dict, b"Columns", resolve, id)? {
        params.columns = u32::try_from(n).map_err(|_| wrong())?;
    }
    if let Some(n) = parameter(dict, b"EarlyChange", resolve, id)? {
        if n != 0 && n != 1 { return Err(wrong()); }
        params.early_change = n == 1;
    }
    Ok(params)
}

fn parameter<F>(
    dict: DictRef<'_>,
    key: &[u8],
    resolve: &mut F,
    id: Option<ObjectRef>,
) -> Result<Option<i64>, DocError>
where
    F: for<'o> FnMut(ValueRef<'o>) -> Result<Value, DocError>,
{
    let Some(value) = dict.get(key) else { return Ok(None) };
    let value = resolve(value)?;
    if value.get().is_null() { return Ok(None); }
    value.get().as_i64().map(Some).ok_or(DocError::WrongType { id, expected: "integer decode parameter" })
}

