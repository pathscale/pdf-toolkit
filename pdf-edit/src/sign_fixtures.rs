//! The signed house fixtures, shared by pdf-sign-fixtures (which writes them)
//! and pdf-roundtrip (which regenerates and compares them byte for byte).
//!
//! Keys are derived from fixed labels with SHA-2 and the certificates are
//! self-signed with RFC 6979 nonces, so every byte is reproducible. These are
//! test identities only.

use std::sync::Arc;

use pdf_core::Limits;
use pdf_toolkit_edit::sign::{self, Curve, SignOptions, Signer, SubFilter};
use pdf_toolkit_edit::Editor;
use pdf_write::doc::{
    Align, Color, DocumentBuilder, Margins, Metadata, PageSetup, PageSize, PdfDate, StandardFont,
    TextStyle, WriteOptions,
};
use sha2::Digest;

pub const SUBJECT_P256: &str = "CN=pdf-toolkit house signer P-256,O=pathscale";
pub const SUBJECT_P384: &str = "CN=pdf-toolkit house signer P-384,O=pathscale";
const BODY: &str = "This page was written by DocumentBuilder and then signed with an \
    incremental update. Later revisions leave the signed bytes untouched.";
pub const TEXT_NORM: &str = "Signed house document This page was written by DocumentBuilder \
    and then signed with an incremental update. Later revisions leave the signed bytes \
    untouched.";
pub const EDITED_TITLE: &str = "Edited after signing";

pub struct SignedFixture {
    pub name: &'static str,
    pub pdf: Vec<u8>,
    pub expected: String,
}

fn fail(error: impl std::fmt::Debug) -> String {
    format!("{error:?}")
}

pub fn identity(curve: Curve) -> Result<Signer, String> {
    let (scalar, subject, serial) = match curve {
        Curve::P256 => (
            sha2::Sha256::digest(b"pdf-toolkit house signer P-256").to_vec(),
            SUBJECT_P256,
            0x256,
        ),
        Curve::P384 => (
            sha2::Sha384::digest(b"pdf-toolkit house signer P-384").to_vec(),
            SUBJECT_P384,
            0x384,
        ),
    };
    let certificate = sign::self_signed_certificate(
        curve,
        &scalar,
        subject,
        serial,
        &PdfDate::utc(2026, 1, 1, 0, 0, 0),
        &PdfDate::utc(2036, 1, 1, 0, 0, 0),
    )
    .map_err(fail)?;
    Ok(Signer {
        curve,
        scalar,
        certificate,
        chain: Vec::new(),
    })
}

pub fn limits() -> Limits {
    Limits::default()
}

fn base() -> Result<Vec<u8>, String> {
    let mut doc = DocumentBuilder::new();
    doc.metadata(&Metadata {
        title: Some("Signed house document".to_owned()),
        author: Some("House author".to_owned()),
        creation_date: Some(PdfDate::utc(2026, 4, 1, 11, 0, 0)),
        ..Metadata::default()
    });
    let bold = doc.standard_font(StandardFont::HelveticaBold);
    let body = doc.standard_font(StandardFont::TimesRoman);
    {
        let mut page =
            doc.page_with(&PageSetup::new(PageSize::Letter).margins(Margins::uniform(72.0)));
        let [x, y, width, height] = page.content_box();
        page.paragraph(
            &TextStyle::new(bold, 18.0),
            [x, y + height - 30.0, width, 30.0],
            "Signed house document",
        );
        page.paragraph(
            &TextStyle::new(body, 12.0).align(Align::Justify),
            [x, y + height - 120.0, width, 80.0],
            BODY,
        );
        page.stroke_rect(72.0, 520.0, 200.0, 60.0, 1.0, Color::rgb(0.2, 0.2, 0.6));
    }
    if let Some(error) = doc.error() {
        return Err(fail(error));
    }
    doc.finish(&WriteOptions::default()).map_err(fail)
}

pub fn first_options() -> SignOptions {
    let mut options = SignOptions::new("HouseSignature", PdfDate::utc(2026, 4, 1, 12, 0, 0));
    options.name = Some("House signer".to_owned());
    options.reason = Some("Fixture approval".to_owned());
    options.location = Some("House lab".to_owned());
    options
}

pub fn second_options() -> SignOptions {
    let mut options = SignOptions::new(
        "SecondSignature",
        PdfDate {
            year: 2026,
            month: 4,
            day: 2,
            hour: 8,
            minute: 30,
            second: 0,
            offset_minutes: 120,
        },
    );
    options.sub_filter = SubFilter::CadesDetached;
    options.rect = [72.0, 520.0, 272.0, 580.0];
    options.reason = Some("Countersigned".to_owned());
    options
}

fn header(sections: u32) -> String {
    format!(
        "version=1.7\nxref_kind=stream\nxref_sections={sections}\nencrypted=false\n\
         refusal=none\npages=1\npage_errors=0\npage.0.media_box=0 0 612 792\n\
         page.0.text_norm={TEXT_NORM}\n"
    )
}

fn first_signature(index: usize, modified: bool, later: u32) -> String {
    format!(
        "signature.{index}.field=HouseSignature\nsignature.{index}.sub_filter=adbe.pkcs7.detached\n\
         signature.{index}.algorithm=ecdsa-p256-sha256\nsignature.{index}.digest=match\n\
         signature.{index}.signature=valid\nsignature.{index}.signer={SUBJECT_P256}\n\
         signature.{index}.signing_time=2026-04-01T12:00:00Z\n\
         signature.{index}.covers_revision=true\nsignature.{index}.modified_after={modified}\n\
         signature.{index}.later_revisions={later}\n"
    )
}

pub fn build() -> Result<Vec<SignedFixture>, String> {
    let first = identity(Curve::P256)?;
    let second = identity(Curve::P384)?;
    let signed =
        sign::sign(Arc::from(base()?), limits(), &first, &first_options()).map_err(fail)?;
    let twice = sign::sign(
        Arc::from(signed.clone()),
        limits(),
        &second,
        &second_options(),
    )
    .map_err(fail)?;
    let edited = {
        let mut editor = Editor::open(Arc::from(signed.clone()), limits()).map_err(fail)?;
        editor.set_info("Title", EDITED_TITLE);
        editor
            .add_uri_link(
                0,
                [72.0, 400.0, 272.0, 420.0],
                "https://example.test/after-signing",
            )
            .map_err(fail)?;
        editor.save().map_err(fail)?
    };

    let mut fixtures = Vec::new();
    let mut expected = header(2);
    expected.push_str("info.title=Signed house document\nsignatures=1\n");
    expected.push_str(&first_signature(0, false, 0));
    fixtures.push(SignedFixture {
        name: "author-signed",
        pdf: signed,
        expected,
    });

    let mut expected = header(3);
    expected.push_str("signatures=2\n");
    expected.push_str(&first_signature(0, true, 1));
    expected.push_str(&format!(
        "signature.1.field=SecondSignature\nsignature.1.sub_filter=ETSI.CAdES.detached\n\
         signature.1.algorithm=ecdsa-p384-sha384\nsignature.1.digest=match\n\
         signature.1.signature=valid\nsignature.1.signer={SUBJECT_P384}\n\
         signature.1.signing_time=2026-04-02T08:30:00+02:00\n\
         signature.1.covers_revision=true\nsignature.1.modified_after=false\n\
         signature.1.later_revisions=0\n"
    ));
    fixtures.push(SignedFixture {
        name: "author-signed-twice",
        pdf: twice,
        expected,
    });

    let mut expected = header(3);
    expected.push_str(&format!(
        "info.title={EDITED_TITLE}\npage.0.links=uri:https://example.test/after-signing\nsignatures=1\n"
    ));
    expected.push_str(&first_signature(0, true, 1));
    fixtures.push(SignedFixture {
        name: "author-signed-edited",
        pdf: edited,
        expected,
    });
    Ok(fixtures)
}
