//! House-authored scan bitmap and expectations, independent of any decoder.

use std::io;

use crate::encode::scan;

use super::{
    Encoding, Fixture, House, Value, fixture_resources, image_dictionary, image_truth, manifest,
    name, rect, sha256,
};

const WIDTH: u32 = 64;
const HEIGHT: u32 = 48;
const BITMAP_BYTES: usize = 384;
const RGBA_BYTES: usize = 12_288;
const BOUNDS: [f64; 4] = [0.0, 0.0, 480.0, 144.0];

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn ink(x: u32, y: u32) -> io::Result<bool> {
    let border = (2..62).contains(&x)
        && (2..46).contains(&y)
        && (!(6..58).contains(&x) || !(6..42).contains(&y));
    let dx = i64::from(x)
        .checked_sub(18)
        .ok_or_else(|| invalid("scan circle x overflow"))?;
    let dy = i64::from(y)
        .checked_sub(16)
        .ok_or_else(|| invalid("scan circle y overflow"))?;
    let distance = dx
        .checked_mul(dx)
        .and_then(|value| {
            dy.checked_mul(dy)
                .and_then(|other| value.checked_add(other))
        })
        .ok_or_else(|| invalid("scan circle distance overflow"))?;
    let circle = distance <= 64;
    let p = ((8..10).contains(&x) && (30..40).contains(&y))
        || ((8..18).contains(&x) && ((30..32).contains(&y) || (34..36).contains(&y)))
        || ((16..18).contains(&x) && (30..36).contains(&y));
    let d = ((26..28).contains(&x) && (30..40).contains(&y))
        || ((26..34).contains(&x) && ((30..32).contains(&y) || (38..40).contains(&y)))
        || ((34..36).contains(&x) && (32..38).contains(&y));
    let f = ((44..46).contains(&x) && (30..40).contains(&y))
        || ((44..54).contains(&x) && (30..32).contains(&y))
        || ((44..52).contains(&x) && (34..36).contains(&y));
    Ok(border || circle || p || d || f)
}

fn bitmap() -> io::Result<(Vec<u8>, Vec<u8>)> {
    let mut packed = Vec::new();
    packed
        .try_reserve_exact(BITMAP_BYTES)
        .map_err(|_| invalid("scan fixture bitmap allocation failed"))?;
    packed.resize(BITMAP_BYTES, 0u8);
    let mut rgba = Vec::new();
    rgba.try_reserve_exact(RGBA_BYTES)
        .map_err(|_| invalid("scan fixture RGBA allocation failed"))?;
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let is_black = ink(x, y)?;
            let position = y
                .checked_mul(WIDTH)
                .and_then(|value| value.checked_add(x))
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| invalid("scan fixture pixel offset overflow"))?;
            if is_black {
                let byte = packed
                    .get_mut(position / 8)
                    .ok_or_else(|| invalid("scan fixture pixel outside bitmap"))?;
                *byte |= 0x80u8 >> (position % 8);
            }
            // CCITT BlackIs1=false and JBIG2 both yield sample 0 for black.
            let gray = if is_black { 0 } else { 255 };
            rgba.extend_from_slice(&[gray, gray, gray, 255]);
        }
    }
    Ok((packed, rgba))
}

pub(super) fn codecs() -> io::Result<Fixture> {
    let (packed, rgba) = bitmap()?;
    let mut house = House::new("1.4")?;
    let g4 = house.writer.alloc()?;
    let g3 = house.writer.alloc()?;
    let jbig2 = house.writer.alloc()?;
    let g4_data = scan::ccitt_g4(&packed, WIDTH, HEIGHT, true)?;
    let g3_data = scan::ccitt_g3(&packed, WIDTH, HEIGHT, true, true)?;
    let jbig2_data = scan::jbig2(&packed, WIDTH, HEIGHT)?;

    for (object, data, k) in [
        (g4, g4_data.as_slice(), -1i64),
        (g3, g3_data.as_slice(), 0i64),
    ] {
        let mut parameters = vec![
            ("K".to_owned(), Value::Int(k)),
            ("Columns".to_owned(), Value::Int(i64::from(WIDTH))),
            ("Rows".to_owned(), Value::Int(i64::from(HEIGHT))),
            ("EndOfBlock".to_owned(), Value::Bool(k < 0)),
        ];
        if k == 0 {
            parameters.push(("EndOfLine".to_owned(), Value::Bool(true)));
            parameters.push(("EncodedByteAlign".to_owned(), Value::Bool(true)));
        }
        // BlackIs1 is omitted to exercise its false default.
        house.writer.stream(
            object,
            image_dictionary(
                WIDTH,
                HEIGHT,
                Some(name("DeviceGray")),
                Some(1),
                vec![
                    ("Filter".to_owned(), name("CCITTFaxDecode")),
                    ("DecodeParms".to_owned(), Value::Dict(parameters)),
                    (
                        "Decode".to_owned(),
                        Value::Array(vec![Value::Int(0), Value::Int(1)]),
                    ),
                    ("Interpolate".to_owned(), Value::Bool(false)),
                ],
            ),
            data,
        )?;
    }
    house.writer.stream(
        jbig2,
        image_dictionary(
            WIDTH,
            HEIGHT,
            Some(name("DeviceGray")),
            Some(1),
            vec![
                ("Filter".to_owned(), name("JBIG2Decode")),
                (
                    "Decode".to_owned(),
                    Value::Array(vec![Value::Int(0), Value::Int(1)]),
                ),
                ("Interpolate".to_owned(), Value::Bool(false)),
            ],
        ),
        &jbig2_data,
    )?;

    let plain = b"q 128 0 0 96 24 24 cm /G4 Do Q\n\
                  q 128 0 0 96 176 24 cm /G3 Do Q\n\
                  q 128 0 0 96 328 24 cm /JBIG2 Do Q\n";
    let page = house.add_page(
        house.pages,
        plain,
        Encoding::Plain,
        vec![
            (
                "Resources".to_owned(),
                fixture_resources(&[], &[("G4", g4), ("G3", g3), ("JBIG2", jbig2)]),
            ),
            ("MediaBox".to_owned(), rect(BOUNDS)),
            ("CropBox".to_owned(), rect(BOUNDS)),
        ],
    )?;
    house.write_base(&[page.id], 1)?;
    house.table(Vec::new())?;

    let mut expected = manifest("1.4", "table", 1, 1, 0, false);
    expected.push_str(&format!(
        "page.0.media_box=0 0 480 144\n\
         page.0.crop_box=0 0 480 144\n\
         page.0.rotate=0\npage.0.contents=1\n\
         page.0.content_sha256={}\n\
         page.0.resource_keys=ProcSet,XObject\n\
         page.0.images=3\npage.0.render=ok\n",
        sha256::hex(plain),
    ));
    for object in [g4, g3, jbig2] {
        image_truth(&mut expected, object, "pixels", Some(&rgba));
    }
    // At 72 dpi the image top is device y=24. Local probes (7,24) and
    // (15,24) have at least three device pixels of clearance from ink edges.
    // The first lies in the four-bitmap-pixel border, the second in white.
    expected.push_str(
        "page.0.pixel.31.48=000000\npage.0.pixel.39.48=ffffff\n\
         page.0.pixel.183.48=000000\npage.0.pixel.191.48=ffffff\n\
         page.0.pixel.335.48=000000\npage.0.pixel.343.48=ffffff\n",
    );
    Ok(house.fixture(expected))
}
