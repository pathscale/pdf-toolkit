//! Embedded libjxl conformance images with independent reference probes.

use std::io;

use super::{
    fixture_resources, image_dictionary, image_truth, manifest, name, rect, reference, sha256,
    Encoding, Fixture, House, Value,
};

const BOUNDS: [f64; 4] = [0.0, 0.0, 402.0, 263.0];
const PAGE_HEIGHT: u32 = 263;
const TOP: u32 = 16;
const CASE_BOUNDS: [f64; 4] = [0.0, 0.0, 712.0, 352.0];
const CASE_PAGE_HEIGHT: u32 = 352;

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

mod animation_spline {
    // libjxl conformance animation_spline, frame 0 of the reference.
    // Scale 1: every source pixel is a displayed center. The file has
    // 60 frames; decoding draws only the first keyframe.
    pub(super) const PROBES: &[(u32, u32, [u8; 3])] = &[
        (76, 4, [0x44, 0x28, 0x26]),
        (100, 20, [0x66, 0x36, 0x40]),
        (108, 20, [0x6b, 0x3c, 0x38]),
        (140, 20, [0x5c, 0x2e, 0x3a]),
        (60, 28, [0x43, 0x26, 0x2a]),
        (196, 196, [0x00, 0x00, 0x00]),
    ];
}

mod alpha_triangles {
    // libjxl alpha_triangles. Scale 8: x and y congruent to 4 modulo 8.
    // The image dictionary carries a DeviceGray SMask, a left-to-right
    // ramp (sample = x * 255 / 1023), which replaces the codestream alpha.
    // Probe RGB is the JXL color composited over white by that mask:
    // (channel * alpha + 127) / 255 + (255 - alpha).
    pub(super) const PROBES: &[(u32, u32, [u8; 3])] = &[
        (4, 4, [0xff, 0xff, 0xff]),
        (708, 68, [0x4f, 0xc9, 0xd1]),
        (260, 196, [0xdb, 0xcf, 0xff]),
        (324, 580, [0xeb, 0xc8, 0xe0]),
        (516, 708, [0xd7, 0xc0, 0xa6]),
        (260, 964, [0xff, 0xcf, 0xd2]),
    ];
}

mod corrupt_gray {
    // First half of grayscale.jxl. image_truth records kind "unsupported:jxl"
    // and this empty table contributes no pixel keys.
    pub(super) const PROBES: &[(u32, u32, [u8; 3])] = &[];
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

fn soft_mask_ramp(width: u32, height: u32) -> io::Result<Vec<u8>> {
    let span = width
        .checked_sub(1)
        .filter(|span| *span != 0)
        .ok_or_else(|| invalid("JXL fixture mask width"))?;
    let count = width
        .checked_mul(height)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| invalid("JXL fixture mask size overflow"))?;
    let mut samples = Vec::with_capacity(count);
    for _y in 0..height {
        for x in 0..width {
            let sample = u32::from(x)
                .checked_mul(255)
                .map(|value| value / span)
                .ok_or_else(|| invalid("JXL fixture mask sample overflow"))?;
            let sample =
                u8::try_from(sample).map_err(|_| invalid("JXL fixture mask sample"))?;
            samples.push(sample);
        }
    }
    Ok(samples)
}

// Page layout, top-aligned at TOP: animation 320x320 at x=16 (scale 1),
// alpha triangles displayed 128x128 at x=352 (scale 8), truncated gray
// 200x200 at x=496 (scale 1). Media box 712 by 352.
pub(super) fn cases() -> io::Result<Fixture> {
    let mut house = House::new("2.0")?;
    let mut resources = Vec::new();
    let mut painted = Vec::new();
    let mut plain = Vec::new();

    let grayscale = include_bytes!("assets/jxl/grayscale.jxl");
    let corrupt = &grayscale[..grayscale.len() / 2];
    if grayscale.is_empty() || corrupt.len() >= grayscale.len() {
        return Err(invalid("JXL fixture truncated stream"));
    }

    let sources = [
        Source {
            resource: "J1",
            data: include_bytes!("assets/jxl/animation_spline.jxl"),
            width: 320,
            height: 320,
            color_space: "DeviceRGB",
            left: 16,
            scale: 1,
            probes: animation_spline::PROBES,
        },
        Source {
            resource: "J2",
            data: include_bytes!("assets/jxl/alpha_triangles.jxl"),
            width: 1024,
            height: 1024,
            color_space: "DeviceRGB",
            left: 352,
            scale: 8,
            probes: alpha_triangles::PROBES,
        },
        Source {
            resource: "J3",
            data: corrupt,
            width: 200,
            height: 200,
            color_space: "DeviceGray",
            left: 496,
            scale: 1,
            probes: corrupt_gray::PROBES,
        },
    ];
    let kinds = ["pixels", "pixels", "unsupported:jxl"];
    let masked = [false, true, false];

    for (index, source) in sources.iter().enumerate() {
        if source.scale == 0
            || source.width % source.scale != 0
            || source.height % source.scale != 0
        {
            return Err(invalid("JXL fixture display scale"));
        }
        let display_width = source.width / source.scale;
        let display_height = source.height / source.scale;
        let bottom = CASE_PAGE_HEIGHT
            .checked_sub(TOP)
            .and_then(|value| value.checked_sub(display_height))
            .ok_or_else(|| invalid("JXL fixture placement overflow"))?;
        let mut extra = vec![
            ("Filter".to_owned(), name("JXLDecode")),
            ("Interpolate".to_owned(), Value::Bool(false)),
        ];
        let use_mask = masked
            .get(index)
            .copied()
            .ok_or_else(|| invalid("JXL fixture case mask"))?;
        if use_mask {
            let samples = soft_mask_ramp(source.width, source.height)?;
            let mask = house.writer.alloc()?;
            house.writer.stream(
                mask,
                image_dictionary(
                    source.width,
                    source.height,
                    Some(name("DeviceGray")),
                    Some(8),
                    vec![("Interpolate".to_owned(), Value::Bool(false))],
                ),
                &samples,
            )?;
            extra.push(("SMask".to_owned(), reference(mask)));
        }
        let object = house.writer.alloc()?;
        house.writer.stream(
            object,
            image_dictionary(
                source.width,
                source.height,
                Some(name(source.color_space)),
                Some(8),
                extra,
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
        let kind = kinds
            .get(index)
            .copied()
            .ok_or_else(|| invalid("JXL fixture case kind"))?;
        painted.push((object, kind));
    }

    let page = house.add_page(
        house.pages,
        &plain,
        Encoding::Plain,
        vec![
            ("Resources".to_owned(), fixture_resources(&[], &resources)),
            ("MediaBox".to_owned(), rect(CASE_BOUNDS)),
            ("CropBox".to_owned(), rect(CASE_BOUNDS)),
        ],
    )?;
    house.write_base(&[page.id], 1)?;
    house.table(Vec::new())?;

    let mut expected = manifest("2.0", "table", 1, 1, 0, false);
    expected.push_str(&format!(
        "page.0.media_box=0 0 712 352\n\
         page.0.crop_box=0 0 712 352\n\
         page.0.rotate=0\npage.0.contents=1\n\
         page.0.content_sha256={}\n\
         page.0.resource_keys=ProcSet,XObject\n\
         page.0.images=3\npage.0.render=ok\n",
        sha256::hex(&plain),
    ));
    for (source, (object, kind)) in sources.iter().zip(painted.iter()) {
        image_truth(&mut expected, *object, kind, None);
        for &(x, y, rgb) in source.probes {
            pixel_truth(&mut expected, source, x, y, rgb)?;
        }
    }
    Ok(house.fixture(expected))
}

