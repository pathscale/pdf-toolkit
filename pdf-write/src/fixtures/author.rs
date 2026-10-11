//! Fixtures written through the high-level authoring API (`crate::doc`).
//!
//! Expectations are derived from the house font's width classes and plain
//! geometry, never by reading the output back.

use std::io;
use std::sync::Arc;

use super::{box_text, manifest, sha256, Fixture};
use crate::doc::{
    Align, Color, Destination, DocumentBuilder, Margins, Metadata, PageSetup, PageSize, Paint,
    PdfDate, StandardFont, TextStyle, WriteOptions,
};

mod font;
mod png;

pub(crate) use font::program as house_font;

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn finish(doc: DocumentBuilder) -> io::Result<Vec<u8>> {
    if let Some(error) = doc.error() {
        return Err(io::Error::new(error.kind(), error.to_string()));
    }
    doc.finish(&WriteOptions::default())
}

/// Appends `page.N.pixel.X.Y` for a probe at user-space (x, y) on a page of
/// the given height.
fn pixel(expected: &mut String, page: usize, height: u32, x: u32, y: u32, rgb: &str) {
    let device = height - y - 1;
    expected.push_str(&format!("page.{page}.pixel.{x}.{device}={rgb}\n"));
}

/// Word width in house font units, kerning included.
fn house_width(word: &str) -> i32 {
    let mut width = 0i32;
    let mut previous: Option<char> = None;
    for character in word.chars() {
        width += i32::from(font::advance(character));
        if let Some(left) = previous {
            for (a, b, value) in font::KERN_PAIRS {
                if a == left && b == character {
                    width += i32::from(value);
                }
            }
        }
        previous = Some(character);
    }
    width
}

/// Independent greedy wrap for the house font, used to predict line breaks.
fn house_lines(text: &str, size: f32, width: f32) -> Vec<String> {
    let limit = (width * 1000.0 / size).floor() as i32;
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        let mut used = 0i32;
        for word in paragraph.split(' ').filter(|word| !word.is_empty()) {
            let extra = house_width(word);
            if !line.is_empty() && used + 250 + extra <= limit {
                line.push(' ');
                line.push_str(word);
                used += 250 + extra;
            } else {
                if !line.is_empty() {
                    lines.push(std::mem::take(&mut line));
                }
                line.push_str(word);
                used = extra;
            }
        }
        lines.push(line);
    }
    lines.into_iter().filter(|line| !line.is_empty()).collect()
}

fn normalize(lines: &[&str]) -> String {
    lines
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

const LEFT: &str = "The quick brown fox jumps over the lazy dog while the house writer \
                    wraps every word inside a fixed box.";
const RIGHT: &str = "Right aligned text ends at the box edge.\nSecond line.";
const JUSTIFY: &str = "Justified lines stretch their spaces so that both edges meet the box, \
                       except the last line.";
const OVERFLOW: &str = "This paragraph is longer than its box, so the flow reports how much \
                        was placed and the rest continues on the next page in the same style.";
const LONG_WORD: &str = "Pneumonoultramicroscopicsilicovolcanoconiosis-and-more-letters";

/// Letter and A4 pages, margins, metadata, page boxes and wrapped Courier
/// paragraphs in all four alignments, with a paragraph that continues onto
/// the second page.
pub(super) fn text() -> io::Result<Fixture> {
    let mut doc = DocumentBuilder::new();
    doc.metadata(&Metadata {
        title: Some("Authoring text".to_owned()),
        author: Some("House author".to_owned()),
        subject: Some("Wrapping and alignment".to_owned()),
        keywords: Some("wrap, align, flow".to_owned()),
        creator: Some("pdf-fixtures".to_owned()),
        producer: Some("pdf-toolkit-write".to_owned()),
        creation_date: Some(PdfDate {
            year: 2026,
            month: 1,
            day: 2,
            hour: 3,
            minute: 4,
            second: 5,
            offset_minutes: 90,
        }),
        modification_date: Some(PdfDate::utc(2026, 1, 2, 3, 4, 5)),
    });
    let bold = doc.standard_font(StandardFont::HelveticaBold);
    let mono = doc.standard_font(StandardFont::Courier);
    let body = TextStyle::new(mono, 10.0);
    let rest;
    {
        let mut page =
            doc.page_with(&PageSetup::new(PageSize::Letter).margins(Margins::uniform(72.0)));
        let [x, _, width, _] = page.content_box();
        page.paragraph(
            &TextStyle::new(bold, 20.0).align(Align::Center),
            [x, 690.0, width, 30.0],
            "Authoring API",
        );
        page.line(72.0, 680.0, 540.0, 680.0, 2.0, Paint::gray(0.5));
        page.paragraph(&body, [72.0, 600.0, 300.0, 70.0], LEFT);
        page.paragraph(&body.align(Align::Right), [72.0, 560.0, 300.0, 30.0], RIGHT);
        page.paragraph(
            &body.align(Align::Center),
            [72.0, 530.0, 300.0, 20.0],
            "Centered",
        );
        page.paragraph(
            &body.align(Align::Justify),
            [72.0, 480.0, 300.0, 40.0],
            JUSTIFY,
        );
        let flow = page.paragraph(&body, [72.0, 400.0, 300.0, 24.0], OVERFLOW);
        if flow.lines != 2 || flow.complete {
            return Err(invalid("overflow paragraph did not stop after two lines"));
        }
        rest = OVERFLOW
            .get(flow.consumed..)
            .ok_or_else(|| invalid("flow offset"))?;
    }
    {
        let setup = PageSetup::new(PageSize::A4)
            .margins(Margins::uniform(50.0))
            .crop_box([10.0, 10.0, 585.0, 831.0])
            .bleed_box([5.0, 5.0, 590.0, 836.0])
            .trim_box([20.0, 20.0, 575.0, 821.0])
            .art_box([50.0, 50.0, 545.276, 791.89]);
        let mut page = doc.page_with(&setup);
        let [x, y, _, height] = page.content_box();
        page.paragraph(
            &body,
            [x, y, 300.0, height],
            &format!("{rest}\n\n{LONG_WORD}"),
        );
    }
    let pdf = finish(doc)?;

    let page0 = [
        "Authoring API",
        "The quick brown fox jumps over the lazy dog while",
        "the house writer wraps every word inside a fixed",
        "box.",
        "Right aligned text ends at the box edge.",
        "Second line.",
        "Centered",
        "Justified lines stretch their spaces so that both",
        "edges meet the box, except the last line.",
        "This paragraph is longer than its box, so the flow",
        "reports how much was placed and the rest continues",
    ];
    let page1 = [
        "on the next page in the same style.",
        "Pneumonoultramicroscopicsilicovolcanoconiosis-and-",
        "more-letters",
    ];
    let mut expected = manifest("1.7", "stream", 1, 2, 0, false);
    expected.push_str(&format!(
        "page.0.media_box=0 0 612 792\npage.0.text_norm={}\npage.0.lines={}\n\
         page.1.media_box={}\npage.1.crop_box=10 10 585 831\npage.1.bleed_box=5 5 590 836\n\
         page.1.trim_box=20 20 575 821\npage.1.art_box={}\n\
         page.1.text_norm={}\npage.1.lines={}\n",
        normalize(&page0),
        page0.join(" / "),
        box_text([0.0, 0.0, 595.276, 841.89]),
        box_text([50.0, 50.0, 545.276, 791.89]),
        normalize(&page1),
        page1.join(" / "),
    ));
    expected.push_str(
        "info.title=Authoring text\ninfo.author=House author\n\
         info.subject=Wrapping and alignment\ninfo.keywords=wrap, align, flow\n\
         info.creator=pdf-fixtures\ninfo.producer=pdf-toolkit-write\n\
         info.creation_date=D:20260102030405+01'30'\ninfo.mod_date=D:20260102030405Z\n\
         outline=\n",
    );
    // The gray rule sits between the title and the first paragraph.
    pixel(&mut expected, 0, 792, 300, 680, "808080");
    pixel(&mut expected, 0, 792, 300, 686, "ffffff");
    Ok(Fixture { pdf, expected })
}

const UNICODE: &str = "Grüße Καλημέρα Привет 你好 🙂";
const JUSTIFIED: &str = "brave house cards under sharp dance faces brown zebra";

/// The house TrueType font: multilingual text through Identity-H, kerning on
/// and off, justification and right alignment, all checked by pixel probes.
pub(super) fn fonts_unicode() -> io::Result<Fixture> {
    let mut doc = DocumentBuilder::new();
    let house = doc.truetype_font(Arc::from(house_font()?))?;
    {
        let mut page = doc.page(400.0, 400.0);
        page.text(house, 16.0, 20.0, 370.0, UNICODE, Color::BLACK);
        let big = TextStyle::new(house, 100.0);
        page.paragraph(&big, [20.0, 230.0, 300.0, 130.0], "AV");
        page.paragraph(&big.kerning(false), [20.0, 110.0, 300.0, 130.0], "AV");
        let small = TextStyle::new(house, 10.0);
        page.paragraph(
            &small.align(Align::Justify),
            [20.0, 60.0, 200.0, 40.0],
            JUSTIFIED,
        );
        page.paragraph(&small.align(Align::Right), [20.0, 10.0, 200.0, 20.0], "abc");
    }
    let pdf = finish(doc)?;
    let lines = house_lines(JUSTIFIED, 10.0, 200.0);
    if lines.len() != 2 {
        return Err(invalid("justified sample should wrap to two lines"));
    }
    let mut all = vec![UNICODE, "AV", "AV"];
    all.extend(lines.iter().map(String::as_str));
    all.push("abc");
    let mut expected = manifest("1.7", "stream", 1, 1, 0, false);
    expected.push_str(&format!(
        "page.0.media_box=0 0 400 400\npage.0.text_norm={}\npage.0.lines={}\npage.0.unmapped=0\n",
        normalize(&all),
        all.join(" / "),
    ));
    // First glyph of the Unicode line.
    pixel(&mut expected, 0, 400, 25, 375, "000000");
    // Kerned "AV": V is pulled 12pt left so its stem covers x=85.
    pixel(&mut expected, 0, 400, 85, 300, "000000");
    pixel(&mut expected, 0, 400, 50, 300, "000000");
    // Unkerned "AV": x=85 falls in the gap between A and V.
    pixel(&mut expected, 0, 400, 85, 190, "ffffff");
    // Justified first line ends at the right edge of the 200pt box.
    pixel(&mut expected, 0, 400, 218, 94, "000000");
    // Right aligned "abc" ends at the right edge; the left of the box stays empty.
    pixel(&mut expected, 0, 400, 218, 24, "000000");
    pixel(&mut expected, 0, 400, 30, 24, "ffffff");
    Ok(Fixture { pdf, expected })
}

/// Fills, strokes, dashes, clipping, transforms, even-odd paths and gray paint.
pub(super) fn graphics() -> io::Result<Fixture> {
    let mut doc = DocumentBuilder::new();
    {
        let mut page = doc.page(300.0, 300.0);
        page.fill_rect(20.0, 20.0, 100.0, 100.0, Paint::gray(0.25));

        page.save();
        page.clip_rect(150.0, 20.0, 50.0, 50.0);
        page.fill_rect(130.0, 0.0, 120.0, 120.0, Color::rgb(1.0, 0.0, 0.0));
        page.restore();

        page.save();
        page.translate(200.0, 200.0);
        page.rotate(45.0);
        page.fill_rect(-20.0, -20.0, 40.0, 40.0, Color::rgb(0.0, 0.0, 1.0));
        page.restore();

        page.dash(&[10.0, 10.0], 0.0);
        page.line(20.0, 150.0, 140.0, 150.0, 6.0, Paint::BLACK);
        page.dash(&[], 0.0);

        page.path()
            .rect(20.0, 180.0, 100.0, 100.0)
            .rect(45.0, 205.0, 50.0, 50.0)
            .fill(Color::rgb(0.0, 1.0, 0.0), true);
        page.path().rect(240.0, 240.0, 50.0, 50.0).fill_stroke(
            Color::rgb(1.0, 1.0, 0.0),
            8.0,
            Paint::BLACK,
            false,
        );
        page.line_cap(crate::doc::LineCap::Round);
        page.path()
            .move_to(150.0, 130.0)
            .cubic_to(170.0, 170.0, 230.0, 90.0, 280.0, 130.0)
            .stroke(3.0, Color::rgb(1.0, 0.5, 0.0));
    }
    let pdf = finish(doc)?;
    let mut expected = manifest("1.7", "stream", 1, 1, 0, false);
    expected.push_str("page.0.media_box=0 0 300 300\npage.0.text_norm=\n");
    let probes: [(u32, u32, &str); 13] = [
        (70, 70, "404040"),
        (175, 45, "ff0000"),
        (140, 10, "ffffff"),
        (230, 100, "ffffff"),
        (200, 225, "0000ff"),
        (218, 218, "ffffff"),
        (25, 150, "000000"),
        (35, 150, "ffffff"),
        (30, 190, "00ff00"),
        (70, 230, "ffffff"),
        (265, 265, "ffff00"),
        (240, 265, "000000"),
        (290, 10, "ffffff"),
    ];
    for (x, y, rgb) in probes {
        pixel(&mut expected, 0, 300, x, y, rgb);
    }
    Ok(Fixture { pdf, expected })
}

/// One baseline grayscale MCU (zero DC, EOB): an 8x8 mid-gray JPEG, the
/// same construction pdf-roundtrip uses.
fn jpeg() -> Vec<u8> {
    let mut bytes = vec![0xff, 0xd8, 0xff, 0xdb, 0x00, 0x43, 0x00];
    bytes.extend_from_slice(&[1; 64]);
    bytes.extend_from_slice(&[
        0xff, 0xc0, 0x00, 0x0b, 8, 0x00, 0x08, 0x00, 0x08, 1, 1, 0x11, 0,
    ]);
    bytes.extend_from_slice(&[0xff, 0xc4, 0x00, 0x26]);
    for table in [0x00, 0x10] {
        bytes.push(table);
        bytes.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        bytes.push(0);
    }
    bytes.extend_from_slice(&[
        0xff, 0xda, 0x00, 0x08, 1, 1, 0x00, 0, 63, 0, 0x3f, 0xff, 0xd9,
    ]);
    bytes
}

fn png_bytes(
    width: u32,
    depth: u8,
    color: u8,
    rows: &[Vec<u8>],
    palette: Option<&[u8]>,
    transparency: Option<&[u8]>,
) -> Vec<u8> {
    png::encode(&png::Png {
        width,
        depth,
        color,
        rows,
        palette,
        transparency,
    })
}

/// The 3x5 RGB image used to exercise every PNG row filter.
fn filter_rows() -> (Vec<Vec<u8>>, Vec<u8>) {
    let mut rows = Vec::new();
    let mut rgba = Vec::new();
    for row in 0..5u32 {
        let mut bytes = Vec::new();
        for column in 0..3u32 {
            let pixel = [
                (row * 50 + column * 20) as u8,
                (255 - row * 40) as u8,
                ((column * 90 + row * 7) % 256) as u8,
            ];
            bytes.extend_from_slice(&pixel);
            rgba.extend_from_slice(&pixel);
            rgba.push(255);
        }
        rows.push(bytes);
    }
    (rows, rgba)
}

/// JPEG passthrough and PNG decoding: RGBA, palette with tRNS, 16-bit gray
/// and an RGB image that uses all five row filters.
pub(super) fn images() -> io::Result<Fixture> {
    let rgba = png_bytes(
        2,
        8,
        6,
        &[
            vec![0, 255, 0, 255, 0, 0, 255, 128],
            vec![0, 0, 0, 0, 64, 64, 64, 255],
        ],
        None,
        None,
    );
    // Two-bit palette: index 0 transparent black, index 1 opaque cyan.
    let palette = png_bytes(
        2,
        2,
        3,
        &[vec![0b0001_0000]],
        Some(&[0, 0, 0, 0, 255, 255]),
        Some(&[0]),
    );
    let gray16 = png_bytes(1, 16, 0, &[vec![0x80, 0x80]], None, None);
    let (rows, filtered_rgba) = filter_rows();
    let filters = png_bytes(3, 8, 2, &rows, None, None);

    let mut doc = DocumentBuilder::new();
    let jpeg = doc.jpeg_auto(Arc::from(jpeg()));
    let rgba_id = doc.png(&rgba);
    let palette_id = doc.png(&palette);
    let gray_id = doc.png(&gray16);
    let filters_id = doc.png(&filters);
    {
        let mut page = doc.page(300.0, 200.0);
        page.image(jpeg, 10.0, 10.0, 80.0, 80.0);
        page.fill_rect(110.0, 10.0, 80.0, 80.0, Color::rgb(1.0, 0.0, 0.0));
        page.image(rgba_id, 110.0, 10.0, 80.0, 80.0);
        page.image(gray_id, 200.0, 10.0, 40.0, 40.0);
        page.image(filters_id, 10.0, 110.0, 60.0, 50.0);
        page.image(palette_id, 200.0, 110.0, 80.0, 40.0);
    }
    let pdf = finish(doc)?;
    let hash = sha256::hex;
    let mut expected = manifest("1.7", "stream", 1, 1, 0, false);
    expected.push_str(&format!(
        "page.0.media_box=0 0 300 200\npage.0.images=5\npage.0.text_norm=\n\
         image.4.kind=jpeg\n\
         image.5.kind=pixels\nimage.5.rgba_sha256={}\n\
         image.7.kind=pixels\nimage.7.rgba_sha256={}\n\
         image.9.kind=pixels\nimage.9.rgba_sha256={}\n\
         image.10.kind=pixels\nimage.10.rgba_sha256={}\n",
        hash(&[0, 255, 0, 255, 0, 0, 255, 128, 0, 0, 0, 0, 64, 64, 64, 255]),
        hash(&[0, 0, 0, 0, 0, 255, 255, 255]),
        hash(&[128, 128, 128, 255]),
        hash(&filtered_rgba),
    ));
    let probes: [(u32, u32, &str); 8] = [
        (50, 50, "808080"),
        (130, 70, "00ff00"),
        (170, 70, "7f0080"),
        (130, 30, "ff0000"),
        (170, 30, "404040"),
        (220, 30, "808080"),
        (220, 130, "ffffff"),
        (260, 130, "00ffff"),
    ];
    for (x, y, rgb) in probes {
        pixel(&mut expected, 0, 200, x, y, rgb);
    }
    Ok(Fixture { pdf, expected })
}

/// URI links, the three destination kinds and a three-level outline.
pub(super) fn links_outline() -> io::Result<Fixture> {
    let mut doc = DocumentBuilder::new();
    let font = doc.standard_font(StandardFont::Helvetica);
    let labels = ["Links page one", "Links page two", "Links page three"];
    for label in labels {
        let mut page = doc.page_with(&PageSetup::new(PageSize::Letter));
        page.text(font, 18.0, 72.0, 700.0, label, Color::BLACK);
    }
    {
        let mut page = doc.page_at(0);
        page.link_uri(72.0, 600.0, 200.0, 20.0, "https://example.test/authoring");
        page.link_to(
            72.0,
            560.0,
            200.0,
            20.0,
            Destination::Xyz {
                page: 1,
                left: Some(0.0),
                top: Some(700.0),
                zoom: None,
            },
        );
        page.link_to(72.0, 520.0, 200.0, 20.0, Destination::Fit { page: 2 });
    }
    doc.page_at(2).link_to(
        72.0,
        600.0,
        200.0,
        20.0,
        Destination::FitWidth {
            page: 0,
            top: Some(792.0),
        },
    );
    doc.outline_to("Chapter one", 1, Destination::Fit { page: 0 });
    doc.outline("Section 1.1", 2, 1, 700.0);
    doc.outline_to(
        "Detail 1.1.1",
        3,
        Destination::FitWidth {
            page: 1,
            top: Some(400.0),
        },
    );
    doc.outline("Chapter two", 1, 2, 700.0);
    let pdf = finish(doc)?;
    let mut expected = manifest("1.7", "stream", 1, 3, 0, false);
    for (index, label) in labels.iter().enumerate() {
        expected.push_str(&format!("page.{index}.text_norm={label}\n"));
    }
    expected.push_str(
        "page.0.links=uri:https://example.test/authoring | page:1 | page:2\n\
         page.1.links=\npage.2.links=page:0\n\
         outline=Chapter one | >Section 1.1 | >>Detail 1.1.1 | Chapter two\n",
    );
    Ok(Fixture { pdf, expected })
}

pub(crate) const EXAMPLE_FIRST: &str = "Field notes are short pages written while the work is \
    fresh. This one shows a title, a small picture, wrapped paragraphs in an embedded font, \
    a link and an outline.\nEvery line here is placed by the builder: words wrap at spaces, \
    the last line of each paragraph stays ragged, and the other lines stretch to both edges.";
pub(crate) const EXAMPLE_SECOND: &str = "The second page continues the notes. Its outline \
    entry points here, and the link on the first page jumps to this heading.";

/// The document from docs/authoring.md, with the house font standing in for
/// the application's font file.
pub(super) fn example() -> io::Result<Fixture> {
    let picture = png_bytes(
        2,
        8,
        2,
        &[vec![255, 0, 0, 0, 0, 255], vec![0, 160, 0, 255, 255, 255]],
        None,
        None,
    );
    let mut doc = DocumentBuilder::new();
    doc.metadata(&Metadata {
        title: Some("Field notes".to_owned()),
        author: Some("House author".to_owned()),
        creation_date: Some(PdfDate::utc(2026, 3, 1, 9, 30, 0)),
        ..Metadata::default()
    });
    let body = doc.truetype_font(Arc::from(house_font()?))?;
    let bold = doc.standard_font(StandardFont::HelveticaBold);
    let logo = doc.png(&picture);
    let heading = TextStyle::new(bold, 24.0);
    let text = TextStyle::new(body, 11.0)
        .align(Align::Justify)
        .line_height(1.4);
    let setup = PageSetup::new(PageSize::A5).margins(Margins::uniform(36.0));
    let top;
    {
        let mut page = doc.page_with(&setup);
        let [x, y, width, height] = page.content_box();
        top = y + height;
        page.paragraph(&heading, [x, top - 30.0, width, 30.0], "Field notes");
        page.image(logo, x, top - 110.0, 64.0, 64.0);
        page.paragraph(&text, [x, y, width, height - 130.0], EXAMPLE_FIRST);
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
        page.paragraph(&text, [x, y, width, height - 40.0], EXAMPLE_SECOND);
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
    let pdf = finish(doc)?;

    let content_width = 419.528 - 72.0;
    let first = house_lines(EXAMPLE_FIRST, 11.0, content_width);
    let second = house_lines(EXAMPLE_SECOND, 11.0, content_width);
    let mut page0: Vec<&str> = vec!["Field notes", "Next page"];
    page0.extend(first.iter().map(String::as_str));
    let mut page1: Vec<&str> = vec!["Continued"];
    page1.extend(second.iter().map(String::as_str));
    let mut expected = manifest("1.7", "stream", 1, 2, 0, false);
    expected.push_str(&format!(
        "page.0.media_box={}\npage.0.images=1\npage.0.text_norm={}\npage.0.lines={}\n\
         page.0.links=page:1\npage.1.text_norm={}\npage.1.lines={}\n\
         page.1.links=uri:https://github.com/pathscale/pdf-toolkit\n\
         outline=Field notes | >Continued\ninfo.title=Field notes\ninfo.author=House author\n\
         info.creation_date=D:20260301093000Z\n",
        box_text([0.0, 0.0, 419.528, 595.276]),
        normalize(&page0),
        page0.join(" / "),
        normalize(&page1),
        page1.join(" / "),
    ));
    Ok(Fixture { pdf, expected })
}

/// The builder's PSChaCha path. Opens only in pdf-toolkit readers.
pub(super) fn encrypted() -> io::Result<Fixture> {
    let mut doc = DocumentBuilder::new();
    doc.encrypt_fixture(b"house-author-password");
    doc.metadata(&Metadata {
        title: Some("Ωmega encrypted".to_owned()),
        ..Metadata::default()
    });
    let helvetica = doc.standard_font(StandardFont::Helvetica);
    let house = doc.truetype_font(Arc::from(house_font()?))?;
    {
        let mut page = doc.page_with(&PageSetup::new(PageSize::Letter));
        page.text(
            helvetica,
            14.0,
            72.0,
            700.0,
            "Encrypted authoring",
            Color::BLACK,
        );
        page.paragraph(
            &TextStyle::new(house, 12.0),
            [72.0, 600.0, 300.0, 60.0],
            "Ωmega keeps its glyphs.",
        );
    }
    let pdf = finish(doc)?;
    let mut expected = manifest("1.7", "table", 1, 1, 0, true);
    expected.push_str(
        "open.password=house-author-password\nopen.without_password=encrypted\n\
         open.wrong_password=malformed\nopen.corrupt_ciphertext=malformed\n\
         page.0.media_box=0 0 612 792\n\
         page.0.text_norm=Encrypted authoring Ωmega keeps its glyphs.\n\
         info.title=Ωmega encrypted\n",
    );
    Ok(Fixture { pdf, expected })
}
