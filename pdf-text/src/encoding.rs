use crate::data::encodings::*;
use crate::font::GlyphText;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BaseEncoding {
    Standard,
    WinAnsi,
    MacRoman,
    MacExpert,
    Symbol,
    ZapfDingbats,
    PdfDoc,
}

impl BaseEncoding {
    pub fn from_name(name: &[u8]) -> Option<Self> {
        match name {
            b"StandardEncoding" | b"Standard" => Some(Self::Standard),
            b"WinAnsiEncoding" | b"WinAnsi" => Some(Self::WinAnsi),
            b"MacRomanEncoding" | b"MacRoman" => Some(Self::MacRoman),
            b"MacExpertEncoding" | b"MacExpert" => Some(Self::MacExpert),
            b"SymbolEncoding" | b"Symbol" => Some(Self::Symbol),
            b"ZapfDingbatsEncoding" | b"ZapfDingbats" => Some(Self::ZapfDingbats),
            b"PDFDocEncoding" | b"PdfDocEncoding" | b"PdfDoc" => Some(Self::PdfDoc),
            _ => None,
        }
    }

    pub fn unicode(self, byte: u8) -> Option<char> {
        let table: &[u16; 256] = match self {
            Self::Standard => &STANDARD,
            Self::WinAnsi => &WIN_ANSI,
            Self::MacRoman => &MAC_ROMAN,
            Self::MacExpert => &MAC_EXPERT,
            Self::Symbol => &SYMBOL,
            Self::ZapfDingbats => &ZAPF_DINGBATS,
            Self::PdfDoc => &PDF_DOC,
        };
        let value = table.get(usize::from(byte)).copied()?;
        if value == 0 {
            None
        } else {
            char::from_u32(u32::from(value))
        }
    }

    pub fn glyph_name(self, byte: u8) -> Option<&'static str> {
        let (first, names): (usize, &'static [&'static str]) = match self {
            Self::Standard => (STANDARD_FIRST, &STANDARD_NAMES),
            Self::WinAnsi => (WIN_ANSI_FIRST, &WIN_ANSI_NAMES),
            Self::MacRoman => (MAC_ROMAN_FIRST, &MAC_ROMAN_NAMES),
            Self::MacExpert => (MAC_EXPERT_FIRST, &MAC_EXPERT_NAMES),
            Self::Symbol => (SYMBOL_FIRST, &SYMBOL_NAMES),
            Self::ZapfDingbats => (ZAPF_DINGBATS_FIRST, &ZAPF_DINGBATS_NAMES),
            Self::PdfDoc => (PDF_DOC_FIRST, &PDF_DOC_NAMES),
        };
        let index = usize::from(byte).checked_sub(first)?;
        names.get(index).copied().filter(|name| !name.is_empty())
    }
}

/// Applies the AGL suffix and component rules without allocating.
/// A name requiring more than four Unicode scalars is left unmapped.
pub fn glyph_name_to_unicode(name: &[u8]) -> Option<GlyphText> {
    let name = name.split(|byte| *byte == b'.').next()?;
    let mut text = GlyphText::new();
    for part in name.split(|byte| *byte == b'_') {
        let component = component_to_unicode(part)?;
        for character in component.as_slice() {
            if !text.push(*character) {
                return None;
            }
        }
    }
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn component_to_unicode(name: &[u8]) -> Option<GlyphText> {
    if name.is_empty() {
        return None;
    }
    if let Ok(index) = GLYPH_NAMES.binary_search_by(|entry| entry.0.as_bytes().cmp(name)) {
        let (_, value) = GLYPH_NAMES.get(index)?;
        return char::from_u32(u32::from(*value)).map(GlyphText::from_char);
    }

    if let Some(digits) = name.strip_prefix(b"uni") {
        if digits.is_empty() || digits.len() > 16 || digits.len() % 4 != 0 {
            return None;
        }
        let mut text = GlyphText::new();
        for group in digits.as_chunks::<4>().0 {
            let character = char::from_u32(hex_number(group)?)?;
            if !text.push(character) {
                return None;
            }
        }
        return Some(text);
    }

    if let Some(digits) = name.strip_prefix(b"u") {
        if !(4..=6).contains(&digits.len()) {
            return None;
        }
        return char::from_u32(hex_number(digits)?).map(GlyphText::from_char);
    }

    // Unknown gNN, cidNN and aNN names do not imply Unicode.
    None
}

fn zapf_glyph_name_to_unicode(name: &[u8]) -> Option<GlyphText> {
    (0..=255u16)
        .map(|code| code as u8)
        .find(|code| {
            BaseEncoding::ZapfDingbats
                .glyph_name(*code)
                .is_some_and(|candidate| candidate.as_bytes() == name)
        })
        .and_then(|code| BaseEncoding::ZapfDingbats.unicode(code))
        .map(GlyphText::from_char)
}

fn hex_number(bytes: &[u8]) -> Option<u32> {
    let mut value = 0u32;
    for byte in bytes {
        let digit = match byte {
            b'0'..=b'9' => u32::from(*byte - b'0'),
            b'A'..=b'F' => u32::from(*byte - b'A' + 10),
            b'a'..=b'f' => u32::from(*byte - b'a' + 10),
            _ => return None,
        };
        value = value.checked_mul(16)?.checked_add(digit)?;
    }
    Some(value)
}

/// An immutable, per-font map. An empty slot also represents an explicit
/// Differences entry whose glyph name could not be mapped.
#[derive(Clone, Debug)]
pub(crate) struct SimpleEncoding {
    text: [GlyphText; 256],
}

impl SimpleEncoding {
    pub(crate) fn new(base: Option<BaseEncoding>) -> Self {
        let mut text = [GlyphText::new(); 256];
        if let Some(base) = base {
            for (index, slot) in text.iter_mut().enumerate() {
                let byte = index as u8;
                *slot = base
                    .unicode(byte)
                    .map(GlyphText::from_char)
                    .or_else(|| {
                        base.glyph_name(byte)
                            .and_then(|name| glyph_name_to_unicode(name.as_bytes()))
                    })
                    .unwrap_or_default();
            }
        }
        Self { text }
    }

    pub(crate) fn set(&mut self, byte: u8, name: &[u8]) -> bool {
        let mapped = glyph_name_to_unicode(name);
        if let Some(slot) = self.text.get_mut(usize::from(byte)) {
            *slot = mapped.unwrap_or_default();
        }
        mapped.is_some()
    }

    pub(crate) fn set_embedded(&mut self, byte: u8, name: &[u8]) {
        let index = usize::from(byte);
        if let Some(slot) = self.text.get_mut(index) {
            *slot = glyph_name_to_unicode(name)
                .or_else(|| zapf_glyph_name_to_unicode(name))
                .unwrap_or_default();
        }
    }

    pub(crate) fn unicode(&self, code: u32) -> Option<GlyphText> {
        let index = usize::try_from(code).ok()?;
        self.text
            .get(index)
            .copied()
            .filter(|text| !text.is_empty())
    }
}
