//! Encrypted fixtures authored independently of the reader.

use std::collections::BTreeSet;
use std::io;

use super::encrypt::{Encryption, Method};
use super::{
    count_value, manifest, name, page_truth, page_value, reference,
    ContentTruth, Fixture, House, LETTER,
};
use crate::{encode, serialize_value, Value, Writer, XrefCompression};

const PLAIN: &[u8] =
    b"BT /F1 12 Tf 14 TL 72 720 Td (Empty user password) Tj T* (Encrypted strings and streams) Tj ET\n";
const TEXT_NORM: &str = "Empty user password Encrypted strings and streams";

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

/// Members are serialized as plaintext, compressed, then encrypted once.
/// Strings within object-stream members must not be encrypted separately.
fn object_stream(
    writer: &mut Writer,
    encryption: &Encryption,
    number: u32,
    members: &[(u32, Value)],
) -> io::Result<()> {
    writer.check_id(number)?;
    let mut seen = BTreeSet::new();
    let mut header = Vec::new();
    let mut body = Vec::new();
    let mut entries = Vec::new();
    for (index, (id, value)) in members.iter().enumerate() {
        writer.check_id(*id)?;
        if *id == number || !seen.insert(*id) {
            return Err(invalid("duplicate or self-contained encrypted member"));
        }
        let index = u16::try_from(index)
            .map_err(|_| invalid("encrypted member index exceeds xref width"))?;
        let serialized = serialize_value(value)?;
        let entry = format!("{id} {} ", body.len());
        header
            .try_reserve(entry.len())
            .map_err(|_| invalid("encrypted object header allocation failed"))?;
        header.extend_from_slice(entry.as_bytes());
        let additional = serialized
            .len()
            .checked_add(1)
            .ok_or_else(|| invalid("encrypted object member length overflow"))?;
        body.try_reserve(additional)
            .map_err(|_| invalid("encrypted object body allocation failed"))?;
        body.extend_from_slice(&serialized);
        body.push(b'\n');
        entries
            .try_reserve(1)
            .map_err(|_| invalid("encrypted object entries allocation failed"))?;
        entries.push((*id, index));
    }
    let first = count_value(header.len())?;
    header
        .try_reserve(body.len())
        .map_err(|_| invalid("encrypted object stream allocation failed"))?;
    header.extend_from_slice(&body);
    let filtered = encode::flate(&header);
    let ciphertext = encryption.encrypt(number, &filtered, b"stream")?;
    writer.stream(
        number,
        vec![
            ("Type".to_owned(), name("ObjStm")),
            ("N".to_owned(), count_value(members.len())?),
            ("First".to_owned(), first),
            ("Filter".to_owned(), name("FlateDecode")),
        ],
        &ciphertext,
    )?;
    for (id, index) in entries {
        writer.entries.insert(
            id,
            crate::Entry::Compressed { stream: number, index },
        );
    }
    Ok(())
}

fn build(method: Method) -> io::Result<Fixture> {
    let (label, title, version, xref_kind) = match method {
        Method::Rc4 => ("encrypted-rc4", "Encrypted RC4", "1.7", "table"),
        Method::Aes128 => (
            "encrypted-aes128",
            "Encrypted AES-128",
            "1.7",
            "stream",
        ),
        Method::Aes256 => (
            "encrypted-aes256",
            "Encrypted AES-256",
            "2.0",
            "table",
        ),
    };
    let encryption = Encryption::new(method, label.as_bytes())?;
    let mut house = House::new(version)?;
    let page = house.writer.alloc()?;
    let content = house.writer.alloc()?;
    let info = house.writer.alloc()?;
    let encrypt = house.writer.alloc()?;

    let filtered = encode::flate(PLAIN);
    let ciphertext = encryption.encrypt(content, &filtered, b"stream")?;
    house.writer.stream(
        content,
        vec![("Filter".to_owned(), name("FlateDecode"))],
        &ciphertext,
    )?;

    let title_ciphertext = encryption.encrypt(info, title.as_bytes(), b"string")?;
    house.writer.object(
        info,
        &Value::Dict(vec![(
            "Title".to_owned(),
            Value::Str(title_ciphertext),
        )]),
    )?;
    house.writer.object(encrypt, &encryption.dictionary())?;

    let page_value = page_value(house.pages, content, Vec::new());
    if method == Method::Aes128 {
        let stream = house.writer.alloc()?;
        let mut members = house.base_values(&[page], 1)?;
        members.push((page, page_value));
        object_stream(&mut house.writer, &encryption, stream, &members)?;
    } else {
        house.write_base(&[page], 1)?;
        house.writer.object(page, &page_value)?;
    }

    let extra = vec![
        ("Encrypt".to_owned(), reference(encrypt)),
        ("Info".to_owned(), reference(info)),
        ("ID".to_owned(), encryption.identifiers()),
    ];
    if method == Method::Aes128 {
        // Neither xref-stream data nor its trailer strings are encrypted.
        house.writer.finish_stream(
            house.catalog,
            XrefCompression::Flate,
            extra,
        )?;
    } else {
        house.table(extra)?;
    }

    let mut expected = manifest(version, xref_kind, 1, 1, 0, true);
    page_truth(
        &mut expected,
        0,
        LETTER,
        LETTER,
        0,
        ContentTruth::Decoded(PLAIN),
    );
    expected.push_str(&format!(
        "page.0.content_refusal=none\npage.0.text_norm={TEXT_NORM}\ninfo.title={title}\n",
    ));
    Ok(house.fixture(expected))
}

pub(super) fn rc4() -> io::Result<Fixture> {
    build(Method::Rc4)
}

pub(super) fn aes128() -> io::Result<Fixture> {
    build(Method::Aes128)
}

pub(super) fn aes256() -> io::Result<Fixture> {
    build(Method::Aes256)
}
