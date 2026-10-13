//! Render fixtures with geometry-derived pixel expectations.

use std::io;

use super::{
    encode, fixture_resources, image_dictionary, manifest, name, rect, reference, sha256, Encoding,
    Fixture, House, Value,
};

mod truetype;

type Point = [f64; 2];
type Curves = [[Point; 4]; 4];

#[derive(Clone, Copy)]
struct Color([f64; 3]);

const WHITE: Color = Color([1.0; 3]);
const BLACK: Color = Color([0.0; 3]);

impl Color {
    fn over(self, background: Self, alpha: f64) -> Self {
        let mut result = [0.0; 3];
        for (out, (foreground, background)) in
            result.iter_mut().zip(self.0.into_iter().zip(background.0))
        {
            *out = foreground * alpha + background * (1.0 - alpha);
        }
        Self(result)
    }

    fn bytes(self) -> [u8; 3] {
        self.0
            .map(|value| (value.clamp(0.0, 1.0) * 255.0).round() as u8)
    }
}

#[derive(Clone, Copy)]
enum Ink {
    Rgb([f64; 3]),
    Cmyk([f64; 4]),
    Gray(f64),
}

impl Ink {
    fn color(self) -> Color {
        match self {
            Self::Rgb(rgb) => Color(rgb),
            Self::Cmyk([c, m, y, k]) => Color([
                (1.0 - c) * (1.0 - k),
                (1.0 - m) * (1.0 - k),
                (1.0 - y) * (1.0 - k),
            ]),
            Self::Gray(gray) => Color([gray; 3]),
        }
    }

    fn operator(self, stroke: bool) -> String {
        match self {
            Self::Rgb([r, g, b]) => {
                format!("{r} {g} {b} {}", if stroke { "RG" } else { "rg" })
            }
            Self::Cmyk([c, m, y, k]) => {
                format!("{c} {m} {y} {k} {}", if stroke { "K" } else { "k" })
            }
            Self::Gray(gray) => format!("{gray} {}", if stroke { "G" } else { "g" }),
        }
    }
}

#[derive(Clone, Copy)]
struct Rectangle([f64; 4]);

impl Rectangle {
    fn contains(self, [px, py]: Point) -> bool {
        let [x, y, width, height] = self.0;
        px > x && px < x + width && py > y && py < y + height
    }

    fn stroke_contains(self, point: Point, width: f64) -> bool {
        let [x, y, w, h] = self.0;
        let half = width * 0.5;
        Rectangle([x - half, y - half, w + width, h + width]).contains(point)
            && !Rectangle([x + half, y + half, w - width, h - width]).contains(point)
    }
}

const FILLS: [(Rectangle, Ink); 3] = [
    (
        Rectangle([20.0, 400.0, 80.0, 50.0]),
        Ink::Rgb([1.0, 0.0, 0.0]),
    ),
    (
        Rectangle([130.0, 400.0, 80.0, 50.0]),
        Ink::Cmyk([0.2, 0.6, 0.0, 0.25]),
    ),
    (Rectangle([240.0, 400.0, 80.0, 50.0]), Ink::Gray(0.25)),
];

const STROKES: [(Rectangle, Ink); 3] = [
    (
        Rectangle([20.0, 330.0, 80.0, 40.0]),
        Ink::Rgb([0.0, 0.6, 0.2]),
    ),
    (
        Rectangle([130.0, 330.0, 80.0, 40.0]),
        Ink::Cmyk([1.0, 0.0, 0.0, 0.0]),
    ),
    (Rectangle([240.0, 330.0, 80.0, 40.0]), Ink::Gray(0.5)),
];

const STAR: [Point; 5] = [
    [0.0, 40.0],
    [24.0, -32.0],
    [-38.0, 12.0],
    [38.0, 12.0],
    [-24.0, -32.0],
];

const BLOB: Curves = [
    [[20.0, 170.0], [20.0, 200.0], [40.0, 210.0], [60.0, 210.0]],
    [[60.0, 210.0], [90.0, 210.0], [100.0, 190.0], [100.0, 170.0]],
    [[100.0, 170.0], [100.0, 140.0], [80.0, 130.0], [60.0, 130.0]],
    [[60.0, 130.0], [30.0, 130.0], [20.0, 150.0], [20.0, 170.0]],
];

const CIRCLE: Curves = [
    [
        [340.0, 180.0],
        [340.0, 202.09139],
        [322.09139, 220.0],
        [300.0, 220.0],
    ],
    [
        [300.0, 220.0],
        [277.90861, 220.0],
        [260.0, 202.09139],
        [260.0, 180.0],
    ],
    [
        [260.0, 180.0],
        [260.0, 157.90861],
        [277.90861, 140.0],
        [300.0, 140.0],
    ],
    [
        [300.0, 140.0],
        [322.09139, 140.0],
        [340.0, 157.90861],
        [340.0, 180.0],
    ],
];

const BASE: Rectangle = Rectangle([40.0, 20.0, 100.0, 50.0]);
const ALPHA: Rectangle = Rectangle([90.0, 40.0, 100.0, 50.0]);
const BLUE: Color = Color([0.0, 0.0, 1.0]);
const RED: Color = Color([1.0, 0.0, 0.0]);

fn append_rectangle(out: &mut Vec<u8>, rectangle: Rectangle, ink: Ink, stroke: bool) {
    let [x, y, w, h] = rectangle.0;
    out.extend_from_slice(
        format!(
            "q {} {}{x} {y} {w} {h} re {} Q\n",
            ink.operator(stroke),
            if stroke { "10 w 0 J 0 j " } else { "" },
            if stroke { "S" } else { "f" },
        )
        .as_bytes(),
    );
}

fn append_curves(out: &mut Vec<u8>, curves: &Curves) {
    let [x, y] = curves[0][0];
    out.extend_from_slice(format!("{x} {y} m\n").as_bytes());
    for curve in curves {
        let [_, [x1, y1], [x2, y2], [x3, y3]] = *curve;
        out.extend_from_slice(format!("{x1} {y1} {x2} {y2} {x3} {y3} c\n").as_bytes());
    }
    out.extend_from_slice(b"h\n");
}

fn append_star(out: &mut Vec<u8>, center: Point, even_odd: bool) {
    for (index, [x, y]) in STAR.into_iter().enumerate() {
        let x = x + center[0];
        let y = y + center[1];
        out.extend_from_slice(
            format!("{x} {y} {}\n", if index == 0 { "m" } else { "l" },).as_bytes(),
        );
    }
    out.extend_from_slice(if even_odd { b"h f*\n" } else { b"h f\n" });
}

/// All callers supply at most 256 vertices. No vertex storage is allocated.
fn contains(points: impl IntoIterator<Item = Point>, point: Point, even_odd: bool) -> bool {
    let mut points = points.into_iter().take(256);
    let Some(first) = points.next() else {
        return false;
    };
    let mut previous = first;
    let mut winding = 0i32;
    for next in points.chain(std::iter::once(first)) {
        let side = (next[0] - previous[0]) * (point[1] - previous[1])
            - (point[0] - previous[0]) * (next[1] - previous[1]);
        if previous[1] <= point[1] && next[1] > point[1] && side > 0.0 {
            winding += 1;
        } else if previous[1] > point[1] && next[1] <= point[1] && side < 0.0 {
            winding -= 1;
        }
        previous = next;
    }
    if even_odd {
        winding & 1 != 0
    } else {
        winding != 0
    }
}

/// A fixed subdivision of the same cubics emitted into the PDF.
/// Probe clearances exceed the subdivision error by more than three pixels.
fn flatten(curves: &Curves) -> [Point; 256] {
    let mut points = [[0.0; 2]; 256];
    for (curve, output) in curves.iter().zip(points.as_chunks_mut::<64>().0) {
        let [p0, p1, p2, p3] = *curve;
        for (step, slot) in output.iter_mut().enumerate() {
            let t = step as f64 / 64.0;
            let s = 1.0 - t;
            for (axis, value) in slot.iter_mut().enumerate() {
                *value = s * s * s * p0[axis]
                    + 3.0 * s * s * t * p1[axis]
                    + 3.0 * s * t * t * p2[axis]
                    + t * t * t * p3[axis];
            }
        }
    }
    points
}

fn pixel(expected: &mut String, x: u32, y: u32, color: Color) {
    let [r, g, b] = color.bytes();
    expected.push_str(&format!("page.0.pixel.{x}.{y}={r:02x}{g:02x}{b:02x}\n"));
}

fn unrotated_pixel(
    expected: &mut String,
    height: u32,
    x: u32,
    y: u32,
    color: Color,
) -> io::Result<()> {
    let device_y = height
        .checked_sub(y)
        .and_then(|value| value.checked_sub(1))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "pixel outside page"))?;
    pixel(expected, x, device_y, color);
    Ok(())
}

fn finish(
    mut house: House,
    plain: &[u8],
    resources: Value,
    resource_keys: &str,
    media: [f64; 4],
    crop: [f64; 4],
    rotate: u16,
) -> io::Result<Fixture> {
    let page = house.add_page(
        house.pages,
        plain,
        Encoding::Plain,
        vec![
            ("Resources".to_owned(), resources),
            ("MediaBox".to_owned(), rect(media)),
            ("CropBox".to_owned(), rect(crop)),
            ("Rotate".to_owned(), Value::Int(i64::from(rotate))),
        ],
    )?;
    house.write_base(&[page.id], 1)?;
    house.table(Vec::new())?;
    let mut expected = manifest("1.4", "table", 1, 1, 0, false);
    expected.push_str(&format!(
        "page.0.media_box={}\npage.0.crop_box={}\n\
         page.0.rotate={rotate}\npage.0.contents=1\n\
         page.0.content_sha256={}\npage.0.resource_keys={resource_keys}\n\
         page.0.render=ok\n",
        super::box_text(media),
        super::box_text(crop),
        sha256::hex(plain),
    ));
    Ok(house.fixture(expected))
}

fn shapes_color(point: Point, blob: &[Point; 256], circle: &[Point; 256]) -> Color {
    let mut color = WHITE;
    for (rectangle, ink) in FILLS {
        if rectangle.contains(point) {
            color = ink.color();
        }
    }
    for (rectangle, ink) in STROKES {
        if rectangle.stroke_contains(point, 10.0) {
            color = ink.color();
        }
    }
    if contains(blob.iter().copied(), point, false) {
        color = Color([1.0, 0.5, 0.0]);
    }
    for (center, even_odd) in [([100.0, 260.0], true), ([250.0, 260.0], false)] {
        if contains(
            STAR.into_iter()
                .map(|[x, y]| [x + center[0], y + center[1]]),
            point,
            even_odd,
        ) {
            color = Color([1.0, 0.0, 1.0]);
        }
    }
    if contains(circle.iter().copied(), point, false) {
        for index in 0..6 {
            if Rectangle([240.0 + f64::from(index) * 20.0, 130.0, 10.0, 100.0]).contains(point) {
                color = Color([0.0, 0.25, 1.0]);
            }
        }
    }
    if point[0] > 20.0
        && point[0] < 200.0
        && (point[1] - 100.0).abs() < 6.0
        && (point[0] - 20.0) % 36.0 < 24.0
    {
        color = BLACK;
    }
    if BASE.contains(point) {
        color = BLUE;
    }
    if ALPHA.contains(point) {
        color = RED.over(color, 0.5);
    }
    color
}

pub(super) fn shapes() -> io::Result<Fixture> {
    let house = House::new("1.4")?;
    let resources = Value::Dict(vec![
        (
            "ExtGState".to_owned(),
            Value::Dict(vec![(
                "Half".to_owned(),
                Value::Dict(vec![
                    ("Type".to_owned(), name("ExtGState")),
                    ("ca".to_owned(), Value::Real(0.5)),
                    ("CA".to_owned(), Value::Real(0.5)),
                ]),
            )]),
        ),
        ("ProcSet".to_owned(), Value::Array(vec![name("PDF")])),
    ]);
    let mut plain = Vec::with_capacity(4096);
    for (rectangle, ink) in FILLS {
        append_rectangle(&mut plain, rectangle, ink, false);
    }
    for (rectangle, ink) in STROKES {
        append_rectangle(&mut plain, rectangle, ink, true);
    }
    plain.extend_from_slice(b"q 1 0.5 0 rg\n");
    append_curves(&mut plain, &BLOB);
    plain.extend_from_slice(b"f Q\nq 1 0 1 rg\n");
    append_star(&mut plain, [100.0, 260.0], true);
    append_star(&mut plain, [250.0, 260.0], false);
    plain.extend_from_slice(b"Q\nq\n");
    append_curves(&mut plain, &CIRCLE);
    plain.extend_from_slice(b"W n\n");
    for index in 0..6 {
        append_rectangle(
            &mut plain,
            Rectangle([240.0 + f64::from(index) * 20.0, 130.0, 10.0, 100.0]),
            Ink::Rgb([0.0, 0.25, 1.0]),
            false,
        );
    }
    plain.extend_from_slice(b"Q\nq 0 0 0 RG 12 w 0 J [24 12] 0 d 20 100 m 200 100 l S Q\n");
    append_rectangle(&mut plain, BASE, Ink::Rgb(BLUE.0), false);
    plain.extend_from_slice(b"q /Half gs\n");
    append_rectangle(&mut plain, ALPHA, Ink::Rgb(RED.0), false);
    plain.extend_from_slice(b"Q\n");
    let bounds = [0.0, 0.0, 400.0, 480.0];
    let mut fixture = finish(
        house,
        &plain,
        resources,
        "ExtGState,ProcSet",
        bounds,
        bounds,
        0,
    )?;
    let blob = flatten(&BLOB);
    let circle = flatten(&CIRCLE);
    // Coordinates select pixel centres at (x + 0.5, y + 0.5) in PDF space.
    for (x, y) in [
        (60, 425),
        (125, 425),
        (170, 425),
        (235, 425),
        (280, 425),
        (330, 425),
        (20, 350),
        (60, 350),
        (130, 350),
        (170, 350),
        (240, 350),
        (280, 350),
        (10, 350),
        (60, 170),
        (12, 170),
        (60, 218),
        (100, 260),
        (100, 280),
        (145, 260),
        (250, 260),
        (250, 280),
        (285, 180),
        (295, 180),
        (345, 180),
        (305, 215),
        (285, 130),
        (32, 100),
        (50, 100),
        (68, 100),
        (210, 100),
        (32, 110),
        (60, 40),
        (170, 60),
        (110, 55),
        (30, 40),
        (200, 60),
    ] {
        let point = [f64::from(x) + 0.5, f64::from(y) + 0.5];
        unrotated_pixel(
            &mut fixture.expected,
            480,
            x,
            y,
            shapes_color(point, &blob, &circle),
        )?;
    }
    Ok(fixture)
}

const RGB_SAMPLES: [u8; 36] = [
    255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 0, 0, 255, 255, 255, 0, 255, 32, 64, 96, 96, 128,
    160, 192, 224, 255, 17, 34, 51, 68, 85, 102, 119, 136, 153,
];
const ALPHA_SAMPLES: [u8; 12] = [255, 192, 128, 64, 0, 255, 255, 192, 128, 64, 32, 16];

pub(super) fn images() -> io::Result<Fixture> {
    let mut house = House::new("1.4")?;
    let image = house.writer.alloc()?;
    let alpha = house.writer.alloc()?;
    house.writer.stream(
        image,
        image_dictionary(
            4,
            3,
            Some(name("DeviceRGB")),
            Some(8),
            vec![
                ("Filter".to_owned(), name("FlateDecode")),
                ("SMask".to_owned(), reference(alpha)),
                ("Interpolate".to_owned(), Value::Bool(false)),
            ],
        ),
        &encode::flate(&RGB_SAMPLES),
    )?;
    house.writer.stream(
        alpha,
        image_dictionary(
            4,
            3,
            Some(name("DeviceGray")),
            Some(8),
            vec![("Filter".to_owned(), name("FlateDecode"))],
        ),
        &encode::flate(&ALPHA_SAMPLES),
    )?;
    let plain = b"q 120 0 0 90 30 30 cm /RGB Do Q\n";
    let bounds = [0.0, 0.0, 180.0, 150.0];
    let mut fixture = finish(
        house,
        plain,
        fixture_resources(&[], &[("RGB", image)]),
        "ProcSet,XObject",
        bounds,
        bounds,
        0,
    )?;
    fixture.expected.push_str("page.0.images=1\n");
    let positions = [
        (44, 44),
        (74, 44),
        (104, 44),
        (134, 44),
        (44, 74),
        (74, 74),
        (104, 74),
        (134, 74),
        (44, 104),
        (74, 104),
        (104, 104),
        (134, 104),
    ];
    for ((samples, alpha), (x, y)) in RGB_SAMPLES
        .as_chunks::<3>()
        .0
        .iter()
        .zip(ALPHA_SAMPLES)
        .zip(positions)
    {
        let [r, g, b] = samples;
        let color = Color([
            f64::from(*r) / 255.0,
            f64::from(*g) / 255.0,
            f64::from(*b) / 255.0,
        ]);
        pixel(
            &mut fixture.expected,
            x,
            y,
            color.over(WHITE, f64::from(alpha) / 255.0),
        );
    }
    for (x, y) in [(20, 20), (20, 75), (160, 75), (75, 20), (75, 130)] {
        pixel(&mut fixture.expected, x, y, WHITE);
    }
    Ok(fixture)
}

fn glyph_color(point: Point) -> Color {
    let square = |origin: f64| {
        contains(
            truetype::SQUARE
                .into_iter()
                .map(|[x, y]| [f64::from(x), f64::from(y)]),
            [(point[0] - origin) / 0.072, (point[1] - 24.0) / 0.072],
            false,
        )
    };
    if square(24.0) {
        return BLACK;
    }
    let triangle_point = [(point[0] - 96.0) / 0.072, (point[1] - 24.0) / 0.072];
    if contains(
        truetype::TRIANGLE
            .into_iter()
            .map(|[x, y]| [f64::from(x), f64::from(y)]),
        triangle_point,
        false,
    ) {
        return BLACK;
    }
    let ring_point = [(point[0] - 168.0) / 0.072, (point[1] - 24.0) / 0.072];
    let hole = contains(
        truetype::HOLE
            .into_iter()
            .map(|[x, y]| [f64::from(x), f64::from(y)]),
        ring_point,
        false,
    );
    if square(168.0) && !hole {
        BLACK
    } else {
        WHITE
    }
}

pub(super) fn truetype() -> io::Result<Fixture> {
    let mut house = House::new("1.4")?;
    let program = truetype::program()?;
    let file = house.writer.alloc()?;
    let length = i64::try_from(program.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "font length exceeds i64"))?;
    house.writer.stream(
        file,
        vec![("Length1".to_owned(), Value::Int(length))],
        &program,
    )?;
    let descriptor = house.writer.alloc()?;
    house.writer.object(
        descriptor,
        &Value::Dict(vec![
            ("Type".to_owned(), name("FontDescriptor")),
            ("FontName".to_owned(), name("HouseShapes-Regular")),
            ("Flags".to_owned(), Value::Int(32)),
            ("FontBBox".to_owned(), rect([100.0, 100.0, 800.0, 800.0])),
            ("ItalicAngle".to_owned(), Value::Int(0)),
            ("Ascent".to_owned(), Value::Int(800)),
            ("Descent".to_owned(), Value::Int(0)),
            ("CapHeight".to_owned(), Value::Int(800)),
            ("StemV".to_owned(), Value::Int(100)),
            ("MissingWidth".to_owned(), Value::Int(1000)),
            ("FontFile2".to_owned(), reference(file)),
        ]),
    )?;
    let font = house.writer.alloc()?;
    house.writer.object(
        font,
        &Value::Dict(vec![
            ("Type".to_owned(), name("Font")),
            ("Subtype".to_owned(), name("TrueType")),
            ("BaseFont".to_owned(), name("HouseShapes-Regular")),
            ("Encoding".to_owned(), name("WinAnsiEncoding")),
            ("FirstChar".to_owned(), Value::Int(65)),
            ("LastChar".to_owned(), Value::Int(79)),
            (
                "Widths".to_owned(),
                Value::Array(vec![Value::Int(1000); 15]),
            ),
            ("FontDescriptor".to_owned(), reference(descriptor)),
        ]),
    )?;
    let plain = b"0 g BT /F1 72 Tf 24 24 Td (ABO) Tj ET\n";
    let bounds = [0.0, 0.0, 264.0, 144.0];
    let mut fixture = finish(
        house,
        plain,
        fixture_resources(&[("F1", font)], &[]),
        "Font,ProcSet",
        bounds,
        bounds,
        0,
    )?;
    fixture
        .expected
        .push_str("page.0.text_norm=ABO\npage.0.unmapped=0\n");
    for (x, y) in [
        (40, 50),
        (40, 75),
        (26, 50),
        (88, 50),
        (125, 48),
        (125, 38),
        (100, 65),
        (160, 50),
        (183, 56),
        (218, 56),
        (200, 75),
        (200, 38),
        (200, 56),
        (236, 50),
        (200, 90),
    ] {
        unrotated_pixel(
            &mut fixture.expected,
            144,
            x,
            y,
            glyph_color([f64::from(x) + 0.5, f64::from(y) + 0.5]),
        )?;
    }
    Ok(fixture)
}

pub(super) fn rotated() -> io::Result<Fixture> {
    let house = House::new("1.4")?;
    let rectangle = Rectangle([30.0, 40.0, 60.0, 30.0]);
    let ink = Ink::Rgb([0.2, 0.4, 1.0]);
    let mut plain = Vec::with_capacity(128);
    append_rectangle(&mut plain, rectangle, ink, false);
    let mut fixture = finish(
        house,
        &plain,
        fixture_resources(&[], &[]),
        "ProcSet",
        [0.0, 0.0, 180.0, 120.0],
        [10.0, 20.0, 170.0, 110.0],
        90,
    )?;
    // Rotated crop space is 90 by 160; inverse transform is (y + 10, x + 20).
    for (x, y) in [(35, 50), (15, 50), (55, 50), (35, 15), (35, 85), (85, 155)] {
        let point = [f64::from(y) + 10.5, f64::from(x) + 20.5];
        pixel(
            &mut fixture.expected,
            x,
            y,
            if rectangle.contains(point) {
                ink.color()
            } else {
                WHITE
            },
        );
    }
    Ok(fixture)
}
