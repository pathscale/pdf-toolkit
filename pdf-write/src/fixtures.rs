//! House-owned fixture bytes and manifests, without consulting a PDF reader.

use std::fs;
use std::io;
use std::path::Path;

use crate::{encode, serialize_value, sha256, Dict, Value, Writer, XrefCompression};

const LETTER: [f64; 4] = [0.0, 0.0, 612.0, 792.0];
const SMALL: [f64; 4] = [0.0, 0.0, 420.0, 595.0];
const A4: [f64; 4] = [0.0, 0.0, 595.276, 841.89];
const CROP: [f64; 4] = [10.0, 10.0, 400.0, 500.0];

fn name(value: &str) -> Value {
    Value::Name(value.to_owned())
}

fn reference(number: u32) -> Value {
    Value::Ref(number, 0)
}

fn rect(values: [f64; 4]) -> Value {
    Value::Array(values.into_iter().map(Value::Real).collect())
}

fn count_value(count: usize) -> io::Result<Value> {
    i64::try_from(count)
        .map(Value::Int)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "page count exceeds i64"))
}

fn text(label: &str) -> io::Result<Vec<u8>> {
    let literal = serialize_value(&Value::Str(label.as_bytes().to_vec()))?;
    let mut bytes = b"BT /F1 12 Tf 72 720 Td ".to_vec();
    bytes.extend_from_slice(&literal);
    bytes.extend_from_slice(b" Tj ET\n");
    Ok(bytes)
}

fn resources(font: u32) -> Value {
    Value::Dict(vec![
        (
            "Font".to_owned(),
            Value::Dict(vec![("F1".to_owned(), reference(font))]),
        ),
        (
            "ProcSet".to_owned(),
            Value::Array(vec![name("PDF"), name("Text")]),
        ),
    ])
}

fn page_value(parent: u32, content: u32, extra: Dict) -> Value {
    let mut dict = vec![
        ("Type".to_owned(), name("Page")),
        ("Parent".to_owned(), reference(parent)),
        ("Contents".to_owned(), reference(content)),
    ];
    dict.extend(extra);
    Value::Dict(dict)
}

fn pages_value(
    parent: Option<u32>,
    kids: &[u32],
    count: usize,
    extra: Dict,
) -> io::Result<Value> {
    let mut dict = vec![
        ("Type".to_owned(), name("Pages")),
        (
            "Kids".to_owned(),
            Value::Array(kids.iter().copied().map(reference).collect()),
        ),
        ("Count".to_owned(), count_value(count)?),
    ];
    if let Some(parent) = parent {
        dict.push(("Parent".to_owned(), reference(parent)));
    }
    dict.extend(extra);
    Ok(Value::Dict(dict))
}

#[derive(Clone, Copy)]
enum Encoding {
    Plain,
    Flate,
    Hex,
    Ascii85,
    Lzw,
    RunLength,
    Up,
    Chain,
}

fn encoded(plain: &[u8], encoding: Encoding) -> io::Result<(Dict, Vec<u8>)> {
    let (filter, data) = match encoding {
        Encoding::Plain => return Ok((Vec::new(), plain.to_vec())),
        Encoding::Flate => (name("FlateDecode"), encode::flate(plain)),
        Encoding::Hex => (name("ASCIIHexDecode"), encode::ascii_hex(plain)),
        Encoding::Ascii85 => (name("ASCII85Decode"), encode::ascii85(plain)),
        Encoding::Lzw => (name("LZWDecode"), encode::lzw(plain)),
        Encoding::RunLength => (name("RunLengthDecode"), encode::run_length(plain)),
        Encoding::Up => {
            let dict = vec![
                ("Filter".to_owned(), name("FlateDecode")),
                (
                    "DecodeParms".to_owned(),
                    Value::Dict(vec![
                        ("Predictor".to_owned(), Value::Int(12)),
                        ("Colors".to_owned(), Value::Int(1)),
                        ("BitsPerComponent".to_owned(), Value::Int(8)),
                        ("Columns".to_owned(), Value::Int(16)),
                    ]),
                ),
            ];
            return Ok((dict, encode::flate(&encode::png_up(plain, 16)?)));
        }
        Encoding::Chain => (
            Value::Array(vec![name("ASCII85Decode"), name("FlateDecode")]),
            encode::ascii85(&encode::flate(plain)),
        ),
    };
    Ok((vec![("Filter".to_owned(), filter)], data))
}

struct AuthoredPage {
    id: u32,
    content: u32,
    data_start: usize,
}

struct House {
    writer: Writer,
    catalog: u32,
    pages: u32,
    font: u32,
}

impl House {
    fn new(version: &str) -> io::Result<Self> {
        let mut writer = Writer::new(version)?;
        let catalog = writer.alloc()?;
        let pages = writer.alloc()?;
        let font = writer.alloc()?;
        Ok(Self { writer, catalog, pages, font })
    }

    fn root_value(&self, kids: &[u32], count: usize) -> io::Result<Value> {
        pages_value(
            None,
            kids,
            count,
            vec![
                ("MediaBox".to_owned(), rect(LETTER)),
                ("Resources".to_owned(), resources(self.font)),
            ],
        )
    }

    fn base_values(&self, kids: &[u32], count: usize) -> io::Result<Vec<(u32, Value)>> {
        Ok(vec![
            (
                self.catalog,
                Value::Dict(vec![
                    ("Type".to_owned(), name("Catalog")),
                    ("Pages".to_owned(), reference(self.pages)),
                ]),
            ),
            (self.pages, self.root_value(kids, count)?),
            (
                self.font,
                Value::Dict(vec![
                    ("Type".to_owned(), name("Font")),
                    ("Subtype".to_owned(), name("Type1")),
                    ("BaseFont".to_owned(), name("Helvetica")),
                ]),
            ),
        ])
    }

    fn write_base(&mut self, kids: &[u32], count: usize) -> io::Result<()> {
        for (number, value) in self.base_values(kids, count)? {
            self.writer.object(number, &value)?;
        }
        Ok(())
    }

    fn add_page(
        &mut self,
        parent: u32,
        plain: &[u8],
        encoding: Encoding,
        extra: Dict,
    ) -> io::Result<AuthoredPage> {
        let id = self.writer.alloc()?;
        let content = self.writer.alloc()?;
        let (dict, bytes) = encoded(plain, encoding)?;
        let range = self.writer.stream(content, dict, &bytes)?;
        self.writer.object(id, &page_value(parent, content, extra))?;
        Ok(AuthoredPage { id, content, data_start: range.start })
    }

    fn table(&mut self, extra: Dict) -> io::Result<()> {
        self.writer.finish_table(self.catalog, extra)?;
        Ok(())
    }

    fn stream(&mut self, compression: XrefCompression) -> io::Result<()> {
        self.writer.finish_stream(self.catalog, compression, Vec::new())?;
        Ok(())
    }

    fn fixture(self, expected: String) -> Fixture {
        Fixture { pdf: self.writer.into_bytes(), expected }
    }
}

struct Fixture {
    pdf: Vec<u8>,
    expected: String,
}

fn manifest(
    version: &str,
    kind: &str,
    sections: u32,
    pages: usize,
    errors: usize,
    encrypted: bool,
) -> String {
    format!(
        "version={version}\nxref_kind={kind}\nxref_sections={sections}\n\
         encrypted={encrypted}\nrefusal=none\npages={pages}\npage_errors={errors}\n"
    )
}

fn box_text(values: [f64; 4]) -> String {
    values
        .into_iter()
        .map(|value| {
            let rounded = (value * 1000.0).round() / 1000.0;
            if rounded == 0.0 {
                "0".to_owned()
            } else {
                rounded.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

enum ContentTruth<'a> {
    Decoded(&'a [u8]),
    Refused(&'a str),
}

fn page_truth(
    expected: &mut String,
    index: usize,
    media: [f64; 4],
    crop: [f64; 4],
    rotate: u16,
    content: ContentTruth<'_>,
) {
    expected.push_str(&format!(
        "page.{index}.media_box={}\npage.{index}.crop_box={}\n\
         page.{index}.rotate={rotate}\npage.{index}.contents=1\n\
         page.{index}.resource_keys=Font,ProcSet\n",
        box_text(media),
        box_text(crop),
    ));
    match content {
        ContentTruth::Decoded(bytes) => {
            expected.push_str(&format!(
                "page.{index}.content_sha256={}\n",
                sha256::hex(bytes),
            ));
        }
        ContentTruth::Refused(code) => {
            expected.push_str(&format!("page.{index}.content_refusal={code}\n"));
        }
    }
}

fn letter_truth(expected: &mut String, index: usize, plain: &[u8]) {
    page_truth(
        expected,
        index,
        LETTER,
        LETTER,
        0,
        ContentTruth::Decoded(plain),
    );
}

fn single(plain: &[u8], encoding: Encoding) -> io::Result<(House, AuthoredPage)> {
    let mut house = House::new("1.4")?;
    let page = house.add_page(house.pages, plain, encoding, Vec::new())?;
    house.write_base(&[page.id], 1)?;
    Ok((house, page))
}

fn ordinary(
    count: usize,
    version: &str,
    encoding: Encoding,
) -> io::Result<(House, Vec<AuthoredPage>, Vec<Vec<u8>>)> {
    let mut house = House::new(version)?;
    let mut pages = Vec::new();
    let mut plains = Vec::new();
    for index in 0..count {
        let plain = text(&format!("House page {}", index + 1))?;
        pages.push(house.add_page(house.pages, &plain, encoding, Vec::new())?);
        plains.push(plain);
    }
    let kids: Vec<_> = pages.iter().map(|page| page.id).collect();
    house.write_base(&kids, count)?;
    Ok((house, pages, plains))
}

fn classic_one_page() -> io::Result<Fixture> {
    let plain = b"BT /F1 12 Tf 72 720 Td (Hello house PDF) Tj ET\n\
                  BT /F1 12 Tf 72 700 Td (Second run) Tj ET\n";
    let (mut house, _) = single(plain, Encoding::Plain)?;
    house.table(Vec::new())?;
    let mut expected = manifest("1.4", "table", 1, 1, 0, false);
    letter_truth(&mut expected, 0, plain);
    Ok(house.fixture(expected))
}

fn compressed_document(count: usize, hybrid: bool) -> io::Result<Fixture> {
    let mut house = House::new("1.5")?;
    let mut kids = Vec::new();
    let mut page_members = Vec::new();
    let mut plains = Vec::new();
    for index in 0..count {
        let page = house.writer.alloc()?;
        let content = house.writer.alloc()?;
        let plain = text(&format!("Compressed house page {}", index + 1))?;
        let (dict, data) = encoded(&plain, Encoding::Flate)?;
        house.writer.stream(content, dict, &data)?;
        kids.push(page);
        page_members.push((page, page_value(house.pages, content, Vec::new())));
        plains.push(plain);
    }
    let mut members = house.base_values(&kids, count)?;
    members.extend(page_members);
    let object_stream = house.writer.alloc()?;
    house.writer.object_stream(object_stream, &members, true)?;
    let kind = if hybrid { "hybrid" } else { "stream" };
    if hybrid {
        let offset = house.writer.xref_stream(
            house.catalog,
            XrefCompression::Flate,
            Vec::new(),
        )?;
        let offset = i64::try_from(offset).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "hybrid offset exceeds i64")
        })?;
        house.table(vec![("XRefStm".to_owned(), Value::Int(offset))])?;
    } else {
        house.stream(XrefCompression::FlateUp)?;
    }
    let mut expected = manifest("1.5", kind, 1, count, 0, false);
    for (index, plain) in plains.iter().enumerate() {
        letter_truth(&mut expected, index, plain);
    }
    Ok(house.fixture(expected))
}

fn xref_stream_objstm() -> io::Result<Fixture> {
    compressed_document(3, false)
}

fn hybrid() -> io::Result<Fixture> {
    compressed_document(2, true)
}

fn incremental() -> io::Result<Fixture> {
    let (mut house, pages, mut plains) = ordinary(2, "1.4", Encoding::Plain)?;
    house.table(Vec::new())?;
    let plain = text("House page 3, incremental")?;
    let third = house.add_page(house.pages, &plain, Encoding::Plain, Vec::new())?;
    let mut kids: Vec<_> = pages.iter().map(|page| page.id).collect();
    kids.push(third.id);
    let root = house.root_value(&kids, 3)?;
    house.writer.object(house.pages, &root)?;
    let first = pages.first().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "missing authored first page")
    })?;
    house.writer.object(
        first.id,
        &page_value(
            house.pages,
            first.content,
            vec![("MediaBox".to_owned(), rect(A4))],
        ),
    )?;
    house.table(Vec::new())?;
    plains.push(plain);
    let mut expected = manifest("1.4", "table", 2, 3, 0, false);
    for (index, plain) in plains.iter().enumerate() {
        let media = if index == 0 { A4 } else { LETTER };
        page_truth(
            &mut expected,
            index,
            media,
            media,
            0,
            ContentTruth::Decoded(plain),
        );
    }
    Ok(house.fixture(expected))
}

fn inherited_tree() -> io::Result<Fixture> {
    let mut house = House::new("1.4")?;
    let left_middle = house.writer.alloc()?;
    let left_lower = house.writer.alloc()?;
    let right_middle = house.writer.alloc()?;
    let right_lower = house.writer.alloc()?;
    let mut left_kids = Vec::new();
    let mut right_kids = Vec::new();
    let mut expected = manifest("1.4", "table", 1, 7, 0, false);
    for index in 0..7 {
        let mut extra = Vec::new();
        if index == 1 {
            extra.push(("Rotate".to_owned(), Value::Int(-90)));
        }
        if index == 2 {
            extra.push(("CropBox".to_owned(), rect(CROP)));
        }
        if index == 6 {
            extra.push(("Rotate".to_owned(), Value::Int(180)));
        }
        let left = index < 3;
        let parent = if left { left_lower } else { right_lower };
        let plain = text(&format!("Inherited house page {}", index + 1))?;
        let page = house.add_page(parent, &plain, Encoding::Plain, extra)?;
        if left {
            left_kids.push(page.id);
        } else {
            right_kids.push(page.id);
        }
        let media = if left { SMALL } else { LETTER };
        let crop = if index == 2 { CROP } else { media };
        let rotate = match index {
            1 => 270,
            6 => 180,
            0 | 2 => 90,
            _ => 0,
        };
        page_truth(
            &mut expected,
            index,
            media,
            crop,
            rotate,
            ContentTruth::Decoded(&plain),
        );
    }
    house.writer.object(
        left_lower,
        &pages_value(Some(left_middle), &left_kids, 3, Vec::new())?,
    )?;
    house.writer.object(
        right_lower,
        &pages_value(Some(right_middle), &right_kids, 4, Vec::new())?,
    )?;
    house.writer.object(
        left_middle,
        &pages_value(
            Some(house.pages),
            &[left_lower],
            3,
            vec![
                ("MediaBox".to_owned(), rect(SMALL)),
                ("Rotate".to_owned(), Value::Int(90)),
            ],
        )?,
    )?;
    house.writer.object(
        right_middle,
        &pages_value(Some(house.pages), &[right_lower], 4, Vec::new())?,
    )?;
    house.write_base(&[left_middle, right_middle], 7)?;
    house.table(Vec::new())?;
    Ok(house.fixture(expected))
}

fn filters() -> io::Result<Fixture> {
    let mut house = House::new("1.4")?;
    let kinds = [
        Encoding::Hex,
        Encoding::Ascii85,
        Encoding::Lzw,
        Encoding::RunLength,
        Encoding::Up,
        Encoding::Chain,
    ];
    let mut kids = Vec::new();
    let mut expected = manifest("1.4", "table", 1, 6, 0, false);
    for (index, kind) in kinds.into_iter().enumerate() {
        let label = match index {
            0 => "ASCIIHex house".to_owned(),
            1 => "ASCII85 house".to_owned(),
            2 => {
                let mut label = "LZW house ".to_owned();
                for number in 0..768 {
                    label.push_str(&format!("{number:04x} "));
                }
                label
            }
            3 => "RunLength house AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_owned(),
            4 => "PNG Up house".to_owned(),
            _ => "ASCII85 then Flate house".to_owned(),
        };
        let mut plain = text(&label)?;
        if index == 4 {
            while plain.len() % 16 != 0 {
                plain.push(b' ');
            }
        }
        let page = house.add_page(house.pages, &plain, kind, Vec::new())?;
        kids.push(page.id);
        letter_truth(&mut expected, index, &plain);
    }
    house.write_base(&kids, 6)?;
    house.table(Vec::new())?;
    Ok(house.fixture(expected))
}

fn prev_cycle() -> io::Result<Fixture> {
    let (mut house, _, plains) = ordinary(2, "1.4", Encoding::Plain)?;
    house.writer.finish_prev_cycle(house.catalog)?;
    let mut expected = manifest("1.4", "table", 2, 2, 0, false);
    expected.push_str("warning=prev_cycle\n");
    for (index, plain) in plains.iter().enumerate() {
        letter_truth(&mut expected, index, plain);
    }
    Ok(house.fixture(expected))
}

fn page_tree_cycle() -> io::Result<Fixture> {
    let mut house = House::new("1.4")?;
    let interior = house.writer.alloc()?;
    let first_plain = text("Cycle house page 1")?;
    let second_plain = text("Cycle house page 2")?;
    let first = house.add_page(interior, &first_plain, Encoding::Plain, Vec::new())?;
    let second = house.add_page(interior, &second_plain, Encoding::Plain, Vec::new())?;
    house.writer.object(
        interior,
        &pages_value(
            Some(house.pages),
            &[first.id, house.pages, second.id],
            2,
            Vec::new(),
        )?,
    )?;
    house.write_base(&[interior], 2)?;
    house.table(Vec::new())?;
    let mut expected = manifest("1.4", "table", 1, 2, 0, false);
    expected.push_str("warning=page_tree_cycle\n");
    letter_truth(&mut expected, 0, &first_plain);
    letter_truth(&mut expected, 1, &second_plain);
    Ok(house.fixture(expected))
}

fn encrypted_marker() -> io::Result<Fixture> {
    let plain = text("Encrypted marker house")?;
    let (mut house, _) = single(&plain, Encoding::Flate)?;
    let encrypt = Value::Dict(vec![
        ("Filter".to_owned(), name("Standard")),
        ("V".to_owned(), Value::Int(1)),
        ("R".to_owned(), Value::Int(2)),
        ("O".to_owned(), Value::HexStr(vec![0x11; 32])),
        ("U".to_owned(), Value::HexStr(vec![0x22; 32])),
        ("P".to_owned(), Value::Int(-4)),
    ]);
    house.table(vec![
        ("Encrypt".to_owned(), encrypt),
        (
            "ID".to_owned(),
            Value::Array(vec![
                Value::HexStr(vec![0x33; 16]),
                Value::HexStr(vec![0x33; 16]),
            ]),
        ),
    ])?;
    let mut expected = manifest("1.4", "table", 1, 1, 0, true);
    page_truth(
        &mut expected,
        0,
        LETTER,
        LETTER,
        0,
        ContentTruth::Refused("encrypted"),
    );
    Ok(house.fixture(expected))
}

fn flate_bomb() -> io::Result<Fixture> {
    let mut plain = vec![b' '; 64 * 1024 * 1024];
    plain.extend_from_slice(&text("Bomb house")?);
    let (mut house, _) = single(&plain, Encoding::Flate)?;
    house.table(Vec::new())?;
    let mut expected = manifest("1.4", "table", 1, 1, 0, false);
    expected.push_str("limit.max_decoded_bytes=1048576\n");
    page_truth(
        &mut expected,
        0,
        LETTER,
        LETTER,
        0,
        ContentTruth::Refused("refused:decoded_bytes"),
    );
    Ok(house.fixture(expected))
}

fn deep_nesting() -> io::Result<Fixture> {
    let mut house = House::new("1.4")?;
    let first_plain = text("Shallow house page")?;
    let first = house.add_page(house.pages, &first_plain, Encoding::Plain, Vec::new())?;
    let mut nested = Value::Null;
    for _ in 0..200 {
        nested = Value::Array(vec![nested]);
    }
    let second_plain = text("Deep house page")?;
    let second = house.add_page(
        house.pages,
        &second_plain,
        Encoding::Plain,
        vec![("Resources".to_owned(), nested)],
    )?;
    house.write_base(&[first.id, second.id], 2)?;
    house.table(Vec::new())?;
    let mut expected = manifest("1.4", "table", 1, 1, 1, false);
    expected.push_str("limit.max_depth=64\n");
    letter_truth(&mut expected, 0, &first_plain);
    Ok(house.fixture(expected))
}

fn bad_startxref() -> io::Result<Fixture> {
    let mut plain = text("Bad startxref house")?;
    plain.extend(std::iter::repeat(b' ').take(16 * 1024));
    let (mut house, page) = single(&plain, Encoding::Plain)?;
    let target = page.data_start.checked_add(8192).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "bad startxref target overflow")
    })?;
    house.writer.finish_bad_startxref(house.catalog, target as u64)?;
    Ok(house.fixture("version=1.4\nrefusal=malformed\n".to_owned()))
}

fn junk_header() -> io::Result<Fixture> {
    let plain = text("Shifted offsets house")?;
    let (mut house, _) = single(&plain, Encoding::Plain)?;
    house.table(Vec::new())?;
    let mut expected = manifest("1.4", "table", 1, 1, 0, false);
    expected.push_str("warning=offsets_shifted\n");
    letter_truth(&mut expected, 0, &plain);
    let original = house.writer.into_bytes();
    let mut pdf = b"%junk1234\n".repeat(10);
    pdf.extend_from_slice(&original);
    Ok(Fixture { pdf, expected })
}

fn many_pages() -> io::Result<Fixture> {
    let mut house = House::new("1.5")?;
    let mut root_kids = Vec::new();
    let mut expected = manifest("1.5", "stream", 1, 1000, 0, false);
    let mut index = 0usize;
    for _ in 0..10 {
        let middle = house.writer.alloc()?;
        root_kids.push(middle);
        let mut middle_kids = Vec::new();
        for _ in 0..10 {
            let lower = house.writer.alloc()?;
            middle_kids.push(lower);
            let mut lower_kids = Vec::new();
            for _ in 0..10 {
                let plain = text(&format!("Many house page {}", index + 1))?;
                let page = house.add_page(lower, &plain, Encoding::Flate, Vec::new())?;
                lower_kids.push(page.id);
                if index == 0 || index == 999 {
                    letter_truth(&mut expected, index, &plain);
                }
                index += 1;
            }
            house.writer.object(
                lower,
                &pages_value(Some(middle), &lower_kids, 10, Vec::new())?,
            )?;
        }
        house.writer.object(
            middle,
            &pages_value(Some(house.pages), &middle_kids, 100, Vec::new())?,
        )?;
    }
    house.write_base(&root_kids, 1000)?;
    house.stream(XrefCompression::Flate)?;
    Ok(house.fixture(expected))
}

/// Writes all fourteen fixtures in a fixed order. No reader is invoked.
pub fn write_fixtures(out: &Path) -> io::Result<usize> {
    type Builder = fn() -> io::Result<Fixture>;
    let builders: [(&str, Builder); 14] = [
        ("classic-one-page", classic_one_page),
        ("xref-stream-objstm", xref_stream_objstm),
        ("incremental", incremental),
        ("hybrid", hybrid),
        ("inherited-tree", inherited_tree),
        ("filters", filters),
        ("prev-cycle", prev_cycle),
        ("page-tree-cycle", page_tree_cycle),
        ("encrypted-marker", encrypted_marker),
        ("flate-bomb", flate_bomb),
        ("deep-nesting", deep_nesting),
        ("bad-startxref", bad_startxref),
        ("junk-header", junk_header),
        ("many-pages", many_pages),
    ];
    fs::create_dir_all(out)?;
    for (name, build) in builders {
        let fixture = build()?;
        fs::write(out.join(format!("{name}.pdf")), &fixture.pdf)?;
        fs::write(out.join(format!("{name}.expect")), fixture.expected.as_bytes())?;
    }
    Ok(builders.len())
}

