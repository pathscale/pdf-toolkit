//! Geometry-derived annotation probes, including appearance state selection.

use std::io;

use super::{manifest, name, rect, reference, sha256, Encoding, Fixture, House, Value};
use crate::annot::{self, Annot, AnnotKind};

const HEIGHT: u32 = 320;
const WHITE: [f32; 3] = [1.0; 3];
const BLUE: [f32; 3] = [0.0, 0.0, 1.0];
const BACKGROUND: [f32; 3] = [0.25, 0.5, 0.75];

fn annotation(kind: AnnotKind, rect: [f32; 4], color: [f32; 3], opacity: f32) -> Annot {
    Annot {
        kind,
        rect,
        color,
        opacity,
        contents: Some("House annotation".to_owned()),
        author: Some("Zoë".to_owned()),
    }
}

fn pixel(
    expected: &mut String,
    point: [u32; 2],
    foreground: [f32; 3],
    opacity: f32,
    background: [f32; 3],
    multiply: bool,
) -> io::Result<()> {
    let [x, y] = point;
    let device_y = HEIGHT
        .checked_sub(y)
        .and_then(|y| y.checked_sub(1))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "probe outside page"))?;
    let mut rgb = [0u8; 3];
    for (byte, (foreground, background)) in
        rgb.iter_mut().zip(foreground.into_iter().zip(background))
    {
        let foreground = f64::from(foreground);
        let background = f64::from(background);
        let opacity = f64::from(opacity);
        let blended = if multiply {
            foreground * background
        } else {
            foreground
        };
        *byte = ((blended * opacity + background * (1.0 - opacity)).clamp(0.0, 1.0) * 255.0).round()
            as u8;
    }
    let [r, g, b] = rgb;
    expected.push_str(&format!(
        "page.0.pixel.{x}.{device_y}={r:02x}{g:02x}{b:02x}\n"
    ));
    Ok(())
}

pub(super) fn build() -> io::Result<Fixture> {
    let highlight = annotation(
        AnnotKind::Highlight {
            quads: vec![
                [20.0, 290.0, 120.0, 290.0, 20.0, 260.0, 120.0, 260.0],
                [140.0, 290.0, 220.0, 290.0, 140.0, 260.0, 220.0, 260.0],
            ],
        },
        [20.0, 260.0, 220.0, 290.0],
        [1.0, 1.0, 0.0],
        1.0,
    );
    let mut note = annotation(
        AnnotKind::Note,
        [250.0, 240.0, 290.0, 290.0],
        [1.0, 0.5, 0.0],
        1.0,
    );
    note.contents = Some("Café \u{1f4ac}".to_owned());
    let ink = annotation(
        AnnotKind::Ink {
            paths: vec![vec![[110.0, 225.0], [210.0, 225.0]], vec![[110.0, 215.0]]],
            width: 8.0,
        },
        [100.0, 210.0, 220.0, 240.0],
        [0.0, 0.5, 1.0],
        1.0,
    );
    let mut text = annotation(
        AnnotKind::FreeText {
            text: "I I I I I I".to_owned(),
            size: 40.0,
        },
        [20.0, 80.0, 84.0, 210.0],
        [0.0; 3],
        1.0,
    );
    text.contents = None;
    let square = annotation(
        AnnotKind::Square { width: 8.0 },
        [100.0, 130.0, 160.0, 190.0],
        [1.0, 0.0, 0.0],
        1.0,
    );
    let circle = annotation(
        AnnotKind::Circle { width: 8.0 },
        [200.0, 130.0, 260.0, 190.0],
        [0.0, 1.0, 0.0],
        1.0,
    );
    let half = annotation(
        AnnotKind::Highlight {
            quads: vec![[20.0, 240.0, 80.0, 240.0, 20.0, 210.0, 80.0, 210.0]],
        },
        [20.0, 210.0, 80.0, 240.0],
        [1.0, 0.0, 0.0],
        0.5,
    );
    let state_square = annotation(
        AnnotKind::Square { width: 8.0 },
        [200.0, 100.0, 260.0, 160.0],
        BLUE,
        1.0,
    );
    let hidden = annotation(
        AnnotKind::Note,
        [300.0, 130.0, 320.0, 150.0],
        [1.0, 0.0, 0.0],
        1.0,
    );
    let no_view = annotation(
        AnnotKind::Note,
        [340.0, 130.0, 360.0, 150.0],
        [1.0, 0.0, 0.0],
        1.0,
    );

    let mut expected = manifest("1.4", "table", 1, 1, 0, false);
    // Interior fills and stroke centres have clearance from antialiased edges.
    pixel(
        &mut expected,
        [50, 275],
        highlight.color,
        highlight.opacity,
        BACKGROUND,
        true,
    )?;
    pixel(
        &mut expected,
        [170, 275],
        highlight.color,
        highlight.opacity,
        WHITE,
        true,
    )?;
    pixel(&mut expected, [130, 275], WHITE, 1.0, WHITE, false)?; // Gap between quads.
    pixel(
        &mut expected,
        [252, 288],
        note.color,
        note.opacity,
        WHITE,
        false,
    )?;
    pixel(&mut expected, [275, 285], WHITE, 1.0, WHITE, false)?; // Fixed 20-point icon.
    pixel(
        &mut expected,
        [150, 225],
        ink.color,
        ink.opacity,
        WHITE,
        false,
    )?;
    pixel(
        &mut expected,
        [110, 215],
        ink.color,
        ink.opacity,
        WHITE,
        false,
    )?;
    // Free text glyphs use unembedded Helvetica, which renders only with a supplied fallback font.
    pixel(&mut expected, [80, 90], WHITE, 1.0, WHITE, false)?;
    pixel(
        &mut expected,
        [104, 160],
        square.color,
        square.opacity,
        BLUE,
        false,
    )?;
    pixel(&mut expected, [130, 160], BLUE, 1.0, BLUE, false)?;
    pixel(
        &mut expected,
        [230, 185],
        circle.color,
        circle.opacity,
        WHITE,
        false,
    )?;
    pixel(&mut expected, [230, 160], WHITE, 1.0, WHITE, false)?;
    pixel(
        &mut expected,
        [50, 225],
        half.color,
        half.opacity,
        WHITE,
        true,
    )?;
    pixel(
        &mut expected,
        [356, 50],
        state_square.color,
        state_square.opacity,
        WHITE,
        false,
    )?;
    pixel(&mut expected, [330, 50], WHITE, 1.0, WHITE, false)?;
    pixel(&mut expected, [302, 148], WHITE, 1.0, WHITE, false)?;
    pixel(&mut expected, [342, 148], WHITE, 1.0, WHITE, false)?;
    pixel(&mut expected, [10, 10], WHITE, 1.0, WHITE, false)?;
    pixel(&mut expected, [390, 310], WHITE, 1.0, WHITE, false)?;

    let specs = vec![
        (highlight, 4, false),
        (note, 4, false),
        (ink, 4, false),
        (text, 4, false),
        (square, 4, false),
        (circle, 4, false),
        (half, 4, false),
        (state_square, 4, true),
        (hidden, 4 | 2, false),
        (no_view, 4 | 32, false),
    ];
    let mut house = House::new("1.4")?;
    let annots = house.writer.alloc()?;
    let mut ids = Vec::new();
    for _ in &specs {
        ids.push(house.writer.alloc()?);
    }
    house.writer.object(
        annots,
        &Value::Array(ids.iter().copied().map(reference).collect()),
    )?;
    let plain = b"q 0.25 0.5 0.75 rg 20 260 100 30 re f Q\n\
                  q 0 0 1 rg 100 130 60 60 re f Q\n\
                  2 0 0 2 0 0 cm\n10 10 m\n";
    let media = [0.0, 0.0, 400.0, f64::from(HEIGHT)];
    let page = house.add_page(
        house.pages,
        plain,
        Encoding::Plain,
        vec![
            ("MediaBox".to_owned(), rect(media)),
            ("CropBox".to_owned(), rect(media)),
            ("Resources".to_owned(), super::fixture_resources(&[], &[])),
            ("Annots".to_owned(), reference(annots)),
        ],
    )?;
    for ((annot, flags, states), id) in specs.iter().zip(ids) {
        let mut parts = annot::build(annot)
            .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?;
        parts.dict.retain(|(key, _)| key != "F");
        parts
            .dict
            .push(("F".to_owned(), Value::Int(i64::from(*flags))));
        parts.dict.push(("P".to_owned(), reference(page.id)));
        let appearance = house.writer.alloc()?;
        let normal = if *states {
            parts.dict.retain(|(key, _)| key != "Rect");
            parts
                .dict
                .push(("Rect".to_owned(), rect([300.0, 20.0, 360.0, 80.0])));
            parts.dict.push(("AS".to_owned(), name("On")));
            parts.ap_dict.push((
                "Matrix".to_owned(),
                Value::Array(
                    [0.0, 2.0, -2.0, 0.0, 11.0, -13.0]
                        .into_iter()
                        .map(Value::Real)
                        .collect(),
                ),
            ));
            let off = house.writer.alloc()?;
            house.writer.stream(off, parts.ap_dict.clone(), b"")?;
            Value::Dict(vec![
                ("On".to_owned(), reference(appearance)),
                ("Off".to_owned(), reference(off)),
            ])
        } else {
            reference(appearance)
        };
        house
            .writer
            .stream(appearance, parts.ap_dict, &parts.ap_content)?;
        parts
            .dict
            .push(("AP".to_owned(), Value::Dict(vec![("N".to_owned(), normal)])));
        house.writer.object(id, &Value::Dict(parts.dict))?;
    }
    house.write_base(&[page.id], 1)?;
    house.table(Vec::new())?;
    expected.push_str(&format!(
        "page.0.media_box={}\npage.0.crop_box={}\npage.0.rotate=0\n\
         page.0.contents=1\npage.0.content_sha256={}\n\
         page.0.resource_keys=ProcSet\npage.0.annotations={}\npage.0.render=ok\n",
        super::box_text(media),
        super::box_text(media),
        sha256::hex(plain),
        specs.len(),
    ));
    Ok(house.fixture(expected))
}
