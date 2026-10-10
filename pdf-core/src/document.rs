use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::crypt::Crypt;
use crate::error::{malformed, DocError, Warning};
use crate::filter::{DecodeParams, Decoded, FilterKind, FilterStep};
use crate::limits::{check, LimitKind, Limits, Refusal};
use crate::object::{parse_indirect_pending, DictRef, Object, PdfStr, Value, ValueRef};
use crate::objstm::ObjectStream;
use crate::source::PdfSource;
use crate::work::ObjectRef;

pub use crate::xref::{Xref, XrefCounts, XrefEntry, XrefKind};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenMode {
    Strict,
    Repair,
}

pub struct Document {
    source: PdfSource,
    xref: Xref,
    trailer: Object,
    limits: Limits,
    warnings: Vec<Warning>,
    encrypted: bool,
    crypt: Option<Arc<Crypt>>,
    permissions: Option<i32>,
    repair_root: Option<ObjectRef>,
    repaired: bool,
}

impl Document {
    pub fn open(source: PdfSource, limits: Limits) -> Result<Document, DocError> {
        Self::open_with(source, limits, OpenMode::Strict)
    }

    pub fn open_with(
        source: PdfSource,
        limits: Limits,
        mode: OpenMode,
    ) -> Result<Document, DocError> {
        let mut document = match crate::xref::read(&source, &limits) {
            Ok((xref, trailer, warnings)) => {
                Ok(Self::from_parts(source, xref, trailer, limits, warnings, None, false))
            }
            Err(DocError::Malformed { .. }) if mode == OpenMode::Repair => {
                crate::repair::rebuild(source, limits)
            }
            Err(error) => Err(error),
        }?;
        document.initialize_crypt()?;
        Ok(document)
    }

    fn initialize_crypt(&mut self) -> Result<(), DocError> {
        let Some(value) = self.trailer.dict().and_then(|dict| dict.get(b"Encrypt"))
            .filter(|value| !value.is_null()).map(|value| value.to_owned()) else {
            return Ok(());
        };
        let mut permissions = None;
        let (result, warnings) = {
            let mut resolver = self.resolver();
            let result = (|| {
                let value = resolver.resolve(value.get())?;
                if let Some(dict) = value.get().as_dict() {
                    permissions = resolver.resolve_dict_key(dict, b"P")?
                        .and_then(|value| value.get().as_i64())
                        .and_then(|number| i32::try_from(number).ok());
                }
                Crypt::open(&mut resolver, &value)
            })();
            (result, resolver.take_warnings())
        };
        self.permissions = permissions;
        self.warnings.extend(warnings);
        match result {
            Ok(crypt) => self.crypt = crypt.map(Arc::new),
            Err(error @ DocError::Refused(_)) => return Err(error),
            Err(_) => self.warnings.push(Warning {
                offset: None,
                object: value.get().as_ref(),
                what: "crypt_unavailable",
            }),
        }
        Ok(())
    }

    pub(crate) fn from_parts(
        source: PdfSource,
        xref: Xref,
        trailer: Object,
        limits: Limits,
        warnings: Vec<Warning>,
        repair_root: Option<ObjectRef>,
        repaired: bool,
    ) -> Document {
        let encrypted = trailer
            .dict()
            .and_then(|dict| dict.get(b"Encrypt"))
            .map(|value| !value.is_null())
            .unwrap_or(false);
        Self {
            source,
            xref,
            trailer,
            limits,
            warnings,
            encrypted,
            crypt: None,
            permissions: None,
            repair_root,
            repaired,
        }
    }

    pub fn source(&self) -> &PdfSource {
        &self.source
    }
    pub fn limits(&self) -> &Limits {
        &self.limits
    }
    pub fn xref(&self) -> &Xref {
        &self.xref
    }
    pub fn trailer(&self) -> &Object {
        &self.trailer
    }
    pub fn root_ref(&self) -> Option<ObjectRef> {
        self.trailer
            .dict()
            .and_then(|dict| dict.get(b"Root"))
            .and_then(|value| value.as_ref())
            .or(self.repair_root)
    }
    pub fn info_ref(&self) -> Option<ObjectRef> {
        self.trailer
            .dict()
            .and_then(|dict| dict.get(b"Info"))
            .and_then(|value| value.as_ref())
    }
    pub fn encrypted(&self) -> bool {
        self.encrypted
    }
    /// Whether stream and string decoding can read this document.
    pub fn decryptable(&self) -> bool {
        !self.encrypted || self.crypt.is_some()
    }
    pub fn permissions(&self) -> Option<i32> {
        self.permissions
    }
    pub(crate) fn crypt(&self) -> Option<Arc<Crypt>> {
        self.crypt.clone()
    }
    pub fn repaired(&self) -> bool {
        self.repaired
    }
    pub fn repaired_counts(&self) -> Option<XrefCounts> {
        if self.repaired {
            Some(self.xref.counts())
        } else {
            None
        }
    }
    pub fn warnings(&self) -> &[Warning] {
        &self.warnings
    }
    pub fn resolver(&self) -> Resolver<'_> {
        Resolver {
            document: self,
            cache: Vec::new(),
            cache_bytes: 0,
            warnings: Vec::new(),
            nesting: 0,
        }
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
    pub fn document(&self) -> &'d Document {
        self.document
    }

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
                    check(
                        LimitKind::RefChain,
                        active.len() as u64,
                        u64::from(self.document.limits.max_ref_chain),
                    )?;
                    active.push(id.number());
                    match self.document.xref.get(id.number()) {
                        Some(XrefEntry::InUse { offset, generation }) => {
                            let offset =
                                usize::try_from(offset).map_err(|_| malformed(0, "object offset"))?;
                            let mut object = parse_indirect_pending(
                                &self.document.source.shared(),
                                offset,
                                id,
                                &self.document.limits,
                            )?;
                            if object
                                .id()
                                .map(|found| {
                                    found.generation() != generation
                                        || found.generation() != id.generation()
                                })
                                .unwrap_or(false)
                            {
                                self.warn(Warning {
                                    offset: Some(offset as u64),
                                    object: Some(id),
                                    what: "generation_mismatch",
                                });
                            }
                            if object.is_stream() && object.stream_raw().is_none() {
                                let length = object
                                    .dict()
                                    .and_then(|dict| dict.get(b"Length"))
                                    .and_then(|value| value.as_ref());
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
                            if !self.document.decryptable() {
                                return Err(DocError::Encrypted);
                            }
                            if id.generation() != 0 {
                                self.warn(Warning {
                                    offset: None,
                                    object: Some(id),
                                    what: "generation_mismatch",
                                });
                            }
                            if let Some(cached) =
                                self.cache.iter().find(|entry| entry.number == stream)
                            {
                                let object =
                                    cached.stream.member(index, id, &self.document.limits)?;
                                active.pop();
                                objects.push(object);
                                continue;
                            }
                            let generation = match self.document.xref.get(stream) {
                                Some(XrefEntry::InUse { generation, .. }) => generation,
                                _ => return Err(malformed(0, "object stream xref")),
                            };
                            tasks.push(GetTask::FinishObjectStream {
                                id,
                                index,
                                number: stream,
                            });
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
                        &container,
                        Arc::<[u8]>::from(decoded.bytes),
                        &self.document.limits,
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
        DocError::Refused(Refusal {
            limit: LimitKind::RefChain,
            requested: maximum.saturating_add(1),
            maximum,
        })
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
        if cost > maximum {
            return;
        }
        if let Some(index) = self.cache.iter().position(|entry| entry.number == number) {
            let removed = self.cache.remove(index);
            self.cache_bytes = self.cache_bytes.saturating_sub(removed.cost);
        }
        while self.cache_bytes.saturating_add(cost) > maximum && !self.cache.is_empty() {
            let removed = self.cache.remove(0);
            self.cache_bytes = self.cache_bytes.saturating_sub(removed.cost);
        }
        self.cache_bytes = self.cache_bytes.saturating_add(cost);
        self.cache.push(CacheEntry {
            number,
            stream,
            cost,
        });
    }

    pub fn resolve(&mut self, value: ValueRef<'_>) -> Result<Value, DocError> {
        let mut value = value.to_owned();
        let mut seen = Vec::new();
        while let Some(id) = value.get().as_ref() {
            if seen.contains(&id) {
                return Err(self.ref_cycle());
            }
            check(
                LimitKind::RefChain,
                seen.len().saturating_add(1) as u64,
                u64::from(self.document.limits.max_ref_chain),
            )?;
            seen.push(id);
            let object = self.get(id)?;
            value = object.root().to_owned();
        }
        Ok(value)
    }

    pub fn resolve_dict_key(
        &mut self,
        dict: DictRef<'_>,
        key: &[u8],
    ) -> Result<Option<Value>, DocError> {
        match dict.get(key) {
            Some(value) => self.resolve(value).map(Some),
            None => Ok(None),
        }
    }

    pub fn decode_stream(&mut self, stream: &Object) -> Result<Decoded, DocError> {
        if !self.document.decryptable() {
            return Err(DocError::Encrypted);
        }
        let raw = stream
            .stream_raw()
            .ok_or(DocError::WrongType {
                id: stream.id(),
                expected: "stream",
            })?;
        let steps = filter_steps_for(stream, |value| self.resolve(value), true)?;
        crate::crypt::decode_stream(self, stream, raw, &steps)
    }

    /// Decode an object-level string. Content and object-stream strings carry
    /// plaintext provenance, so they are never decrypted a second time.
    /// This Vec-returning API records failures as warnings and returns no text.
    pub fn decode_string(&mut self, owner: ObjectRef, s: PdfStr<'_>) -> Vec<u8> {
        let id = s.owner().unwrap_or(owner);
        if !self.document.decryptable() {
            self.warn(Warning { offset: None, object: Some(id), what: "encrypted" });
            return Vec::new();
        }
        s.decode_with(|bytes| {
            let Some(crypt) = self.document.crypt() else { return bytes };
            if s.plaintext() || crypt.excluded(Some(id)) {
                return bytes;
            }
            match crypt.decrypt_owned(id, crypt.strings(), bytes, self.document.limits.max_decoded_bytes) {
                Ok((bytes, padding)) => {
                    if padding {
                        self.warn(Warning { offset: None, object: Some(id), what: "crypt_padding" });
                    }
                    bytes
                }
                Err(error) => {
                    self.warn(Warning { offset: None, object: Some(id), what: error.code() });
                    Vec::new()
                }
            }
        })
    }

    pub fn take_warnings(&mut self) -> Vec<Warning> {
        core::mem::take(&mut self.warnings)
    }

    pub fn warn(&mut self, warning: Warning) {
        self.warnings.push(warning);
    }
}

pub(crate) fn filter_steps<F>(
    stream: &Object,
    resolve: F,
) -> Result<Vec<FilterStep>, DocError>
where
    F: for<'o> FnMut(ValueRef<'o>) -> Result<Value, DocError>,
{
    filter_steps_for(stream, resolve, false)
}

fn filter_steps_for<F>(
    stream: &Object,
    mut resolve: F,
    allow_crypt: bool,
) -> Result<Vec<FilterStep>, DocError>
where
    F: for<'o> FnMut(ValueRef<'o>) -> Result<Value, DocError>,
{
    let wrong = || DocError::WrongType {
        id: stream.id(),
        expected: "stream filters",
    };
    let dict = stream.dict().ok_or_else(wrong)?;
    let Some(filters) = dict.get(b"Filter") else {
        return Ok(Vec::new());
    };
    let filters = resolve(filters)?;
    if filters.get().is_null() {
        return Ok(Vec::new());
    }
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
            .ok_or(DocError::Unsupported {
                feature: "filter:unknown",
            })?;
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
        let decoded_params = parameters(param.as_ref(), kind, &mut resolve, stream.id(), allow_crypt)?;
        steps.push(FilterStep {
            kind,
            params: decoded_params,
        });
    }
    Ok(steps)
}

fn parameters<F>(
    value: Option<&Value>,
    kind: FilterKind,
    resolve: &mut F,
    id: Option<ObjectRef>,
    allow_crypt: bool,
) -> Result<DecodeParams, DocError>
where
    F: for<'o> FnMut(ValueRef<'o>) -> Result<Value, DocError>,
{
    let mut params = DecodeParams::default();
    let Some(value) = value else {
        return Ok(params);
    };
    if value.get().is_null() {
        return Ok(params);
    }
    let wrong = || DocError::WrongType {
        id,
        expected: "decode parameters",
    };
    let dict = value.get().as_dict().ok_or_else(wrong)?;
    if kind == FilterKind::Crypt {
        if allow_crypt {
            return Ok(params);
        }
        if let Some(name) = dict.get(b"Name") {
            let name = resolve(name)?;
            if !name
                .get()
                .as_name()
                .map(|name| name.eq_bytes(b"Identity"))
                .unwrap_or(false)
            {
                return Err(DocError::Unsupported {
                    feature: "filter:Crypt",
                });
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
        if n != 0 && n != 1 {
            return Err(wrong());
        }
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
    let Some(value) = dict.get(key) else {
        return Ok(None);
    };
    let value = resolve(value)?;
    if value.get().is_null() {
        return Ok(None);
    }
    value
        .get()
        .as_i64()
        .map(Some)
        .ok_or(DocError::WrongType {
            id,
            expected: "integer decode parameter",
        })
}

