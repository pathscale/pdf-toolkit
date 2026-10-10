use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use pdf_core::{
    DictRef, DocError, LimitKind, Limits, ObjectRef, Refusal, Resolver, Value, Warning,
};
use pdf_text::cmap::CMap;
use pdf_text::encoding::{glyph_name_to_unicode, BaseEncoding};
use pdf_text::font::{Font, FontKind, Glyph};
use skrifa::raw::ps::cff::CffFontRef;
use skrifa::raw::tables::cmap::PlatformId;
use skrifa::{GlyphId, GlyphNameSource, MetadataProvider};

use crate::display::{Path, PathEl};
use crate::sfnt::{self, Bytes, Program};
use crate::type1::{parse_type1, Type1Font};

const MAX_FONT_BYTES: u64 = 64 << 20;
const MAX_PATH_ELEMENTS: u64 = 1_000_000;
const MAX_CMAP_ENTRIES: u32 = 100_000;
const MAX_CID_MAP_BYTES: u64 = 131_072;
const MAX_NAME_BYTES: usize = 63;

#[derive(Clone)]
pub struct GlyphSource {
    data: Arc<SourceData>,
}

struct GlyphSourceEntry {
    id: ObjectRef,
    source: GlyphSource,
}

/// Caller-owned glyph-program cache. The cap includes conservative retained
/// allocation charges and entry capacity, excluding document source bytes.
/// Changing the source, parser limits or fallback allocation invalidates it.
pub struct GlyphSourceCache {
    entries: Vec<GlyphSourceEntry>,
    used: u64,
    maximum: u64,
    document: Option<(Weak<[u8]>, Limits)>,
    fallback: Option<Weak<[u8]>>,
}

impl Default for GlyphSourceCache {
    fn default() -> Self { Self::new(64 << 20) }
}

impl GlyphSourceCache {
    pub fn new(maximum_bytes: u64) -> Self {
        Self {
            entries: Vec::new(), used: 0,
            maximum: maximum_bytes.min(isize::MAX as u64),
            document: None, fallback: None,
        }
    }

    pub fn clear(&mut self) {
        self.entries = Vec::new();
        self.used = 0;
        self.document = None;
        self.fallback = None;
    }

    pub fn bytes(&self) -> u64 {
        self.used.saturating_add((self.entries.capacity() as u64)
            .saturating_mul(core::mem::size_of::<GlyphSourceEntry>() as u64))
    }

    pub fn maximum_bytes(&self) -> u64 { self.maximum }

    pub fn len(&self) -> usize { self.entries.len() }

    pub fn is_empty(&self) -> bool { self.entries.is_empty() }

    /// `value` is the resolved dictionary from which `font` was loaded.
    /// Direct dictionaries and unavailable programs are never retained.
    pub fn load(
        &mut self,
        resolver: &mut Resolver<'_>,
        font: &Font,
        value: &Value,
        fallback: Option<&Arc<[u8]>>,
    ) -> Result<Option<GlyphSource>, DocError> {
        if !resolver.document().decryptable() {
            return Err(DocError::Encrypted);
        }
        let source = Arc::downgrade(&resolver.document().source().shared());
        let limits = *resolver.document().limits();
        let same_fallback = match (&self.fallback, fallback) {
            (None, None) => true,
            (Some(old), Some(new)) => Weak::ptr_eq(old, &Arc::downgrade(new)),
            _ => false,
        };
        if !same_fallback || !self.document.as_ref().is_some_and(|(old, old_limits)| {
            Weak::ptr_eq(old, &source) && *old_limits == limits
        }) {
            self.clear();
            self.document = Some((source, limits));
            self.fallback = fallback.map(Arc::downgrade);
        }
        let dict = value.get().as_dict().ok_or_else(failed)?;
        let id = value.object().id().filter(|_| {
            value.get().span() == value.object().root().span()
        });
        if let Some(id) = id {
            let key = (id.number(), id.generation());
            if let Ok(index) = self.entries.binary_search_by_key(&key, |entry| {
                (entry.id.number(), entry.id.generation())
            }) {
                if let Some(entry) = self.entries.get(index) {
                    for warning in &entry.source.data.warnings {
                        resolver.warn(warning.clone());
                    }
                    return Ok(Some(entry.source.clone()));
                }
            }
        }
        let source = GlyphSource::load(resolver, font, dict, fallback)?;
        if let (Some(id), Some(source)) = (id, source.as_ref()) {
            self.retain(id, source, self.maximum.min(limits.max_total_decoded_bytes));
        }
        Ok(source)
    }

    fn retain(&mut self, id: ObjectRef, source: &GlyphSource, maximum: u64) {
        let cost = source.data.cache_bytes;
        let entry_bytes = core::mem::size_of::<GlyphSourceEntry>() as u64;
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
        if index <= self.entries.len() {
            self.entries.insert(index, GlyphSourceEntry { id, source: source.clone() });
            self.used = self.used.saturating_add(cost);
        }
    }
}

struct SourceData {
    cache_bytes: u64,
    warnings: Vec<Warning>,
    backend: Backend,
    mapping: Mapping,
    maximum_bytes: u64,
    maximum_elements: u64,
}

enum Backend {
    Sfnt(Program),
    Type1(Type1Font),
}

enum Mapping {
    Simple([Option<u16>; 256]),
    Type1([Option<GlyphName>; 256]),
    Cid {
        encoding: Option<CMap>,
        gids: Option<Bytes>,
    },
    Unicode {
        subtable: u16,
    },
}

#[derive(Clone, Copy)]
struct GlyphName {
    bytes: [u8; MAX_NAME_BYTES],
    len: u8,
}

impl GlyphName {
    fn new(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > MAX_NAME_BYTES || core::str::from_utf8(bytes).is_err() {
            return None;
        }
        let mut result = Self {
            bytes: [0; MAX_NAME_BYTES],
            len: u8::try_from(bytes.len()).ok()?,
        };
        result.bytes.get_mut(..bytes.len())?.copy_from_slice(bytes);
        Some(result)
    }

    fn bytes(&self) -> &[u8] {
        self.bytes.get(..usize::from(self.len)).unwrap_or(&[])
    }

    fn text(&self) -> Option<&str> {
        core::str::from_utf8(self.bytes()).ok()
    }
}

impl GlyphSource {
    pub fn load(
        resolver: &mut Resolver<'_>,
        font: &Font,
        dict: DictRef<'_>,
        fallback: Option<&Arc<[u8]>>,
    ) -> Result<Option<GlyphSource>, DocError> {
        if !resolver.document().decryptable() {
            return Err(DocError::Encrypted);
        }
        if font.kind() == FontKind::Type3 {
            return Ok(None);
        }
        let object = dict.get(b"Subtype")
            .and_then(|value| value.to_owned().object().id());
        match load_source(resolver, font, dict, fallback) {
            Ok(Some(data)) => Ok(Some(Self { data: Arc::new(data) })),
            Ok(None) => Ok(None),
            Err(error @ (DocError::Encrypted | DocError::Refused(_))) => Err(error),
            Err(_) => {
                resolver.warn(Warning {
                    offset: None,
                    object,
                    what: "font_program_failed",
                });
                Ok(None)
            }
        }
    }

    /// Returns an outline in 1/1000 em units, with y pointing up.
    pub fn outline(&self, glyph: &Glyph) -> Option<Path> {
        self.outline_checked(glyph).ok().flatten()
    }

    /// Allows a renderer to preserve refusals and report malformed outlines.
    pub fn outline_checked(&self, glyph: &Glyph) -> Result<Option<Path>, DocError> {
        match (&self.data.backend, &self.data.mapping) {
            (Backend::Type1(program), Mapping::Type1(names)) => {
                let name = usize::try_from(glyph.code).ok()
                    .and_then(|code| names.get(code))
                    .and_then(Option::as_ref)
                    .and_then(GlyphName::text);
                let Some(name) = name else { return Ok(None) };
                let Some(mut path) = program.outline(name) else { return Ok(None) };
                sfnt::validate_path(
                    &path,
                    self.data.maximum_elements,
                    self.data.maximum_bytes,
                )?;
                // Type1Font applies FontMatrix, yielding em/text-space units.
                // The renderer's shared outline convention is 1/1000 em.
                scale_type1(&mut path)?;
                Ok(Some(path))
            }
            (Backend::Sfnt(program), mapping) => {
                let gid = match mapping {
                    Mapping::Simple(gids) => usize::try_from(glyph.code).ok()
                        .and_then(|code| gids.get(code))
                        .copied()
                        .flatten(),
                    Mapping::Cid { encoding, gids } => {
                        let cid = encoding.as_ref()
                            .map(|map| map.cid(glyph.code))
                            .unwrap_or(glyph.code);
                        if cid > u32::from(u16::MAX) {
                            None
                        } else if let Some(gids) = gids {
                            usize::try_from(cid).ok()
                                .and_then(|cid| cid.checked_mul(2))
                                .and_then(|offset| {
                                    let end = offset.checked_add(2)?;
                                    match gids.bytes().get(offset..end)? {
                                        [high, low] => Some(u16::from_be_bytes([*high, *low])),
                                        _ => None,
                                    }
                                })
                        } else {
                            u16::try_from(cid).ok()
                        }
                    }
                    Mapping::Unicode { subtable } => {
                        if !glyph.mapped {
                            None
                        } else {
                            let character = match glyph.text.as_slice() {
                                [character] => Some(*character),
                                _ => None,
                            };
                            character.and_then(|character| {
                                let font = program.font()?;
                                sfnt::map(&font, Some(*subtable), u32::from(character))
                            })
                        }
                    }
                    Mapping::Type1(_) => None,
                };
                match gid.filter(|gid| *gid != 0 && *gid < program.glyph_count()) {
                    Some(gid) => program.outline_checked(gid),
                    None => Ok(None),
                }
            }
            _ => Ok(None),
        }
    }
}

fn load_source(
    resolver: &mut Resolver<'_>,
    font: &Font,
    dict: DictRef<'_>,
    fallback: Option<&Arc<[u8]>>,
) -> Result<Option<SourceData>, DocError> {
    let limits = *resolver.document().limits();
    let maximum_bytes = limits.max_decoded_bytes
        .min(limits.max_total_decoded_bytes)
        .min(MAX_FONT_BYTES)
        .min(isize::MAX as u64);
    let maximum_elements = limits.max_content_ops.min(MAX_PATH_ELEMENTS);
    let mut budget = Budget { limits, used: 0, cmap_bytes: 0, warnings: Vec::new() };

    let descendant = if font.kind() == FontKind::Type0 {
        let descendants = resolver.resolve_dict_key(dict, b"DescendantFonts")?
            .ok_or_else(failed)?;
        let first = descendants.get().as_array()
            .and_then(|array| array.get(0))
            .ok_or_else(failed)?;
        let descendant = resolver.resolve(first)?;
        if descendant.get().as_dict().is_none() {
            return Err(failed());
        }
        Some(descendant)
    } else {
        None
    };
    let metrics = descendant.as_ref()
        .and_then(|value| value.get().as_dict())
        .unwrap_or(dict);
    let descriptor = resolver.resolve_dict_key(metrics, b"FontDescriptor")?
        .filter(|value| !value.get().is_null());
    let descriptor_dict = match descriptor.as_ref() {
        Some(value) => Some(value.get().as_dict().ok_or_else(failed)?),
        None => None,
    };

    let mut embedded = None;
    if let Some(descriptor) = descriptor_dict {
        for (key, kind) in [
            (b"FontFile2".as_slice(), 2u8),
            (b"FontFile3".as_slice(), 3u8),
            (b"FontFile".as_slice(), 1u8),
        ] {
            if let Some(value) = resolver.resolve_dict_key(descriptor, key)?
                .filter(|value| !value.get().is_null())
            {
                embedded = Some((value, kind));
                break;
            }
        }
    }

    let Some((embedded, kind)) = embedded else {
        let Some(fallback) = fallback else { return Ok(None) };
        let bytes = Bytes::shared(fallback.clone());
        check_bytes(bytes.len(), maximum_bytes)?;
        let program = Program::new(bytes, maximum_bytes, maximum_elements)?
            .ok_or_else(failed)?;
        let reference = program.font().ok_or_else(failed)?;
        let subtable = sfnt::cmap_index(&reference, PlatformId::Windows, 1)
            .or_else(|| sfnt::cmap_index(&reference, PlatformId::Windows, 10))
            .or_else(|| sfnt::cmap_index(&reference, PlatformId::Unicode, 4))
            .or_else(|| sfnt::cmap_index(&reference, PlatformId::Unicode, 3))
            .or_else(|| sfnt::cmap_index(&reference, PlatformId::Unicode, 0))
            .ok_or_else(failed)?;
        return Ok(Some(SourceData {
            cache_bytes: budget.cache_bytes().saturating_add(fallback.len() as u64),
            warnings: budget.warnings,
            backend: Backend::Sfnt(program),
            mapping: Mapping::Unicode { subtable },
            maximum_bytes,
            maximum_elements,
        }));
    };

    let stream_dict = embedded.object().dict().ok_or_else(failed)?;
    let bytes = budget.stream(resolver, &embedded, maximum_bytes)?;
    if kind == 1 {
        if font.kind() == FontKind::Type0 {
            return Err(failed());
        }
        let length1 = length(resolver, stream_dict, b"Length1")?;
        let length2 = length(resolver, stream_dict, b"Length2")?;
        if length1.checked_add(length2).filter(|size| *size <= bytes.len()).is_none() {
            return Err(failed());
        }
        let program = parse_type1(bytes.bytes(), length1, length2).ok_or_else(failed)?;
        let mut names = base_names(default_encoding(font));
        if let Some(encoding) = program.encoding() {
            for (slot, name) in names.iter_mut().zip(encoding.iter()) {
                *slot = match name {
                    Some(name) => Some(checked_name(name.as_bytes(), &limits)?),
                    None => None,
                };
            }
        }
        apply_encoding(resolver, dict, &mut names, &limits)?;
        return Ok(Some(SourceData {
            // Type1 may reserve 65,536 glyph slots and sparse subroutine slots
            // even for a tiny input. Include those fixed worst-case capacities,
            // plus decoded charstrings and names, without changing its parser.
            cache_bytes: budget.cache_bytes()
                .saturating_add(budget.used.saturating_mul(4))
                .saturating_add(8 << 20),
            warnings: budget.warnings,
            backend: Backend::Type1(program),
            mapping: Mapping::Type1(names),
            maximum_bytes,
            maximum_elements,
        }));
    }

    let bytes = if kind == 3 {
        let subtype = resolver.resolve_dict_key(stream_dict, b"Subtype")?
            .ok_or_else(failed)?;
        let subtype = subtype.get().as_name().ok_or_else(failed)?;
        if subtype.eq_bytes(b"Type1C") || subtype.eq_bytes(b"CIDFontType0C") {
            let wrapped = sfnt::wrap_cff(&bytes, maximum_bytes)?.ok_or_else(failed)?;
            budget.charge(wrapped.len())?;
            wrapped
        } else if subtype.eq_bytes(b"OpenType") {
            bytes
        } else {
            return Err(failed());
        }
    } else {
        bytes
    };
    let program = Program::new(bytes, maximum_bytes, maximum_elements)?
        .ok_or_else(failed)?;

    let mapping = if font.kind() == FontKind::Type0 {
        let encoding = cid_encoding(resolver, dict, &mut budget, maximum_bytes)?;
        let subtype = resolver.resolve_dict_key(metrics, b"Subtype")?
            .ok_or_else(failed)?;
        let subtype = subtype.get().as_name().ok_or_else(failed)?;
        let gids = if subtype.eq_bytes(b"CIDFontType2") {
            match resolver.resolve_dict_key(metrics, b"CIDToGIDMap")?
                .filter(|value| !value.get().is_null())
            {
                None => None,
                Some(value) if value.get().as_name()
                    .is_some_and(|name| name.eq_bytes(b"Identity")) => None,
                Some(value) => {
                    let bytes = budget.stream(
                        resolver,
                        &value,
                        maximum_bytes.min(MAX_CID_MAP_BYTES),
                    )?;
                    if bytes.len() % 2 != 0 {
                        return Err(failed());
                    }
                    Some(bytes)
                }
            }
        } else if subtype.eq_bytes(b"CIDFontType0") {
            // The shared renderer contract uses GID = CID for CID-keyed CFF.
            None
        } else {
            return Err(failed());
        };
        Mapping::Cid { encoding, gids }
    } else {
        let mut names = base_names(default_encoding(font));
        let reference = program.font().ok_or_else(failed)?;
        if let Some(cff) = cff(&reference) {
            if cff.is_cid() {
                return Err(failed());
            }
            let encoding = cff.encoding().ok_or_else(failed)?;
            let charset = cff.charset().ok_or_else(failed)?;
            for (code, slot) in names.iter_mut().enumerate() {
                *slot = encoding.map(code as u8)
                    .filter(|gid| *gid != GlyphId::NOTDEF)
                    .and_then(|gid| charset.string_id(gid).ok())
                    .and_then(|sid| cff.string(sid))
                    .and_then(GlyphName::new);
            }
        }
        apply_encoding(resolver, dict, &mut names, &limits)?;
        Mapping::Simple(simple_gids(&program, font, &names)?)
    };

    Ok(Some(SourceData {
        cache_bytes: budget.cache_bytes(),
        warnings: budget.warnings,
        backend: Backend::Sfnt(program),
        mapping,
        maximum_bytes,
        maximum_elements,
    }))
}

fn default_encoding(font: &Font) -> BaseEncoding {
    match font.base_name() {
        b"Symbol" => BaseEncoding::Symbol,
        b"ZapfDingbats" => BaseEncoding::ZapfDingbats,
        _ if font.kind() == FontKind::TrueType => BaseEncoding::WinAnsi,
        _ => BaseEncoding::Standard,
    }
}

fn base_names(base: BaseEncoding) -> [Option<GlyphName>; 256] {
    core::array::from_fn(|code| {
        base.glyph_name(code as u8)
            .and_then(|name| GlyphName::new(name.as_bytes()))
    })
}

fn checked_name(bytes: &[u8], limits: &Limits) -> Result<GlyphName, DocError> {
    sfnt::check(
        LimitKind::ObjectBytes,
        bytes.len() as u64,
        limits.max_object_bytes.min(MAX_NAME_BYTES as u64),
    )?;
    GlyphName::new(bytes).ok_or_else(failed)
}

fn named_base(value: &Value) -> Result<BaseEncoding, DocError> {
    let name = value.get().as_name().ok_or_else(failed)?;
    // Compare decoded names without allocating a temporary string.
    for (name_bytes, base) in [
        (b"StandardEncoding".as_slice(), BaseEncoding::Standard),
        (b"WinAnsiEncoding".as_slice(), BaseEncoding::WinAnsi),
        (b"MacRomanEncoding".as_slice(), BaseEncoding::MacRoman),
        (b"MacExpertEncoding".as_slice(), BaseEncoding::MacExpert),
        (b"SymbolEncoding".as_slice(), BaseEncoding::Symbol),
        (b"ZapfDingbatsEncoding".as_slice(), BaseEncoding::ZapfDingbats),
        (b"PDFDocEncoding".as_slice(), BaseEncoding::PdfDoc),
    ] {
        if name.eq_bytes(name_bytes) {
            return Ok(base);
        }
    }
    Err(failed())
}

fn apply_encoding(
    resolver: &mut Resolver<'_>,
    dict: DictRef<'_>,
    names: &mut [Option<GlyphName>; 256],
    limits: &Limits,
) -> Result<(), DocError> {
    let Some(encoding) = resolver.resolve_dict_key(dict, b"Encoding")?
        .filter(|value| !value.get().is_null())
    else { return Ok(()) };
    if encoding.get().as_name().is_some() {
        *names = base_names(named_base(&encoding)?);
        return Ok(());
    }
    let dict = encoding.get().as_dict().ok_or_else(failed)?;
    if let Some(base) = resolver.resolve_dict_key(dict, b"BaseEncoding")?
        .filter(|value| !value.get().is_null())
    {
        *names = base_names(named_base(&base)?);
    }
    let Some(differences) = resolver.resolve_dict_key(dict, b"Differences")?
        .filter(|value| !value.get().is_null())
    else { return Ok(()) };
    let array = differences.get().as_array().ok_or_else(failed)?;
    sfnt::check(
        LimitKind::Tokens,
        array.len() as u64,
        u64::from(limits.max_tokens_per_object),
    )?;
    let mut code = None;
    for item in array.iter() {
        let item = resolver.resolve(item)?;
        if let Some(number) = item.get().as_i64() {
            code = u8::try_from(number).ok();
        } else if let Some(name) = item.get().as_name() {
            let Some(byte) = code else { continue };
            // A decoded name cannot be longer than its encoded representation.
            sfnt::check(
                LimitKind::ObjectBytes,
                name.encoded().len() as u64,
                limits.max_object_bytes.min((MAX_NAME_BYTES * 3) as u64),
            )?;
            let decoded = name.decoded();
            let name = checked_name(decoded.as_ref(), limits)?;
            if let Some(slot) = names.get_mut(usize::from(byte)) {
                *slot = Some(name);
            }
            code = byte.checked_add(1);
        } else {
            code = None;
        }
    }
    Ok(())
}

fn cff<'a>(font: &skrifa::FontRef<'a>) -> Option<CffFontRef<'a>> {
    let data = font.table_data(skrifa::Tag::new(b"CFF "))?;
    CffFontRef::new_cff(data.as_bytes(), 0, None).ok()
}

fn match_name(
    name: &[u8],
    gid: u16,
    names: &[Option<GlyphName>; 256],
    gids: &mut [Option<u16>; 256],
) {
    if gid == 0 {
        return;
    }
    for (target, slot) in names.iter().zip(gids.iter_mut()) {
        if slot.is_none() && target.as_ref().is_some_and(|target| target.bytes() == name) {
            *slot = Some(gid);
        }
    }
}

fn name_gids(
    program: &Program,
    names: &[Option<GlyphName>; 256],
) -> Result<[Option<u16>; 256], DocError> {
    let font = program.font().ok_or_else(failed)?;
    let glyph_names = font.glyph_names();
    let mut gids = [None; 256];
    if glyph_names.source() != GlyphNameSource::Synthesized {
        for (gid, name) in glyph_names.iter().take(usize::from(program.glyph_count())) {
            if !name.is_synthesized() {
                if let Ok(gid) = u16::try_from(gid.to_u32()) {
                    match_name(name.as_str().as_bytes(), gid, names, &mut gids);
                }
            }
        }
    } else if let Some(cff) = cff(&font) {
        // GlyphNames can synthesize names when the CFF charset operator was
        // omitted. CFF's default ISOAdobe charset still supplies real names.
        let charset = cff.charset().ok_or_else(failed)?;
        for (gid, sid) in charset.iter().take(usize::from(program.glyph_count())) {
            if let (Ok(gid), Some(name)) = (u16::try_from(gid.to_u32()), cff.string(sid)) {
                match_name(name, gid, names, &mut gids);
            }
        }
    }
    Ok(gids)
}

fn simple_gids(
    program: &Program,
    pdf_font: &Font,
    names: &[Option<GlyphName>; 256],
) -> Result<[Option<u16>; 256], DocError> {
    let font = program.font().ok_or_else(failed)?;
    let mut gids = name_gids(program, names)?;
    if cff(&font).is_some() {
        return Ok(gids);
    }
    let symbol = sfnt::cmap_index(&font, PlatformId::Windows, 0);
    let unicode = sfnt::cmap_index(&font, PlatformId::Windows, 1);
    let mac = sfnt::cmap_index(&font, PlatformId::Macintosh, 0);
    for (code, slot) in gids.iter_mut().enumerate() {
        let codepoint = code as u32;
        *slot = if pdf_font.flags().symbolic {
            sfnt::map(&font, symbol, 0xf000u32.saturating_add(codepoint))
                .or_else(|| sfnt::map(&font, mac, codepoint))
        } else {
            let character = names.get(code).and_then(Option::as_ref)
                .and_then(|name| glyph_name_to_unicode(name.bytes()))
                .and_then(|text| match text.as_slice() {
                    [character] => Some(*character),
                    _ => None,
                });
            character.and_then(|character| {
                sfnt::map(&font, unicode, u32::from(character))
            }).or_else(|| sfnt::map(&font, mac, codepoint))
                .or(*slot)
        };
        *slot = slot.filter(|gid| *gid != 0 && *gid < program.glyph_count());
    }
    Ok(gids)
}

fn cid_encoding(
    resolver: &mut Resolver<'_>,
    dict: DictRef<'_>,
    budget: &mut Budget,
    maximum_bytes: u64,
) -> Result<Option<CMap>, DocError> {
    let Some(value) = resolver.resolve_dict_key(dict, b"Encoding")?
        .filter(|value| !value.get().is_null())
    else { return Ok(None) };
    if let Some(name) = value.get().as_name() {
        if !name.eq_bytes(b"Identity-H") && !name.eq_bytes(b"Identity-V") {
            let warning = Warning {
                offset: None,
                object: value.object().id(),
                what: "predefined_cmap_unsupported",
            };
            budget.warnings.push(warning.clone());
            resolver.warn(warning);
        }
        // Match pdf-text's identity approximation for predefined CMaps.
        return Ok(None);
    }
    let bytes = budget.stream(resolver, &value, maximum_bytes)?;
    let maximum = budget.limits.max_tokens_per_object.min(MAX_CMAP_ENTRIES);
    // Match the conservative charge for pdf-text's private CMap vectors.
    budget.cmap_bytes = budget.cmap_bytes.saturating_add(
        (bytes.len().min(100_000) as u64).saturating_mul(128).saturating_add(8192),
    );
    let (map, warning) = CMap::parse(bytes.bytes(), maximum);
    match warning {
        Some("cmap_limit") => Err(DocError::Refused(Refusal {
            limit: LimitKind::Tokens,
            requested: u64::from(maximum).saturating_add(1),
            maximum: u64::from(maximum),
        })),
        Some(_) => Err(failed()),
        None => Ok(Some(map)),
    }
}

struct Budget {
    limits: Limits,
    used: u64,
    cmap_bytes: u64,
    warnings: Vec<Warning>,
}

impl Budget {
    fn cache_bytes(&self) -> u64 {
        self.used.saturating_add(self.cmap_bytes)
            .saturating_add(core::mem::size_of::<SourceData>() as u64)
            .saturating_add(core::mem::size_of::<Program>() as u64)
            .saturating_add(128)
            .saturating_add((self.warnings.capacity() as u64)
                .saturating_mul(core::mem::size_of::<Warning>() as u64))
    }
    fn charge(&mut self, bytes: usize) -> Result<(), DocError> {
        let requested = self.used.checked_add(bytes as u64).unwrap_or(u64::MAX);
        sfnt::check(
            LimitKind::TotalDecodedBytes,
            requested,
            self.limits.max_total_decoded_bytes,
        )?;
        self.used = requested;
        Ok(())
    }

    fn stream(
        &mut self,
        resolver: &mut Resolver<'_>,
        value: &Value,
        maximum: u64,
    ) -> Result<Bytes, DocError> {
        let object = value.object();
        let dict = object.dict().ok_or_else(failed)?;
        if !object.is_stream() {
            return Err(failed());
        }
        let filters = resolver.resolve_dict_key(dict, b"Filter")?;
        let unfiltered = filters.as_ref().map(|value| {
            value.get().is_null()
                || value.get().as_array().is_some_and(|array| array.is_empty())
        }).unwrap_or(true);
        let bytes = if unfiltered {
            let range = object.stream_range().ok_or_else(failed)?;
            Bytes::range(object.buffer().clone(), range).ok_or_else(failed)?
        } else {
            let decoded = resolver.decode_stream(object)?;
            if decoded.image.is_some() || decoded.truncated {
                return Err(failed());
            }
            Bytes::owned(decoded.bytes)
        };
        check_bytes(bytes.len(), maximum)?;
        self.charge(bytes.len())?;
        Ok(bytes)
    }
}

fn length(
    resolver: &mut Resolver<'_>,
    dict: DictRef<'_>,
    key: &[u8],
) -> Result<usize, DocError> {
    match resolver.resolve_dict_key(dict, key)? {
        None => Ok(0),
        Some(value) => value.get().as_i64()
            .and_then(|number| usize::try_from(number).ok())
            .ok_or_else(failed),
    }
}

fn check_bytes(length: usize, maximum: u64) -> Result<(), DocError> {
    sfnt::check(LimitKind::DecodedBytes, length as u64, maximum)
}

fn scale_type1(path: &mut Path) -> Result<(), DocError> {
    fn component(value: &mut f32) -> Result<(), DocError> {
        let scaled = f64::from(*value) * 1000.0;
        if !scaled.is_finite() || scaled.abs() > f64::from(f32::MAX) {
            return Err(failed());
        }
        *value = scaled as f32;
        Ok(())
    }
    for element in &mut path.els {
        match element {
            PathEl::MoveTo(x, y) | PathEl::LineTo(x, y) => {
                component(x)?;
                component(y)?;
            }
            PathEl::CubicTo(x0, y0, x1, y1, x, y) => {
                component(x0)?;
                component(y0)?;
                component(x1)?;
                component(y1)?;
                component(x)?;
                component(y)?;
            }
            PathEl::Close => {}
        }
    }
    Ok(())
}

fn failed() -> DocError {
    DocError::Malformed { offset: 0, what: "font_program_failed" }
}
