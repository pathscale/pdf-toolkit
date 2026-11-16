use super::*;

use alloc::format;
use alloc::vec;
use alloc::vec::Vec;

fn object_stream_payload(number: u32, value: &[u8]) -> Vec<u8> {
    let header = format!("{number} 0 ");
    let mut payload = header.into_bytes();
    payload.extend_from_slice(value);
    payload
}

fn lookup_document(
    first_stream: Option<(u32, u32, Vec<u8>)>,
    second_stream: Option<(u32, u32, Vec<u8>)>,
    additional_compressed: &[(u32, u32, u32)],
    general: &[u8],
    limits: Limits,
) -> Document {
    let mut bytes = Vec::from(&b"%PDF-1.7\n"[..]);
    let mut offsets = vec![None; 31];

    offsets[1] = Some(bytes.len());
    bytes.extend_from_slice(b"1 0 obj\n<< /Type /Catalog >>\nendobj\n");

    for (stream_number, _member_number, payload) in
        [&first_stream, &second_stream].into_iter().flatten()
    {
        let first = payload
            .iter()
            .position(|byte| *byte == b'(')
            .expect("synthetic object stream has a literal member");
        let count = 1 + additional_compressed
            .iter()
            .filter(|(_, stream, _)| stream == stream_number)
            .count();
        offsets[*stream_number as usize] = Some(bytes.len());
        bytes.extend_from_slice(
            format!(
                "{stream_number} 0 obj\n<< /Type /ObjStm /N {count} /First {first} /Length {} >>\nstream\n",
                payload.len(),
            )
            .as_bytes(),
        );
        bytes.extend_from_slice(&payload);
        bytes.extend_from_slice(b"\nendstream\nendobj\n");
    }

    offsets[30] = Some(bytes.len());
    bytes.extend_from_slice(
        format!(
            "30 0 obj\n<< /Length {} >>\nstream\n",
            general.len()
        )
        .as_bytes(),
    );
    bytes.extend_from_slice(general);
    bytes.extend_from_slice(b"\nendstream\nendobj\n");

    let xref_offset = bytes.len();
    bytes.extend_from_slice(b"xref\n0 31\n0000000000 65535 f \n");
    for number in 1..31 {
        match offsets[number] {
            Some(offset) => bytes.extend_from_slice(
                format!("{offset:010} 00000 n \n").as_bytes(),
            ),
            None => bytes.extend_from_slice(b"0000000000 00000 f \n"),
        }
    }
    bytes.extend_from_slice(
        format!(
            "trailer\n<< /Size 31 /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n"
        )
        .as_bytes(),
    );

    let mut document = Document::open(PdfSource::new(bytes).unwrap(), limits).unwrap();
    let mut entries = vec![XrefEntry::Free; 31];
    entries[1] = XrefEntry::InUse {
        offset: offsets[1].unwrap() as u64,
        generation: 0,
    };
    if let Some((stream_number, member_number, _)) = first_stream {
        entries[stream_number as usize] = XrefEntry::InUse {
            offset: offsets[stream_number as usize].unwrap() as u64,
            generation: 0,
        };
        entries[member_number as usize] = XrefEntry::Compressed {
            stream: stream_number,
            index: 0,
        };
    }
    if let Some((stream_number, member_number, _)) = second_stream {
        entries[stream_number as usize] = XrefEntry::InUse {
            offset: offsets[stream_number as usize].unwrap() as u64,
            generation: 0,
        };
        entries[member_number as usize] = XrefEntry::Compressed {
            stream: stream_number,
            index: 0,
        };
    }
    for (member_number, stream_number, index) in additional_compressed {
        entries[*member_number as usize] = XrefEntry::Compressed {
            stream: *stream_number,
            index: *index,
        };
    }
    entries[30] = XrefEntry::InUse {
        offset: offsets[30].unwrap() as u64,
        generation: 0,
    };
    document.xref = Xref::from_repaired(entries, XrefKind::Table);
    document
}

fn one_member(number: u32, value: &[u8]) -> Vec<u8> {
    object_stream_payload(number, value)
}

fn refusal(error: DocError, kind: LimitKind, requested: u64, maximum: u64) {
    assert!(matches!(
        error,
        DocError::Refused(Refusal { limit, requested: got, maximum: cap })
            if limit == kind && got == requested && cap == maximum
    ));
}

#[test]
fn public_compressed_lookup_and_general_decode_share_totals_in_both_orders() {
    let objstm = one_member(11, b"(a)"); // 8 decoded bytes
    let general = b"1234"; // 4 decoded bytes
    let limits = Limits {
        max_decoded_bytes: 64,
        max_objstm_cache_bytes: 64,
        max_total_decoded_bytes: 11,
        max_total_decode_work_bytes: 128,
        ..Limits::default()
    };

    for object_first in [true, false] {
        let document = lookup_document(
            Some((10, 11, objstm.clone())),
            None,
            &[],
            general,
            limits,
        );
        let mut resolver = document.resolver();
        if object_first {
            assert!(resolver.get(ObjectRef::new(11, 0)).is_ok());
            let ordinary = resolver.get(ObjectRef::new(30, 0)).unwrap();
            refusal(
                resolver.decode_stream(&ordinary).unwrap_err(),
                LimitKind::TotalDecodedBytes,
                12,
                11,
            );
        } else {
            // Resolve the ordinary stream before borrowing it for the shared resolver.
            let ordinary = resolver.get(ObjectRef::new(30, 0)).unwrap();
            assert_eq!(resolver.decode_stream(&ordinary).unwrap().bytes, general);
            refusal(
                resolver.get(ObjectRef::new(11, 0)).unwrap_err(),
                LimitKind::TotalDecodedBytes,
                12,
                11,
            );
        }
    }

    let exact_limits = Limits {
        max_decoded_bytes: 64,
        max_objstm_cache_bytes: 64,
        max_total_decoded_bytes: 12,
        max_total_decode_work_bytes: 128,
        ..Limits::default()
    };
    for object_first in [true, false] {
        let document = lookup_document(
            Some((10, 11, objstm.clone())),
            None,
            &[],
            general,
            exact_limits,
        );
        let mut resolver = document.resolver();
        if object_first {
            assert!(resolver.get(ObjectRef::new(11, 0)).is_ok());
            let ordinary = resolver.get(ObjectRef::new(30, 0)).unwrap();
            assert_eq!(resolver.decode_stream(&ordinary).unwrap().bytes, general);
        } else {
            let ordinary = resolver.get(ObjectRef::new(30, 0)).unwrap();
            assert_eq!(resolver.decode_stream(&ordinary).unwrap().bytes, general);
            assert!(resolver.get(ObjectRef::new(11, 0)).is_ok());
        }
    }
}

#[test]
fn compressed_cache_hit_does_not_charge_objstm_output_again() {
    let mut payload = Vec::from(&b"11 0 12 3 "[..]);
    payload.extend_from_slice(b"(a)(b)"); // 16 bytes total
    let document = lookup_document(
        Some((10, 11, payload)),
        None,
        &[(12, 10, 1)],
        b"x",
        Limits {
            max_decoded_bytes: 64,
            max_objstm_cache_bytes: 64,
            max_total_decoded_bytes: 17,
            max_total_decode_work_bytes: 128,
            ..Limits::default()
        },
    );
    let mut resolver = document.resolver();
    assert!(resolver.get(ObjectRef::new(11, 0)).is_ok()); // cache miss, 16 output bytes
    assert!(resolver.get(ObjectRef::new(12, 0)).is_ok()); // cache hit, zero decode bytes
    let general = resolver.get(ObjectRef::new(30, 0)).unwrap();
    assert_eq!(resolver.decode_stream(&general).unwrap().bytes, b"x");
}

#[test]
fn evicted_object_stream_reload_is_charged_again() {
    let first = one_member(11, b"(a)"); // 8 bytes
    let second = one_member(21, b"(b)"); // 8 bytes
    let one_cache_cost = (first.len() as u64)
        .saturating_add(core::mem::size_of::<(u32, core::ops::Range<usize>)>() as u64);
    let document = lookup_document(
        Some((10, 11, first)),
        Some((20, 21, second)),
        &[],
        b"1234",
        Limits {
            max_decoded_bytes: 64,
            max_objstm_cache_bytes: one_cache_cost.saturating_add(1),
            max_total_decoded_bytes: 24,
            max_total_decode_work_bytes: 128,
            ..Limits::default()
        },
    );
    let mut resolver = document.resolver();
    assert!(resolver.get(ObjectRef::new(11, 0)).is_ok()); // decode container 10
    assert!(resolver.get(ObjectRef::new(21, 0)).is_ok()); // evicts container 10
    assert!(resolver.get(ObjectRef::new(11, 0)).is_ok()); // reload and decode 10 again
    let general = resolver.get(ObjectRef::new(30, 0)).unwrap();
    refusal(
        resolver.decode_stream(&general).unwrap_err(),
        LimitKind::TotalDecodedBytes,
        25,
        24,
    );
}
