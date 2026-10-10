//! Embedded libjxl conformance images with independent reference probes.

use std::io;

use super::{
    fixture_resources, image_dictionary, image_truth, manifest, name, rect, sha256, Encoding,
    Fixture, House, Value,
};

const BOUNDS: [f64; 4] = [0.0, 0.0, 402.0, 263.0];
const PAGE_HEIGHT: u32 = 263;
const TOP: u32 = 16;

// Probe coordinates refer to the original reference PNG, with row zero at
// the top. RGB values are reference pixels composited over white.
// For scale 6, select x and y congruent to 3 modulo 6, the centers of
// displayed pixels. No pixel expectations are emitted for an empty table.
mod grayscale {
    // libjxl testcases/grayscale/ref.png.
    pub(super) const PROBES: &[(u32, u32, [u8; 3])] = &[
        (0, 0, [0x00, 0x00, 0x00]),
        (50, 50, [0x50, 0x50, 0x50]),
        (100, 100, [0xa4, 0xa4, 0xa4]),
        (150, 150, [0xe1, 0xe1, 0xe1]),
        (199, 199, [0xff, 0xff, 0xff]),
    ];
}

mod sunset_logo {
    // libjxl testcases/sunset_logo/ref.png; every probe is fully opaque there.
    pub(super) const PROBES: &[(u32, u32, [u8; 3])] = &[
        (3, 3, [0x00, 0x40, 0x97]),
        (99, 99, [0x00, 0x55, 0x9d]),
        (459, 693, [0x8b, 0x54, 0x36]),
        (453, 1203, [0x12, 0x20, 0x61]),
        (615, 1059, [0x61, 0x28, 0x45]),
        (903, 1371, [0x00, 0x52, 0xbd]),
    ];
}

struct Source {
    resource: &'static str,
    data: &'static [u8],
    width: u32,
    height: u32,
    color_space: &'static str,
    left: u32,
    scale: u32,
    probes: &'static [(u32, u32, [u8; 3])],
}

const SOURCES: [Source; 2] = [
    Source {
        resource: "J1",
        data: include_bytes!("assets/jxl/grayscale.jxl"),
        width: 200,
        height: 200,
        color_space: "DeviceGray",
        left: 16,
        scale: 1,
        probes: grayscale::PROBES,
    },
    Source {
        resource: "J2",
        data: include_bytes!("assets/jxl/sunset_logo.jxl"),
        width: 924,
        height: 1386,
        color_space: "DeviceRGB",
        left: 232,
        scale: 6,
        probes: sunset_logo::PROBES,
    },
];

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.to_owned())
}

fn pixel_truth(
    expected: &mut String,
    source: &Source,
    x: u32,
    y: u32,
    rgb: [u8; 3],
) -> io::Result<()> {
    if source.scale == 0 || x >= source.width || y >= source.height {
        return Err(invalid("JXL fixture probe outside image"));
    }
    let center = source.scale / 2;
    if x % source.scale != center || y % source.scale != center {
        return Err(invalid(
            "JXL fixture probe must select a displayed pixel center",
        ));
    }
    let page_x = source
        .left
        .checked_add(x / source.scale)
        .ok_or_else(|| invalid("JXL fixture page probe x overflow"))?;
    let page_y = TOP
        .checked_add(y / source.scale)
        .ok_or_else(|| invalid("JXL fixture page probe y overflow"))?;
    let [r, g, b] = rgb;
    expected.push_str(&format!(
        "page.0.pixel.{page_x}.{page_y}={r:02x}{g:02x}{b:02x}\n",
    ));
    Ok(())
}

pub(super) fn images() -> io::Result<Fixture> {
    let mut house = House::new("2.0")?;
    let mut resources = Vec::new();
    let mut objects = Vec::new();
    let mut plain = Vec::new();

    for source in &SOURCES {
        if source.scale == 0
            || source.width % source.scale != 0
            || source.height % source.scale != 0
        {
            return Err(invalid("JXL fixture display scale"));
        }
        let display_width = source.width / source.scale;
        let display_height = source.height / source.scale;
        let bottom = PAGE_HEIGHT
            .checked_sub(TOP)
            .and_then(|value| value.checked_sub(display_height))
            .ok_or_else(|| invalid("JXL fixture placement overflow"))?;
        let object = house.writer.alloc()?;
        house.writer.stream(
            object,
            image_dictionary(
                source.width,
                source.height,
                Some(name(source.color_space)),
                Some(8),
                vec![
                    ("Filter".to_owned(), name("JXLDecode")),
                    ("Interpolate".to_owned(), Value::Bool(false)),
                ],
            ),
            source.data,
        )?;
        plain.extend_from_slice(
            format!(
                "q {display_width} 0 0 {display_height} {} {bottom} cm /{} Do Q\n",
                source.left, source.resource,
            )
            .as_bytes(),
        );
        resources.push((source.resource, object));
        objects.push(object);
    }

    let page = house.add_page(
        house.pages,
        &plain,
        Encoding::Plain,
        vec![
            ("Resources".to_owned(), fixture_resources(&[], &resources)),
            ("MediaBox".to_owned(), rect(BOUNDS)),
            ("CropBox".to_owned(), rect(BOUNDS)),
        ],
    )?;
    house.write_base(&[page.id], 1)?;
    house.table(Vec::new())?;

    let mut expected = manifest("2.0", "table", 1, 1, 0, false);
    expected.push_str(&format!(
        "page.0.media_box=0 0 402 263\n\
         page.0.crop_box=0 0 402 263\n\
         page.0.rotate=0\npage.0.contents=1\n\
         page.0.content_sha256={}\n\
         page.0.resource_keys=ProcSet,XObject\n\
         page.0.images=2\npage.0.render=ok\n",
        sha256::hex(&plain),
    ));
    for (source, object) in SOURCES.iter().zip(objects.iter()) {
        image_truth(&mut expected, *object, "pixels", None);
        for &(x, y, rgb) in source.probes {
            pixel_truth(&mut expected, source, x, y, rgb)?;
        }
    }
    Ok(house.fixture(expected))
}
