//! Paragraph layout: greedy word wrapping inside a box, alignment, explicit
//! line breaks and pair kerning. One glyph per Unicode scalar, left to right.

use std::io;

use super::bounds::*;
use super::page::{PageBuilder, Paint, use_font, win_ansi};
use super::{DocumentBuilder, Font, FontId};
use crate::{Value, serialize_value};

/// Tolerance in points when a line exactly fills the box.
const FIT_EPSILON: f32 = 0.001;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Align {
    Left,
    Right,
    Center,
    /// Stretches the spaces of every line except the last line of each
    /// paragraph and lines that end at an explicit line break.
    Justify,
}

/// `line_height` is a multiple of `size`; the distance between baselines is
/// `size * line_height`. Kerning applies a TrueType `kern` table when the font
/// has one; standard fonts are never kerned.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextStyle {
    pub font: FontId,
    pub size: f32,
    pub color: Paint,
    pub line_height: f32,
    pub align: Align,
    pub kerning: bool,
}

impl TextStyle {
    pub fn new(font: FontId, size: f32) -> Self {
        Self {
            font,
            size,
            color: Paint::BLACK,
            line_height: 1.2,
            align: Align::Left,
            kerning: true,
        }
    }

    pub fn color(mut self, color: impl Into<Paint>) -> Self {
        self.color = color.into();
        self
    }

    pub fn line_height(mut self, line_height: f32) -> Self {
        self.line_height = line_height;
        self
    }

    pub fn align(mut self, align: Align) -> Self {
        self.align = align;
        self
    }

    pub fn kerning(mut self, kerning: bool) -> Self {
        self.kerning = kerning;
        self
    }
}

/// The result of laying out a paragraph.
///
/// `consumed` is the byte length of the text prefix that was placed. When
/// `complete` is false the box was too short, and `&text[consumed..]` can be
/// continued in another box, for example on the next page.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Flow {
    pub lines: usize,
    pub height: f32,
    pub consumed: usize,
    pub complete: bool,
}

struct Line {
    /// Byte ranges of the words, joined by single spaces when drawn.
    words: Vec<(usize, usize)>,
    /// Byte offset where this line starts in the source text.
    start: usize,
    width: f32,
    /// The last line of a paragraph or before an explicit break.
    last: bool,
}

/// Advance in 1/1000 em, the encoded bytes, and the glyph to record as used.
fn glyph(font: &Font, character: char) -> io::Result<(f32, [u8; 2], usize, Option<u16>)> {
    match font {
        Font::Standard(standard) => {
            let code = win_ansi(character);
            let shown = if code == b'?' { '?' } else { character };
            let width = pdf_text::standard14::width(standard.name().as_bytes(), shown)
                .or_else(|| pdf_text::standard14::width(standard.name().as_bytes(), '?'))
                .unwrap_or(500.0);
            Ok((width as f32, [code, 0], 1, None))
        }
        Font::TrueType(font) => {
            let glyph = font.glyph(character)?;
            let advance = font.scale(i32::from(font.metric(glyph)?.0));
            Ok((advance as f32, glyph.to_be_bytes(), 2, Some(glyph)))
        }
    }
}

fn kerning(font: &Font, left: char, right: char) -> io::Result<f32> {
    match font {
        Font::Standard(_) => Ok(0.0),
        Font::TrueType(font) => {
            let value = font.kerning(font.glyph(left)?, font.glyph(right)?)?;
            Ok(font.scale(i32::from(value)) as f32)
        }
    }
}

/// Ascender in 1/1000 em.
fn ascent(font: &Font) -> io::Result<f32> {
    match font {
        Font::Standard(standard) => Ok(standard.vertical().0),
        Font::TrueType(font) => Ok(font.ascent()? as f32),
    }
}

fn font_at(doc: &DocumentBuilder, font: FontId) -> io::Result<&Font> {
    let index = usize::try_from(font.0).map_err(|_| invalid("font identifier overflow"))?;
    doc.fonts
        .get(index)
        .ok_or_else(|| invalid("unknown font identifier"))
}

fn size_check(size: f32) -> io::Result<()> {
    coordinate(size)?;
    if size <= 0.0 {
        return Err(invalid("text size must be positive"));
    }
    Ok(())
}

/// Width in 1/1000 em of `text`, including kerning between its characters.
fn units(font: &Font, text: &str, kern: bool) -> io::Result<f32> {
    let mut total = 0.0f32;
    let mut previous = None;
    for character in text.chars() {
        if let (true, Some(left)) = (kern, previous) {
            total += kerning(font, left, character)?;
        }
        total += glyph(font, character)?.0;
        previous = Some(character);
    }
    Ok(total)
}

pub(super) fn measure(
    doc: &DocumentBuilder,
    font: FontId,
    size: f32,
    text: &str,
    kern: bool,
) -> io::Result<f32> {
    check("text_bytes", text.len(), MAX_TEXT_BYTES)?;
    size_check(size)?;
    Ok(units(font_at(doc, font)?, text, kern)? * size / 1000.0)
}

/// Splits a word wider than the box into pieces that fit, one character
/// at least per piece.
fn split_word(
    font: &Font,
    text: &str,
    start: usize,
    end: usize,
    limit: f32,
    kern: bool,
) -> io::Result<Vec<(usize, usize, f32)>> {
    let mut pieces = Vec::new();
    let word = text.get(start..end).ok_or_else(|| invalid("word range"))?;
    let mut piece_start = start;
    let mut width = 0.0f32;
    let mut previous = None;
    for (offset, character) in word.char_indices() {
        let at = add(start, offset)?;
        let mut step = glyph(font, character)?.0;
        if let (true, Some(left)) = (kern, previous) {
            step += kerning(font, left, character)?;
        }
        if at > piece_start && width + step > limit + FIT_EPSILON {
            push(&mut pieces, (piece_start, at, width), "text_lines", MAX_OPS)?;
            piece_start = at;
            width = glyph(font, character)?.0;
        } else {
            width += step;
        }
        previous = Some(character);
    }
    push(
        &mut pieces,
        (piece_start, end, width),
        "text_lines",
        MAX_OPS,
    )?;
    Ok(pieces)
}

fn layout(font: &Font, text: &str, width: f32, size: f32, kern: bool) -> io::Result<Vec<Line>> {
    // Work in 1/1000 em so the box width is compared once per decision.
    let limit = width * 1000.0 / size;
    let space = glyph(font, ' ')?.0;
    let mut lines = Vec::new();
    let mut offset = 0usize;
    for hard in text.split('\n') {
        let hard_start = offset;
        offset = add(add(offset, hard.len())?, 1)?;
        let body = hard.strip_suffix('\r').unwrap_or(hard);
        let mut words = Vec::new();
        let mut word_start = None;
        for (index, character) in body.char_indices() {
            let blank = character == ' ' || character == '\t';
            match (blank, word_start) {
                (true, Some(start)) => {
                    push(
                        &mut words,
                        (add(hard_start, start)?, add(hard_start, index)?),
                        "text_words",
                        MAX_OPS,
                    )?;
                    word_start = None;
                }
                (false, None) => word_start = Some(index),
                _ => {}
            }
        }
        if let Some(start) = word_start {
            push(
                &mut words,
                (add(hard_start, start)?, add(hard_start, body.len())?),
                "text_words",
                MAX_OPS,
            )?;
        }
        let mut current = Line {
            words: Vec::new(),
            start: hard_start,
            width: 0.0,
            last: false,
        };
        let mut last_char: Option<char> = None;
        for (start, end) in words {
            let word = text.get(start..end).ok_or_else(|| invalid("word range"))?;
            let first = word.chars().next().ok_or_else(|| invalid("empty word"))?;
            let word_width = units(font, word, kern)?;
            let joined = match last_char {
                Some(left) => {
                    let mut gap = space;
                    if kern {
                        gap += kerning(font, left, ' ')? + kerning(font, ' ', first)?;
                    }
                    current.width + gap + word_width
                }
                None => word_width,
            };
            // Append to a line that has room, or start an empty line with a word that fits.
            let fits = if current.words.is_empty() {
                word_width <= limit + FIT_EPSILON
            } else {
                joined <= limit + FIT_EPSILON
            };
            if fits {
                push(&mut current.words, (start, end), "text_words", MAX_OPS)?;
                current.width = if current.words.len() == 1 {
                    word_width
                } else {
                    joined
                };
                last_char = word.chars().last();
                continue;
            }
            if !current.words.is_empty() {
                let next = Line {
                    words: Vec::new(),
                    start,
                    width: 0.0,
                    last: false,
                };
                push(
                    &mut lines,
                    std::mem::replace(&mut current, next),
                    "text_lines",
                    MAX_OPS,
                )?;
            }
            if word_width <= limit + FIT_EPSILON {
                push(&mut current.words, (start, end), "text_words", MAX_OPS)?;
                current.width = word_width;
                last_char = word.chars().last();
                continue;
            }
            let mut pieces = split_word(font, text, start, end, limit, kern)?;
            let tail = pieces.pop().ok_or_else(|| invalid("word pieces"))?;
            for (piece_start, piece_end, piece_width) in pieces {
                push(
                    &mut lines,
                    Line {
                        words: vec![(piece_start, piece_end)],
                        start: piece_start,
                        width: piece_width,
                        last: false,
                    },
                    "text_lines",
                    MAX_OPS,
                )?;
            }
            current = Line {
                words: vec![(tail.0, tail.1)],
                start: tail.0,
                width: tail.2,
                last: false,
            };
            last_char = text
                .get(tail.0..tail.1)
                .and_then(|piece| piece.chars().last());
        }
        current.last = true;
        push(&mut lines, current, "text_lines", MAX_OPS)?;
    }
    Ok(lines)
}

fn line_text(text: &str, line: &Line) -> io::Result<String> {
    let mut out = String::new();
    for (index, (start, end)) in line.words.iter().enumerate() {
        if index != 0 {
            out.push(' ');
        }
        out.push_str(
            text.get(*start..*end)
                .ok_or_else(|| invalid("word range"))?,
        );
    }
    Ok(out)
}

impl PageBuilder<'_> {
    /// Lays out `text` inside `area` (`[x, y, width, height]`, y up), starting
    /// at the top edge. Words wrap at spaces; a word wider than the box breaks
    /// between characters. `\n` forces a line break and an empty line keeps
    /// its height. Runs of spaces and tabs collapse to one space.
    pub fn paragraph(&mut self, style: &TextStyle, area: [f32; 4], text: &str) -> Flow {
        let mut flow = Flow::default();
        self.perform(|doc, index| {
            flow = paragraph(doc, index, style, area, text)?;
            Ok(())
        });
        flow
    }
}

fn paragraph(
    doc: &mut DocumentBuilder,
    index: usize,
    style: &TextStyle,
    area: [f32; 4],
    text: &str,
) -> io::Result<Flow> {
    check("text_bytes", text.len(), MAX_TEXT_BYTES)?;
    size_check(style.size)?;
    let [x, y, width, height] = area;
    rect(x, y, width, height)?;
    if width <= 0.0 {
        return Err(invalid("paragraph width must be positive"));
    }
    if !style.line_height.is_finite() || style.line_height <= 0.0 || style.line_height > 100.0 {
        return Err(invalid("line height must be positive and at most 100"));
    }
    let color = style.color.command(false)?;
    let font = font_at(doc, style.font)?;
    let embedded = matches!(font, Font::TrueType(_));
    let lines = layout(font, text, width, style.size, style.kerning)?;
    let ascent = ascent(font)? * style.size / 1000.0;
    let advance = style.size * style.line_height;
    let top = y + height;
    let mut flow = Flow {
        complete: true,
        consumed: text.len(),
        ..Flow::default()
    };
    let mut placed = 0usize;
    for line in &lines {
        let bottom = (placed as f32 + 1.0) * advance;
        if bottom > height + FIT_EPSILON {
            flow.complete = false;
            flow.consumed = line.start;
            break;
        }
        placed += 1;
    }
    for (number, line) in lines.iter().take(placed).enumerate() {
        let content = line_text(text, line)?;
        if content.is_empty() {
            continue;
        }
        let line_width = line.width * style.size / 1000.0;
        let gaps = line.words.len().saturating_sub(1);
        let (offset, stretch) = match style.align {
            Align::Left => (0.0, 0.0),
            Align::Right => (width - line_width, 0.0),
            Align::Center => ((width - line_width) / 2.0, 0.0),
            Align::Justify if !line.last && gaps > 0 && line_width < width => {
                (0.0, (width - line_width) / gaps as f32)
            }
            Align::Justify => (0.0, 0.0),
        };
        let baseline = top - ascent - number as f32 * advance;
        let mut parts: Vec<Value> = Vec::new();
        let mut segment = Vec::new();
        let mut previous = None;
        let font = font_at(doc, style.font)?;
        let mut used = Vec::new();
        for character in content.chars() {
            let mut adjust = 0.0f32;
            if let (true, Some(left)) = (style.kerning, previous) {
                adjust -= kerning(font, left, character)?;
            }
            if previous == Some(' ') {
                adjust -= stretch * 1000.0 / style.size;
            }
            if adjust.abs() > 0.0005 {
                if !segment.is_empty() {
                    let bytes = std::mem::take(&mut segment);
                    parts.push(if embedded {
                        Value::HexStr(bytes)
                    } else {
                        Value::Str(bytes)
                    });
                }
                parts.push(Value::Real(f64::from(adjust)));
            }
            let (_, code, length, glyph_id) = glyph(font, character)?;
            append(
                &mut segment,
                code.get(..length).ok_or_else(|| invalid("glyph code"))?,
                MAX_COMMAND_BYTES,
            )?;
            if let Some(glyph_id) = glyph_id {
                push(&mut used, (glyph_id, character), "text_glyphs", MAX_OPS)?;
            }
            previous = Some(character);
        }
        if !segment.is_empty() {
            parts.push(if embedded {
                Value::HexStr(segment)
            } else {
                Value::Str(segment)
            });
        }
        if let Some(Font::TrueType(font)) = usize::try_from(style.font.0)
            .ok()
            .and_then(|font_index| doc.fonts.get_mut(font_index))
        {
            for (glyph_id, character) in used {
                font.used.entry(glyph_id).or_insert(character);
            }
        }
        let mut out = copied(b"q\n", MAX_COMMAND_BYTES)?;
        if embedded {
            append(&mut out, b"/Span << /ActualText ", MAX_COMMAND_BYTES)?;
            let actual = serialize_value(&Value::HexStr(unicode(&content, MAX_TEXT_BYTES)?))?;
            append(&mut out, &actual, MAX_COMMAND_BYTES)?;
            append(&mut out, b" >> BDC\n", MAX_COMMAND_BYTES)?;
        }
        append(&mut out, &color, MAX_COMMAND_BYTES)?;
        append(
            &mut out,
            format!("BT\n/F{} ", style.font.0).as_bytes(),
            MAX_COMMAND_BYTES,
        )?;
        append(&mut out, &numbers(&[style.size])?, MAX_COMMAND_BYTES)?;
        append(&mut out, b"Tf\n1 0 0 1 ", MAX_COMMAND_BYTES)?;
        append(
            &mut out,
            &numbers(&[x + offset, baseline])?,
            MAX_COMMAND_BYTES,
        )?;
        append(&mut out, b"Tm\n", MAX_COMMAND_BYTES)?;
        append(
            &mut out,
            &serialize_value(&Value::Array(parts))?,
            MAX_COMMAND_BYTES,
        )?;
        append(&mut out, b" TJ\nET\n", MAX_COMMAND_BYTES)?;
        if embedded {
            append(&mut out, b"EMC\n", MAX_COMMAND_BYTES)?;
        }
        append(&mut out, b"Q\n", MAX_COMMAND_BYTES)?;
        doc.charge(mul(content.len(), 4)?)?;
        use_font(doc, index, style.font)?;
        doc.emit(index, &out)?;
    }
    flow.lines = placed;
    flow.height = placed as f32 * advance;
    Ok(flow)
}
