use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::ops::Range;

use crate::interp::{PageText, PositionedGlyph};

const MAX_CUT_DEPTH: u8 = 8;
const BASELINE_TOLERANCE: f32 = 0.3;
const SPACE_GAP: f32 = 0.25;
const COLUMN_GUTTER: f32 = 1.5;
const SIZE_CHANGE: f32 = 0.15;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockKind {
    Paragraph,
    Heading(u8),
    ListItem,
    Caption,
}

#[derive(Clone, Debug)]
pub struct Line {
    pub bbox: [f32; 4],
    pub text: String,
    pub size: f32,
    pub glyphs: Range<u32>,
}

#[derive(Clone, Debug)]
pub struct Block {
    pub kind: BlockKind,
    pub bbox: [f32; 4],
    pub lines: Range<u32>,
    pub column: u8,
}

#[derive(Clone, Debug)]
pub struct PageLayout {
    pub lines: Vec<Line>,
    pub blocks: Vec<Block>,
    pub order: Vec<u32>,
    pub columns: u8,
    pub body_size: f32,
}

impl PageLayout {
    pub fn block_text(&self, block: &Block) -> String {
        let mut out = String::new();
        let lines = self
            .lines
            .get(block.lines.start as usize..block.lines.end as usize)
            .unwrap_or(&[]);
        for (index, line) in lines.iter().enumerate() {
            if index != 0 {
                out.push('\n');
            }
            out.push_str(&line.text);
        }
        out
    }

    pub fn plain_text(&self) -> String {
        let mut out = String::new();
        for block in &self.blocks {
            let text = self.block_text(block);
            if text.is_empty() {
                continue;
            }
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(&text);
        }
        out
    }
}

#[derive(Clone, Copy)]
struct Item {
    id: u32,
    seq: u32,
    angle: i16,
    along: f32,
    across: f32,
    width: f32,
    size: f32,
    bbox: [f32; 4],
    bold: bool,
}

struct Run {
    angle: i16,
    across: f32,
    size: f32,
    items: Vec<Item>,
}

struct PendingLine {
    line: Line,
    ids: Vec<u32>,
    angle: i16,
    bold: bool,
}

fn magnitude(value: f32) -> f32 {
    if value < 0.0 { -value } else { value }
}

fn median(mut values: Vec<f32>) -> f32 {
    values.sort_unstable_by(f32::total_cmp);
    values.get(values.len() / 2).copied().unwrap_or(0.0)
}

fn count(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn union(left: [f32; 4], right: [f32; 4]) -> [f32; 4] {
    [
        left[0].min(right[0]),
        left[1].min(right[1]),
        left[2].max(right[2]),
        left[3].max(right[3]),
    ]
}

fn direction(angle: i16) -> (f32, f32) {
    match angle {
        0 => return (1.0, 0.0),
        90 => return (0.0, 1.0),
        180 => return (-1.0, 0.0),
        270 => return (0.0, -1.0),
        _ => {}
    }
    let degrees = if angle > 180 {
        i32::from(angle) - 360
    } else {
        i32::from(angle)
    };
    let x = f64::from(degrees) * core::f64::consts::PI / 180.0;
    let x2 = x * x;
    let sin = x * (1.0 - x2 * (1.0 / 6.0 - x2 * (1.0 / 120.0
        - x2 * (1.0 / 5040.0 - x2 * (1.0 / 362880.0 - x2 / 39916800.0)))));
    let cos = 1.0 - x2 * (0.5 - x2 * (1.0 / 24.0
        - x2 * (1.0 / 720.0 - x2 * (1.0 / 40320.0 - x2 / 3628800.0))));
    (cos as f32, sin as f32)
}

fn positioned(id: u32, glyph: &PositionedGlyph, text: &PageText) -> Option<Item> {
    if ![glyph.x, glyph.y, glyph.width, glyph.size]
        .iter()
        .all(|value| value.is_finite())
    {
        return None;
    }
    let size = magnitude(glyph.size).max(0.01);
    let angle = i32::from(glyph.angle).rem_euclid(360) as i16;
    let (dx, dy) = direction(angle);
    let end_x = glyph.x + dx * glyph.width;
    let end_y = glyph.y + dy * glyph.width;
    let upper_x = glyph.x + dy * size;
    let upper_y = glyph.y - dx * size;
    let upper_end_x = end_x + dy * size;
    let upper_end_y = end_y - dx * size;
    let bbox = [
        glyph.x.min(end_x).min(upper_x).min(upper_end_x),
        glyph.y.min(end_y).min(upper_y).min(upper_end_y),
        glyph.x.max(end_x).max(upper_x).max(upper_end_x),
        glyph.y.max(end_y).max(upper_y).max(upper_end_y),
    ];
    let along = dx * glyph.x + dy * glyph.y;
    let across = -dy * glyph.x + dx * glyph.y;
    if !bbox.iter().all(|value| value.is_finite())
        || !along.is_finite()
        || !across.is_finite()
    {
        return None;
    }
    Some(Item {
        id,
        seq: glyph.seq,
        angle,
        along,
        across,
        width: glyph.width,
        size,
        bbox,
        bold: text.fonts.get(usize::from(glyph.font))
            .map(|font| font.flags.bold).unwrap_or(false),
    })
}

type DuplicateKey = (i16, [char; 4], u8, u16, i64, i64);

fn unique_items(items: Vec<Item>, text: &PageText) -> Vec<Item> {
    let mut seen = BTreeMap::<DuplicateKey, (f32, f32, f32)>::new();
    let mut out = Vec::new();
    for item in items {
        let Some(glyph) = text.glyphs.get(item.id as usize) else { continue };
        let mut chars = ['\0'; 4];
        let mut len = 0u8;
        for (slot, character) in chars.iter_mut().zip(glyph.text.as_slice()) {
            *slot = *character;
            len = len.saturating_add(1);
        }
        let bucket = ((item.size.to_bits() >> 23) & 255) as u16;
        let mut duplicate = false;
        // A bounded spatial lookup also catches overprints drawn in later runs.
        for level in bucket.saturating_sub(1).max(1)..=bucket.saturating_add(1).min(254) {
            let cell = f32::from_bits(u32::from(level) << 23) * 0.1;
            let x = (glyph.x / cell) as i64;
            let y = (glyph.y / cell) as i64;
            for ox in -1i64..=1 {
                for oy in -1i64..=1 {
                    let key = (
                        item.angle, chars, len, level,
                        x.saturating_add(ox), y.saturating_add(oy),
                    );
                    if let Some(&(old_x, old_y, old_size)) = seen.get(&key) {
                        let tolerance = 0.1 * item.size.min(old_size);
                        if magnitude(glyph.x - old_x) <= tolerance
                            && magnitude(glyph.y - old_y) <= tolerance
                        {
                            duplicate = true;
                        }
                    }
                }
            }
        }
        if duplicate {
            continue;
        }
        let cell = f32::from_bits(u32::from(bucket) << 23) * 0.1;
        seen.insert(
            (item.angle, chars, len, bucket,
                (glyph.x / cell) as i64, (glyph.y / cell) as i64),
            (glyph.x, glyph.y, item.size),
        );
        out.push(item);
    }
    out
}

fn finish_line(items: Vec<Item>, text: &PageText, out: &mut Vec<PendingLine>) {
    let Some(first) = items.first() else { return };
    let angle = first.angle;
    let mut bbox = first.bbox;
    let mut value = String::new();
    let mut previous: Option<Item> = None;
    let mut bold = 0usize;
    let mut ids = Vec::new();
    for item in &items {
        let Some(glyph) = text.glyphs.get(item.id as usize) else { continue };
        if let Some(last) = previous {
            let gap = item.along - last.along - last.width;
            if gap > SPACE_GAP * item.size.max(last.size)
                && !value.is_empty() && !value.ends_with(' ')
            {
                value.push(' ');
            }
        }
        if glyph.text.is_empty() && !glyph.mapped {
            value.push('\u{fffd}');
        } else {
            for character in glyph.text.as_slice() {
                if character.is_whitespace() {
                    if !value.is_empty() && !value.ends_with(' ') {
                        value.push(' ');
                    }
                } else {
                    value.push(*character);
                }
            }
        }
        bbox = union(bbox, item.bbox);
        bold = bold.saturating_add(usize::from(item.bold));
        ids.push(item.id);
        previous = Some(*item);
    }
    while value.ends_with(' ') {
        value.pop();
    }
    let size = median(items.iter().map(|item| item.size).collect());
    out.push(PendingLine {
        line: Line { bbox, text: value, size, glyphs: 0..0 },
        ids,
        angle,
        bold: bold > items.len() / 2,
    });
}

fn finish_band(mut items: Vec<Item>, text: &PageText, out: &mut Vec<PendingLine>) {
    items.sort_unstable_by_key(|item| (item.seq, item.id));
    let jumps = items.windows(2).any(|pair| {
        match (pair.first(), pair.get(1)) {
            (Some(left), Some(right)) =>
                right.along < left.along - 0.1 * left.size.min(right.size),
            _ => false,
        }
    });
    if jumps {
        items.sort_unstable_by(|left, right| {
            left.along.total_cmp(&right.along)
                .then_with(|| left.across.total_cmp(&right.across))
                .then_with(|| left.seq.cmp(&right.seq))
                .then_with(|| left.id.cmp(&right.id))
        });
    }
    let mut line: Vec<Item> = Vec::new();
    for item in items {
        let split = line.last().map(|last| {
            item.along - last.along - last.width
                >= COLUMN_GUTTER * item.size.max(last.size)
        }).unwrap_or(false);
        if split {
            finish_line(core::mem::take(&mut line), text, out);
        }
        line.push(item);
    }
    finish_line(line, text, out);
}

fn candidate_lines(items: Vec<Item>, text: &PageText) -> Vec<PendingLine> {
    let mut runs: Vec<Run> = Vec::new();
    for item in items {
        let adjacent = runs.last().map(|run| {
            run.angle == item.angle
                && magnitude(run.across - item.across)
                    < BASELINE_TOLERANCE * run.size.min(item.size)
                && run.items.last().map(|last| {
                    let gap = item.along - last.along - last.width;
                    gap >= -0.3 * last.size.min(item.size)
                        && gap <= COLUMN_GUTTER * last.size.max(item.size)
                }).unwrap_or(false)
        }).unwrap_or(false);
        if adjacent {
            if let Some(run) = runs.last_mut() {
                run.items.push(item);
            }
        } else {
            runs.push(Run {
                angle: item.angle,
                across: item.across,
                size: item.size,
                items: alloc::vec![item],
            });
        }
    }
    runs.sort_unstable_by(|left, right| {
        left.angle.cmp(&right.angle)
            .then_with(|| left.across.total_cmp(&right.across))
            .then_with(|| {
                let a = left.items.first().map(|item| item.seq).unwrap_or(0);
                let b = right.items.first().map(|item| item.seq).unwrap_or(0);
                a.cmp(&b)
            })
    });
    let mut out = Vec::new();
    let mut band = Vec::new();
    let mut anchor: Option<(i16, f32, f32)> = None;
    for run in runs {
        let same = anchor.map(|(angle, across, size)| {
            angle == run.angle
                && magnitude(across - run.across)
                    < BASELINE_TOLERANCE * size.min(run.size)
        }).unwrap_or(false);
        if !same {
            finish_band(core::mem::take(&mut band), text, &mut out);
            anchor = Some((run.angle, run.across, run.size));
        }
        band.extend(run.items);
    }
    finish_band(band, text, &mut out);
    out
}

struct CutResult {
    order: Vec<(usize, u8)>,
    columns: u8,
}

enum CutTask {
    Split(Vec<usize>, u8),
    Join(bool),
}

fn gutter(ids: &[usize], lines: &[PendingLine], axis: usize, minimum: f32) -> Option<f32> {
    let mut intervals = Vec::new();
    for id in ids {
        if let Some(line) = lines.get(*id) {
            intervals.push((line.line.bbox[axis], line.line.bbox[axis + 2]));
        }
    }
    intervals.sort_unstable_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.total_cmp(&b.1)));
    let (_, mut end) = intervals.first().copied()?;
    let mut best = minimum;
    let mut cut = None;
    for (start, next_end) in intervals.into_iter().skip(1) {
        let gap = start - end;
        if gap.is_finite() && gap >= best && gap > 0.0 {
            best = gap;
            cut = Some(end + gap / 2.0);
        }
        end = end.max(next_end);
    }
    cut
}

/// A vertical gutter only makes columns when each side holds at least two lines:
/// one offset line (a repeated form, a margin note) is not a column, and treating
/// it as one would pull it out of its place in the flow.
fn is_column_cut(ids: &[usize], lines: &[PendingLine], cut: f32) -> bool {
    let (mut left, mut right) = (0u32, 0u32);
    for line in ids.iter().filter_map(|id| lines.get(*id)) {
        if line.line.bbox[0] < cut {
            left += 1;
        } else {
            right += 1;
        }
    }
    left >= 2 && right >= 2
}

fn xy_order(ids: Vec<usize>, lines: &[PendingLine], body_size: f32) -> CutResult {
    let mut tasks = alloc::vec![CutTask::Split(ids, 0)];
    let mut results: Vec<CutResult> = Vec::new();
    while let Some(task) = tasks.pop() {
        match task {
            CutTask::Split(mut ids, depth) => {
                let cut = if depth < MAX_CUT_DEPTH && ids.len() > 1 {
                    gutter(&ids, lines, 0, COLUMN_GUTTER * body_size)
                        .filter(|cut| is_column_cut(&ids, lines, *cut))
                        .map(|cut| (0usize, cut))
                        .or_else(|| gutter(&ids, lines, 1, 0.5 * body_size)
                            .map(|cut| (1usize, cut)))
                } else {
                    None
                };
                if let Some((axis, cut)) = cut {
                    let mut left = Vec::new();
                    let mut right = Vec::new();
                    for id in ids {
                        if let Some(line) = lines.get(id) {
                            if line.line.bbox[axis] < cut {
                                left.push(id);
                            } else {
                                right.push(id);
                            }
                        }
                    }
                    tasks.push(CutTask::Join(axis == 0));
                    tasks.push(CutTask::Split(right, depth.saturating_add(1)));
                    tasks.push(CutTask::Split(left, depth.saturating_add(1)));
                } else {
                    ids.sort_unstable_by(|a, b| {
                        match (lines.get(*a), lines.get(*b)) {
                            (Some(a_line), Some(b_line)) =>
                                a_line.line.bbox[1].total_cmp(&b_line.line.bbox[1])
                                    .then_with(|| a_line.line.bbox[0].total_cmp(&b_line.line.bbox[0]))
                                    .then_with(|| a.cmp(b)),
                            _ => a.cmp(b),
                        }
                    });
                    let columns = if ids.is_empty() { 0 } else { 1 };
                    results.push(CutResult {
                        order: ids.into_iter().map(|id| (id, 0)).collect(),
                        columns,
                    });
                }
            }
            CutTask::Join(vertical) => {
                let (Some(mut right), Some(mut left)) = (results.pop(), results.pop()) else {
                    continue;
                };
                if vertical {
                    for (_, column) in &mut right.order {
                        *column = column.saturating_add(left.columns).min(254);
                    }
                    left.columns = left.columns.saturating_add(right.columns);
                } else {
                    left.columns = left.columns.max(right.columns);
                }
                left.order.extend(right.order);
                results.push(left);
            }
        }
    }
    results.pop().unwrap_or(CutResult { order: Vec::new(), columns: 0 })
}

fn line_kind(text: &str, size: f32, body_size: f32) -> BlockKind {
    let token = text.split_whitespace().next().unwrap_or("");
    let numbered = token.strip_suffix('.').or_else(|| token.strip_suffix(')'))
        .map(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
        .unwrap_or(false);
    if matches!(token, "\u{2022}" | "\u{25cf}" | "\u{25e6}" | "\u{25aa}" | "-" | "*")
        || numbered
    {
        BlockKind::ListItem
    } else if size < 0.9 * body_size
        && (text.starts_with("Figure ") || text.starts_with("Fig. ") || text.starts_with("Table "))
    {
        BlockKind::Caption
    } else {
        BlockKind::Paragraph
    }
}

struct DraftBlock {
    block: Block,
    size: f32,
    angle: i16,
    bold: bool,
    words: usize,
}

fn build_blocks(page: &mut PageLayout, metadata: &[(i16, bool, u8)]) {
    let mut drafts: Vec<DraftBlock> = Vec::new();
    for (index, line) in page.lines.iter().enumerate() {
        let (angle, bold, column) = metadata.get(index).copied().unwrap_or((0, false, 0));
        let kind = line_kind(&line.text, line.size, page.body_size);
        let words = line.text.split_whitespace().count();
        let merge = drafts.last().map(|last| {
            angle == 0 && last.angle == 0 && last.bold == bold
                && last.block.column == column && last.block.kind == kind
                && kind != BlockKind::ListItem
                && magnitude(last.size - line.size)
                    <= SIZE_CHANGE * last.size.min(line.size)
                && line.bbox[1] - last.block.bbox[3]
                    <= 1.2 * line.size.max(last.size)
        }).unwrap_or(false);
        if merge {
            if let Some(last) = drafts.last_mut() {
                last.block.bbox = union(last.block.bbox, line.bbox);
                last.block.lines.end = count(index.saturating_add(1));
                last.size = last.size.max(line.size);
                last.words = last.words.saturating_add(words);
            }
        } else {
            drafts.push(DraftBlock {
                block: Block {
                    kind,
                    bbox: line.bbox,
                    lines: count(index)..count(index.saturating_add(1)),
                    column,
                },
                size: line.size,
                angle,
                bold,
                words,
            });
        }
    }
    let headings: Vec<bool> = drafts.iter().enumerate().map(|(index, draft)| {
        let followed_by_body = drafts.get(index.saturating_add(1)).map(|next| {
            next.angle == 0 && !next.bold && next.size <= 1.15 * page.body_size
        }).unwrap_or(false);
        draft.angle == 0 && draft.block.kind == BlockKind::Paragraph
            && (draft.size >= 1.25 * page.body_size
                || (draft.bold && draft.words > 0 && draft.words < 12 && followed_by_body))
    }).collect();
    let mut sizes: Vec<f32> = drafts.iter().zip(&headings)
        .filter_map(|(draft, heading)| if *heading { Some(draft.size) } else { None })
        .collect();
    sizes.sort_unstable_by(|a, b| b.total_cmp(a));
    let mut ranks: Vec<f32> = Vec::new();
    for size in sizes {
        if ranks.len() < 3 && ranks.last().map(|last| {
            magnitude(size - *last) > SIZE_CHANGE * *last
        }).unwrap_or(true) {
            ranks.push(size);
        }
    }
    for (draft, heading) in drafts.iter_mut().zip(headings) {
        if heading {
            let mut level = 3u8;
            for (rank, size) in ranks.iter().enumerate() {
                if draft.size >= (1.0 - SIZE_CHANGE) * *size {
                    level = (rank as u8).saturating_add(1).min(3);
                    break;
                }
            }
            draft.block.kind = BlockKind::Heading(level);
        }
    }
    page.blocks = drafts.into_iter().map(|draft| draft.block).collect();
}

pub fn layout(text: &PageText) -> PageLayout {
    let mut items = Vec::new();
    for (index, glyph) in text.glyphs.iter().take(u32::MAX as usize).enumerate() {
        if let Ok(id) = u32::try_from(index) {
            if let Some(item) = positioned(id, glyph, text) {
                items.push(item);
            }
        }
    }
    items.sort_unstable_by_key(|item| (item.seq, item.id));
    let items = unique_items(items, text);
    let body_size = median(items.iter().map(|item| item.size).collect());
    let mut pending = candidate_lines(items, text);
    let mut main = Vec::new();
    let mut rotated = Vec::new();
    for (index, line) in pending.iter().enumerate() {
        if line.angle == 0 {
            main.push(index);
        } else {
            rotated.push(index);
        }
    }
    let mut flow = xy_order(main, &pending, body_size.max(0.01));
    // Order rotated lines in their own text frame: "down" is perpendicular to the
    // baseline, so a page turned 90 degrees reads its lines right to left in
    // display space, exactly as it reads top to bottom before the turn.
    let frame_key = |line: &PendingLine| {
        let (dx, dy) = direction(line.angle);
        let [x0, y0, x1, y1] = line.line.bbox;
        let (cx, cy) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
        (cx * -dy + cy * dx, cx * dx + cy * dy)
    };
    rotated.sort_unstable_by(|a, b| {
        match (pending.get(*a), pending.get(*b)) {
            (Some(left), Some(right)) => {
                let (left_across, left_along) = frame_key(left);
                let (right_across, right_along) = frame_key(right);
                left.angle.cmp(&right.angle)
                    .then_with(|| left_across.total_cmp(&right_across))
                    .then_with(|| left_along.total_cmp(&right_along))
                    .then_with(|| a.cmp(b))
            }
            _ => a.cmp(b),
        }
    });
    if !rotated.is_empty() {
        flow.columns = flow.columns.max(1);
    }
    flow.order.extend(rotated.into_iter().map(|id| (id, 0)));
    let mut page = PageLayout {
        lines: Vec::new(),
        blocks: Vec::new(),
        order: Vec::new(),
        columns: flow.columns,
        body_size,
    };
    let mut metadata = Vec::new();
    for (id, column) in flow.order {
        let Some(source) = pending.get_mut(id) else { continue };
        let start = count(page.order.len());
        page.order.extend(core::mem::take(&mut source.ids));
        let end = count(page.order.len());
        page.lines.push(Line {
            bbox: source.line.bbox,
            text: core::mem::take(&mut source.line.text),
            size: source.line.size,
            glyphs: start..end,
        });
        metadata.push((source.angle, source.bold, column));
    }
    build_blocks(&mut page, &metadata);
    page
}
