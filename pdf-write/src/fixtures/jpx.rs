//! Source-authored JPX pixels and expectations, independent of any decoder.

use std::io;

use crate::encode;
use crate::encode_jpx::{self, Settings};

use super::{
    fixture_resources, image_dictionary, image_truth, manifest, name, rect, sha256, Encoding,
    Fixture, House, Value,
};

const BOUNDS: [f64; 4] = [0.0, 0.0, 352.0, 96.0];
const TOP: u32 = 16;
const PAGE_HEIGHT: u32 = 96;

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.to_owned())
}

struct Source {
    width: u32,
    height: u32,
    planes: Vec<Vec<u16>>,
    rgba: Vec<u8>,
}

fn scaled(value: u16, bit_depth: u8) -> io::Result<u8> {
    if !matches!(bit_depth, 8 | 12 | 16) {
        return Err(invalid("JPX fixture precision"));
    }
    let maximum = (1u32 << bit_depth).saturating_sub(1);
    let numerator = u32::from(value)
        .checked_mul(255)
        .and_then(|v| v.checked_add(maximum / 2))
        .ok_or_else(|| invalid("JPX fixture scaling overflow"))?;
    u8::try_from(numerator / maximum).map_err(|_| invalid("JPX fixture sample exceeds precision"))
}

fn source(kind: usize) -> io::Result<Source> {
    let (width, height, channels, bit_depth) = match kind {
        0 => (64u32, 48u32, 3usize, 8u8),
        1 => (40, 40, 1, 8),
        2 => (96, 64, 3, 8),
        3 => (32, 32, 4, 8),
        4 => (24, 24, 1, 16),
        _ => return Err(invalid("JPX fixture source kind")),
    };
    let count = width
        .checked_mul(height)
        .and_then(|v| usize::try_from(v).ok())
        .ok_or_else(|| invalid("JPX fixture dimensions overflow"))?;
    let mut planes = vec![Vec::with_capacity(count); channels];
    let mut rgba = Vec::with_capacity(
        count
            .checked_mul(4)
            .ok_or_else(|| invalid("JPX fixture RGBA size overflow"))?,
    );
    for y in 0..height {
        for x in 0..width {
            // All arithmetic below uses the fixed dimensions selected above.
            let samples = match kind {
                0 => [
                    (x * 255 / 63) as u16,
                    (y * 255 / 47) as u16,
                    ((x * 3 + y * 5) & 255) as u16,
                    255,
                ],
                1 => {
                    let gray = if (x / 5 + y / 5) & 1 == 0 { 24 } else { 232 };
                    [gray, gray, gray, 255]
                }
                2 => [
                    ((x * 7 + y * 3) & 255) as u16,
                    (((x * 11) ^ (y * 13)) & 255) as u16,
                    if (x / 12 + y / 8) & 1 == 0 { 15 } else { 241 },
                    255,
                ],
                3 => [
                    255,
                    if (x / 4 + y / 4) & 1 == 0 { 0 } else { 255 },
                    ((x * 8 + y * 5) & 255) as u16,
                    if x < 8 {
                        0
                    } else if x >= 24 {
                        255
                    } else {
                        ((x - 8) * 17) as u16
                    },
                ],
                4 => {
                    let gray = ((y * 24 + x) * 65535 / 575) as u16;
                    [gray, gray, gray, 65535]
                }
                _ => return Err(invalid("JPX fixture source kind")),
            };
            for (plane, sample) in planes.iter_mut().zip(samples.iter()) {
                plane.push(*sample);
            }
            if channels == 1 {
                let gray = scaled(samples[0], bit_depth)?;
                rgba.extend_from_slice(&[gray, gray, gray, 255]);
            } else {
                rgba.extend_from_slice(&[
                    scaled(samples[0], bit_depth)?,
                    scaled(samples[1], bit_depth)?,
                    scaled(samples[2], bit_depth)?,
                    if channels == 4 {
                        scaled(samples[3], bit_depth)?
                    } else {
                        255
                    },
                ]);
            }
        }
    }
    Ok(Source {
        width,
        height,
        planes,
        rgba,
    })
}

fn pixel_truth(
    expected: &mut String,
    source: &Source,
    left: u32,
    x: u32,
    y: u32,
) -> io::Result<()> {
    if x >= source.width || y >= source.height {
        return Err(invalid("JPX fixture probe outside image"));
    }
    let offset = y
        .checked_mul(source.width)
        .and_then(|v| v.checked_add(x))
        .and_then(|v| v.checked_mul(4))
        .and_then(|v| usize::try_from(v).ok())
        .ok_or_else(|| invalid("JPX fixture probe overflow"))?;
    let end = offset
        .checked_add(4)
        .ok_or_else(|| invalid("JPX fixture probe end overflow"))?;
    let pixel = source
        .rgba
        .get(offset..end)
        .ok_or_else(|| invalid("JPX fixture probe outside RGBA"))?;
    let [r, g, b, a] = pixel else {
        return Err(invalid("JPX fixture probe size"));
    };
    let alpha = u32::from(*a);
    let composite = |value: u8| -> u32 { (u32::from(value) * alpha + 127) / 255 + (255 - alpha) };
    let page_x = left
        .checked_add(x)
        .ok_or_else(|| invalid("JPX fixture page probe x overflow"))?;
    let page_y = TOP
        .checked_add(y)
        .ok_or_else(|| invalid("JPX fixture page probe y overflow"))?;
    expected.push_str(&format!(
        "page.0.pixel.{page_x}.{page_y}={:02x}{:02x}{:02x}\n",
        composite(*r),
        composite(*g),
        composite(*b),
    ));
    Ok(())
}

pub(super) fn images() -> io::Result<Fixture> {
    let mut house = House::new("1.5")?;
    let mut resources = Vec::new();
    let mut truths = Vec::new();
    let mut plain = Vec::new();
    let names = ["J1", "J2", "J3", "J4", "J5"];
    let lefts = [16u32, 96, 152, 264, 312];

    for kind in 0..5usize {
        let source = source(kind)?;
        let resource_name = names
            .get(kind)
            .ok_or_else(|| invalid("JPX fixture resource name"))?;
        let left = *lefts
            .get(kind)
            .ok_or_else(|| invalid("JPX fixture placement"))?;
        let settings = match kind {
            0 => Settings {
                levels: 2,
                rct: true,
                ..Settings::default()
            },
            1 => Settings {
                jp2: true,
                ..Settings::default()
            },
            2 => Settings {
                levels: 1,
                tile_width: 48,
                tile_height: 32,
                ..Settings::default()
            },
            3 => Settings {
                code_block_size: 64,
                jp2: true,
                alpha: true,
                ..Settings::default()
            },
            4 => Settings {
                bit_depth: 16,
                ..Settings::default()
            },
            _ => return Err(invalid("JPX fixture encoding kind")),
        };
        let components: Vec<&[u16]> = source.planes.iter().map(Vec::as_slice).collect();
        let encoded = encode_jpx::encode(source.width, source.height, &components, settings)?;
        let object = house.writer.alloc()?;
        let color_space = match kind {
            1 => Some(name("DeviceGray")),
            2 => Some(name("DeviceRGB")),
            _ => None,
        };
        let mut extra = vec![
            (
                "Filter".to_owned(),
                if kind == 2 {
                    Value::Array(vec![name("FlateDecode"), name("JPXDecode")])
                } else {
                    name("JPXDecode")
                },
            ),
            ("Interpolate".to_owned(), Value::Bool(false)),
        ];
        if kind == 1 {
            extra.push((
                "Decode".to_owned(),
                Value::Array(vec![Value::Int(0), Value::Int(1)]),
            ));
        }
        if kind == 2 {
            extra.push((
                "Decode".to_owned(),
                Value::Array(vec![
                    Value::Int(0),
                    Value::Int(1),
                    Value::Int(0),
                    Value::Int(1),
                    Value::Int(0),
                    Value::Int(1),
                ]),
            ));
        }
        if kind == 3 {
            extra.push(("SMaskInData".to_owned(), Value::Int(1)));
        }
        // The 16-bit codestream deliberately declares 8 in the PDF dictionary.
        // JPX precision, rather than /BitsPerComponent, controls its scaling.
        let bits_per_component = if matches!(kind, 1 | 2 | 4) {
            Some(8)
        } else {
            None
        };
        let data = if kind == 2 {
            encode::flate(&encoded)
        } else {
            encoded
        };
        house.writer.stream(
            object,
            image_dictionary(
                source.width,
                source.height,
                color_space,
                bits_per_component,
                extra,
            ),
            &data,
        )?;
        let bottom = PAGE_HEIGHT
            .checked_sub(TOP)
            .and_then(|v| v.checked_sub(source.height))
            .ok_or_else(|| invalid("JPX fixture placement overflow"))?;
        plain.extend_from_slice(
            format!(
                "q {} 0 0 {} {left} {bottom} cm /{resource_name} Do Q\n",
                source.width, source.height,
            )
            .as_bytes(),
        );
        resources.push((*resource_name, object));
        truths.push((object, left, source));
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

    let mut expected = manifest("1.5", "table", 1, 1, 0, false);
    expected.push_str(&format!(
        "page.0.media_box=0 0 352 96\n\
         page.0.crop_box=0 0 352 96\n\
         page.0.rotate=0\npage.0.contents=1\n\
         page.0.content_sha256={}\n\
         page.0.resource_keys=ProcSet,XObject\n\
         page.0.images=5\npage.0.render=ok\n\
         page.0.pixel.8.8=ffffff\n",
        sha256::hex(&plain),
    ));
    for (kind, (object, left, source)) in truths.iter().enumerate() {
        image_truth(&mut expected, *object, "pixels", Some(&source.rgba));
        let probes: &[(u32, u32)] = match kind {
            0 => &[(8, 8), (32, 24), (61, 45)],
            1 => &[(8, 8), (13, 8), (37, 37)],
            2 => &[(8, 8), (47, 31), (48, 32), (93, 61)],
            3 => &[(4, 12), (28, 8), (27, 27)],
            4 => &[(0, 0), (12, 12), (23, 23)],
            _ => return Err(invalid("JPX fixture probe kind")),
        };
        for (x, y) in probes {
            pixel_truth(&mut expected, source, *left, *x, *y)?;
        }
    }
    Ok(house.fixture(expected))
}

fn cmyk_source() -> Source {
    let width = 48u32;
    let height = 32u32;
    // Fixed fixture dimensions bound every product and sample below.
    let count = 48usize * 32;
    let mut planes: Vec<Vec<_>> = (0..4).map(|_| Vec::with_capacity(count)).collect();
    let mut rgba = Vec::with_capacity(count * 4);
    for y in 0..height {
        for x in 0..width {
            let samples = [
                ((x * 5 + y * 3) & 255) as u16,
                ((x * 7 + y * 11) & 255) as u16,
                ((x * 17 + y * 5) & 255) as u16,
                ((x * 13 + y * 7) & 255) as u16,
            ];
            for (plane, sample) in planes.iter_mut().zip(samples) {
                plane.push(sample);
            }
            // Match pdf-text's naive CMYK conversion, including truncation.
            let black = 255u32 - u32::from(samples[3]);
            let component =
                |value: u16| -> u8 { ((255u32 - u32::from(value)) * black / 255) as u8 };
            rgba.extend_from_slice(&[
                component(samples[0]),
                component(samples[1]),
                component(samples[2]),
                255,
            ]);
        }
    }
    Source {
        width,
        height,
        planes,
        rgba,
    }
}

fn gray12_source() -> io::Result<Source> {
    // Every possible unsigned 12-bit value appears once, in row-major order.
    let mut gray = Vec::with_capacity(4096);
    let mut rgba = Vec::with_capacity(4096 * 4);
    for value in 0..4096u16 {
        gray.push(value);
        let byte = scaled(value, 12)?;
        rgba.extend_from_slice(&[byte, byte, byte, 255]);
    }
    Ok(Source {
        width: 64,
        height: 64,
        planes: vec![gray],
        rgba,
    })
}

pub(super) fn tiles() -> io::Result<Fixture> {
    let mut house = House::new("1.5")?;
    let mut resources = Vec::new();
    let mut truths = Vec::new();
    let mut plain = Vec::new();
    let names = ["T1", "T2", "T3", "T4"];
    let lefts = [16u32, 128, 192, 240];
    let tile_coding = [
        encode_jpx::TileCoding {
            levels: 0,
            rct: true,
        },
        encode_jpx::TileCoding {
            levels: 1,
            rct: false,
        },
        encode_jpx::TileCoding {
            levels: 2,
            rct: false,
        },
        encode_jpx::TileCoding {
            levels: 1,
            rct: true,
        },
    ];

    for kind in 0..4usize {
        let source = match kind {
            0 => source(2)?,
            1 => cmyk_source(),
            2 => {
                let mut source = source(3)?;
                // Move planes to A, R, G, B; keep the source RGBA truth intact.
                source.planes.rotate_right(1);
                source
            }
            3 => gray12_source()?,
            _ => return Err(invalid("JPX tile fixture source kind")),
        };
        let resource_name = names
            .get(kind)
            .ok_or_else(|| invalid("JPX tile fixture resource name"))?;
        let left = *lefts
            .get(kind)
            .ok_or_else(|| invalid("JPX tile fixture placement"))?;
        let settings = match kind {
            0 => Settings {
                levels: 2,
                tile_width: 48,
                tile_height: 32,
                ..Settings::default()
            },
            1 => Settings::default(),
            2 => Settings {
                jp2: true,
                alpha: true,
                alpha_first: true,
                ..Settings::default()
            },
            3 => Settings {
                bit_depth: 12,
                ..Settings::default()
            },
            _ => return Err(invalid("JPX tile fixture encoding kind")),
        };
        let components: Vec<&[u16]> = source.planes.iter().map(Vec::as_slice).collect();
        let encoded = if kind == 0 {
            encode_jpx::encode_with_tiles(
                source.width,
                source.height,
                &components,
                settings,
                &tile_coding,
            )?
        } else {
            encode_jpx::encode(source.width, source.height, &components, settings)?
        };
        let object = house.writer.alloc()?;
        let color_space = match kind {
            0 | 2 => name("DeviceRGB"),
            1 => name("DeviceCMYK"),
            3 => name("DeviceGray"),
            _ => return Err(invalid("JPX tile fixture color space")),
        };
        let mut extra = vec![
            ("Filter".to_owned(), name("JPXDecode")),
            ("Interpolate".to_owned(), Value::Bool(false)),
        ];
        if kind == 2 {
            extra.push(("SMaskInData".to_owned(), Value::Int(1)));
        }
        // Deliberately declare 8 even for the 12-bit gray codestream.
        house.writer.stream(
            object,
            image_dictionary(
                source.width,
                source.height,
                Some(color_space),
                Some(8),
                extra,
            ),
            &encoded,
        )?;
        let bottom = PAGE_HEIGHT
            .checked_sub(TOP)
            .and_then(|v| v.checked_sub(source.height))
            .ok_or_else(|| invalid("JPX tile fixture placement overflow"))?;
        plain.extend_from_slice(
            format!(
                "q {} 0 0 {} {left} {bottom} cm /{resource_name} Do Q\n",
                source.width, source.height,
            )
            .as_bytes(),
        );
        resources.push((*resource_name, object));
        truths.push((object, left, source));
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

    let mut expected = manifest("1.5", "table", 1, 1, 0, false);
    expected.push_str(&format!(
        "page.0.media_box=0 0 352 96\n\
         page.0.crop_box=0 0 352 96\n\
         page.0.rotate=0\npage.0.contents=1\n\
         page.0.content_sha256={}\n\
         page.0.resource_keys=ProcSet,XObject\n\
         page.0.images=4\npage.0.render=ok\n\
         page.0.pixel.8.8=ffffff\n",
        sha256::hex(&plain),
    ));
    for (kind, (object, left, source)) in truths.iter().enumerate() {
        image_truth(&mut expected, *object, "pixels", Some(&source.rgba));
        let probes: &[(u32, u32)] = match kind {
            0 => &[
                (8, 8),
                (56, 8),
                (8, 40),
                (56, 40),
                (47, 31),
                (48, 31),
                (47, 32),
                (48, 32),
            ],
            1 => &[(0, 0), (13, 8), (31, 19), (47, 31)],
            2 => &[(4, 12), (16, 12), (28, 8), (27, 27)],
            3 => &[(0, 0), (8, 0), (9, 0), (63, 31), (0, 32), (63, 63)],
            _ => return Err(invalid("JPX tile fixture probe kind")),
        };
        for (x, y) in probes {
            pixel_truth(&mut expected, source, *left, *x, *y)?;
        }
    }
    Ok(house.fixture(expected))
}
