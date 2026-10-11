# Authoring PDFs

`pdf_toolkit_write::doc::DocumentBuilder` writes complete documents: pages with
margins and page boxes, metadata, wrapped and aligned text in standard or
embedded fonts, vector graphics, JPEG and PNG images, links and an outline.
Through the umbrella crate it is `pdf_toolkit::author` (feature `author`).
Output is deterministic: the same calls give the same bytes, and nothing reads
the clock or a random source (encrypted output excepted, see below).

## A complete example

Two A5 pages with a title, an image, justified paragraphs in an embedded font,
an internal link, a web link and a two-level outline. This is
`pdf-write/examples/authoring.rs`:

```sh
cargo run -p pdf-toolkit-write --example authoring -- FONT.ttf LOGO.png out.pdf
```

The house fixture `fixtures/house/author-example.pdf` is the same document with
a generated font, and its `.expect` file records the extracted lines, links and
outline that `pdf-inspect check` verifies.

```rust
use std::sync::Arc;

use pdf_toolkit_write::doc::{
    Align, Color, DocumentBuilder, Margins, Metadata, PageSetup, PageSize, PdfDate, StandardFont,
    TextStyle, WriteOptions,
};

const FIRST: &str = "Field notes are short pages written while the work is fresh. This one \
    shows a title, a small picture, wrapped paragraphs in an embedded font, a link and an \
    outline.\nEvery line here is placed by the builder: words wrap at spaces, the last line of \
    each paragraph stays ragged, and the other lines stretch to both edges.";
const SECOND: &str = "The second page continues the notes. Its outline entry points here, \
    and the link on the first page jumps to this heading.";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [font, logo, out] = args.as_slice() else {
        return Err("usage: authoring FONT.ttf LOGO.png OUT.pdf".into());
    };

    let mut doc = DocumentBuilder::new();
    doc.metadata(&Metadata {
        title: Some("Field notes".to_owned()),
        author: Some("House author".to_owned()),
        // Dates are always supplied by the caller; the builder never reads the clock.
        creation_date: Some(PdfDate::utc(2026, 3, 1, 9, 30, 0)),
        ..Metadata::default()
    });
    // An embedded TrueType font is subset, written as Identity-H Type0 with a
    // ToUnicode map, so any text it covers can be copied back out.
    let body = doc.truetype_font(Arc::from(std::fs::read(font)?))?;
    let bold = doc.standard_font(StandardFont::HelveticaBold);
    let logo = doc.png(&std::fs::read(logo)?);

    let heading = TextStyle::new(bold, 24.0);
    let text = TextStyle::new(body, 11.0).align(Align::Justify).line_height(1.4);
    let setup = PageSetup::new(PageSize::A5).margins(Margins::uniform(36.0));

    let top;
    {
        let mut page = doc.page_with(&setup);
        // [x, y, width, height] of the page inside its margins.
        let [x, y, width, height] = page.content_box();
        top = y + height;
        page.paragraph(&heading, [x, top - 30.0, width, 30.0], "Field notes");
        page.image(logo, x, top - 110.0, 64.0, 64.0);
        let flow = page.paragraph(&text, [x, y, width, height - 130.0], FIRST);
        // A paragraph that does not fit reports where it stopped.
        assert!(flow.complete);
        page.paragraph(
            &TextStyle::new(body, 11.0).color(Color::rgb(0.0, 0.2, 0.8)),
            [x + 80.0, top - 80.0, width - 80.0, 20.0],
            "Next page",
        );
        page.link_page(x + 80.0, top - 80.0, width - 80.0, 20.0, 1, top);
    }
    {
        let mut page = doc.page_with(&setup);
        let [x, y, width, height] = page.content_box();
        page.paragraph(&heading, [x, top - 30.0, width, 30.0], "Continued");
        page.paragraph(&text, [x, y, width, height - 40.0], SECOND);
        page.link_uri(x, y, width, 20.0, "https://github.com/pathscale/pdf-toolkit");
    }
    doc.outline("Field notes", 1, 0, top);
    doc.outline("Continued", 2, 1, top);

    // Errors are retained by the builder and returned here.
    let bytes = doc.finish(&WriteOptions::default())?;
    std::fs::write(out, bytes)?;
    Ok(())
}
```

## Pages

`doc.page(width, height)` adds a page in points. `doc.page_with(&PageSetup)`
takes a preset or custom size, margins and optional boxes:

- `PageSize::{Letter, Legal, Tabloid, A3, A4, A5, B5, Custom { width, height }}`,
  `.landscape()` swaps the sides, `.dimensions()` returns them.
- `Margins { top, right, bottom, left }` or `Margins::uniform(points)`.
  `PageBuilder::content_box()` returns `[x, y, width, height]` inside them.
- `.crop_box`, `.bleed_box`, `.trim_box`, `.art_box` take `[x0, y0, x1, y1]`
  and must lie inside the media box.
- `doc.page_at(index)` reopens an earlier page, for example to add a link to a
  page that did not exist yet. `doc.page_count()` counts pages.

## Metadata

`doc.metadata(&Metadata { title, author, subject, keywords, creator, producer,
creation_date, modification_date })` replaces the Info dictionary. Dates are
`PdfDate { year, month, day, hour, minute, second, offset_minutes }` or
`PdfDate::utc(...)`; they are validated and written as
`D:YYYYMMDDHHmmSSZ` or `D:YYYYMMDDHHmmSS+HH'mm'`. Non-ASCII text is written
as UTF-16BE.

## Text

Fonts:

- `doc.truetype_font(bytes)` embeds a TrueType font, or an OpenType font with
  TrueType (`glyf`) outlines, as an Identity-H Type0 font. Only the glyphs used
  are kept (glyph IDs are preserved), and a ToUnicode map plus an ActualText
  span per line make the text extractable. Fonts whose OS/2 `fsType` forbids
  embedding or subsetting are refused.
- `doc.standard_font(StandardFont::...)` uses one of the 14 standard fonts
  without embedding. Widths come from the Adobe metrics, but viewers substitute
  their own fonts, so measured layout can differ slightly on screen. Text is
  WinAnsi encoded; other characters become `?`. Prefer an embedded font when
  exact widths or non-Latin text matter.

`page.text(font, size, x, y, text, color)` draws one unwrapped run with its
baseline at `y`.

`page.paragraph(&TextStyle, [x, y, width, height], text)` wraps text inside a
box, starting at its top edge:

- `TextStyle::new(font, size)` with `.color(..)`, `.line_height(..)` (a
  multiple of the size, default 1.2), `.align(Align::Left | Right | Center |
  Justify)` and `.kerning(bool)` (default on).
- Words break at spaces and tabs (runs collapse to one space); a word wider
  than the box breaks between characters. `\n` forces a break and an empty
  line keeps its height. Justified lines stretch the spaces; the last line of
  each paragraph and lines ending at `\n` stay left aligned.
- The returned `Flow { lines, height, consumed, complete }` says how much fit.
  `&text[flow.consumed..]` is the remainder to continue on the next page.
- `doc.text_width(font, size, text, kerning)` measures a run.

Kerning reads the TrueType `kern` table (format 0). Text is not shaped:
characters map one to one to glyphs, so complex scripts (Arabic, Indic and
others needing shaping), bidirectional reordering, ligatures and GPOS kerning
are out of scope. Left-to-right scripts without shaping, such as Latin, Greek,
Cyrillic and CJK, work with a font that covers them.

## Graphics

`fill_rect`, `stroke_rect`, `line` and `path()` (with `move_to`, `line_to`,
`cubic_to`, `rect`, `close`, then `fill`, `stroke`, `fill_stroke` or `clip`,
each with nonzero or even-odd rules) draw vector shapes. Paint is
`Color::rgb(r, g, b)` or `Paint::gray(level)`. Graphics state: `line_width`,
`dash(pattern, phase)`, `line_cap`, `line_join`, `clip_rect`, `save` and
`restore` (unbalanced saves are closed when the page ends), `transform`,
`translate`, `scale` and `rotate(degrees)`.

## Images

- `doc.jpeg_auto(bytes)` reads the size and components from the JPEG header
  and embeds the bytes unchanged as DCTDecode (gray, RGB or Adobe CMYK).
- `doc.png(&bytes)` decodes a PNG with the same inflate and predictor code the
  reader uses (all bit depths and colour types, palettes, tRNS; not
  interlaced), and writes FlateDecode RGB with an SMask when there is alpha.
- `doc.rgba(width, height, &pixels)` takes raw RGBA.

Draw with `page.image(id, x, y, width, height)`; `doc.image_size(id)` returns
the pixel size.

## Links and outline

- `page.link_uri(x, y, w, h, uri)` adds a web link.
- `page.link_to(x, y, w, h, Destination)` jumps inside the document:
  `Destination::Xyz { page, left, top, zoom }`, `Fit { page }` or
  `FitWidth { page, top }`. `page.link_page(.., page, top)` is the XYZ short
  form.
- `doc.outline(title, level, page, top)` and `doc.outline_to(title, level,
  Destination)` add outline entries; levels start at 1 and nest under the
  previous entry with a lower level.

## Encryption

`doc.encrypt(password, costs)` writes the private PSChaCha format described in
`docs/encryption.md`. Only pdf-toolkit readers (the pdf-toolkit viewer and
chuzz) can open these files; other PDF readers refuse them. Encrypted output
uses a random salt and nonces, so it is not byte for byte reproducible.

## Signing

`pdf_toolkit::sign` (feature `sign`, or `pdf_toolkit_edit::sign`) adds ECDSA
P-256 or P-384 signatures to a finished file as an incremental update, so
earlier signatures stay valid, and verifies them offline:

```rust
use std::sync::Arc;
use pdf_toolkit_core::Limits;
use pdf_toolkit_edit::sign::{self, Curve, SignOptions, Signer};
use pdf_toolkit_write::doc::PdfDate;

// pdf: a finished PDF; scalar: the 32-byte P-256 private key; certificate: DER.
let signer = Signer { curve: Curve::P256, scalar, certificate, chain: Vec::new() };
let options = SignOptions::new("Approval", PdfDate::utc(2026, 4, 1, 12, 0, 0));
let signed = sign::sign(Arc::from(pdf), Limits::default(), &signer, &options)?;
for report in sign::verify(Arc::from(signed), Limits::default())? {
    println!("{} {} {}", report.field, report.digest.name(), report.signature.name());
}
```

- The caller supplies the private scalar, its DER certificate (and optional
  chain) and the signing time. Nonces follow RFC 6979, so output is
  deterministic.
- `/SubFilter` is `adbe.pkcs7.detached` (default, with a CMS signingTime) or
  `ETSI.CAdES.detached` (with signingCertificateV2 and the time in `/M`).
- Encrypted (PSChaCha) files are refused.
- `verify` reports the byte range digest against the signed messageDigest,
  ECDSA validity with the signer certificate key, the signer subject, the
  signing time, whether the byte range covers a whole revision, and the bytes
  and revisions appended after it. It does not build or trust certificate
  chains, check revocation or timestamps, or use the network. RSA and other
  algorithms are reported as `unsupported_algorithm`.
- `pdf-inspect signatures FILE` prints the same report.

## Not supported

- Text shaping, bidirectional text, ligatures, GPOS kerning, vertical writing.
- CFF-outline OpenType (`OTTO`) and font collections; Type 3 fonts.
- Interlaced PNG; JPEG 2000, JPEG XL and other codecs on the writing side.
- Tables (lay them out with `paragraph`, `line` and `fill_rect`).
- RSA signatures, timestamps (RFC 3161), long-term validation data, DocMDP.
