use crate::data::standard14 as afm;

/// Advance width in 1/1000 em from Adobe's Core 14 metrics, for a base font
/// name (tolerant: Arial, TimesNewRoman, ",Bold", "-BoldItalic", subset tags).
/// None when the font is not one of the 14 or the glyph is not in it.
pub fn width(base_name: &[u8], unicode: char) -> Option<f64> {
    let family = family(base_name)?;
    let name = strip_subset(base_name);
    let bold = contains(name, b"bold") || contains(name, b"black") || contains(name, b"heavy");
    let italic = contains(name, b"italic") || contains(name, b"oblique");
    let table: &[(u16, u16)] = match (family, bold, italic) {
        (Family::Courier, false, false) => &afm::COURIER,
        (Family::Courier, true, false) => &afm::COURIER_BOLD,
        (Family::Courier, false, true) => &afm::COURIER_OBLIQUE,
        (Family::Courier, true, true) => &afm::COURIER_BOLD_OBLIQUE,
        (Family::Helvetica, false, false) => &afm::HELVETICA,
        (Family::Helvetica, true, false) => &afm::HELVETICA_BOLD,
        (Family::Helvetica, false, true) => &afm::HELVETICA_OBLIQUE,
        (Family::Helvetica, true, true) => &afm::HELVETICA_BOLD_OBLIQUE,
        (Family::Times, false, false) => &afm::TIMES_ROMAN,
        (Family::Times, true, false) => &afm::TIMES_BOLD,
        (Family::Times, false, true) => &afm::TIMES_ITALIC,
        (Family::Times, true, true) => &afm::TIMES_BOLD_ITALIC,
        (Family::Symbol, ..) => &afm::SYMBOL,
        (Family::ZapfDingbats, ..) => &afm::ZAPF_DINGBATS,
    };
    let code = u16::try_from(u32::from(unicode)).ok()?;
    match table.binary_search_by_key(&code, |&(scalar, _)| scalar) {
        Ok(index) => table.get(index).map(|&(_, width)| f64::from(width)),
        // Every Courier glyph is 600 wide, including ones the table lacks.
        Err(_) if family == Family::Courier => Some(600.0),
        Err(_) => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Family {
    Courier,
    Helvetica,
    Times,
    Symbol,
    ZapfDingbats,
}

pub(crate) fn family(name: &[u8]) -> Option<Family> {
    let name = strip_subset(name);
    if starts(name, b"courier") {
        Some(Family::Courier)
    } else if starts(name, b"helvetica") || starts(name, b"arial") {
        Some(Family::Helvetica)
    } else if starts(name, b"times") {
        Some(Family::Times)
    } else if starts(name, b"symbol") {
        Some(Family::Symbol)
    } else if starts(name, b"zapfdingbats") || starts(name, b"itczapfdingbats") {
        Some(Family::ZapfDingbats)
    } else {
        None
    }
}

pub(crate) fn contains(name: &[u8], hint: &[u8]) -> bool {
    !hint.is_empty()
        && name.windows(hint.len()).any(|part| {
            part.iter()
                .zip(hint)
                .all(|(left, right)| left.eq_ignore_ascii_case(right))
        })
}

fn starts(name: &[u8], prefix: &[u8]) -> bool {
    name.get(..prefix.len())
        .map(|part| {
            part.iter()
                .zip(prefix)
                .all(|(left, right)| left.eq_ignore_ascii_case(right))
        })
        .unwrap_or(false)
}

fn strip_subset(name: &[u8]) -> &[u8] {
    if name
        .get(..6)
        .is_some_and(|tag| tag.iter().all(u8::is_ascii_uppercase))
        && name.get(6) == Some(&b'+')
    {
        name.get(7..).unwrap_or(&[])
    } else {
        name
    }
}
