use alloc::borrow::Cow;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::cmp::Ordering;

use pdf_core::{ArrayRef, DictRef, DocError, LimitKind, Limits, ObjectRef, Refusal, Resolver,
    Value, ValueRef, Warning};

use crate::cmap::{self, CMap};
use crate::encoding::{BaseEncoding, SimpleEncoding};
use crate::standard14::{self, Family};

const DEFAULT_MATRIX: [f64; 6] = [0.001, 0.0, 0.0, 0.001, 0.0, 0.0];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FontKind {
    Type1,
    MmType1,
    TrueType,
    Type3,
    Type0,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FontFlags {
    pub bold: bool,
    pub italic: bool,
    pub serif: bool,
    pub fixed_pitch: bool,
    pub symbolic: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GlyphText {
    len: u8,
    chars: [char; 4],
}

impl GlyphText {
    pub const fn new() -> Self {
        Self { len: 0, chars: ['\0'; 4] }
    }

    pub const fn from_char(c: char) -> Self {
        Self { len: 1, chars: [c, '\0', '\0', '\0'] }
    }

    pub fn push(&mut self, c: char) -> bool {
        let Some(slot) = self.chars.get_mut(usize::from(self.len)) else {
            return false;
        };
        *slot = c;
        self.len = self.len.saturating_add(1);
        true
    }

    pub fn as_slice(&self) -> &[char] {
        self.chars.get(..usize::from(self.len)).unwrap_or(&[])
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn push_to(&self, out: &mut String) {
        for character in self.as_slice() {
            out.push(*character);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Glyph {
    pub code: u32,
    pub byte_len: u8,
    pub width: f64,
    pub text: GlyphText,
    pub mapped: bool,
    pub word_space: bool,
}

#[derive(Clone, Debug)]
pub struct Font {
    data: Arc<FontData>,
}

struct FontEntry {
    id: ObjectRef,
    font: Font,
}

/// Caller-owned cache for one immutable document. Direct dictionaries bypass it.
/// The cap charges retained allocations conservatively, including entry capacity,
/// but excludes the document's already-owned source bytes. Cache pressure clears
/// retained entries; oversized fonts and allocation failures simply bypass it.
pub struct FontCache {
    entries: Vec<FontEntry>,
    used: u64,
    maximum: u64,
    document: Option<(Weak<[u8]>, Limits)>,
}

impl Default for FontCache {
    fn default() -> Self { Self::new(64 << 20) }
}

impl FontCache {
    pub fn new(maximum_bytes: u64) -> Self {
        Self {
            entries: Vec::new(), used: 0,
            maximum: maximum_bytes.min(isize::MAX as u64), document: None,
        }
    }

    pub fn clear(&mut self) {
        self.entries = Vec::new();
        self.used = 0;
        self.document = None;
    }

    pub fn bytes(&self) -> u64 {
        self.used.saturating_add((self.entries.capacity() as u64)
            .saturating_mul(core::mem::size_of::<FontEntry>() as u64))
    }

    pub fn maximum_bytes(&self) -> u64 { self.maximum }

    pub fn len(&self) -> usize { self.entries.len() }

    pub fn is_empty(&self) -> bool { self.entries.is_empty() }

    /// `value` is an already-resolved font dictionary. Only an indirect
    /// object's root qualifies, never a dictionary nested in its resources.
    pub fn load(&mut self, resolver: &mut Resolver<'_>, value: &Value)
        -> Result<Font, DocError>
    {
        if resolver.document().encrypted() {
            return Err(DocError::Encrypted);
        }
        let source = Arc::downgrade(&resolver.document().source().shared());
        let limits = *resolver.document().limits();
        if !self.document.as_ref().is_some_and(|(old, old_limits)| {
            Weak::ptr_eq(old, &source) && *old_limits == limits
        }) {
            self.clear();
            self.document = Some((source, limits));
        }
        let dict = value.get().as_dict().ok_or(DocError::WrongType {
            id: value.object().id(), expected: "font dictionary",
        })?;
        let id = value.object().id().filter(|_| {
            value.get().span() == value.object().root().span()
        });
        let key = id.map(|id| (id.number(), id.generation()));
        if let Some(key) = key {
            if let Ok(index) = self.entries.binary_search_by_key(&key, |entry| {
                (entry.id.number(), entry.id.generation())
            }) {
                if let Some(entry) = self.entries.get(index) {
                    for what in &entry.font.data.warnings {
                        resolver.warn(Warning { offset: None, object: id, what: *what });
                    }
                    return Ok(entry.font.clone());
                }
            }
        }
        let font = Font::load(resolver, dict, value.object().id())?;
        if let Some(id) = id {
            self.retain(id, &font, self.maximum.min(limits.max_total_decoded_bytes));
        }
        Ok(font)
    }

    fn retain(&mut self, id: ObjectRef, font: &Font, maximum: u64) {
        let cost = font.data.cache_bytes;
        let entry_bytes = core::mem::size_of::<FontEntry>() as u64;
        if cost.saturating_add(entry_bytes) > maximum { return; }
        let slots = self.entries.len().saturating_add(1).max(self.entries.capacity());
        let requested = self.used.saturating_add(cost)
            .saturating_add((slots as u64).saturating_mul(entry_bytes));
        if requested > maximum {
            self.entries = Vec::new();
            self.used = 0;
        }
        if self.entries.try_reserve_exact(1).is_err() { return; }
        if self.bytes().saturating_add(cost) > maximum {
            self.entries = Vec::new();
            self.used = 0;
            return;
        }
        let key = (id.number(), id.generation());
        let index = self.entries.binary_search_by_key(&key, |entry| {
            (entry.id.number(), entry.id.generation())
        }).unwrap_or_else(|index| index);
        // binary_search always yields a position at or below len.
        if index <= self.entries.len() {
            self.entries.insert(index, FontEntry { id, font: font.clone() });
            self.used = self.used.saturating_add(cost);
        }
    }
}

// CMap's private vectors contain at most 100,000 mappings and 256 codespaces.
// Charge generously for all vector capacities without exposing parser internals.
fn cmap_cache_bytes(length: usize) -> u64 {
    (length.min(100_000) as u64).saturating_mul(128).saturating_add(8192)
}

#[derive(Debug)]
struct FontData {
    kind: FontKind,
    name: FontName,
    flags: FontFlags,
    matrix: [f64; 6],
    simple: SimpleEncoding,
    simple_widths: [f64; 256],
    encoding: Option<CMap>,
    to_unicode: Option<CMap>,
    cid_widths: Vec<CidWidth>,
    default_width: f64,
    cache_bytes: u64,
    warnings: Vec<&'static str>,
}

#[derive(Clone, Debug, Default)]
struct FontName {
    value: Option<Value>,
    decoded: Option<Arc<[u8]>>,
    skip: usize,
}

impl FontName {
    fn new(value: Value) -> Option<Self> {
        let decoded = match value.get().as_name()?.decoded() {
            Cow::Borrowed(_) => None,
            Cow::Owned(bytes) => Some(Arc::<[u8]>::from(bytes)),
        };
        let mut name = Self { value: Some(value), decoded, skip: 0 };
        let raw = name.raw();
        if raw.get(..6).is_some_and(|tag| tag.iter().all(u8::is_ascii_uppercase))
            && raw.get(6) == Some(&b'+')
        {
            name.skip = 7;
        }
        Some(name)
    }

    fn raw(&self) -> &[u8] {
        if let Some(bytes) = &self.decoded {
            bytes.as_ref()
        } else {
            self.value.as_ref()
                .and_then(|value| value.get().as_name())
                .map(|name| name.encoded())
                .unwrap_or(&[])
        }
    }

    fn bytes(&self) -> &[u8] {
        self.raw().get(self.skip..).unwrap_or(&[])
    }
}

#[derive(Clone, Copy, Debug)]
struct CidWidth {
    start: u32,
    end: u32,
    width: f64,
}

impl Font {
    pub fn load(
        resolver: &mut Resolver<'_>,
        dict: DictRef<'_>,
        id: Option<ObjectRef>,
    ) -> Result<Font, DocError> {
        if resolver.document().encrypted() {
            return Err(DocError::Encrypted);
        }
        let mut loader = Loader {
            resolver,
            id,
            warnings: Vec::new(),
            width_operands: 0,
            cmap_bytes: 0,
        };
        let subtype = loader.key(dict, b"Subtype", "font_subtype_unknown")?;
        let kind = match subtype.as_ref().and_then(|value| value.get().as_name()) {
            Some(name) if name.eq_bytes(b"Type1") => FontKind::Type1,
            Some(name) if name.eq_bytes(b"MMType1") => FontKind::MmType1,
            Some(name) if name.eq_bytes(b"TrueType") => FontKind::TrueType,
            Some(name) if name.eq_bytes(b"Type3") => FontKind::Type3,
            Some(name) if name.eq_bytes(b"Type0") => FontKind::Type0,
            _ => {
                loader.warn("font_subtype_unknown");
                FontKind::Type1
            }
        };

        let descendant = if kind == FontKind::Type0 {
            load_descendant(&mut loader, dict)?
        } else {
            None
        };
        let metrics_dict = descendant.as_ref()
            .and_then(|value| value.get().as_dict())
            .unwrap_or(dict);

        let mut name = loader.name(dict, b"BaseFont")?;
        if name.is_none() && kind == FontKind::Type0 {
            name = loader.name(metrics_dict, b"BaseFont")?;
        }
        let name = name.unwrap_or_default();
        let descriptor = loader.key(metrics_dict, b"FontDescriptor", "font_descriptor_failed")?;
        let descriptor_dict = descriptor.as_ref().and_then(|value| value.get().as_dict());
        if descriptor.is_some() && descriptor_dict.is_none() {
            loader.warn("font_descriptor_failed");
        }
        let flags = load_flags(&mut loader, descriptor_dict, name.bytes())?;
        let missing_width = match descriptor_dict {
            Some(dict) => loader.number(dict, b"MissingWidth", "widths_invalid")?.unwrap_or(0.0),
            None => 0.0,
        };
        let matrix = if kind == FontKind::Type3 {
            load_matrix(&mut loader, dict)?
        } else {
            DEFAULT_MATRIX
        };

        let to_unicode = match loader.key(dict, b"ToUnicode", "tounicode_failed")? {
            Some(value) => loader.cmap(&value, "tounicode_failed")?,
            None => None,
        };

        let mut data = FontData {
            kind,
            name,
            flags,
            matrix,
            simple: SimpleEncoding::new(None),
            simple_widths: [0.0; 256],
            encoding: None,
            to_unicode,
            cid_widths: Vec::new(),
            default_width: 1.0,
            cache_bytes: 0,
            warnings: Vec::new(),
        };
        if kind == FontKind::Type0 {
            data.encoding = Some(load_type0_encoding(&mut loader, dict)?);
            data.default_width = loader.number(metrics_dict, b"DW", "widths_invalid")?
                .unwrap_or(1000.0) / 1000.0;
            data.cid_widths = load_cid_widths(&mut loader, metrics_dict)?;
        } else {
            let builtin = match standard14::family(data.name.bytes()) {
                Some(Family::Symbol) => Some(BaseEncoding::Symbol),
                Some(Family::ZapfDingbats) => Some(BaseEncoding::ZapfDingbats),
                _ if data.flags.symbolic => None,
                _ if kind == FontKind::TrueType => Some(BaseEncoding::WinAnsi),
                _ => Some(BaseEncoding::Standard),
            };
            data.simple = load_simple_encoding(&mut loader, dict, builtin)?;
            data.simple_widths = load_simple_widths(
                &mut loader, dict, &data, missing_width,
            )?;
        }
        let mut cost = (core::mem::size_of::<FontData>() as u64).saturating_add(64)
            .saturating_add(loader.cmap_bytes)
            .saturating_add((data.cid_widths.capacity() as u64)
                .saturating_mul(core::mem::size_of::<CidWidth>() as u64));
        if data.encoding.is_some() {
            // Also covers the small allocation made by Identity-H/Identity-V.
            cost = cost.saturating_add(8192);
        }
        if let Some(bytes) = &data.name.decoded {
            cost = cost.saturating_add(bytes.len() as u64).saturating_add(32);
        }
        if let Some(value) = &data.name.value {
            let object = value.object();
            // Nodes and dictionary/array vectors are bounded by token count.
            cost = cost.saturating_add(u64::from(object.token_count()).saturating_mul(256))
                .saturating_add(256);
            if !Arc::ptr_eq(object.buffer(), &loader.resolver.document().source().shared()) {
                cost = cost.saturating_add(object.buffer().len() as u64).saturating_add(32);
            }
        }
        cost = cost.saturating_add((loader.warnings.capacity() as u64)
            .saturating_mul(core::mem::size_of::<&'static str>() as u64));
        data.cache_bytes = cost;
        data.warnings = loader.warnings;
        Ok(Font { data: Arc::new(data) })
    }

    pub fn kind(&self) -> FontKind {
        self.data.kind
    }

    pub fn base_name(&self) -> &[u8] {
        self.data.name.bytes()
    }

    pub fn flags(&self) -> FontFlags {
        self.data.flags
    }

    pub fn is_vertical(&self) -> bool {
        self.data.encoding.as_ref().map(CMap::is_vertical).unwrap_or(false)
    }

    pub fn font_matrix(&self) -> [f64; 6] {
        self.data.matrix
    }

    pub fn glyphs<'a>(&'a self, bytes: &'a [u8]) -> impl Iterator<Item = Glyph> + 'a {
        let mut remaining = bytes;
        core::iter::from_fn(move || {
            let first = remaining.first().copied()?;
            let (code, byte_len) = match &self.data.encoding {
                Some(encoding) => encoding.next_code(remaining),
                None => (u32::from(first), 1),
            };
            remaining = remaining.get(usize::from(byte_len)..).unwrap_or(&[]);
            let valid = self.data.encoding.as_ref()
                .map(|encoding| encoding.accepts(code, byte_len))
                .unwrap_or(true);
            let text = if valid {
                self.data.to_unicode.as_ref().and_then(|map| map.unicode(code))
                    .or_else(|| {
                        if self.data.kind == FontKind::Type0 {
                            None
                        } else {
                            self.data.simple.unicode(code)
                        }
                    })
                    .filter(|text| !text.is_empty())
            } else {
                None
            };

            let width = if let Some(encoding) = &self.data.encoding {
                let cid = if valid { encoding.cid(code) } else { 0 };
                self.data.cid_widths.binary_search_by(|range| {
                    if range.end < cid {
                        Ordering::Less
                    } else if range.start > cid {
                        Ordering::Greater
                    } else {
                        Ordering::Equal
                    }
                }).ok()
                    .and_then(|index| self.data.cid_widths.get(index))
                    .map(|range| range.width)
                    .unwrap_or(self.data.default_width)
            } else {
                usize::try_from(code).ok()
                    .and_then(|index| self.data.simple_widths.get(index))
                    .copied().unwrap_or(0.0)
            };
            Some(Glyph {
                code,
                byte_len,
                width,
                text: text.unwrap_or_else(|| GlyphText::from_char('\u{fffd}')),
                mapped: text.is_some(),
                word_space: byte_len == 1 && code == 32,
            })
        })
    }
}

struct Loader<'r, 'd> {
    resolver: &'r mut Resolver<'d>,
    id: Option<ObjectRef>,
    warnings: Vec<&'static str>,
    width_operands: u64,
    cmap_bytes: u64,
}

impl Loader<'_, '_> {
    fn warn(&mut self, what: &'static str) {
        if !self.warnings.contains(&what) {
            self.warnings.push(what);
            self.resolver.warn(Warning { offset: None, object: self.id, what });
        }
    }

    fn recover<T>(
        &mut self,
        result: Result<T, DocError>,
        what: &'static str,
    ) -> Result<Option<T>, DocError> {
        match result {
            Ok(value) => Ok(Some(value)),
            Err(error @ (DocError::Encrypted | DocError::Refused(_))) => Err(error),
            Err(_) => {
                self.warn(what);
                Ok(None)
            }
        }
    }

    fn key(
        &mut self,
        dict: DictRef<'_>,
        key: &[u8],
        what: &'static str,
    ) -> Result<Option<Value>, DocError> {
        let result = self.resolver.resolve_dict_key(dict, key);
        Ok(self.recover(result, what)?.flatten().filter(|value| !value.get().is_null()))
    }

    fn resolve(
        &mut self,
        value: ValueRef<'_>,
        what: &'static str,
    ) -> Result<Option<Value>, DocError> {
        let result = self.resolver.resolve(value);
        self.recover(result, what)
    }

    fn name(&mut self, dict: DictRef<'_>, key: &[u8]) -> Result<Option<FontName>, DocError> {
        let Some(value) = self.key(dict, key, "font_name_invalid")? else {
            return Ok(None);
        };
        let name = FontName::new(value);
        if name.is_none() {
            self.warn("font_name_invalid");
        }
        Ok(name)
    }

    fn number(
        &mut self,
        dict: DictRef<'_>,
        key: &[u8],
        what: &'static str,
    ) -> Result<Option<f64>, DocError> {
        let Some(value) = self.key(dict, key, what)? else { return Ok(None) };
        let number = value.get().as_f64().filter(|number| number.is_finite());
        if number.is_none() {
            self.warn(what);
        }
        Ok(number)
    }

    fn number_value(
        &mut self,
        value: ValueRef<'_>,
        what: &'static str,
    ) -> Result<Option<f64>, DocError> {
        let Some(value) = self.resolve(value, what)? else { return Ok(None) };
        let number = value.get().as_f64().filter(|number| number.is_finite());
        if number.is_none() {
            self.warn(what);
        }
        Ok(number)
    }

    fn uint_value(
        &mut self,
        value: ValueRef<'_>,
        what: &'static str,
    ) -> Result<Option<u32>, DocError> {
        let Some(value) = self.resolve(value, what)? else { return Ok(None) };
        let number = value.get().as_i64().and_then(|number| u32::try_from(number).ok());
        if number.is_none() {
            self.warn(what);
        }
        Ok(number)
    }

    fn cmap(&mut self, value: &Value, what: &'static str) -> Result<Option<CMap>, DocError> {
        if value.get().as_dict().is_none() || !value.object().is_stream() {
            self.warn(what);
            return Ok(None);
        }
        let result = self.resolver.decode_stream(value.object());
        let Some(decoded) = self.recover(result, what)? else { return Ok(None) };
        if decoded.image.is_some() {
            self.warn(what);
            return Ok(None);
        }
        if decoded.truncated {
            self.warn(what);
        }
        self.cmap_bytes = self.cmap_bytes.saturating_add(cmap_cache_bytes(decoded.bytes.len()));
        let (map, warning) = cmap::parse(&decoded.bytes, 100_000);
        if let Some(warning) = warning {
            self.warn(warning);
            self.warn(what);
        }
        Ok(Some(map))
    }

    fn charge_widths(&mut self, count: usize) -> Result<(), DocError> {
        let count = u64::try_from(count).unwrap_or(u64::MAX);
        let requested = self.width_operands.saturating_add(count);
        let maximum = u64::from(self.resolver.document().limits().max_tokens_per_object);
        if requested > maximum {
            return Err(DocError::Refused(Refusal {
                limit: LimitKind::Tokens,
                requested,
                maximum,
            }));
        }
        self.width_operands = requested;
        Ok(())
    }
}

fn load_descendant(loader: &mut Loader<'_, '_>, dict: DictRef<'_>)
    -> Result<Option<Value>, DocError>
{
    let Some(descendants) = loader.key(dict, b"DescendantFonts", "font_descendant_missing")? else {
        loader.warn("font_descendant_missing");
        return Ok(None);
    };
    let Some(array) = descendants.get().as_array() else {
        loader.warn("font_descendant_missing");
        return Ok(None);
    };
    let Some(first) = array.get(0) else {
        loader.warn("font_descendant_missing");
        return Ok(None);
    };
    let Some(descendant) = loader.resolve(first, "font_descendant_missing")? else {
        return Ok(None);
    };
    let Some(dict) = descendant.get().as_dict() else {
        loader.warn("font_descendant_missing");
        return Ok(None);
    };
    let subtype = loader.key(dict, b"Subtype", "font_descendant_unknown")?;
    if !subtype.as_ref().and_then(|value| value.get().as_name())
        .is_some_and(|name| name.eq_bytes(b"CIDFontType0") || name.eq_bytes(b"CIDFontType2"))
    {
        loader.warn("font_descendant_unknown");
    }
    Ok(Some(descendant))
}

fn load_flags(
    loader: &mut Loader<'_, '_>,
    descriptor: Option<DictRef<'_>>,
    name: &[u8],
) -> Result<FontFlags, DocError> {
    let mut bits = 0u32;
    if let Some(descriptor) = descriptor {
        if let Some(value) = loader.key(descriptor, b"Flags", "font_flags_invalid")? {
            if let Some(flags) = value.get().as_i64().and_then(|flags| u32::try_from(flags).ok()) {
                bits = flags;
            } else {
                loader.warn("font_flags_invalid");
            }
        }
    }
    let family = standard14::family(name);
    Ok(FontFlags {
        bold: bits & 262_144 != 0
            || standard14::contains(name, b"bold")
            || standard14::contains(name, b"black")
            || standard14::contains(name, b"demi"),
        italic: bits & 64 != 0
            || standard14::contains(name, b"italic")
            || standard14::contains(name, b"oblique"),
        serif: bits & 2 != 0 || family == Some(Family::Times),
        fixed_pitch: bits & 1 != 0 || family == Some(Family::Courier),
        symbolic: bits & 4 != 0
            || matches!(family, Some(Family::Symbol | Family::ZapfDingbats)),
    })
}

fn load_matrix(loader: &mut Loader<'_, '_>, dict: DictRef<'_>)
    -> Result<[f64; 6], DocError>
{
    let Some(value) = loader.key(dict, b"FontMatrix", "font_matrix_invalid")? else {
        loader.warn("font_matrix_missing");
        return Ok(DEFAULT_MATRIX);
    };
    let Some(array) = value.get().as_array().filter(|array| array.len() == 6) else {
        loader.warn("font_matrix_invalid");
        return Ok(DEFAULT_MATRIX);
    };
    let mut matrix = DEFAULT_MATRIX;
    for (index, slot) in matrix.iter_mut().enumerate() {
        let Some(component) = array.get(index) else {
            loader.warn("font_matrix_invalid");
            return Ok(DEFAULT_MATRIX);
        };
        let Some(number) = loader.number_value(component, "font_matrix_invalid")? else {
            return Ok(DEFAULT_MATRIX);
        };
        *slot = number;
    }
    Ok(matrix)
}

fn load_type0_encoding(loader: &mut Loader<'_, '_>, dict: DictRef<'_>)
    -> Result<CMap, DocError>
{
    let Some(value) = loader.key(dict, b"Encoding", "encoding_unknown")? else {
        loader.warn("encoding_unknown");
        return Ok(CMap::identity(true, false));
    };
    if let Some(name) = value.get().as_name() {
        if name.eq_bytes(b"Identity-H") {
            return Ok(CMap::identity(true, false));
        }
        if name.eq_bytes(b"Identity-V") {
            return Ok(CMap::identity(true, true));
        }
        loader.warn("predefined_cmap_unsupported");
        return Ok(CMap::identity(true, name.decoded().ends_with(b"-V")));
    }
    Ok(loader.cmap(&value, "encoding_failed")?
        .unwrap_or_else(|| CMap::identity(true, false)))
}

fn load_simple_encoding(
    loader: &mut Loader<'_, '_>,
    dict: DictRef<'_>,
    builtin: Option<BaseEncoding>,
) -> Result<SimpleEncoding, DocError> {
    let encoding = loader.key(dict, b"Encoding", "encoding_unknown")?;
    let Some(encoding) = encoding else {
        if builtin.is_none() {
            loader.warn("encoding_unknown");
        }
        return Ok(SimpleEncoding::new(builtin));
    };
    if let Some(name) = encoding.get().as_name() {
        let base = BaseEncoding::from_name(name.decoded().as_ref());
        if base.is_none() {
            loader.warn("encoding_unknown");
        }
        return Ok(SimpleEncoding::new(base.or(builtin)));
    }
    let Some(dict) = encoding.get().as_dict() else {
        loader.warn("encoding_unknown");
        return Ok(SimpleEncoding::new(builtin));
    };
    let mut base = builtin;
    if let Some(value) = loader.key(dict, b"BaseEncoding", "encoding_unknown")? {
        let named = value.get().as_name()
            .and_then(|name| BaseEncoding::from_name(name.decoded().as_ref()));
        if named.is_none() {
            loader.warn("encoding_unknown");
        }
        base = named.or(base);
    }
    if base.is_none() {
        loader.warn("encoding_unknown");
    }
    let mut map = SimpleEncoding::new(base);
    if let Some(differences) = loader.key(dict, b"Differences", "differences_invalid")? {
        if let Some(array) = differences.get().as_array() {
            let mut code = None;
            for item in array.iter() {
                let Some(item) = loader.resolve(item, "differences_invalid")? else {
                    code = None;
                    continue;
                };
                if let Some(number) = item.get().as_i64() {
                    code = u8::try_from(number).ok();
                    if code.is_none() {
                        loader.warn("differences_invalid");
                    }
                } else if let Some(name) = item.get().as_name() {
                    if let Some(byte) = code {
                        if !map.set(byte, name.decoded().as_ref()) {
                            loader.warn("glyph_name_unknown");
                        }
                        code = byte.checked_add(1);
                    } else {
                        loader.warn("differences_invalid");
                    }
                } else {
                    code = None;
                    loader.warn("differences_invalid");
                }
            }
        } else {
            loader.warn("differences_invalid");
        }
    }
    Ok(map)
}

fn load_simple_widths(
    loader: &mut Loader<'_, '_>,
    dict: DictRef<'_>,
    data: &FontData,
    missing_width: f64,
) -> Result<[f64; 256], DocError> {
    let widths = loader.key(dict, b"Widths", "widths_missing")?;
    let array = widths.as_ref().and_then(|value| value.get().as_array());
    if array.is_none() {
        loader.warn("widths_missing");
    }
    let first = match loader.key(dict, b"FirstChar", "widths_invalid")? {
        Some(value) => match value.get().as_i64().and_then(|number| u8::try_from(number).ok()) {
            Some(first) => usize::from(first),
            None => {
                loader.warn("widths_invalid");
                0
            }
        },
        None => {
            if array.is_some() {
                loader.warn("widths_invalid");
            }
            0
        }
    };
    if array.is_some_and(|array| array.len() > 256usize.saturating_sub(first)) {
        loader.warn("widths_invalid");
    }

    let family = standard14::family(data.name.bytes());
    let mut result = [0.0; 256];
    for (code, slot) in result.iter_mut().enumerate() {
        let item = code.checked_sub(first)
            .and_then(|index| array.and_then(|array| array.get(index)));
        let explicit = match item {
            Some(item) => loader.number_value(item, "widths_invalid")?,
            None => None,
        };
        let width = if let Some(width) = explicit {
            width
        } else {
            // A standard 14 font may omit /Widths: Adobe's metrics are the
            // specified widths then, so only other fonts warn.
            if family.is_none() {
                loader.warn("widths_missing");
            }
            if missing_width != 0.0 {
                missing_width
            } else if data.kind != FontKind::Type3 {
                let character = data.simple.unicode(code as u32)
                    .and_then(|text| text.as_slice().first().copied())
                    .unwrap_or('\u{fffd}');
                match standard14::width(data.name.bytes(), character) {
                    Some(width) => width,
                    None if family.is_some() => {
                        // A glyph the font does not have: half an em keeps text flowing.
                        loader.warn("standard14_glyph_missing");
                        500.0
                    }
                    None => 0.0,
                }
            } else {
                0.0
            }
        };
        let scaled = if data.kind == FontKind::Type3 {
            width * data.matrix.first().copied().unwrap_or(0.001)
        } else {
            width / 1000.0
        };
        if scaled.is_finite() {
            *slot = scaled;
        } else {
            loader.warn("font_geometry_invalid");
            *slot = 0.0;
        }
    }
    Ok(result)
}

fn load_cid_widths(loader: &mut Loader<'_, '_>, dict: DictRef<'_>)
    -> Result<Vec<CidWidth>, DocError>
{
    let Some(value) = loader.key(dict, b"W", "widths_invalid")? else {
        return Ok(Vec::new());
    };
    let Some(array) = value.get().as_array() else {
        loader.warn("widths_invalid");
        return Ok(Vec::new());
    };
    loader.charge_widths(array.len())?;
    let mut widths = Vec::new();
    let mut position = 0usize;
    while position < array.len() {
        let Some(start_value) = array.get(position) else { break };
        let Some(start) = loader.uint_value(start_value, "widths_invalid")? else { break };
        position = position.saturating_add(1);
        let Some(next) = array.get(position) else {
            loader.warn("widths_invalid");
            break;
        };
        let Some(next) = loader.resolve(next, "widths_invalid")? else { break };
        position = position.saturating_add(1);
        if let Some(list) = next.get().as_array() {
            loader.charge_widths(list.len())?;
            append_width_list(loader, &mut widths, start, list)?;
        } else {
            let Some(end) = next.get().as_i64().and_then(|number| u32::try_from(number).ok()) else {
                loader.warn("widths_invalid");
                break;
            };
            let Some(width_value) = array.get(position) else {
                loader.warn("widths_invalid");
                break;
            };
            position = position.saturating_add(1);
            let width = loader.number_value(width_value, "widths_invalid")?;
            if end < start {
                loader.warn("widths_invalid");
            } else if let Some(width) = width {
                widths.push(CidWidth { start, end, width: width / 1000.0 });
            }
        }
    }

    widths.sort_by_key(|range| (range.start, range.end));
    let mut last = None;
    let mut overlap = false;
    widths.retain(|range| {
        if last.is_some_and(|end| range.start <= end) {
            overlap = true;
            false
        } else {
            last = Some(range.end);
            true
        }
    });
    if overlap {
        loader.warn("widths_overlap");
    }
    Ok(widths)
}

fn append_width_list(
    loader: &mut Loader<'_, '_>,
    widths: &mut Vec<CidWidth>,
    start: u32,
    list: ArrayRef<'_>,
) -> Result<(), DocError> {
    for (offset, value) in list.iter().enumerate() {
        let Some(code) = u32::try_from(offset).ok().and_then(|offset| start.checked_add(offset)) else {
            loader.warn("widths_invalid");
            break;
        };
        if let Some(width) = loader.number_value(value, "widths_invalid")? {
            widths.push(CidWidth { start: code, end: code, width: width / 1000.0 });
        }
    }
    Ok(())
}

