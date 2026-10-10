use std::fmt;
use std::io;

pub(super) const MAX_BYTES: usize = 256 << 20;
pub(super) const MAX_OUTPUT: usize = 512 << 20;
pub(super) const MAX_FONT_BYTES: usize = 64 << 20;
pub(super) const MAX_TEXT_BYTES: usize = 1 << 20;
pub(super) const MAX_STRING_BYTES: usize = 64 << 10;
pub(super) const MAX_COMMAND_BYTES: usize = 16 << 20;
pub(super) const MAX_VALUE_BYTES: usize = 8 << 20;
pub(super) const MAX_PAGES: usize = 10_000;
pub(super) const MAX_FONTS: usize = 1024;
pub(super) const MAX_IMAGES: usize = 4096;
pub(super) const MAX_PIXELS: usize = 16 << 20;
pub(super) const MAX_LINKS: usize = 100_000;
pub(super) const MAX_OUTLINES: usize = 10_000;
pub(super) const MAX_DEPTH: usize = 32;
pub(super) const MAX_OPS: usize = 1_000_000;
pub(super) const MAX_OBJECTS: usize = 200_000;

/// Available through `io::Error::get_ref().downcast_ref::<Refusal>()`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Refusal {
    pub limit: &'static str,
    pub requested: usize,
    pub maximum: usize,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} requested {}, maximum {}",
            self.limit, self.requested, self.maximum
        )
    }
}

impl std::error::Error for Refusal {}

pub(super) fn refusal(kind: &'static str, requested: usize, maximum: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        Refusal { limit: kind, requested, maximum },
    )
}

pub(super) fn check(kind: &'static str, requested: usize, maximum: usize) -> io::Result<()> {
    if requested > maximum {
        Err(refusal(kind, requested, maximum))
    } else {
        Ok(())
    }
}

pub(super) fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

pub(super) fn add(a: usize, b: usize) -> io::Result<usize> {
    a.checked_add(b)
        .ok_or_else(|| refusal("size_arithmetic", usize::MAX, usize::MAX - 1))
}

pub(super) fn mul(a: usize, b: usize) -> io::Result<usize> {
    a.checked_mul(b)
        .ok_or_else(|| refusal("size_arithmetic", usize::MAX, usize::MAX - 1))
}

pub(super) fn append(out: &mut Vec<u8>, bytes: &[u8], maximum: usize) -> io::Result<()> {
    check("buffer_bytes", add(out.len(), bytes.len())?, maximum)?;
    out.try_reserve(bytes.len()).map_err(|_| {
        refusal("allocation_bytes", maximum.saturating_add(1), maximum)
    })?;
    out.extend_from_slice(bytes);
    Ok(())
}

pub(super) fn copied(bytes: &[u8], maximum: usize) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    append(&mut out, bytes, maximum)?;
    Ok(out)
}

pub(super) fn zeroed(length: usize, maximum: usize) -> io::Result<Vec<u8>> {
    check("buffer_bytes", length, maximum)?;
    let mut out = Vec::new();
    out.try_reserve_exact(length).map_err(|_| {
        refusal("allocation_bytes", maximum.saturating_add(1), maximum)
    })?;
    out.resize(length, 0);
    Ok(out)
}

pub(super) fn push<T>(
    out: &mut Vec<T>,
    value: T,
    kind: &'static str,
    maximum: usize,
) -> io::Result<()> {
    let count = add(out.len(), 1)?;
    check(kind, count, maximum)?;
    out.try_reserve(1).map_err(|_| {
        refusal(kind, maximum.saturating_add(1), maximum)
    })?;
    out.push(value);
    Ok(())
}

pub(super) fn unicode(text: &str, maximum: usize) -> io::Result<Vec<u8>> {
    check("string_bytes", text.len(), maximum)?;
    let capacity = add(mul(text.encode_utf16().count(), 2)?, 2)?;
    let mut out = zeroed(0, capacity)?;
    append(&mut out, &[0xfe, 0xff], capacity)?;
    for unit in text.encode_utf16() {
        append(&mut out, &unit.to_be_bytes(), capacity)?;
    }
    Ok(out)
}

pub(super) fn coordinate(value: f32) -> io::Result<()> {
    if !value.is_finite() {
        return Err(invalid("coordinate must be finite"));
    }
    if value.abs() > 10_000_000.0 {
        return Err(refusal("coordinate_points", value.abs() as usize, 10_000_000));
    }
    Ok(())
}

pub(super) fn numbers(values: &[f32]) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    for value in values {
        coordinate(*value)?;
        let number = crate::serialize_value(&crate::Value::Real(f64::from(*value)))?;
        append(&mut out, &number, MAX_COMMAND_BYTES)?;
        append(&mut out, b" ", MAX_COMMAND_BYTES)?;
    }
    Ok(out)
}

pub(super) fn rect(x: f32, y: f32, w: f32, h: f32) -> io::Result<[f32; 4]> {
    for value in [x, y, w, h] {
        coordinate(value)?;
    }
    if w < 0.0 || h < 0.0 {
        return Err(invalid("rectangle dimensions must be nonnegative"));
    }
    let right = x + w;
    let top = y + h;
    coordinate(right)?;
    coordinate(top)?;
    Ok([x, y, right, top])
}

