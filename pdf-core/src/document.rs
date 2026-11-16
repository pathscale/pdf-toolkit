use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::crypt::Crypt;
use crate::error::{malformed, DocError, Warning};
use crate::filter::{DecodeParams, Decoded, FilterKind, FilterStep};
use crate::limits::{check, LimitKind, Limits, Refusal};
use crate::object::{
    parse_indirect_pending, parse_indirect_pending_repair, DictRef, Object, ParseScratch, PdfStr,
    Value, ValueRef,
};
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
    private_crypt: Option<Arc<crate::encryption::PrivateCrypt>>,
    permissions: Option<i32>,
    repair_root: Option<ObjectRef>,
    repaired: bool,
    repair_mode: bool,
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
        Self::open_with_password_mode(source, limits, mode, None)
    }

    pub fn open_with_password(
        source: PdfSource,
        limits: Limits,
        password: &[u8],
    ) -> Result<Document, DocError> {
        Self::open_with_password_mode(source, limits, OpenMode::Strict, Some(password))
    }

    /// None means no password; Some(b"") explicitly supplies the empty password.
    pub fn open_with_password_mode(
        source: PdfSource,
        limits: Limits,
        mode: OpenMode,
        password: Option<&[u8]>,
    ) -> Result<Document, DocError> {
        let header_recovered = source.header().recovered();
        if header_recovered && mode != OpenMode::Repair {
            return Err(malformed(source.header().offset(), "header"));
        }
        let mut document = match crate::xref::read(&source, &limits) {
            Ok((xref, trailer, warnings)) => Ok(Self::from_parts(
                source, xref, trailer, limits, warnings, None, false,
            )),
            Err(error) if mode == OpenMode::Repair && !crate::repair::is_hard_error(&error) => {
                match crate::repair::rebuild(source, limits) {
                    Ok(document) => {
                        if matches!(error, DocError::Filter(_) | DocError::Unsupported { .. })
                            && crate::repair::needs_rebuild(&document)?
                        {
                            Err(error)
                        } else {
                            Ok(document)
                        }
                    }
                    Err(repair_error) if crate::repair::is_hard_error(&repair_error) => {
                        Err(repair_error)
                    }
                    // Keep an accurate codec or unsupported-feature refusal when
                    // scanning could not recover a usable document.
                    Err(_)
                        if matches!(error, DocError::Filter(_) | DocError::Unsupported { .. }) =>
                    {
                        Err(error)
                    }
                    Err(repair_error) => Err(repair_error),
                }
            }
            Err(error) => Err(error),
        }?;
        document.repair_mode = mode == OpenMode::Repair;
        if mode == OpenMode::Repair
            && !document.repaired
            && crate::repair::needs_rebuild(&document)?
        {
            match crate::repair::rebuild(document.source.clone(), limits) {
                Ok(rebuilt) => document = rebuilt,
                Err(error) if crate::repair::is_hard_error(&error) => return Err(error),
                Err(_) => document.warnings.push(Warning {
                    offset: None,
                    object: None,
                    what: "repair_incomplete",
                }),
            }
        }
        if header_recovered {
            document.warnings.push(Warning {
                offset: Some(document.source.header().offset() as u64),
                object: None,
                what: "header_recovered",
            });
        }
        document.initialize_crypt(password)?;
        Ok(document)
    }

    fn initialize_crypt(&mut self, password: Option<&[u8]>) -> Result<(), DocError> {
        let Some(value) = self
            .trailer
            .dict()
            .and_then(|dict| dict.get(b"Encrypt"))
            .filter(|value| !value.is_null())
            .map(|value| value.to_owned())
        else {
            if password.is_some() {
                return Err(malformed(0, "password supplied for unencrypted document"));
            }
            return Ok(());
        };
        let mut permissions = None;
        let mut private_handler = false;
        let mut private_crypt = None;
        let (result, warnings) = {
            let mut resolver = self.resolver();
            let result = (|| {
                let value = resolver.resolve(value.get())?;
                if crate::encryption::PrivateCrypt::recognizes(&value) {
                    private_handler = true;
                    private_crypt = Some(Arc::new(crate::encryption::PrivateCrypt::open(
                        &value,
                        password,
                        resolver.document().limits(),
                    )?));
                    return Ok(None);
                }
                if let Some(dict) = value.get().as_dict() {
                    permissions = resolver
                        .resolve_dict_key(dict, b"P")?
                        .and_then(|value| value.get().as_i64())
                        .and_then(|number| i32::try_from(number).ok());
                }
                if password.is_some_and(|password| !password.is_empty()) {
                    return Err(DocError::Encrypted);
                }
                Crypt::open(&mut resolver, &value)
            })();
            (result, resolver.take_warnings())
        };
        self.permissions = permissions;
        self.warnings.extend(warnings);
        if private_handler {
            result?;
            self.private_crypt = private_crypt;
            return self.verify_private_objects();
        }
        match result {
            Ok(Some(crypt)) => self.crypt = Some(Arc::new(crypt)),
            Ok(None) => self.warnings.push(Warning {
                offset: None,
                object: value.get().as_ref(),
                what: "crypt_auth_failed",
            }),
            Err(error @ (DocError::Encrypted | DocError::Refused(_))) => return Err(error),
            Err(error @ DocError::Unsupported { .. }) => return Err(error),
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
            private_crypt: None,
            permissions: None,
            repair_root,
            repaired,
            repair_mode: repaired,
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
        self.repair_root.or_else(|| {
            self.trailer
                .dict()
                .and_then(|dict| dict.get(b"Root"))
                .and_then(|value| value.as_ref())
        })
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
        !self.encrypted || self.crypt.is_some() || self.private_crypt.is_some()
    }
    /// Opaque authenticated file key for incremental PSChaCha writes.
    pub fn chacha_key(&self) -> Option<Arc<crate::encryption::ChaChaKey>> {
        self.private_crypt.as_ref().map(|crypt| crypt.key.clone())
    }

    pub fn encryption_ref(&self) -> Option<ObjectRef> {
        self.trailer
            .dict()
            .and_then(|dict| dict.get(b"Encrypt"))
            .and_then(|value| value.as_ref())
    }

    fn verify_private_objects(&self) -> Result<(), DocError> {
        let Some(crypt) = &self.private_crypt else {
            return Ok(());
        };
        let mut resolver = self.resolver();
        let mut total = 0u64;
        for number in 1..self.xref.size() {
            let Some(XrefEntry::InUse { generation, .. }) = self.xref.get(number) else {
                continue;
            };
            let object = resolver.get(ObjectRef::new(number, generation))?;
            if crypt.excluded(&object) {
                continue;
            }
            total = total.saturating_add(object.decrypted_string_bytes());
            if let Some(raw) = object.stream_raw() {
                let id = object
                    .id()
                    .ok_or_else(|| malformed(0, "PSChaCha object ID"))?;
                total = total.saturating_add(
                    crypt
                        .key
                        .open(id, raw, self.limits.max_decoded_bytes)?
                        .len() as u64,
                );
            }
            check(
                LimitKind::TotalDecodedBytes,
                total,
                self.limits.max_total_decoded_bytes,
            )?;
        }
        Ok(())
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
    /// Whether callers may use bounded recovery for damaged document data.
    pub fn repair_mode(&self) -> bool {
        self.repair_mode
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
            decoded_bytes: 0,
            decode_work_bytes: 0,
            warnings: Vec::new(),
            nesting: 0,
            parse_scratch: ParseScratch::default(),
            tasks: Vec::new(),
            objects: Vec::new(),
            active: Vec::new(),
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
    decoded_bytes: u64,
    decode_work_bytes: u64,
    warnings: Vec<Warning>,
    /// Nested `get` calls in flight: an indirect /Length or an object stream's
    /// indirect filter parameters re-enter `get`; this bounds that recursion.
    nesting: u32,
    parse_scratch: ParseScratch,
    tasks: Vec<GetTask>,
    objects: Vec<Object>,
    active: Vec<u32>,
}

enum GetTask {
    Object(ObjectRef),
    FinishObjectStream {
        id: ObjectRef,
        index: u32,
        number: u32,
    },
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
        if self.nesting == 1 {
            let mut tasks = core::mem::take(&mut self.tasks);
            let mut objects = core::mem::take(&mut self.objects);
            let mut active = core::mem::take(&mut self.active);
            tasks.clear();
            objects.clear();
            active.clear();
            let result = self.get_with_buffers(id, &mut tasks, &mut objects, &mut active);
            tasks.clear();
            objects.clear();
            active.clear();
            self.tasks = tasks;
            self.objects = objects;
            self.active = active;
            result
        } else {
            let mut tasks = Vec::new();
            let mut objects = Vec::new();
            let mut active = Vec::new();
            self.get_with_buffers(id, &mut tasks, &mut objects, &mut active)
        }
    }

    fn get_with_buffers(
        &mut self,
        id: ObjectRef,
        tasks: &mut Vec<GetTask>,
        objects: &mut Vec<Object>,
        active: &mut Vec<u32>,
    ) -> Result<Object, DocError> {
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
                            let offset = usize::try_from(offset)
                                .map_err(|_| malformed(0, "object offset"))?;
                            let parse = if self.document.repair_mode {
                                parse_indirect_pending_repair
                            } else {
                                parse_indirect_pending
                            };
                            let mut object = parse(
                                &self.document.source.shared(),
                                offset,
                                id,
                                &self.document.limits,
                                &mut self.parse_scratch,
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
                                    Some(length) => match self.get(length) {
                                        Ok(target) => target
                                            .root()
                                            .as_i64()
                                            .and_then(|n| u64::try_from(n).ok()),
                                        Err(error @ DocError::Refused(Refusal {
                                            limit: LimitKind::TotalDecodedBytes
                                                | LimitKind::DecodeWorkBytes,
                                            ..
                                        })) => return Err(error),
                                        Err(_) => None,
                                    },
                                    None => None,
                                };
                                if self.document.repair_mode {
                                    object.finish_stream_repair(resolved, &self.document.limits)?;
                                } else {
                                    object.finish_stream(resolved, &self.document.limits)?;
                                }
                            }
                            if self.document.xref.parsed_stream(&object) {
                                object.mark_xref_stream();
                            }
                            if let Some(crypt) = &self.document.private_crypt {
                                crypt.prepare(&mut object, id, &self.document.limits)?;
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
                                let object = cached.stream.member(
                                    index,
                                    id,
                                    &self.document.limits,
                                    &mut self.parse_scratch,
                                )?;
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
                    let container = objects
                        .pop()
                        .ok_or_else(|| malformed(0, "resolver state"))?;
                    let limits = self.document.limits;
                    let decoded = self.decode_stream_accounted(&container, None, true)?;
                    check(
                        LimitKind::DecodedBytes,
                        decoded.bytes.len() as u64,
                        limits.max_objstm_cache_bytes,
                    )?;
                    if decoded.image.is_some() || (decoded.truncated && !self.document.repair_mode)
                    {
                        return Err(malformed(0, "object stream decode"));
                    }
                    let truncated = decoded.truncated;
                    let buf = Arc::<[u8]>::from(decoded.bytes);
                    let stream = if self.document.repair_mode {
                        let (stream, recovered) =
                            ObjectStream::parse_repair(&container, buf, &self.document.limits)?;
                        if recovered || truncated {
                            self.warn(Warning {
                                offset: container.stream_range().map(|range| range.start as u64),
                                object: container.id(),
                                what: "object_stream_recovered",
                            });
                        }
                        stream
                    } else {
                        ObjectStream::parse(&container, buf, &self.document.limits)?
                    };
                    let object =
                        stream.member(index, id, &self.document.limits, &mut self.parse_scratch)?;
                    self.cache_stream(number, stream)?;
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
        if object.syntax_recovered() {
            self.warn(Warning {
                offset: Some(object.root().span().start as u64),
                object: object.id(),
                what: "object_syntax_recovered",
            });
        }
        if object.tail_recovered() {
            self.warn(Warning {
                offset: Some(object.root().span().end as u64),
                object: object.id(),
                what: "object_tail_recovered",
            });
        }
        if object.stream_recovered() {
            self.warn(Warning {
                offset: object.stream_range().map(|range| range.start as u64),
                object: object.id(),
                what: "stream_length_recovered",
            });
        }
    }

    fn cache_stream(&mut self, number: u32, stream: ObjectStream) -> Result<(), DocError> {
        let cost = stream.cost();
        let maximum = self.document.limits.max_objstm_cache_bytes;
        check(LimitKind::DecodedBytes, cost, maximum)?;
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
        Ok(())
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

    /// Decode a stream while charging cumulative output and byte-work limits to
    /// this resolver. Reuse one resolver when decoding related streams under a
    /// shared aggregate budget.
    pub fn decode_stream(&mut self, stream: &Object) -> Result<Decoded, DocError> {
        self.decode_stream_accounted(stream, None, false)
    }

    /// Decode a stream under this resolver's cumulative output and byte-work
    /// limits and an additional caller-owned cumulative output budget.
    /// The caller must pass `used <= maximum` and charge its own output after
    /// success; this method only enforces the remaining caller ceiling.
    pub fn decode_stream_with_total_limit(
        &mut self,
        stream: &Object,
        used: u64,
        maximum: u64,
    ) -> Result<Decoded, DocError> {
        debug_assert!(used <= maximum);
        self.decode_stream_accounted(stream, Some((used, maximum)), false)
    }

    fn decode_stream_accounted(
        &mut self,
        stream: &Object,
        caller: Option<(u64, u64)>,
        object_stream: bool,
    ) -> Result<Decoded, DocError> {
        if !self.document.decryptable() {
            return Err(DocError::Encrypted);
        }
        let limits = self.document.limits;
        let raw = stream.stream_raw().ok_or(DocError::WrongType {
            id: stream.id(),
            expected: "stream",
        })?;
        let input_bytes = u64::try_from(raw.len()).unwrap_or(u64::MAX).max(1);
        self.charge_decode_work(input_bytes)?;

        let steps = filter_steps_for(stream, |value| self.resolve(value), true)?;
        if let Some(crypt) = &self.document.private_crypt {
            crypt.check_xref_claim(stream)?;
            if !crypt.excluded(stream) && steps.iter().any(|step| step.kind == FilterKind::Crypt) {
                return Err(DocError::Unsupported { feature: "PSChaCha Crypt filter" });
            }
        }
        let names = crate::crypt::prepare_crypt_names(self, stream, &steps)?;
        let caller_remaining = caller.map(|(used, maximum)| maximum.saturating_sub(used));
        let local_maximum = limits.max_decoded_bytes.min(if object_stream {
            limits.max_objstm_cache_bytes
        } else {
            u64::MAX
        });
        let total_before = self.decoded_bytes;
        let work_before = self.decode_work_bytes;
        let mut maximum = local_maximum
            .min(limits.max_total_decoded_bytes.saturating_sub(total_before))
            .min(limits.max_total_decode_work_bytes.saturating_sub(work_before));
        if let Some(remaining) = caller_remaining {
            maximum = maximum.min(remaining);
        }
        let private = self.document.private_crypt.as_ref().filter(|crypt| !crypt.excluded(stream));
        let decoded = if let Some(crypt) = private {
            let id = stream.id().ok_or_else(|| malformed(0, "PSChaCha object ID"))?;
            let plain = crypt.key.open(id, raw, maximum).map_err(|error| {
                match &error {
                    DocError::Refused(refusal) if refusal.limit == LimitKind::DecodedBytes => {
                        crate::crypt::StreamDecodeError::Ceiling(error)
                    }
                    _ => crate::crypt::StreamDecodeError::Other(error),
                }
            });
            match plain {
                Ok(plain) => crate::crypt::decode_stream_prepared(
                    self, stream, &plain, &steps, maximum, &names,
                ),
                Err(error) => Err(error),
            }
        } else {
            crate::crypt::decode_stream_prepared(self, stream, raw, &steps, maximum, &names)
        };
        let decoded = match decoded {
            Ok(decoded) => decoded,
            Err(crate::crypt::StreamDecodeError::Other(error)) => return Err(error),
            Err(crate::crypt::StreamDecodeError::Ceiling(error)) => {
                self.decode_work_bytes = self.decode_work_bytes.checked_add(maximum).unwrap_or(u64::MAX);
                if let Some(refusal) = binding_decode_refusal(
                    caller,
                    total_before,
                    work_before,
                    limits,
                    local_maximum,
                ) {
                    return Err(DocError::Refused(refusal));
                }
                if object_stream
                    && matches!(
                        &error,
                        &DocError::Filter(crate::filter::FilterError::OutputLimit { .. })
                    )
                {
                    return Err(DocError::Refused(Refusal {
                        limit: LimitKind::DecodedBytes,
                        requested: local_maximum.saturating_add(1),
                        maximum: local_maximum,
                    }));
                }
                return Err(error);
            }
        };
        let output_bytes = u64::try_from(decoded.bytes.len()).unwrap_or(u64::MAX);
        self.charge_total_decoded(output_bytes)?;
        self.charge_decode_work(output_bytes)?;
        Ok(decoded)
    }

    fn charge_total_decoded(&mut self, bytes: u64) -> Result<(), DocError> {
        let requested = self.decoded_bytes.checked_add(bytes).unwrap_or(u64::MAX);
        check(
            LimitKind::TotalDecodedBytes,
            requested,
            self.document.limits.max_total_decoded_bytes,
        )?;
        self.decoded_bytes = requested;
        Ok(())
    }

    fn charge_decode_work(&mut self, bytes: u64) -> Result<(), DocError> {
        let requested = self
            .decode_work_bytes
            .checked_add(bytes)
            .unwrap_or(u64::MAX);
        check(
            LimitKind::DecodeWorkBytes,
            requested,
            self.document.limits.max_total_decode_work_bytes,
        )?;
        self.decode_work_bytes = requested;
        Ok(())
    }

    /// Decode an object-level string. Content and object-stream strings carry
    /// plaintext provenance, so they are never decrypted a second time.
    /// This Vec-returning API records failures as warnings and returns no text.
    pub fn decode_string(&mut self, owner: ObjectRef, s: PdfStr<'_>) -> Vec<u8> {
        let id = s.owner().unwrap_or(owner);
        if !self.document.decryptable() {
            self.warn(Warning {
                offset: None,
                object: Some(id),
                what: "encrypted",
            });
            return Vec::new();
        }
        s.decode_with(|bytes| {
            let Some(crypt) = self.document.crypt() else {
                return bytes;
            };
            if s.plaintext() || crypt.excluded(Some(id)) {
                return bytes;
            }
            match crypt.decrypt_owned(
                id,
                crypt.strings(),
                bytes,
                self.document.limits.max_decoded_bytes,
            ) {
                Ok((bytes, padding)) => {
                    if padding {
                        self.warn(Warning {
                            offset: None,
                            object: Some(id),
                            what: "crypt_padding",
                        });
                    }
                    bytes
                }
                Err(error) => {
                    self.warn(Warning {
                        offset: None,
                        object: Some(id),
                        what: error.code(),
                    });
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

fn binding_decode_refusal(
    caller: Option<(u64, u64)>,
    total_used: u64,
    work_used: u64,
    limits: Limits,
    local_maximum: u64,
) -> Option<Refusal> {
    let total_remaining = limits.max_total_decoded_bytes.saturating_sub(total_used);
    let work_remaining = limits.max_total_decode_work_bytes.saturating_sub(work_used);
    let caller_remaining = caller.map(|(used, maximum)| maximum.saturating_sub(used));
    let effective = caller_remaining
        .into_iter()
        .chain([total_remaining, work_remaining])
        .min()?;
    if effective > local_maximum {
        return None;
    }
    let (limit, used, maximum) = if caller_remaining == Some(effective) {
        let (used, maximum) = caller?;
        (LimitKind::TotalDecodedBytes, used, maximum)
    } else if total_remaining == effective {
        (
            LimitKind::TotalDecodedBytes,
            total_used,
            limits.max_total_decoded_bytes,
        )
    } else if work_remaining == effective {
        (
            LimitKind::DecodeWorkBytes,
            work_used,
            limits.max_total_decode_work_bytes,
        )
    } else {
        return None;
    };
    let requested = used
        .checked_add(maximum.saturating_sub(used).saturating_add(1))
        .unwrap_or(u64::MAX);
    Some(Refusal {
        limit,
        requested,
        maximum,
    })
}

#[cfg(test)]
mod resolver_accounting_tests {
    use super::*;

    fn document_with_raw_stream(payload: &[u8], limits: Limits) -> Document {
        document_with_stream_dictionary(payload, "", limits)
    }

    fn document_with_stream_dictionary(
        payload: &[u8],
        entries: &str,
        limits: Limits,
    ) -> Document {
        let mut bytes = alloc::vec::Vec::from(&b"%PDF-1.7\n"[..]);
        let object_offset = bytes.len();
        bytes.extend_from_slice(
            alloc::format!("1 0 obj\n<< /Length {} {} >>\nstream\n", payload.len(), entries).as_bytes(),
        );
        bytes.extend_from_slice(payload);
        bytes.extend_from_slice(b"\nendstream\nendobj\n");
        let xref_offset = bytes.len();
        bytes.extend_from_slice(
            alloc::format!(
                "xref\n0 2\n0000000000 65535 f \n{:010} 00000 n \ntrailer\n<< /Size 2 /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
                object_offset, xref_offset
            )
            .as_bytes(),
        );
        Document::open(PdfSource::new(bytes).unwrap(), limits).unwrap()
    }

    fn stream(document: &Document) -> Object {
        document.resolver().get(ObjectRef::new(1, 0)).unwrap()
    }

    fn zlib_stored(payload: &[u8]) -> alloc::vec::Vec<u8> {
        let len = u16::try_from(payload.len()).unwrap();
        let mut encoded = alloc::vec![0x78, 0x01, 0x01];
        encoded.extend_from_slice(&len.to_le_bytes());
        encoded.extend_from_slice(&(!len).to_le_bytes());
        encoded.extend_from_slice(payload);
        let mut a = 1u32;
        let mut b = 0u32;
        for byte in payload {
            a = (a + u32::from(*byte)) % 65_521;
            b = (b + a) % 65_521;
        }
        encoded.extend_from_slice(&((b << 16) | a).to_be_bytes());
        encoded
    }

    fn refusal(error: DocError, expected: LimitKind, requested: u64, maximum: u64) {
        assert!(matches!(
            error,
            DocError::Refused(Refusal { limit, requested: got, maximum: cap })
                if limit == expected && got == requested && cap == maximum
        ));
    }

    #[test]
    fn terminal_image_does_not_resolve_later_crypt_name() {
        let document = document_with_stream_dictionary(
            b"image bytes",
            "/Filter [/DCTDecode /Crypt] /DecodeParms [null << /Name 999 0 R >>]",
            Limits::default(),
        );
        let object = stream(&document);
        let decoded = document.resolver().decode_stream(&object).unwrap();
        assert_eq!(decoded.image, Some(FilterKind::Dct));
        assert_eq!(decoded.bytes, b"image bytes");
    }

    #[test]
    fn object_and_general_raw_decode_share_totals_in_both_orders() {
        let limits = Limits {
            max_decoded_bytes: 64,
            max_objstm_cache_bytes: 64,
            max_total_decoded_bytes: 20,
            max_total_decode_work_bytes: 128,
            ..Limits::default()
        };
        for first_is_object_stream in [false, true] {
            let document = document_with_raw_stream(b"0123456789", limits);
            let object = stream(&document);
            let mut resolver = document.resolver();
            let first = resolver
                .decode_stream_accounted(&object, None, first_is_object_stream)
                .unwrap();
            assert_eq!(first.bytes, b"0123456789");
            let second = resolver
                .decode_stream_accounted(&object, None, !first_is_object_stream)
                .unwrap();
            assert_eq!(second.bytes, b"0123456789");
            refusal(
                resolver
                    .decode_stream_accounted(&object, None, first_is_object_stream)
                    .unwrap_err(),
                LimitKind::TotalDecodedBytes,
                21,
                20,
            );
        }

        let exact = document_with_raw_stream(
            b"0123456789",
            Limits {
                max_decoded_bytes: 64,
                max_objstm_cache_bytes: 64,
                max_total_decoded_bytes: 20,
                max_total_decode_work_bytes: 128,
                ..Limits::default()
            },
        );
        let object = stream(&exact);
        let mut resolver = exact.resolver();
        resolver.decode_stream_accounted(&object, None, true).unwrap();
        resolver.decode_stream_accounted(&object, None, false).unwrap();
    }

    #[test]
    fn object_stream_flate_limit_keeps_historical_local_fields() {
        let encoded = zlib_stored(b"0123456789");
        let document = document_with_stream_dictionary(
            &encoded,
            "/Filter /FlateDecode",
            Limits {
                max_decoded_bytes: 6,
                max_objstm_cache_bytes: 64,
                max_total_decoded_bytes: 128,
                max_total_decode_work_bytes: 128,
                ..Limits::default()
            },
        );
        let object = stream(&document);
        let error = document
            .resolver()
            .decode_stream_accounted(&object, None, true)
            .unwrap_err();
        refusal(error, LimitKind::DecodedBytes, 7, 6);
    }

    #[test]
    fn binding_refusal_ties_follow_caller_total_work_local_order() {
        let limits = Limits {
            max_total_decoded_bytes: 15,
            max_total_decode_work_bytes: 16,
            ..Limits::default()
        };
        let caller_tie = binding_decode_refusal(Some((3, 8)), 10, 0, limits, 8).unwrap();
        assert_eq!(caller_tie, Refusal {
            limit: LimitKind::TotalDecodedBytes,
            requested: 9,
            maximum: 8,
        });

        let total_work_tie = binding_decode_refusal(None, 10, 11, limits, 8).unwrap();
        assert_eq!(total_work_tie, Refusal {
            limit: LimitKind::TotalDecodedBytes,
            requested: 16,
            maximum: 15,
        });

        let work_local_tie = binding_decode_refusal(
            None,
            0,
            11,
            Limits {
                max_total_decoded_bytes: 100,
                max_total_decode_work_bytes: 16,
                ..Limits::default()
            },
            5,
        ).unwrap();
        assert_eq!(work_local_tie.limit, LimitKind::DecodeWorkBytes);
        assert_eq!(work_local_tie.requested, 17);
        assert_eq!(work_local_tie.maximum, 16);

        assert!(binding_decode_refusal(
            None,
            0,
            0,
            Limits {
                max_total_decoded_bytes: 100,
                max_total_decode_work_bytes: 100,
                ..Limits::default()
            },
            5,
        ).is_none());
    }

    #[test]
    fn resolver_and_external_output_caps_share_accounted_path() {
        let limits = Limits {
            max_total_decoded_bytes: 16,
            max_total_decode_work_bytes: 128,
            ..Limits::default()
        };
        let document = document_with_raw_stream(b"0123456789", limits);
        let object = stream(&document);
        let mut resolver = document.resolver();
        assert_eq!(resolver.decode_stream(&object).unwrap().bytes.len(), 10);
        refusal(
            resolver.decode_stream(&object).unwrap_err(),
            LimitKind::TotalDecodedBytes,
            17,
            16,
        );

        let document = document_with_raw_stream(b"0123456789", limits);
        let object = stream(&document);
        refusal(
            document
                .resolver()
                .decode_stream_with_total_limit(&object, 10, 16)
                .unwrap_err(),
            LimitKind::TotalDecodedBytes,
            17,
            16,
        );

        let work_limited = Limits {
            max_total_decoded_bytes: 128,
            max_total_decode_work_bytes: 16,
            ..Limits::default()
        };
        let document = document_with_raw_stream(b"0123456789", work_limited);
        let object = stream(&document);
        refusal(
            document.resolver().decode_stream(&object).unwrap_err(),
            LimitKind::DecodeWorkBytes,
            17,
            16,
        );

        let exact = document_with_raw_stream(
            b"123456",
            Limits {
                max_total_decoded_bytes: 6,
                max_total_decode_work_bytes: 64,
                ..Limits::default()
            },
        );
        assert_eq!(
            exact
                .resolver()
                .decode_stream(&stream(&exact))
                .unwrap()
                .bytes,
            b"123456"
        );

        let local = document_with_raw_stream(
            b"0123456789",
            Limits {
                max_decoded_bytes: 6,
                max_total_decoded_bytes: 128,
                max_total_decode_work_bytes: 128,
                ..Limits::default()
            },
        );
        refusal(
            local
                .resolver()
                .decode_stream(&stream(&local))
                .unwrap_err(),
            LimitKind::DecodedBytes,
            10,
            6,
        );
    }
}

pub(crate) fn filter_steps<F>(stream: &Object, resolve: F) -> Result<Vec<FilterStep>, DocError>
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
        let kind = FilterKind::from_name(name.decoded().as_ref()).ok_or(DocError::Unsupported {
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
        let decoded_params =
            parameters(param.as_ref(), kind, &mut resolve, stream.id(), allow_crypt)?;
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
    value.get().as_i64().map(Some).ok_or(DocError::WrongType {
        id,
        expected: "integer decode parameter",
    })
}

#[cfg(test)]
#[path = "resolver_crypto_tests.rs"]
mod resolver_crypto_tests;

#[cfg(test)]
#[path = "resolver_lookup_tests.rs"]
mod resolver_lookup_tests;

#[cfg(test)]
#[path = "resolver_dependency_tests.rs"]
mod resolver_dependency_tests;
