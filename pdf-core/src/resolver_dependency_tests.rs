use super::{DocError, Document, LimitKind, Limits, ObjectRef, OpenMode, PdfSource, Refusal};
use alloc::{format, vec::Vec};

const OUTER: ObjectRef = ObjectRef::new(1, 0);
const MEMBER: ObjectRef = ObjectRef::new(6, 0);
const MEMBER_HEADER: &[u8] = b"6 0 ";

fn object_stream(member: &[u8]) -> Vec<u8> {
    let mut data = Vec::from(MEMBER_HEADER);
    data.extend_from_slice(member);
    // Keep the object stream larger than the 35-byte xref stream so the
    // aggregate budgets can admit the xref and still refuse this dependency.
    data.resize(48, b' ');
    data
}

fn ascii_hex(bytes: &[u8]) -> Vec<u8> {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = Vec::with_capacity(bytes.len() * 2 + 1);
    for &byte in bytes {
        encoded.push(HEX[usize::from(byte >> 4)]);
        encoded.push(HEX[usize::from(byte & 0x0f)]);
    }
    encoded.push(b'>');
    encoded
}

fn dependency_pdf(
    payload: &[u8],
    dictionary: &str,
    object_data: &[u8],
    object_filter: &str,
    limits: Limits,
    mode: OpenMode,
) -> Document {
    let mut bytes = Vec::from(&b"%PDF-1.7\n"[..]);

    let outer_offset = bytes.len();
    bytes.extend_from_slice(format!("1 0 obj\n<< {dictionary} >>\nstream\n").as_bytes());
    bytes.extend_from_slice(payload);
    bytes.extend_from_slice(b"\nendstream\nendobj\n");

    let object_stream_offset = bytes.len();
    bytes.extend_from_slice(
        format!(
            "2 0 obj\n<< /Type /ObjStm /N 1 /First {} /Length {} {object_filter} >>\nstream\n",
            MEMBER_HEADER.len(),
            object_data.len(),
        )
        .as_bytes(),
    );
    bytes.extend_from_slice(object_data);
    bytes.extend_from_slice(b"\nendstream\nendobj\n");

    // Repair mode probes the catalog and page tree during open.
    let catalog_offset = bytes.len();
    bytes.extend_from_slice(b"3 0 obj\n<< /Type /Catalog /Pages 4 0 R >>\nendobj\n");
    let pages_offset = bytes.len();
    bytes.extend_from_slice(b"4 0 obj\n<< /Type /Pages /Kids [] /Count 0 >>\nendobj\n");

    let xref_offset = bytes.len();
    let mut entries = Vec::from([0, 0, 0, 0xff, 0xff]); // Free object 0.
    for offset in [
        outer_offset,
        object_stream_offset,
        catalog_offset,
        pages_offset,
        xref_offset,
    ] {
        entries.push(1);
        entries.extend_from_slice(&u16::try_from(offset).unwrap().to_be_bytes());
        entries.extend_from_slice(&0u16.to_be_bytes());
    }
    entries.extend_from_slice(&[2, 0, 2, 0, 0]); // Object 6, first member of object 2.
    bytes.extend_from_slice(
        format!(
            "5 0 obj\n<< /Type /XRef /Size 7 /W [1 2 2] /Root 3 0 R /Length {} >>\nstream\n",
            entries.len(),
        )
        .as_bytes(),
    );
    bytes.extend_from_slice(&entries);
    bytes.extend_from_slice(b"\nendstream\nendobj\nstartxref\n");
    bytes.extend_from_slice(format!("{xref_offset}\n%%EOF\n").as_bytes());

    let source = PdfSource::new(bytes).unwrap();
    let document =
        Document::open_with(source, limits, mode).expect("dependency fixture must open");
    assert!(!document.repaired(), "fixture must retain its original xref");
    document
}

fn assert_refusal(error: DocError, limit: LimitKind, requested: u64, maximum: u64) {
    assert_eq!(
        error,
        DocError::Refused(Refusal {
            limit,
            requested,
            maximum,
        }),
    );
}

#[test]
fn compressed_filter_and_crypt_name_dependencies_reduce_outer_ceiling() {
    let cases: [(&str, &[u8]); 2] = [
        ("/Filter 6 0 R", b"[/ASCIIHexDecode /ASCIIHexDecode]"),
        (
            "/Filter [/Crypt /ASCIIHexDecode /ASCIIHexDecode] /DecodeParms [<< /Name 6 0 R >> null null]",
            b"/Identity",
        ),
    ];

    // The filters decode this to "78>" and then "x". The final byte fits
    // either budget, so the refusal depends on bounding the intermediate output.
    let payload = b"37383E>";
    for (filter, member) in cases {
        let dictionary = format!("/Length {} {filter}", payload.len());
        let object_data = object_stream(member);
        let decode = |remaining| {
            let document = dependency_pdf(
                payload,
                &dictionary,
                &object_data,
                "",
                Limits {
                    max_total_decoded_bytes: object_data.len() as u64 + remaining,
                    ..Limits::default()
                },
                OpenMode::Strict,
            );
            let mut resolver = document.resolver();
            let outer = resolver.get(OUTER).unwrap();
            resolver.decode_stream(&outer)
        };

        let exact = object_data.len() as u64 + 3;
        assert_refusal(
            decode(2).unwrap_err(),
            LimitKind::TotalDecodedBytes,
            exact,
            exact - 1,
        );
        assert_eq!(decode(3).unwrap().bytes, b"x");
    }
}

#[test]
fn indirect_length_dependency_budget_refusal_propagates_in_strict_and_repair() {
    // A fallback terminator scan would return only the prefix.
    let payload = b"prefix\nendstream\nendobj\nsuffix";
    let object_data = object_stream(format!("{}", payload.len()).as_bytes());
    let encoded = ascii_hex(&object_data);
    let output = object_data.len() as u64;
    let work = encoded.len() as u64 + output;

    for mode in [OpenMode::Strict, OpenMode::Repair] {
        let lookup = |limits| {
            dependency_pdf(
                payload,
                "/Length 6 0 R",
                &encoded,
                "/Filter /ASCIIHexDecode",
                limits,
                mode,
            )
            .resolver()
            .get(OUTER)
        };

        assert_refusal(
            lookup(Limits {
                max_total_decoded_bytes: output - 1,
                ..Limits::default()
            })
            .unwrap_err(),
            LimitKind::TotalDecodedBytes,
            output,
            output - 1,
        );
        assert_refusal(
            lookup(Limits {
                max_total_decode_work_bytes: work - 1,
                ..Limits::default()
            })
            .unwrap_err(),
            LimitKind::DecodeWorkBytes,
            work,
            work - 1,
        );
        let outer = lookup(Limits {
            max_total_decoded_bytes: output,
            max_total_decode_work_bytes: work,
            ..Limits::default()
        })
        .unwrap();
        assert_eq!(outer.stream_raw(), Some(&payload[..]));
    }
}

#[test]
fn object_stream_cache_refusal_counts_payload_and_member_metadata() {
    let object_data = object_stream(b"42");
    let payload_bytes = object_data.len() as u64;
    let lookup = |cache_limit| {
        dependency_pdf(
            b"x",
            "/Length 1",
            &object_data,
            "",
            Limits {
                max_decoded_bytes: payload_bytes,
                max_objstm_cache_bytes: cache_limit,
                ..Limits::default()
            },
            OpenMode::Strict,
        )
        .resolver()
        .get(MEMBER)
    };

    let DocError::Refused(refusal) = lookup(payload_bytes).unwrap_err() else {
        panic!("expected a cache refusal");
    };
    assert_eq!(refusal.limit, LimitKind::DecodedBytes);
    assert_eq!(refusal.maximum, payload_bytes);
    assert!(refusal.requested > payload_bytes);

    // Use the reported cost to check the public boundary on either pointer width.
    let cost = refusal.requested;
    assert_refusal(
        lookup(cost - 1).unwrap_err(),
        LimitKind::DecodedBytes,
        cost,
        cost - 1,
    );
    assert_eq!(lookup(cost).unwrap().root().as_i64(), Some(42));
}
