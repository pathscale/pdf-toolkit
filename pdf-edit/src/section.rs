use std::collections::BTreeMap;

use pdf_core::lex::{Lexer, LexerLimits};
use pdf_core::{DocError, LimitKind, Limits, ObjectRef, XrefKind};
use pdf_write::{Dict, Value};

use crate::{check, get, malformed, set, value, Editor};

pub(crate) fn startxref(bytes: &[u8]) -> Result<u64, DocError> {
    let base = bytes.len().saturating_sub(1024);
    let tail = bytes.get(base..).ok_or_else(|| malformed("startxref"))?;
    for (relative, window) in tail.windows(9).enumerate().rev() {
        if window != b"startxref" {
            continue;
        }
        let start = base
            .checked_add(relative)
            .and_then(|n| n.checked_add(9))
            .ok_or_else(|| malformed("startxref offset"))?;
        let Some(rest) = bytes.get(start..) else {
            continue;
        };
        let mut lexer = Lexer::with_limits(rest, LexerLimits::new(1, 32, 0));
        if let Some(Ok(token)) = lexer.next() {
            if !token.bytes().is_empty() && token.bytes().iter().all(u8::is_ascii_digit) {
                if let Some(offset) = std::str::from_utf8(token.bytes())
                    .ok()
                    .and_then(|text| text.parse::<u64>().ok())
                {
                    return Ok(offset);
                }
            }
        }
    }
    Err(malformed("startxref"))
}

struct Output<'a> {
    bytes: Vec<u8>,
    limits: &'a Limits,
}

impl<'a> Output<'a> {
    fn append(&mut self, bytes: &[u8]) -> Result<(), DocError> {
        let maximum = self.limits.max_file_bytes.min(u64::from(u32::MAX));
        let length = value::add(
            self.bytes.len() as u64,
            bytes.len() as u64,
            LimitKind::FileBytes,
            maximum,
        )?;
        usize::try_from(length).map_err(|_| malformed("output size"))?;
        self.bytes
            .try_reserve(bytes.len())
            .map_err(|_| DocError::Unsupported {
                feature: "edit:allocation",
            })?;
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    fn position(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn serialized(&mut self, body: &Value) -> Result<(), DocError> {
        let size = value::measure(body, self.limits)?;
        value::add(
            self.position(),
            size,
            LimitKind::FileBytes,
            self.limits.max_file_bytes.min(u64::from(u32::MAX)),
        )?;
        let bytes =
            pdf_write::serialize_value(body).map_err(|_| malformed("value serialization"))?;
        self.append(&bytes)
    }

    fn object(&mut self, id: ObjectRef, body: &Value, data: Option<&[u8]>) -> Result<(), DocError> {
        self.append(format!("{} {} obj\n", id.number(), id.generation()).as_bytes())?;
        self.serialized(body)?;
        match data {
            Some(data) => {
                self.append(b"\nstream\n")?;
                self.append(data)?;
                self.append(b"\nendstream\nendobj\n")
            }
            None => self.append(b"\nendobj\n"),
        }
    }

    fn eof(&mut self, offset: u64) -> Result<(), DocError> {
        self.append(format!("startxref\n{offset}\n%%EOF\n").as_bytes())
    }
}

fn trailer(editor: &Editor, size: u32, second_id: Vec<u8>) -> Result<Dict, DocError> {
    let root = editor
        .document
        .root_ref()
        .ok_or_else(|| malformed("catalog"))?;
    let first = match &editor.first_id {
        Some(first) => first.clone(),
        None => pdf_write::sha256::digest(&editor.bytes)
            .into_iter()
            .take(16)
            .collect(),
    };
    let prev = i64::try_from(editor.prev).map_err(|_| malformed("Prev offset"))?;
    let mut dict = vec![
        ("Size".to_owned(), Value::Int(i64::from(size))),
        (
            "Root".to_owned(),
            Value::Ref(root.number(), root.generation()),
        ),
        ("Prev".to_owned(), Value::Int(prev)),
        (
            "ID".to_owned(),
            Value::Array(vec![Value::HexStr(first), Value::HexStr(second_id)]),
        ),
    ];
    if let Some(info) = editor.info_new.or_else(|| editor.document.info_ref()) {
        dict.push((
            "Info".to_owned(),
            Value::Ref(info.number(), info.generation()),
        ));
    }
    if editor.document.chacha_key().is_some() {
        let encrypt = editor
            .document
            .encryption_ref()
            .ok_or_else(|| malformed("PSChaCha Encrypt reference"))?;
        dict.push((
            "Encrypt".to_owned(),
            Value::Ref(encrypt.number(), encrypt.generation()),
        ));
    }
    Ok(dict)
}

pub(crate) fn save(editor: &Editor) -> Result<Vec<u8>, DocError> {
    let page_updates = editor.page_updates()?;
    let mut objects: BTreeMap<_, _> = editor
        .pending
        .iter()
        .map(|(number, pending)| (*number, pending))
        .collect();
    for pending in &page_updates {
        objects.insert(pending.id.number(), pending);
    }
    let limits = editor.document.limits();
    check(
        LimitKind::PrevChain,
        u64::from(editor.document.xref().sections()).saturating_add(1),
        u64::from(limits.max_prev_chain),
    )?;
    let stream = editor.document.xref().kind() != XrefKind::Table;
    let size = if stream {
        editor.fresh(1)?
    } else {
        editor.next
    };
    let mut output = Output {
        bytes: Vec::new(),
        limits,
    };
    output.append(&editor.bytes)?;
    // Also terminates a trailing comment or an EOF without a final newline.
    output.append(b"\n")?;
    let mut entries = BTreeMap::new();
    let key = editor.document.chacha_key();
    for pending in objects.values() {
        let offset = output.position();
        if let Some(key) = &key {
            if editor.document.encryption_ref() == Some(pending.id) {
                return Err(DocError::Unsupported {
                    feature: "edit:PSChaCha security dictionary",
                });
            }
            if matches!(&pending.value, Value::Dict(dict)
                if matches!(get(dict, "Type"), Some(Value::Name(kind)) if kind == "XRef"))
            {
                return Err(DocError::Unsupported {
                    feature: "edit:PSChaCha xref replacement",
                });
            }
            let data = pending
                .data
                .as_deref()
                .map(|data| {
                    pdf_write::encryption::encrypt_stream(key, pending.id, data)
                        .map_err(|_| malformed("PSChaCha stream encryption"))
                })
                .transpose()?;
            let mut body = pdf_write::encryption::encrypt_value(key, pending.id, &pending.value)
                .map_err(|_| malformed("PSChaCha string encryption"))?;
            if let Some(data) = &data {
                let Value::Dict(dict) = &mut body else {
                    return Err(malformed("PSChaCha stream dictionary"));
                };
                let length =
                    i64::try_from(data.len()).map_err(|_| malformed("PSChaCha stream Length"))?;
                set(dict, "Length", Value::Int(length));
            }
            editor.cost(pending.id, &body, data.as_deref())?;
            output.object(pending.id, &body, data.as_deref())?;
        } else {
            output.object(pending.id, &pending.value, pending.data.as_deref())?;
        }
        entries.insert(pending.id.number(), (offset, pending.id.generation()));
    }
    let second_id = pdf_write::sha256::digest(&output.bytes)
        .into_iter()
        .take(16)
        .collect();
    let offset = output.position();
    let mut dict = trailer(editor, size, second_id)?;
    if !stream {
        output.append(b"xref\n0 1\n0000000000 65535 f \n")?;
        for (number, (position, generation)) in &entries {
            check(
                LimitKind::FileBytes,
                *position,
                limits.max_file_bytes.min(9_999_999_999),
            )?;
            output.append(format!("{number} 1\n{position:010} {generation:05} n \n").as_bytes())?;
        }
        output.append(b"trailer\n")?;
        output.serialized(&Value::Dict(dict))?;
        output.append(b"\n")?;
    } else {
        let id = ObjectRef::new(editor.next, 0);
        entries.insert(id.number(), (offset, 0));
        let count = entries
            .len()
            .checked_add(1)
            .ok_or_else(|| malformed("xref row count"))?;
        check(
            LimitKind::Objects,
            count as u64,
            u64::from(limits.max_objects),
        )?;
        let length = count
            .checked_mul(11)
            .ok_or_else(|| malformed("xref row size"))?;
        check(
            LimitKind::DecodedBytes,
            length as u64,
            limits.max_decoded_bytes,
        )?;
        check(
            LimitKind::ObjectBytes,
            length as u64,
            limits.max_object_bytes,
        )?;
        let mut rows = Vec::new();
        rows.try_reserve_exact(length)
            .map_err(|_| DocError::Unsupported {
                feature: "edit:allocation",
            })?;
        rows.push(0);
        rows.extend_from_slice(&0u64.to_be_bytes());
        rows.extend_from_slice(&u16::MAX.to_be_bytes());
        let index_length = count
            .checked_mul(2)
            .ok_or_else(|| malformed("xref Index size"))?;
        check(
            LimitKind::Tokens,
            index_length as u64,
            u64::from(limits.max_tokens_per_object),
        )?;
        let mut index = Vec::new();
        index
            .try_reserve_exact(index_length)
            .map_err(|_| DocError::Unsupported {
                feature: "edit:allocation",
            })?;
        index.push(Value::Int(0));
        index.push(Value::Int(1));
        for (number, (position, generation)) in entries {
            index.push(Value::Int(i64::from(number)));
            index.push(Value::Int(1));
            rows.push(1);
            rows.extend_from_slice(&position.to_be_bytes());
            rows.extend_from_slice(&generation.to_be_bytes());
        }
        dict.push(("Type".to_owned(), Value::Name("XRef".to_owned())));
        dict.push((
            "W".to_owned(),
            Value::Array(vec![Value::Int(1), Value::Int(8), Value::Int(2)]),
        ));
        dict.push(("Index".to_owned(), Value::Array(index)));
        let length = i64::try_from(rows.len()).map_err(|_| malformed("xref stream Length"))?;
        dict.push(("Length".to_owned(), Value::Int(length)));
        let body = Value::Dict(dict);
        editor.cost(id, &body, Some(&rows))?;
        output.object(id, &body, Some(&rows))?;
    }
    output.eof(offset)?;
    Ok(output.bytes)
}
