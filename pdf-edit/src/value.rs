use std::slice::Iter;

use pdf_core::{DocError, LimitKind, Limits, Object, ValueKind, ValueRef};
use pdf_write::Value;

use crate::{check, malformed, refused};

const MAX_VALUE_DEPTH: u64 = 128;

pub(crate) fn add(
    left: u64,
    right: u64,
    kind: LimitKind,
    maximum: u64,
) -> Result<u64, DocError> {
    let requested = left.checked_add(right)
        .ok_or_else(|| refused(kind, u64::MAX, maximum))?;
    check(kind, requested, maximum)?;
    Ok(requested)
}

fn name_size(name: &str, maximum: u64) -> Result<u64, DocError> {
    check(
        LimitKind::ObjectBytes,
        (name.len() as u64).saturating_add(1),
        maximum,
    )?;
    let mut size = 1;
    for byte in name.bytes() {
        let escaped = !(33..=126).contains(&byte)
            || matches!(
                byte,
                b'#' | b'(' | b')' | b'<' | b'>' | b'[' | b']'
                    | b'{' | b'}' | b'/' | b'%'
            );
        size = add(
            size,
            if escaped { 3 } else { 1 },
            LimitKind::ObjectBytes,
            maximum,
        )?;
    }
    Ok(size)
}

fn string_size(bytes: &[u8], maximum: u64) -> Result<u64, DocError> {
    check(
        LimitKind::ObjectBytes,
        (bytes.len() as u64).saturating_add(2),
        maximum,
    )?;
    let mut size = 2;
    for byte in bytes {
        let length = match *byte {
            b'(' | b')' | b'\\' | b'\n' | b'\r' | b'\t' | 8 | 12 => 2,
            32..=126 => 1,
            _ => 4,
        };
        size = add(size, length, LimitKind::ObjectBytes, maximum)?;
    }
    Ok(size)
}

/// Measures exactly the syntax produced by pdf_write::serialize_value before
/// that serializer allocates. Only one sibling iterator is stored per level.
pub(crate) fn measure(value: &Value, limits: &Limits) -> Result<u64, DocError> {
    enum Part<'a> {
        Value(&'a Value, u64),
        Array(Iter<'a, Value>, u64),
        Dict(Iter<'a, (String, Value)>, u64),
    }

    let maximum = limits.max_object_bytes;
    let max_tokens = u64::from(limits.max_tokens_per_object);
    let max_depth = u64::from(limits.max_depth).min(MAX_VALUE_DEPTH);
    let mut size = 0;
    let mut tokens = 0;
    let mut stack = vec![Part::Value(value, 0)];
    while let Some(part) = stack.pop() {
        match part {
            Part::Array(mut items, depth) => {
                if let Some(child) = items.next() {
                    stack.push(Part::Array(items, depth));
                    stack.push(Part::Value(child, depth));
                }
            }
            Part::Dict(mut entries, depth) => {
                if let Some((key, child)) = entries.next() {
                    tokens = add(tokens, 1, LimitKind::Tokens, max_tokens)?;
                    size = add(size, name_size(key, maximum)?, LimitKind::ObjectBytes, maximum)?;
                    stack.push(Part::Dict(entries, depth));
                    stack.push(Part::Value(child, depth));
                }
            }
            Part::Value(value, depth) => {
                check(LimitKind::Depth, depth, max_depth)?;
                let count = match value {
                    Value::Array(_) | Value::Dict(_) => 2,
                    Value::Ref(_, _) => 3,
                    _ => 1,
                };
                tokens = add(tokens, count, LimitKind::Tokens, max_tokens)?;
                let length = match value {
                    Value::Null => 4,
                    Value::Bool(true) => 4,
                    Value::Bool(false) => 5,
                    Value::Int(number) => number.to_string().len() as u64,
                    Value::Real(number) => {
                        if !number.is_finite() {
                            return Err(malformed("non-finite PDF real"));
                        }
                        pdf_write::serialize_value(&Value::Real(*number))
                            .map_err(|_| malformed("PDF real serialization"))?
                            .len() as u64
                    }
                    Value::Name(name) => name_size(name, maximum)?,
                    Value::Str(bytes) => string_size(bytes, maximum)?,
                    Value::HexStr(bytes) => {
                        let digits = (bytes.len() as u64).checked_mul(2)
                            .ok_or_else(|| refused(LimitKind::ObjectBytes, u64::MAX, maximum))?;
                        add(digits, 2, LimitKind::ObjectBytes, maximum)?
                    }
                    Value::Array(items) => {
                        let child_depth = depth.checked_add(1)
                            .ok_or_else(|| refused(LimitKind::Depth, u64::MAX, max_depth))?;
                        stack.push(Part::Array(items.iter(), child_depth));
                        add(
                            2,
                            items.len().saturating_sub(1) as u64,
                            LimitKind::ObjectBytes,
                            maximum,
                        )?
                    }
                    Value::Dict(entries) => {
                        let child_depth = depth.checked_add(1)
                            .ok_or_else(|| refused(LimitKind::Depth, u64::MAX, max_depth))?;
                        stack.push(Part::Dict(entries.iter(), child_depth));
                        let spaces = (entries.len() as u64).checked_mul(2)
                            .ok_or_else(|| refused(LimitKind::ObjectBytes, u64::MAX, maximum))?;
                        add(5, spaces, LimitKind::ObjectBytes, maximum)?
                    }
                    Value::Ref(number, generation) => {
                        format!("{number} {generation} R").len() as u64
                    }
                };
                size = add(size, length, LimitKind::ObjectBytes, maximum)?;
            }
        }
    }
    Ok(size)
}

/// Disposes caller-supplied recursive values iteratively, including values
/// refused before their depth could be accepted.
pub(crate) fn dispose(value: Value) {
    let mut stack = vec![value];
    while let Some(value) = stack.pop() {
        match value {
            Value::Array(children) => stack.extend(children),
            Value::Dict(entries) => {
                for (_, child) in entries {
                    stack.push(child);
                }
            }
            _ => {}
        }
    }
}

pub(crate) fn text_string(text: &str, limits: &Limits) -> Result<Vec<u8>, DocError> {
    check(LimitKind::ObjectBytes, text.len() as u64, limits.max_object_bytes)?;
    if text.is_ascii() {
        return Ok(text.as_bytes().to_vec());
    }
    let units = text.encode_utf16().count() as u64;
    let bytes = units.checked_mul(2)
        .ok_or_else(|| refused(LimitKind::ObjectBytes, u64::MAX, limits.max_object_bytes))?;
    let size = add(bytes, 2, LimitKind::ObjectBytes, limits.max_object_bytes)?;
    let capacity = usize::try_from(size)
        .map_err(|_| refused(LimitKind::ObjectBytes, size, usize::MAX as u64))?;
    let mut result = Vec::new();
    result.try_reserve_exact(capacity)
        .map_err(|_| DocError::Unsupported { feature: "edit:allocation" })?;
    result.extend_from_slice(&[0xfe, 0xff]);
    for unit in text.encode_utf16() {
        result.extend_from_slice(&unit.to_be_bytes());
    }
    Ok(result)
}

pub(crate) fn checked_object(obj: &Object, limits: &Limits) -> Result<Value, DocError> {
    let max_depth = u64::from(limits.max_depth).min(MAX_VALUE_DEPTH);
    let maximum = u64::from(limits.max_tokens_per_object);
    let mut seen = 0;
    let mut stack = vec![(obj.root(), 0u64)];
    while let Some((node, depth)) = stack.pop() {
        seen = add(seen, 1, LimitKind::Tokens, maximum)?;
        check(LimitKind::Depth, depth, max_depth)?;
        if let Some(name) = node.as_name() {
            check_name(name)?;
        }
        let child_depth = depth.checked_add(1)
            .ok_or_else(|| refused(LimitKind::Depth, u64::MAX, max_depth))?;
        if let Some(array) = node.as_array() {
            check(
                LimitKind::Tokens,
                seen.saturating_add(stack.len() as u64).saturating_add(array.len() as u64),
                maximum,
            )?;
            for child in array.iter() {
                stack.push((child, child_depth));
            }
        } else if let Some(dict) = node.as_dict() {
            check(
                LimitKind::Tokens,
                seen.saturating_add(stack.len() as u64).saturating_add(dict.len() as u64),
                maximum,
            )?;
            for (name, child) in dict.iter() {
                check_name(name)?;
                stack.push((child, child_depth));
            }
        }
    }
    let value = to_value(obj);
    measure(&value, limits)?;
    Ok(value)
}

fn check_name(name: pdf_core::Name<'_>) -> Result<(), DocError> {
    if std::str::from_utf8(name.decoded().as_ref()).is_err() {
        Err(DocError::Unsupported { feature: "edit:non_utf8_name" })
    } else {
        Ok(())
    }
}

// Callers validate names and depth first. pdf_write::Value uses UTF-8 names,
// so binary names are refused rather than changed by a dictionary rewrite.
fn to_value(obj: &pdf_core::Object) -> pdf_write::Value {
    enum Task<'a> {
        Visit(ValueRef<'a>),
        Array(usize),
        Dict(Vec<String>),
    }

    let mut tasks = vec![Task::Visit(obj.root())];
    let mut values = Vec::new();
    while let Some(task) = tasks.pop() {
        match task {
            Task::Array(count) => {
                let start = values.len().saturating_sub(count);
                let children = values.split_off(start);
                values.push(Value::Array(children));
            }
            Task::Dict(keys) => {
                let start = values.len().saturating_sub(keys.len());
                let children = values.split_off(start);
                values.push(Value::Dict(keys.into_iter().zip(children).collect()));
            }
            Task::Visit(node) => {
                if let Some(array) = node.as_array() {
                    let children: Vec<_> = array.iter().collect();
                    tasks.push(Task::Array(children.len()));
                    for child in children.into_iter().rev() {
                        tasks.push(Task::Visit(child));
                    }
                    continue;
                }
                if let Some(dict) = node.as_dict() {
                    let entries: Vec<_> = dict.iter().collect();
                    let keys = entries.iter().map(|(name, _)| {
                        String::from_utf8_lossy(name.decoded().as_ref()).into_owned()
                    }).collect();
                    tasks.push(Task::Dict(keys));
                    for (_, child) in entries.into_iter().rev() {
                        tasks.push(Task::Visit(child));
                    }
                    continue;
                }
                let value = match node.kind() {
                    ValueKind::Bool => match node.as_bool() {
                        Some(value) => Value::Bool(value),
                        None => Value::Null,
                    },
                    ValueKind::Int => match node.as_i64() {
                        Some(value) => Value::Int(value),
                        None => Value::Null,
                    },
                    ValueKind::Real => match node.as_f64() {
                        Some(value) => Value::Real(value),
                        None => Value::Null,
                    },
                    ValueKind::Name => match node.as_name() {
                        Some(name) => Value::Name(
                            String::from_utf8_lossy(name.decoded().as_ref()).into_owned(),
                        ),
                        None => Value::Null,
                    },
                    ValueKind::String => match node.as_str() {
                        Some(string) if string.is_hex() => Value::HexStr(string.decode()),
                        Some(string) => Value::Str(string.decode()),
                        None => Value::Null,
                    },
                    ValueKind::Ref => match node.as_ref() {
                        Some(id) => Value::Ref(id.number(), id.generation()),
                        None => Value::Null,
                    },
                    _ => Value::Null,
                };
                values.push(value);
            }
        }
    }
    match values.pop() {
        Some(value) => value,
        None => Value::Null,
    }
}

