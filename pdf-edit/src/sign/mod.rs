//! PDF signatures with ECDSA P-256 and P-384, signed and checked offline.
//!
//! `sign` appends an incremental update holding a signature field, its widget
//! and a `/Sig` dictionary (`/Filter /Adobe.PPKLite`, `/SubFilter
//! /adbe.pkcs7.detached` or `/ETSI.CAdES.detached`), then fills the
//! `/ByteRange` and the detached CMS `SignedData` in `/Contents`. Earlier
//! revisions are never rewritten, so earlier signatures stay valid.
//!
//! The caller supplies the private scalar, its certificate and the signing
//! time; nothing reads a clock or a random source. ECDSA nonces follow
//! RFC 6979, so the same inputs give the same bytes.
//!
//! `verify` reports, per signature field: whether the byte range digest
//! matches the CMS `messageDigest`, whether the ECDSA signature over the
//! signed attributes is valid for the signer certificate's key, the signer
//! subject, and whether bytes or revisions follow the signed revision. It does
//! not build or check certificate chains, revocation or timestamps, and makes
//! no network requests. RSA and other algorithms are reported as
//! `unsupported_algorithm`, never checked.

use std::sync::Arc;

use pdf_core::{DocError, Limits, ObjectRef};
use pdf_write::doc::PdfDate;
use pdf_write::Value;

use crate::{get, malformed, set, value, wrong, Editor, Pending};

mod cms;
mod verify;

pub use cms::self_signed_certificate;
pub use verify::{verify, DigestCheck, SignatureCheck, SignatureReport};

/// Bytes reserved for the DER `SignedData` inside `/Contents`.
const DEFAULT_RESERVE: usize = 8192;
const MAX_RESERVE: usize = 1 << 20;
const BYTE_RANGE_PLACEHOLDER: &[u8] = b"[0 9999999999 9999999999 9999999999]";

/// The signing curve. P-256 signs with SHA-256 and P-384 with SHA-384.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Curve {
    P256,
    P384,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SubFilter {
    /// `adbe.pkcs7.detached`, with a CMS `signingTime` attribute.
    #[default]
    Pkcs7Detached,
    /// `ETSI.CAdES.detached` (PAdES baseline B): `signingCertificateV2`
    /// instead of `signingTime`; the time is carried by `/M` only.
    CadesDetached,
}

impl SubFilter {
    pub fn name(self) -> &'static str {
        match self {
            Self::Pkcs7Detached => "adbe.pkcs7.detached",
            Self::CadesDetached => "ETSI.CAdES.detached",
        }
    }
}

/// A caller-supplied signing identity.
#[derive(Clone, Debug)]
pub struct Signer {
    pub curve: Curve,
    /// The private scalar, big-endian, 32 bytes for P-256 and 48 for P-384.
    pub scalar: Vec<u8>,
    /// DER X.509 certificate whose public key matches `scalar`.
    pub certificate: Vec<u8>,
    /// Further DER certificates to embed (intermediates), signer excluded.
    pub chain: Vec<Vec<u8>>,
}

#[derive(Clone, Debug)]
pub struct SignOptions {
    /// Partial field name (`/T`); must not already exist at the top level.
    pub field: String,
    /// Page index that receives the widget annotation.
    pub page: u32,
    /// Widget rectangle; `[0, 0, 0, 0]` makes an invisible signature.
    pub rect: [f32; 4],
    /// Signing time, written to `/M` and (for PKCS#7) the CMS attributes.
    pub time: PdfDate,
    pub sub_filter: SubFilter,
    pub name: Option<String>,
    pub reason: Option<String>,
    pub location: Option<String>,
    pub contact: Option<String>,
    /// Bytes reserved for the CMS blob; 0 selects 8192.
    pub reserve: usize,
}

impl SignOptions {
    pub fn new(field: &str, time: PdfDate) -> Self {
        Self {
            field: field.to_owned(),
            page: 0,
            rect: [0.0; 4],
            time,
            sub_filter: SubFilter::default(),
            name: None,
            reason: None,
            location: None,
            contact: None,
            reserve: 0,
        }
    }
}

fn unsupported(feature: &'static str) -> DocError {
    DocError::Unsupported { feature }
}

fn reference(id: ObjectRef) -> Value {
    Value::Ref(id.number(), id.generation())
}

/// Signs `bytes` and returns the file with one more incremental revision.
/// Encrypted files are refused.
pub fn sign(
    bytes: Arc<[u8]>,
    limits: Limits,
    signer: &Signer,
    options: &SignOptions,
) -> Result<Vec<u8>, DocError> {
    let reserve = if options.reserve == 0 {
        DEFAULT_RESERVE
    } else {
        options.reserve
    };
    if reserve > MAX_RESERVE {
        return Err(unsupported("sign:reserve too large"));
    }
    if options.field.is_empty() || options.field.contains('.') {
        return Err(malformed("signature field name"));
    }
    if options.rect.iter().any(|number| !number.is_finite()) {
        return Err(malformed("signature rectangle"));
    }
    let time = options
        .time
        .to_pdf_string()
        .map_err(|_| malformed("signing time"))?;
    // Check the key and certificate before touching the document.
    cms::check_signer(signer)?;

    let original = bytes.len();
    let mut editor = Editor::open(bytes, limits)?;
    if editor.document.encrypted() {
        return Err(unsupported("sign:encrypted document"));
    }
    add_field(&mut editor, options, &time, reserve)?;
    let mut out = editor.save()?;

    let (range_at, contents_at, contents_end) = placeholders(&out, original)?;
    let after = contents_end;
    let total = out.len();
    let byte_range = [0, contents_at, after, total - after];
    let mut text = format!("[0 {} {} {}]", byte_range[1], byte_range[2], byte_range[3]);
    if text.len() > BYTE_RANGE_PLACEHOLDER.len() {
        return Err(malformed("signature byte range"));
    }
    while text.len() < BYTE_RANGE_PLACEHOLDER.len() {
        text.push(' ');
    }
    out.get_mut(range_at..range_at + BYTE_RANGE_PLACEHOLDER.len())
        .ok_or_else(|| malformed("signature byte range"))?
        .copy_from_slice(text.as_bytes());

    let signed = [
        out.get(..contents_at)
            .ok_or_else(|| malformed("signature byte range"))?,
        out.get(after..)
            .ok_or_else(|| malformed("signature byte range"))?,
    ];
    let der = cms::signed_data(signer, options, &signed)?;
    if der.len() > reserve {
        return Err(unsupported("sign:signature larger than reserved space"));
    }
    let mut hex = String::with_capacity(der.len() * 2);
    for byte in &der {
        hex.push_str(&format!("{byte:02x}"));
    }
    out.get_mut(contents_at + 1..contents_at + 1 + hex.len())
        .ok_or_else(|| malformed("signature contents"))?
        .copy_from_slice(hex.as_bytes());
    Ok(out)
}

/// Finds the ByteRange placeholder and the Contents hex string after `from`.
fn placeholders(out: &[u8], from: usize) -> Result<(usize, usize, usize), DocError> {
    let tail = out
        .get(from..)
        .ok_or_else(|| malformed("signature placeholder"))?;
    let find = |needle: &[u8]| {
        tail.windows(needle.len())
            .position(|window| window == needle)
            .map(|at| at + from)
    };
    let range_key = find(b"/ByteRange ").ok_or_else(|| malformed("signature ByteRange"))?;
    let range_at = range_key + b"/ByteRange ".len();
    if out.get(range_at..range_at + BYTE_RANGE_PLACEHOLDER.len()) != Some(BYTE_RANGE_PLACEHOLDER) {
        return Err(malformed("signature ByteRange"));
    }
    let contents_key = find(b"/Contents <").ok_or_else(|| malformed("signature Contents"))?;
    let contents_at = contents_key + b"/Contents ".len();
    let length = out
        .get(contents_at + 1..)
        .unwrap_or(&[])
        .iter()
        .position(|byte| *byte == b'>')
        .ok_or_else(|| malformed("signature Contents"))?;
    Ok((range_at, contents_at, contents_at + 1 + length + 1))
}

fn field_names(editor: &Editor, fields: &[Value]) -> Result<Vec<Vec<u8>>, DocError> {
    let mut names = Vec::new();
    for field in fields {
        let Value::Ref(number, generation) = field else {
            continue;
        };
        let dict = editor.dict_at(ObjectRef::new(*number, *generation))?;
        match get(&dict, "T") {
            Some(Value::Str(name)) | Some(Value::HexStr(name)) => names.push(name.clone()),
            _ => {}
        }
    }
    Ok(names)
}

fn add_field(
    editor: &mut Editor,
    options: &SignOptions,
    time: &str,
    reserve: usize,
) -> Result<(), DocError> {
    editor.clean()?;
    let limits = *editor.document.limits();
    let root = editor
        .document
        .root_ref()
        .ok_or_else(|| malformed("catalog"))?;
    let mut catalog = editor.dict_at(root)?;
    // The AcroForm may be direct in the catalog or an indirect object.
    let (form_id, mut form) = match get(&catalog, "AcroForm") {
        Some(Value::Ref(number, generation)) => {
            let id = ObjectRef::new(*number, *generation);
            (Some(id), editor.dict_at(id)?)
        }
        Some(Value::Dict(dict)) => (None, dict.clone()),
        Some(Value::Null) | None => (None, Vec::new()),
        Some(_) => return Err(wrong(Some(root), "AcroForm dictionary")),
    };
    let slot = get(&form, "Fields").cloned().unwrap_or(Value::Null);
    let mut fields = editor.array_slot(slot, false)?;
    let field_name = value::text_string(&options.field, &limits)?;
    if field_names(editor, &fields)?.contains(&field_name) {
        return Err(malformed("signature field name already exists"));
    }

    let page_id = editor.page_ref(options.page)?;
    let mut page = editor.dict_at(page_id)?;
    let slot = get(&page, "Annots").cloned().unwrap_or(Value::Null);
    let mut annots = editor.array_slot(slot, false)?;

    let first = editor.next;
    let after = editor.fresh(3)?;
    let signature_id = ObjectRef::new(first, 0);
    let field_id = ObjectRef::new(first + 1, 0);
    let appearance_id = ObjectRef::new(first + 2, 0);

    let mut signature = vec![
        ("Type".to_owned(), Value::Name("Sig".to_owned())),
        ("Filter".to_owned(), Value::Name("Adobe.PPKLite".to_owned())),
        (
            "SubFilter".to_owned(),
            Value::Name(options.sub_filter.name().to_owned()),
        ),
        (
            "ByteRange".to_owned(),
            Value::Array(vec![
                Value::Int(0),
                Value::Int(9_999_999_999),
                Value::Int(9_999_999_999),
                Value::Int(9_999_999_999),
            ]),
        ),
        ("Contents".to_owned(), Value::HexStr(vec![0; reserve])),
        ("M".to_owned(), Value::Str(time.as_bytes().to_vec())),
    ];
    for (key, text) in [
        ("Name", &options.name),
        ("Reason", &options.reason),
        ("Location", &options.location),
        ("ContactInfo", &options.contact),
    ] {
        if let Some(text) = text {
            signature.push((
                key.to_owned(),
                Value::Str(value::text_string(text, &limits)?),
            ));
        }
    }
    let [x0, y0, x1, y1] = options.rect;
    let rect = [x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1)];
    let width = f64::from(rect[2] - rect[0]);
    let height = f64::from(rect[3] - rect[1]);
    // An empty appearance stream keeps viewers from drawing their own.
    let appearance = Value::Dict(vec![
        ("Type".to_owned(), Value::Name("XObject".to_owned())),
        ("Subtype".to_owned(), Value::Name("Form".to_owned())),
        (
            "BBox".to_owned(),
            Value::Array(vec![
                Value::Int(0),
                Value::Int(0),
                Value::Real(width),
                Value::Real(height),
            ]),
        ),
        ("Length".to_owned(), Value::Int(0)),
    ]);
    let widget = Value::Dict(vec![
        ("Type".to_owned(), Value::Name("Annot".to_owned())),
        ("Subtype".to_owned(), Value::Name("Widget".to_owned())),
        ("FT".to_owned(), Value::Name("Sig".to_owned())),
        ("T".to_owned(), Value::Str(field_name)),
        ("V".to_owned(), reference(signature_id)),
        ("F".to_owned(), Value::Int(132)),
        ("P".to_owned(), reference(page_id)),
        (
            "Rect".to_owned(),
            Value::Array(
                rect.into_iter()
                    .map(|number| Value::Real(f64::from(number)))
                    .collect(),
            ),
        ),
        (
            "AP".to_owned(),
            Value::Dict(vec![("N".to_owned(), reference(appearance_id))]),
        ),
    ]);
    annots.push(reference(field_id));
    set(&mut page, "Annots", Value::Array(annots));
    fields.push(reference(field_id));
    set(&mut form, "Fields", Value::Array(fields));
    let flags = match get(&form, "SigFlags") {
        Some(Value::Int(flags)) => flags | 3,
        _ => 3,
    };
    set(&mut form, "SigFlags", Value::Int(flags));

    let mut changes = vec![
        (signature_id, Value::Dict(signature), None),
        (field_id, widget, None),
        (
            appearance_id,
            appearance,
            Some(crate::import::StreamData::owned(Arc::from(&[][..]))),
        ),
        (page_id, Value::Dict(page), None),
    ];
    match form_id {
        Some(id) => changes.push((id, Value::Dict(form), None)),
        None => {
            set(&mut catalog, "AcroForm", Value::Dict(form));
        }
    }
    if form_id.is_none() {
        changes.push((root, Value::Dict(catalog), None));
    }
    let mut costs = Vec::new();
    for (id, body, data) in &changes {
        let bytes = data.as_ref().map(|_| &[][..]);
        costs.push((*id, editor.cost(*id, body, bytes)?));
    }
    let total = editor.changed_size(&costs)?;
    for ((id, body, data), (_, cost)) in changes.into_iter().zip(costs) {
        editor.install(Pending {
            id,
            value: body,
            data,
            cost,
        });
    }
    editor.pending_bytes = total;
    editor.next = after;
    Ok(())
}
