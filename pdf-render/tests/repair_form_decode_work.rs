use pdf_core::pages::PageWalk;
use pdf_core::{
    DocError, Document, LimitKind, Limits, ObjectRef, OpenMode, PdfSource, Refusal, Resolver,
};
use pdf_toolkit_render::{display_list, RenderOptions};
use pdf_text::page_text;

const PAGE_CONTENT: &[u8] = b"/A Do\n/B Do\n";
const FORM_DECODED_LEN: usize = 129;
const FORM_ENCODED_LEN: usize = 140;
const LOCAL_DECODE_LIMIT: u64 = 128;
const TIGHT_WORK_LIMIT: u64 = 559;
const HEADROOM_WORK_LIMIT: u64 = 561;

fn adler32(bytes: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for byte in bytes {
        a = (a + u32::from(*byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

fn form_payload() -> Vec<u8> {
    let decoded = vec![b' '; FORM_DECODED_LEN];
    let mut encoded = vec![0x78, 0x01, 0x01];
    let len = FORM_DECODED_LEN as u16;
    encoded.extend_from_slice(&len.to_le_bytes());
    encoded.extend_from_slice(&(!len).to_le_bytes());
    encoded.extend_from_slice(&decoded);
    encoded.extend_from_slice(&adler32(&decoded).to_be_bytes());
    assert_eq!(decoded.len(), FORM_DECODED_LEN);
    assert_eq!(encoded.len(), FORM_ENCODED_LEN);
    encoded
}

fn append_object(bytes: &mut Vec<u8>, offsets: &mut [usize; 7], number: usize, body: &[u8]) {
    offsets[number] = bytes.len();
    bytes.extend_from_slice(format!("{number} 0 obj\n").as_bytes());
    bytes.extend_from_slice(body);
    bytes.extend_from_slice(b"\nendobj\n");
}

fn append_stream_object(
    bytes: &mut Vec<u8>,
    offsets: &mut [usize; 7],
    number: usize,
    dictionary: &[u8],
    payload: &[u8],
) {
    offsets[number] = bytes.len();
    bytes.extend_from_slice(format!("{number} 0 obj\n").as_bytes());
    bytes.extend_from_slice(dictionary);
    bytes.extend_from_slice(b"\nstream\n");
    bytes.extend_from_slice(payload);
    bytes.extend_from_slice(b"\nendstream\nendobj\n");
}

fn pdf_bytes() -> Vec<u8> {
    let mut bytes = b"%PDF-1.7\n".to_vec();
    let mut offsets = [0usize; 7];
    let content_stream = b"<< /Length 12 >>";
    assert_eq!(PAGE_CONTENT.len(), 12);
    let form = form_payload();
    append_object(
        &mut bytes,
        &mut offsets,
        1,
        b"<< /Type /Catalog /Pages 2 0 R >>",
    );
    append_object(
        &mut bytes,
        &mut offsets,
        2,
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
    );
    append_object(
        &mut bytes,
        &mut offsets,
        3,
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 16 16] /Resources << /XObject << /A 5 0 R /B 6 0 R >> >> /Contents 4 0 R >>",
    );
    append_stream_object(
        &mut bytes,
        &mut offsets,
        4,
        content_stream,
        PAGE_CONTENT,
    );
    let form_dictionary = format!(
        "<< /Type /XObject /Subtype /Form /BBox [0 0 16 16] /Resources << >> /Filter /FlateDecode /Length {FORM_ENCODED_LEN} >>"
    );
    append_stream_object(
        &mut bytes,
        &mut offsets,
        5,
        form_dictionary.as_bytes(),
        &form,
    );
    append_stream_object(
        &mut bytes,
        &mut offsets,
        6,
        form_dictionary.as_bytes(),
        &form,
    );

    let xref_offset = bytes.len();
    bytes.extend_from_slice(b"xref\n0 7\n0000000000 65535 f \n");
    for offset in offsets.iter().skip(1) {
        bytes.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    bytes.extend_from_slice(
        format!(
            "trailer\n<< /Size 7 /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n"
        )
        .as_bytes(),
    );
    bytes
}

fn page_with_work_limit(work_limit: u64) -> (Document, pdf_core::pages::Page) {
    let source = PdfSource::new(pdf_bytes()).unwrap();
    let limits = Limits {
        max_decoded_bytes: LOCAL_DECODE_LIMIT,
        max_total_decoded_bytes: 1_000_000,
        max_total_decode_work_bytes: work_limit,
        ..Limits::default()
    };
    let document = Document::open_with(source, limits, OpenMode::Repair).unwrap();
    assert!(document.repair_mode());
    let mut resolver = document.resolver();
    let page = {
        let mut walk = PageWalk::new(&mut resolver).unwrap();
        let page = walk.next().expect("one page").unwrap();
        assert!(walk.next().is_none());
        page
    };
    resolver.take_warnings();
    (document, page)
}

fn skipped_form_objects(resolver: &mut Resolver<'_>) -> Vec<Option<ObjectRef>> {
    resolver
        .take_warnings()
        .into_iter()
        .filter(|warning| warning.what == "resource_decode_skipped")
        .map(|warning| warning.object)
        .collect()
}

fn assert_work_refusal(result: Result<(), DocError>) {
    match result {
        Err(DocError::Refused(Refusal {
            limit: LimitKind::DecodeWorkBytes,
            requested: 560,
            maximum: 559,
        })) => {}
        other => panic!("expected DecodeWorkBytes refusal 560/559, got {other:?}"),
    }
}

#[test]
fn text_repair_form_work_exhaustion_escapes() {
    let (document, page) = page_with_work_limit(TIGHT_WORK_LIMIT);
    let mut resolver = document.resolver();
    let result = page_text(&mut resolver, &page).map(|_| ());
    assert_work_refusal(result);
    assert_eq!(
        skipped_form_objects(&mut resolver),
        vec![Some(ObjectRef::new(5, 0))]
    );
}

#[test]
fn display_repair_form_work_exhaustion_escapes() {
    let (document, page) = page_with_work_limit(TIGHT_WORK_LIMIT);
    let mut resolver = document.resolver();
    let result = display_list(&mut resolver, &page, &RenderOptions { scale: 0.25, ..RenderOptions::default() }).map(|_| ());
    assert_work_refusal(result);
    assert_eq!(
        skipped_form_objects(&mut resolver),
        vec![Some(ObjectRef::new(5, 0))]
    );
}

#[test]
fn text_repair_forms_with_work_headroom_are_skipped() {
    let (document, page) = page_with_work_limit(HEADROOM_WORK_LIMIT);
    let mut resolver = document.resolver();
    let text = match page_text(&mut resolver, &page) {
        Ok(text) => text,
        Err(error) => panic!("expected successful text extraction, got {error:?}"),
    };
    assert!(text.glyphs.is_empty());
    assert_eq!(
        skipped_form_objects(&mut resolver),
        vec![Some(ObjectRef::new(5, 0)), Some(ObjectRef::new(6, 0))]
    );
}

#[test]
fn display_repair_forms_with_work_headroom_are_skipped() {
    let (document, page) = page_with_work_limit(HEADROOM_WORK_LIMIT);
    let mut resolver = document.resolver();
    let list = match display_list(&mut resolver, &page, &RenderOptions { scale: 0.25, ..RenderOptions::default() }) {
        Ok(list) => list,
        Err(error) => panic!("expected successful display list, got {error:?}"),
    };
    assert!(list.items.is_empty());
    assert_eq!(
        skipped_form_objects(&mut resolver),
        vec![Some(ObjectRef::new(5, 0)), Some(ObjectRef::new(6, 0))]
    );
}
