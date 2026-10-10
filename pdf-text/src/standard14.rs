/// Approximate advances, in units of 1/1000 em. These character classes
/// are written from scratch; no font metric tables are used.
pub fn width(base_name: &[u8], unicode: char) -> Option<f64> {
    let family = family(base_name)?;
    if family == Family::Courier {
        return Some(600.0);
    }
    if unicode.is_control() {
        return Some(0.0);
    }
    let times = family == Family::Times;
    let symbolic = matches!(family, Family::Symbol | Family::ZapfDingbats);
    let width = if unicode.is_whitespace() {
        if times || symbolic { 250.0 } else { 280.0 }
    } else if matches!(unicode, 'i' | 'l' | 'I' | 'j' | '!' | '.' | ',' | ':' | ';'
        | '\'' | '`' | '|' | '\u{2018}' | '\u{2019}' | '\u{0131}') {
        if times { 250.0 } else { 240.0 }
    } else if matches!(unicode, '(' | ')' | '[' | ']' | '{' | '}' | '/' | '\\'
        | '-' | '\u{2013}' | 'f' | 't' | 'r') {
        if times { 330.0 } else { 350.0 }
    } else if unicode.is_ascii_digit() {
        if times || symbolic { 500.0 } else { 560.0 }
    } else if matches!(unicode, 'm' | 'w' | 'M' | 'W' | '@' | '%'
        | '\u{00c6}' | '\u{00e6}' | '\u{0152}' | '\u{0153}'
        | '\u{03a9}' | '\u{03c9}' | '\u{2014}') {
        if times { 850.0 } else { 880.0 }
    } else if unicode.is_uppercase() {
        if times { 670.0 } else { 680.0 }
    } else if unicode.is_lowercase() {
        if times { 480.0 } else { 530.0 }
    } else if family == Family::ZapfDingbats {
        750.0
    } else if family == Family::Symbol {
        600.0
    } else if unicode.is_ascii_punctuation() {
        500.0
    } else {
        if times { 500.0 } else { 560.0 }
    };
    Some(width)
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
    !hint.is_empty() && name.windows(hint.len()).any(|part| {
        part.iter().zip(hint).all(|(left, right)| left.eq_ignore_ascii_case(right))
    })
}

fn starts(name: &[u8], prefix: &[u8]) -> bool {
    name.get(..prefix.len()).map(|part| {
        part.iter().zip(prefix).all(|(left, right)| left.eq_ignore_ascii_case(right))
    }).unwrap_or(false)
}

fn strip_subset(name: &[u8]) -> &[u8] {
    if name.get(..6).is_some_and(|tag| tag.iter().all(u8::is_ascii_uppercase))
        && name.get(6) == Some(&b'+')
    {
        name.get(7..).unwrap_or(&[])
    } else {
        name
    }
}

