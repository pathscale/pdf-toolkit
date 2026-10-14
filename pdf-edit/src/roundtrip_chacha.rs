//! Encryption proofs run by pdf-roundtrip, without unit tests.

use std::sync::Arc;

use pdf_core::pages::PageWalk;
use pdf_core::{DocError, Document, Limits, ObjectRef, PdfSource};
use pdf_write::{Value, Writer, XrefCompression, encode};

use super::{CheckResult, Config, Editor, describe};

const PASSWORD: &[u8] = b"roundtrip-chacha-password";
const CONTENT: &[u8] = b"BT /F1 12 Tf 72 720 Td (ChaCha round trip) Tj ET\n";

fn require(value: bool, what: &str) -> CheckResult<()> {
    if value { Ok(()) } else { Err(what.to_owned()) }
}

fn name(value: &str) -> Value {
    Value::Name(value.to_owned())
}

fn reference(number: u32) -> Value {
    Value::Ref(number, 0)
}

fn open(bytes: Arc<[u8]>, password: &[u8]) -> Result<Document, DocError> {
    Document::open_with_password(PdfSource::new(bytes)?, Limits::default(), password)
}

fn title(document: &Document) -> CheckResult<Vec<u8>> {
    let info = document
        .info_ref()
        .ok_or_else(|| "missing Info".to_owned())?;
    let object = document.resolver().get(info).map_err(describe)?;
    object
        .dict()
        .and_then(|dict| dict.get(b"Title"))
        .and_then(|value| value.as_str())
        .map(|string| string.decode())
        .ok_or_else(|| "missing Title".to_owned())
}

fn text(document: &Document) -> CheckResult<String> {
    let mut resolver = document.resolver();
    let pages = PageWalk::new(&mut resolver)
        .map_err(describe)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(describe)?;
    require(pages.len() == 1, "encrypted page count")?;
    let page = pages
        .first()
        .ok_or_else(|| "missing encrypted page".to_owned())?;
    let text = pdf_text::page_text(&mut resolver, page).map_err(describe)?;
    Ok(pdf_text::layout(&text).plain_text())
}

fn auth_failure(bytes: Vec<u8>, password: &[u8]) -> CheckResult<()> {
    match open(Arc::from(bytes), password) {
        Err(DocError::Malformed {
            what: "PSChaCha authentication",
            ..
        }) => Ok(()),
        Err(error) => Err(format!("unexpected corruption error: {error}")),
        Ok(_) => Err("corrupted ciphertext opened".to_owned()),
    }
}

fn generated(password: &[u8], stream_xref: bool) -> CheckResult<(Arc<[u8]>, ObjectRef, ObjectRef)> {
    let mut writer = Writer::new_encrypted("1.7", password, None).map_err(describe)?;
    let root = writer.alloc().map_err(describe)?;
    let pages = writer.alloc().map_err(describe)?;
    let page = writer.alloc().map_err(describe)?;
    let font = writer.alloc().map_err(describe)?;
    let content = writer.alloc().map_err(describe)?;
    let twin = writer.alloc().map_err(describe)?;
    let info = writer.alloc().map_err(describe)?;
    let filtered = encode::flate(CONTENT);
    for number in [content, twin] {
        writer
            .stream(
                number,
                vec![
                    ("Filter".to_owned(), name("FlateDecode")),
                    (
                        "Private".to_owned(),
                        Value::Str(b"stream dictionary".to_vec()),
                    ),
                ],
                &filtered,
            )
            .map_err(describe)?;
    }
    writer
        .object(
            font,
            &Value::Dict(vec![
                ("Type".to_owned(), name("Font")),
                ("Subtype".to_owned(), name("Type1")),
                ("BaseFont".to_owned(), name("Helvetica")),
            ]),
        )
        .map_err(describe)?;
    writer
        .object(
            page,
            &Value::Dict(vec![
                ("Type".to_owned(), name("Page")),
                ("Parent".to_owned(), reference(pages)),
                (
                    "MediaBox".to_owned(),
                    Value::Array(vec![
                        Value::Int(0),
                        Value::Int(0),
                        Value::Int(612),
                        Value::Int(792),
                    ]),
                ),
                (
                    "Resources".to_owned(),
                    Value::Dict(vec![(
                        "Font".to_owned(),
                        Value::Dict(vec![("F1".to_owned(), reference(font))]),
                    )]),
                ),
                ("Contents".to_owned(), reference(content)),
                (
                    "Private".to_owned(),
                    Value::Array(vec![
                        Value::Str(Vec::new()),
                        Value::HexStr(b"binary\0string".to_vec()),
                    ]),
                ),
            ]),
        )
        .map_err(describe)?;
    writer
        .object(
            pages,
            &Value::Dict(vec![
                ("Type".to_owned(), name("Pages")),
                ("Count".to_owned(), Value::Int(1)),
                ("Kids".to_owned(), Value::Array(vec![reference(page)])),
            ]),
        )
        .map_err(describe)?;
    writer
        .object(
            root,
            &Value::Dict(vec![
                ("Type".to_owned(), name("Catalog")),
                ("Pages".to_owned(), reference(pages)),
            ]),
        )
        .map_err(describe)?;
    writer
        .object(
            info,
            &Value::Dict(vec![(
                "Title".to_owned(),
                Value::Str(b"ChaCha original".to_vec()),
            )]),
        )
        .map_err(describe)?;
    let extra = vec![("Info".to_owned(), reference(info))];
    if stream_xref {
        writer
            .finish_stream(root, XrefCompression::Flate, extra)
            .map_err(describe)?;
    } else {
        writer.finish_table(root, extra).map_err(describe)?;
    }
    Ok((
        Arc::from(writer.into_bytes()),
        ObjectRef::new(content, 0),
        ObjectRef::new(twin, 0),
    ))
}

pub(super) fn run(config: &Config) -> CheckResult<()> {
    for (label, password, stream_xref) in [
        ("chacha-table", PASSWORD, false),
        ("chacha-stream", PASSWORD, true),
        ("chacha-empty", &b""[..], false),
    ] {
        let (original, content, twin) = generated(password, stream_xref)?;
        require(
            matches!(
                Document::open(
                    PdfSource::new(original.clone()).map_err(describe)?,
                    Limits::default()
                ),
                Err(DocError::Encrypted)
            ),
            "missing password was accepted",
        )?;
        let mut wrong = password.to_vec();
        wrong.push(0);
        require(
            matches!(
                open(original.clone(), &wrong),
                Err(DocError::Malformed {
                    what: "PSChaCha password authentication",
                    ..
                })
            ),
            "wrong password was accepted",
        )?;
        let document = open(original.clone(), password).map_err(describe)?;
        require(
            text(&document)?.contains("ChaCha round trip"),
            "encrypted text",
        )?;
        require(
            title(&document)? == b"ChaCha original",
            "encrypted original title",
        )?;
        let mut resolver = document.resolver();
        let first = resolver.get(content).map_err(describe)?;
        let second = resolver.get(twin).map_err(describe)?;
        let first_range = first
            .stream_range()
            .ok_or_else(|| "first stream range".to_owned())?;
        let second_range = second
            .stream_range()
            .ok_or_else(|| "second stream range".to_owned())?;
        let first_raw = first
            .stream_raw()
            .ok_or_else(|| "first stream".to_owned())?;
        let second_raw = second
            .stream_raw()
            .ok_or_else(|| "second stream".to_owned())?;
        require(first_raw.len() == second_raw.len(), "twin stream lengths")?;
        require(first_raw.get(..24) != second_raw.get(..24), "nonce reuse")?;
        require(
            resolver.decode_stream(&first).map_err(describe)?.bytes == CONTENT,
            "filtered encrypted stream",
        )?;
        let mut damaged = original.to_vec();
        let position = first_range
            .start
            .checked_add(24)
            .ok_or_else(|| "ciphertext position".to_owned())?;
        *damaged
            .get_mut(position)
            .ok_or_else(|| "ciphertext byte".to_owned())? ^= 1;
        auth_failure(damaged, password)?;

        let mut swapped = original.to_vec();
        swapped
            .get_mut(first_range.clone())
            .ok_or_else(|| "first swap range".to_owned())?
            .copy_from_slice(second_raw);
        swapped
            .get_mut(second_range)
            .ok_or_else(|| "second swap range".to_owned())?
            .copy_from_slice(first_raw);
        auth_failure(swapped, password)?;

        let info = document
            .info_ref()
            .ok_or_else(|| "Info reference".to_owned())?;
        let info_object = resolver.get(info).map_err(describe)?;
        let title_value = info_object
            .dict()
            .and_then(|dict| dict.get(b"Title"))
            .ok_or_else(|| "Title value".to_owned())?;
        let mut damaged = original.to_vec();
        let position = title_value
            .span()
            .start
            .checked_add(49)
            .ok_or_else(|| "string ciphertext position".to_owned())?;
        let byte = damaged
            .get_mut(position)
            .ok_or_else(|| "string ciphertext byte".to_owned())?;
        *byte = if *byte == b'0' { b'1' } else { b'0' };
        auth_failure(damaged, password)?;

        let mut editor = Editor::open_with_password(original.clone(), Limits::default(), password)
            .map_err(describe)?;
        editor.set_info("Title", "ChaCha edited");
        editor
            .add_uri_link(
                0,
                [30.0, 30.0, 130.0, 50.0],
                "https://example.invalid/chacha",
            )
            .map_err(describe)?;
        editor
            .append_page_content(0, b"BT /F1 12 Tf 72 700 Td (ChaCha added) Tj ET\n")
            .map_err(describe)?;
        let saved = editor.save().map_err(describe)?;
        require(
            saved.starts_with(&original),
            "encrypted original prefix changed",
        )?;
        let after = open(Arc::from(saved.clone()), password).map_err(describe)?;
        require(
            after.encryption_ref() == document.encryption_ref(),
            "Encrypt reference changed",
        )?;
        require(
            after.root_ref() == document.root_ref(),
            "catalog reference changed",
        )?;
        require(
            after.xref().sections() == 2,
            "encrypted incremental section count",
        )?;
        require(title(&after)? == b"ChaCha edited", "encrypted edited title")?;
        let after_text = text(&after)?;
        require(
            after_text.contains("ChaCha round trip"),
            "original text lost",
        )?;
        require(
            after_text.contains("ChaCha added"),
            "new encrypted stream lost",
        )?;
        require(
            matches!(
                Document::open(
                    PdfSource::new(Arc::<[u8]>::from(saved.clone())).map_err(describe)?,
                    Limits::default(),
                ),
                Err(DocError::Encrypted)
            ),
            "encrypted update opened without password",
        )?;
        std::fs::write(config.output.join(format!("{label}.pdf")), saved).map_err(describe)?;
    }
    Ok(())
}
