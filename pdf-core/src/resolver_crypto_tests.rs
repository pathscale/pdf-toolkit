use super::*;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use alloc::sync::Arc;
use crate::object::parse_indirect;
use crate::encryption::{ChaChaKey, KdfParams};

const PASSWORD: &[u8] = b"synthetic-test-password";
const SALT: [u8; 16] = [0x31; 16];
const FILE_ID: [u8; 16] = [0x62; 16];
const STREAM_ID: ObjectRef = ObjectRef::new(1, 0);

fn hex(bytes: &[u8]) -> String {
    let mut result = String::new();
    for byte in bytes {
        result.push_str(&format!("{byte:02x}"));
    }
    result
}

fn private_key(limits: &Limits) -> ChaChaKey {
    ChaChaKey::derive(
        PASSWORD,
        &SALT,
        KdfParams {
            memory_kib: 8,
            iterations: 1,
            parallelism: 1,
        },
        limits,
    )
    .unwrap()
}

fn synthetic_document(
    stream_bytes: &[u8],
    stream_entries: &str,
    check_token: Option<&[u8; 16]>,
    limits: Limits,
) -> Result<Document, DocError> {
    let mut bytes = Vec::from(&b"%PDF-1.7\n"[..]);
    let mut offsets = Vec::new();

    offsets.push(bytes.len());
    bytes.extend_from_slice(
        format!(
            "1 0 obj\n<< /Length {} {} >>\nstream\n",
            stream_bytes.len(),
            stream_entries
        )
        .as_bytes(),
    );
    bytes.extend_from_slice(stream_bytes);
    bytes.extend_from_slice(b"\nendstream\nendobj\n");

    if let Some(check_token) = check_token {
        offsets.push(bytes.len());
        bytes.extend_from_slice(
            format!(
                "2 0 obj\n<< /Filter /PSChaCha /V 1 /Length 256 /KDF /Argon2id /M 8 /T 1 /P 1 /Salt <{}> /Check <{}> >>\nendobj\n",
                hex(&SALT),
                hex(check_token)
            )
            .as_bytes(),
        );
    }

    let xref_offset = bytes.len();
    let count = offsets.len() + 1;
    bytes.extend_from_slice(format!("xref\n0 {count}\n").as_bytes());
    bytes.extend_from_slice(b"0000000000 65535 f \n");
    for offset in offsets {
        bytes.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    if check_token.is_some() {
        bytes.extend_from_slice(
            format!(
                "trailer\n<< /Size {count} /Root 1 0 R /Encrypt 2 0 R /ID [<{}><{}>] >>\nstartxref\n{xref_offset}\n%%EOF\n",
                hex(&FILE_ID),
                hex(&FILE_ID)
            )
            .as_bytes(),
        );
        Document::open_with_password(
            PdfSource::new(bytes).unwrap(),
            limits,
            PASSWORD,
        )
    } else {
        bytes.extend_from_slice(
            format!(
                "trailer\n<< /Size {count} /Root 1 0 R /ID [<{}><{}>] >>\nstartxref\n{xref_offset}\n%%EOF\n",
                hex(&FILE_ID),
                hex(&FILE_ID)
            )
            .as_bytes(),
        );
        Document::open(PdfSource::new(bytes).unwrap(), limits)
    }
}

#[test]
fn valid_private_encryption_exhaustion_keeps_decode_work_origin() {
    let limits = Limits {
        max_decoded_bytes: 16384,
        max_total_decoded_bytes: 128,
        max_total_decode_work_bytes: 51,
        ..Limits::default()
    };
    let key = private_key(&limits);
    let check = key.password_check(&SALT, KdfParams {
        memory_kib: 8,
        iterations: 1,
        parallelism: 1,
    }).unwrap();
    let ciphertext = key.seal(STREAM_ID, [0x17; 24], b"123456").unwrap();
    let document = synthetic_document(&ciphertext, "", Some(&check), limits).unwrap();
    let stream = document.resolver().get(STREAM_ID).unwrap();

    let error = document.resolver().decode_stream(&stream).unwrap_err();
    assert_eq!(
        error,
        DocError::Refused(Refusal {
            limit: LimitKind::DecodeWorkBytes,
            requested: 52,
            maximum: 51,
        })
    );
}

#[test]
fn private_authentication_error_is_rejected_during_eager_document_open() {
    let limits = Limits {
        max_decoded_bytes: 16384,
        max_total_decoded_bytes: 128,
        ..Limits::default()
    };
    let key = private_key(&limits);
    let check = key.password_check(&SALT, KdfParams {
        memory_kib: 8,
        iterations: 1,
        parallelism: 1,
    }).unwrap();
    let mut ciphertext = key.seal(STREAM_ID, [0x29; 24], b"authenticated bytes").unwrap();
    let tag_byte = ciphertext.len() - 1;
    ciphertext[tag_byte] ^= 1;

    let error = match synthetic_document(&ciphertext, "", Some(&check), limits) {
        Err(error) => error,
        Ok(_) => panic!("corrupt authenticated stream unexpectedly opened"),
    };
    assert_eq!(
        error,
        DocError::Malformed {
            offset: 0,
            what: "PSChaCha authentication",
        }
    );
}

#[test]
fn private_authentication_error_is_returned_without_reclassification() {
    let limits = Limits {
        max_decoded_bytes: 16384,
        max_total_decoded_bytes: 1024,
        max_total_decode_work_bytes: 1024,
        ..Limits::default()
    };
    let key = private_key(&limits);
    let check = key.password_check(&SALT, KdfParams {
        memory_kib: 8,
        iterations: 1,
        parallelism: 1,
    }).unwrap();
    let valid_ciphertext = key.seal(STREAM_ID, [0x29; 24], b"authenticated bytes").unwrap();
    let document = synthetic_document(&valid_ciphertext, "", Some(&check), limits).unwrap();

    let mut corrupt_ciphertext = valid_ciphertext;
    let tag_byte = corrupt_ciphertext.len() - 1;
    corrupt_ciphertext[tag_byte] ^= 1;
    let mut bytes = Vec::from(&b"1 0 obj\n"[..]);
    bytes.extend_from_slice(
        format!("<< /Length {} >>\nstream\n", corrupt_ciphertext.len()).as_bytes(),
    );
    bytes.extend_from_slice(&corrupt_ciphertext);
    bytes.extend_from_slice(b"\nendstream\nendobj\n");
    let backing: Arc<[u8]> = Arc::from(bytes.into_boxed_slice());
    let corrupt_stream = parse_indirect(&backing, 0, STREAM_ID, &limits).unwrap();

    assert_eq!(
        document.resolver().decode_stream(&corrupt_stream).unwrap_err(),
        DocError::Malformed {
            offset: 0,
            what: "PSChaCha authentication",
        }
    );
}

#[test]
fn unresolved_crypt_name_preserves_original_dependency_error() {
    let document = synthetic_document(
        b"synthetic bytes",
        "/Filter /Crypt /DecodeParms << /Name 999 0 R >>",
        None,
        Limits::default(),
    )
    .unwrap();
    let stream = document.resolver().get(STREAM_ID).unwrap();

    assert_eq!(
        document.resolver().decode_stream(&stream).unwrap_err(),
        DocError::MissingObject {
            id: ObjectRef::new(999, 0),
        }
    );
}

#[test]
fn private_crypt_filter_rejects_before_resolving_name_dependency() {
    let limits = Limits {
        max_decoded_bytes: 16384,
        max_total_decoded_bytes: 128,
        ..Limits::default()
    };
    let key = private_key(&limits);
    let check = key.password_check(&SALT, KdfParams {
        memory_kib: 8,
        iterations: 1,
        parallelism: 1,
    }).unwrap();
    let ciphertext = key.seal(STREAM_ID, [0x43; 24], b"valid private payload").unwrap();
    let document = synthetic_document(
        &ciphertext,
        "/Filter /Crypt /DecodeParms << /Name 999 0 R >>",
        Some(&check),
        limits,
    )
    .unwrap();
    let stream = document.resolver().get(STREAM_ID).unwrap();

    assert_eq!(
        document.resolver().decode_stream(&stream).unwrap_err(),
        DocError::Unsupported {
            feature: "PSChaCha Crypt filter",
        }
    );
}
