//! Deterministic PDF serialization for house-authored fixtures.
//!
//! This crate is independent of pdf-core. Object numbers are reserved explicitly,
//! and every physical offset is measured from the bytes written by Writer.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::ops::Range;

pub mod doc;
pub mod encode;
pub mod fixtures;
pub mod sha256;

pub type Dict = Vec<(String, Value)>;

/// Dictionary order and array order are preserved.
#[derive(Clone, Debug)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Real(f64),
    Name(String),
    Str(Vec<u8>),
    HexStr(Vec<u8>),
    Array(Vec<Value>),
    Dict(Dict),
    Ref(u32, u16),
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.to_owned())
}

fn set(dict: &mut Dict, key: &str, value: Value) {
    dict.retain(|(existing, _)| existing != key);
    dict.push((key.to_owned(), value));
}

fn integer(value: usize) -> io::Result<Value> {
    i64::try_from(value)
        .map(Value::Int)
        .map_err(|_| invalid("PDF integer exceeds i64"))
}

fn name_bytes(out: &mut Vec<u8>, name: &str) {
    out.push(b'/');
    for byte in name.bytes() {
        if !(33..=126).contains(&byte)
            || matches!(
                byte,
                b'#' | b'(' | b')' | b'\\' | b'<' | b'>' | b'[' | b']'
                    | b'{' | b'}' | b'/' | b'%'
            )
        {
            out.extend_from_slice(format!("#{byte:02X}").as_bytes());
        } else {
            out.push(byte);
        }
    }
}

/// Serializes direct values iteratively, including deliberately deep fixtures.
pub fn serialize_value(value: &Value) -> io::Result<Vec<u8>> {
    enum Part<'a> {
        Value(&'a Value),
        Bytes(&'static [u8]),
        Name(&'a str),
    }

    let mut out = Vec::new();
    let mut stack = vec![Part::Value(value)];
    while let Some(part) = stack.pop() {
        match part {
            Part::Bytes(bytes) => out.extend_from_slice(bytes),
            Part::Name(name) => name_bytes(&mut out, name),
            Part::Value(value) => match value {
                Value::Null => out.extend_from_slice(b"null"),
                Value::Bool(true) => out.extend_from_slice(b"true"),
                Value::Bool(false) => out.extend_from_slice(b"false"),
                Value::Int(number) => {
                    out.extend_from_slice(number.to_string().as_bytes());
                }
                Value::Real(number) => {
                    if !number.is_finite() {
                        return Err(invalid("PDF real must be finite"));
                    }
                    let mut text = format!("{number:.4}");
                    while text.ends_with('0') {
                        text.pop();
                    }
                    if text.ends_with('.') {
                        text.pop();
                    }
                    if text == "-0" {
                        text = "0".to_owned();
                    }
                    out.extend_from_slice(text.as_bytes());
                }
                Value::Name(name) => name_bytes(&mut out, name),
                Value::Str(bytes) => {
                    out.push(b'(');
                    for byte in bytes {
                        match *byte {
                            b'(' | b')' | b'\\' => {
                                out.push(b'\\');
                                out.push(*byte);
                            }
                            b'\n' => out.extend_from_slice(b"\\n"),
                            b'\r' => out.extend_from_slice(b"\\r"),
                            b'\t' => out.extend_from_slice(b"\\t"),
                            8 => out.extend_from_slice(b"\\b"),
                            12 => out.extend_from_slice(b"\\f"),
                            32..=126 => out.push(*byte),
                            _ => {
                                out.extend_from_slice(
                                    format!("\\{byte:03o}").as_bytes(),
                                );
                            }
                        }
                    }
                    out.push(b')');
                }
                Value::HexStr(bytes) => {
                    out.push(b'<');
                    for byte in bytes {
                        out.extend_from_slice(format!("{byte:02x}").as_bytes());
                    }
                    out.push(b'>');
                }
                Value::Array(values) => {
                    out.push(b'[');
                    stack.push(Part::Bytes(b"]"));
                    for (index, child) in values.iter().enumerate().rev() {
                        stack.push(Part::Value(child));
                        if index != 0 {
                            stack.push(Part::Bytes(b" "));
                        }
                    }
                }
                Value::Dict(entries) => {
                    out.extend_from_slice(b"<<");
                    stack.push(Part::Bytes(b" >>"));
                    for (key, child) in entries.iter().rev() {
                        stack.push(Part::Value(child));
                        stack.push(Part::Bytes(b" "));
                        stack.push(Part::Name(key));
                        stack.push(Part::Bytes(b" "));
                    }
                }
                Value::Ref(number, generation) => {
                    out.extend_from_slice(
                        format!("{number} {generation} R").as_bytes(),
                    );
                }
            },
        }
    }
    Ok(out)
}

#[derive(Clone, Copy, Debug)]
enum Entry {
    Free,
    InUse(u64),
    Compressed { stream: u32, index: u16 },
}

#[derive(Clone, Copy, Debug)]
pub enum XrefCompression {
    None,
    Flate,
    FlateUp,
}

/// All written indirect objects use generation zero.
/// Full xref tables are also valid incremental sections.
pub struct Writer {
    bytes: Vec<u8>,
    entries: BTreeMap<u32, Entry>,
    next: u32,
    last_xref: Option<u64>,
}

impl Writer {
    pub fn new(version: &str) -> io::Result<Self> {
        if !matches!(
            version,
            "1.0" | "1.1" | "1.2" | "1.3" | "1.4"
                | "1.5" | "1.6" | "1.7" | "2.0"
        ) {
            return Err(invalid("unsupported PDF header version"));
        }
        let mut bytes = format!("%PDF-{version}\n").into_bytes();
        bytes.extend_from_slice(b"%\xE2\xE3\xCF\xD3\n");
        Ok(Self {
            bytes,
            entries: BTreeMap::new(),
            next: 1,
            last_xref: None,
        })
    }

    pub fn alloc(&mut self) -> io::Result<u32> {
        let number = self.next;
        self.next = self
            .next
            .checked_add(1)
            .ok_or_else(|| invalid("object number overflow"))?;
        self.entries.insert(number, Entry::Free);
        Ok(number)
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    pub fn position(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn check_id(&self, number: u32) -> io::Result<()> {
        if !self.entries.contains_key(&number) {
            return Err(invalid("object number was not reserved"));
        }
        Ok(())
    }

    pub fn object(&mut self, number: u32, value: &Value) -> io::Result<u64> {
        self.check_id(number)?;
        let body = serialize_value(value)?;
        let offset = self.position();
        self.bytes
            .extend_from_slice(format!("{number} 0 obj\n").as_bytes());
        self.bytes.extend_from_slice(&body);
        self.bytes.extend_from_slice(b"\nendobj\n");
        self.entries.insert(number, Entry::InUse(offset));
        Ok(offset)
    }

    /// Returns the exact physical range occupied by the encoded stream data.
    pub fn stream(
        &mut self,
        number: u32,
        mut dict: Dict,
        data: &[u8],
    ) -> io::Result<Range<usize>> {
        self.check_id(number)?;
        set(&mut dict, "Length", integer(data.len())?);
        let body = serialize_value(&Value::Dict(dict))?;
        let offset = self.position();
        self.bytes
            .extend_from_slice(format!("{number} 0 obj\n").as_bytes());
        self.bytes.extend_from_slice(&body);
        self.bytes.extend_from_slice(b"\nstream\n");
        let start = self.bytes.len();
        self.bytes.extend_from_slice(data);
        let end = self.bytes.len();
        self.bytes.extend_from_slice(b"\nendstream\nendobj\n");
        self.entries.insert(number, Entry::InUse(offset));
        Ok(start..end)
    }

    pub fn object_stream(
        &mut self,
        number: u32,
        members: &[(u32, Value)],
        flate: bool,
    ) -> io::Result<()> {
        self.check_id(number)?;
        let mut seen = BTreeSet::new();
        let mut header = Vec::new();
        let mut body = Vec::new();
        let mut compressed = Vec::new();
        for (index, (id, value)) in members.iter().enumerate() {
            self.check_id(*id)?;
            if *id == number || !seen.insert(*id) {
                return Err(invalid("duplicate or self-contained object stream member"));
            }
            let index = u16::try_from(index)
                .map_err(|_| invalid("object stream index exceeds xref width"))?;
            let serialized = serialize_value(value)?;
            header.extend_from_slice(format!("{id} {} ", body.len()).as_bytes());
            body.extend_from_slice(&serialized);
            body.push(b'\n');
            compressed.push((*id, index));
        }
        let mut dict = vec![
            ("Type".to_owned(), Value::Name("ObjStm".to_owned())),
            ("N".to_owned(), integer(members.len())?),
            ("First".to_owned(), integer(header.len())?),
        ];
        header.extend_from_slice(&body);
        let data = if flate {
            set(&mut dict, "Filter", Value::Name("FlateDecode".to_owned()));
            encode::flate(&header)
        } else {
            header
        };
        self.stream(number, dict, &data)?;
        for (id, index) in compressed {
            self.entries.insert(id, Entry::Compressed { stream: number, index });
        }
        Ok(())
    }

    fn trailer(&self, root: u32, mut extra: Dict, prev: Option<u64>) -> io::Result<Dict> {
        self.check_id(root)?;
        set(&mut extra, "Size", Value::Int(i64::from(self.next)));
        set(&mut extra, "Root", Value::Ref(root, 0));
        extra.retain(|(key, _)| key != "Prev");
        if let Some(offset) = prev {
            let offset = i64::try_from(offset)
                .map_err(|_| invalid("previous xref offset exceeds i64"))?;
            set(&mut extra, "Prev", Value::Int(offset));
        }
        Ok(extra)
    }

    fn table_bytes(&self, root: u32, extra: Dict, prev: Option<u64>) -> io::Result<Vec<u8>> {
        let mut bytes = format!("xref\n0 {}\n0000000000 65535 f \n", self.next).into_bytes();
        for number in 1..self.next {
            match self.entries.get(&number).copied().unwrap_or(Entry::Free) {
                Entry::InUse(offset) => {
                    if offset > 9_999_999_999 {
                        return Err(invalid("classic xref offset exceeds ten digits"));
                    }
                    bytes.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
                }
                Entry::Free | Entry::Compressed { .. } => {
                    bytes.extend_from_slice(b"0000000000 00000 f \n");
                }
            }
        }
        bytes.extend_from_slice(b"trailer\n");
        bytes.extend_from_slice(&serialize_value(&Value::Dict(
            self.trailer(root, extra, prev)?,
        ))?);
        bytes.push(b'\n');
        Ok(bytes)
    }

    fn eof(offset: u64) -> Vec<u8> {
        format!("startxref\n{offset}\n%%EOF\n").into_bytes()
    }

    fn table_section(
        &mut self,
        root: u32,
        extra: Dict,
        prev: Option<u64>,
        reported: Option<u64>,
    ) -> io::Result<u64> {
        let offset = self.position();
        let section = self.table_bytes(root, extra, prev)?;
        self.bytes.extend_from_slice(&section);
        self.bytes.extend_from_slice(&Self::eof(reported.unwrap_or(offset)));
        self.last_xref = Some(offset);
        Ok(offset)
    }

    /// A second call appends an incremental revision with a computed /Prev.
    pub fn finish_table(&mut self, root: u32, extra: Dict) -> io::Result<u64> {
        self.table_section(root, extra, self.last_xref, None)
    }

    /// Writes a complete xref stream without startxref, for hybrid construction.
    /// The stream itself is included in the xref entries.
    pub fn xref_stream(
        &mut self,
        root: u32,
        compression: XrefCompression,
        extra: Dict,
    ) -> io::Result<u64> {
        self.check_id(root)?;
        let number = self.alloc()?;
        let offset = self.position();
        self.entries.insert(number, Entry::InUse(offset));
        let mut rows = Vec::new();
        for id in 0..self.next {
            let (kind, field, generation) = if id == 0 {
                (0, 0, 65535)
            } else {
                match self.entries.get(&id).copied().unwrap_or(Entry::Free) {
                    Entry::Free => (0, 0, 0),
                    Entry::InUse(position) => {
                        let position = u32::try_from(position)
                            .map_err(|_| invalid("xref stream offset exceeds four bytes"))?;
                        (1, position, 0)
                    }
                    Entry::Compressed { stream, index } => (2, stream, index),
                }
            };
            rows.push(kind);
            rows.extend_from_slice(&field.to_be_bytes());
            rows.extend_from_slice(&generation.to_be_bytes());
        }
        let mut dict = self.trailer(root, extra, self.last_xref)?;
        set(&mut dict, "Type", Value::Name("XRef".to_owned()));
        set(
            &mut dict,
            "W",
            Value::Array(vec![Value::Int(1), Value::Int(4), Value::Int(2)]),
        );
        set(
            &mut dict,
            "Index",
            Value::Array(vec![Value::Int(0), Value::Int(i64::from(self.next))]),
        );
        let data = match compression {
            XrefCompression::None => rows,
            XrefCompression::Flate => {
                set(&mut dict, "Filter", Value::Name("FlateDecode".to_owned()));
                encode::flate(&rows)
            }
            XrefCompression::FlateUp => {
                set(&mut dict, "Filter", Value::Name("FlateDecode".to_owned()));
                set(
                    &mut dict,
                    "DecodeParms",
                    Value::Dict(vec![
                        ("Predictor".to_owned(), Value::Int(12)),
                        ("Columns".to_owned(), Value::Int(7)),
                    ]),
                );
                encode::flate(&encode::png_up(&rows, 7)?)
            }
        };
        self.stream(number, dict, &data)?;
        Ok(offset)
    }

    pub fn finish_stream(
        &mut self,
        root: u32,
        compression: XrefCompression,
        extra: Dict,
    ) -> io::Result<u64> {
        let offset = self.xref_stream(root, compression, extra)?;
        self.bytes.extend_from_slice(&Self::eof(offset));
        self.last_xref = Some(offset);
        Ok(offset)
    }

    /// Deliberately malformed startxref; object and table offsets remain measured.
    pub fn finish_bad_startxref(&mut self, root: u32, target: u64) -> io::Result<()> {
        self.table_section(root, Vec::new(), self.last_xref, Some(target))?;
        Ok(())
    }

    /// Authors a two-section /Prev cycle by measuring a trial serialization.
    /// The forward /Prev number reaches a fixed point as its digit count settles.
    pub fn finish_prev_cycle(&mut self, root: u32) -> io::Result<()> {
        let older = self.position();
        let mut newer = older;
        for _ in 0..32 {
            let trial = self.table_bytes(root, Vec::new(), Some(newer))?;
            let length = trial
                .len()
                .checked_add(Self::eof(older).len())
                .ok_or_else(|| invalid("xref section length overflow"))?;
            let measured = older
                .checked_add(length as u64)
                .ok_or_else(|| invalid("xref section offset overflow"))?;
            if measured == newer {
                self.table_section(root, Vec::new(), Some(newer), None)?;
                if self.position() != newer {
                    return Err(invalid("cycle layout changed after measurement"));
                }
                self.table_section(root, Vec::new(), Some(older), None)?;
                return Ok(());
            }
            newer = measured;
        }
        Err(invalid("cycle layout did not converge"))
    }
}

