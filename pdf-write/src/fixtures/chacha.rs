//! Deterministic PSChaCha fixture with the production writer path.

use std::io;

use super::{
    manifest, name, page_truth, reference, ContentTruth, Fixture, House, LETTER,
};
use crate::{Value, Writer};

const PASSWORD: &[u8] = b"house-chacha-password";
const PLAIN: &[u8] =
    b"BT /F1 12 Tf 14 TL 72 720 Td (Empty user password) Tj T* (Encrypted strings and streams) Tj ET\n";
const TEXT_NORM: &str = "Empty user password Encrypted strings and streams";
const TITLE: &str = "Encrypted ChaCha";

pub(super) fn build() -> io::Result<Fixture> {
    let mut writer = Writer::new_encrypted_fixture("1.7", PASSWORD)?;
    let catalog = writer.alloc()?;
    let pages = writer.alloc()?;
    let font = writer.alloc()?;
    let mut house = House { writer, catalog, pages, font };
    let page = house.add_page(
        pages,
        PLAIN,
        super::Encoding::Flate,
        vec![(
            "PrivateStrings".to_owned(),
            Value::Array(vec![
                Value::Str(b"nested literal".to_vec()),
                Value::Dict(vec![(
                    "Nested".to_owned(),
                    Value::HexStr(b"nested hexadecimal".to_vec()),
                )]),
            ]),
        )],
    )?;
    house.write_base(&[page.id], 1)?;
    let info = house.writer.alloc()?;
    house.writer.object(info, &Value::Dict(vec![(
        "Title".to_owned(), Value::Str(TITLE.as_bytes().to_vec()),
    )]))?;
    house.table(vec![("Info".to_owned(), reference(info))])?;

    let mut expected = manifest("1.7", "table", 1, 1, 0, true);
    expected.push_str(
        "open.password=house-chacha-password\n\
         open.without_password=encrypted\n\
         open.wrong_password=malformed\n\
         open.corrupt_ciphertext=malformed\n",
    );
    page_truth(
        &mut expected, 0, LETTER, LETTER, 0, ContentTruth::Decoded(PLAIN),
    );
    expected.push_str(&format!(
        "page.0.content_refusal=none\npage.0.text_norm={TEXT_NORM}\ninfo.title={TITLE}\n",
    ));
    // Keep the stream filter visible while its payload is authenticated.
    let _ = name("FlateDecode");
    Ok(house.fixture(expected))
}
