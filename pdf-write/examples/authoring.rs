//! The two-page document from docs/authoring.md.
//!
//! cargo run -p pdf-toolkit-write --example authoring -- FONT.ttf LOGO.png OUT.pdf
//!
//! The house fixture `author-example` builds the same document with a
//! generated font in place of FONT.ttf.

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

#[path = "../../pdf-apps/src/app_caps.rs"]
mod app_caps;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _caps = app_caps::start([536870912, 30, 120, 256, 1, 32, 16777216, 10000, 10000]);
    app_caps::turn("PDF authoring");
    let args: Vec<String> = app_caps::args().skip(1).collect();
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
    let body = doc.truetype_font(Arc::from(app_caps::read_bytes(font)?))?;
    let bold = doc.standard_font(StandardFont::HelveticaBold);
    let logo = doc.png(&app_caps::read_bytes(logo)?);

    let heading = TextStyle::new(bold, 24.0);
    let text = TextStyle::new(body, 11.0)
        .align(Align::Justify)
        .line_height(1.4);
    let setup = PageSetup::new(PageSize::A5).margins(Margins::uniform(36.0));

    let top;
    {
        app_caps::turn("PDF authoring: page");
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
        app_caps::turn("PDF authoring: page");
        let mut page = doc.page_with(&setup);
        let [x, y, width, height] = page.content_box();
        page.paragraph(&heading, [x, top - 30.0, width, 30.0], "Continued");
        page.paragraph(&text, [x, y, width, height - 40.0], SECOND);
        page.link_uri(
            x,
            y,
            width,
            20.0,
            "https://github.com/pathscale/pdf-toolkit",
        );
    }
    doc.outline("Field notes", 1, 0, top);
    doc.outline("Continued", 2, 1, top);

    // Errors are retained by the builder and returned here.
    let bytes = doc.finish(&WriteOptions::default())?;
    std::fs::write(out, bytes)?;
    Ok(())
}
