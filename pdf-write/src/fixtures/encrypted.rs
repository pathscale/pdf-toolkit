//! Encrypted fixtures authored independently of the reader.

use std::io;

use super::encrypt::Encryption;
use super::{
    manifest, name, page_truth, page_value, reference, ContentTruth, Fixture, House, LETTER,
};
use crate::{encode, Value};

const PLAIN: &[u8] =
    b"BT /F1 12 Tf 14 TL 72 720 Td (Empty user password) Tj T* (Encrypted strings and streams) Tj ET\n";
const TEXT_NORM: &str = "Empty user password Encrypted strings and streams";

fn build() -> io::Result<Fixture> {
    let (label, title, version, xref_kind) = ("encrypted-rc4", "Encrypted RC4", "1.7", "table");
    let encryption = Encryption::new(label.as_bytes())?;
    let mut house = House::new(version)?;
    let page = house.writer.alloc()?;
    let content = house.writer.alloc()?;
    let info = house.writer.alloc()?;
    let encrypt = house.writer.alloc()?;

    let filtered = encode::flate(PLAIN);
    let ciphertext = encryption.encrypt(content, &filtered)?;
    house.writer.stream(
        content,
        vec![("Filter".to_owned(), name("FlateDecode"))],
        &ciphertext,
    )?;

    let title_ciphertext = encryption.encrypt(info, title.as_bytes())?;
    house.writer.object(
        info,
        &Value::Dict(vec![("Title".to_owned(), Value::Str(title_ciphertext))]),
    )?;
    house.writer.object(encrypt, &encryption.dictionary())?;

    let page_value = page_value(house.pages, content, Vec::new());
    house.write_base(&[page], 1)?;
    house.writer.object(page, &page_value)?;

    let extra = vec![
        ("Encrypt".to_owned(), reference(encrypt)),
        ("Info".to_owned(), reference(info)),
        ("ID".to_owned(), encryption.identifiers()),
    ];
    house.table(extra)?;

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
    build()
}
