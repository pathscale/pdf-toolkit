//! Independent lossless JPEG 2000 Part 1 fixture encoder.
//!
//! One layer, LRCP, one precinct per resolution, unsigned components, reversible
//! 5/3 lifting, optional RCT, and ordinary MQ code-block coding. No decoder is
//! used. Tile origins must align with the selected decomposition grid.

use std::io;

const MAX_SIDE: usize = 4096;
const MAX_SAMPLES: usize = 4_194_304;
const MAX_TILES: usize = 4096;
const MAX_BYTES: usize = 67_108_864;
const GUARD_BITS: u32 = 4;

/// Encoding parameters. Zero tile dimensions select the entire image.
#[derive(Clone, Copy, Debug)]
pub struct Settings {
    pub bit_depth: u8,
    pub levels: u8,
    pub tile_width: u32,
    pub tile_height: u32,
    pub code_block_size: u8,
    pub rct: bool,
    pub jp2: bool,
    pub alpha: bool,
    /// With JP2 opacity, the caller supplies A, R, G, B instead of R, G, B, A.
    pub alpha_first: bool,
}

/// Tile-local coding, in row-major tile order.
#[derive(Clone, Copy, Debug)]
pub struct TileCoding {
    pub levels: u8,
    pub rct: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            bit_depth: 8,
            levels: 0,
            tile_width: 0,
            tile_height: 0,
            code_block_size: 32,
            rct: false,
            jp2: false,
            alpha: false,
            alpha_first: false,
        }
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.to_owned())
}

fn limit(kind: &str, requested: usize, maximum: usize) -> io::Result<()> {
    if requested > maximum {
        return Err(invalid(&format!(
            "refused:{kind}:requested={requested}:maximum={maximum}"
        )));
    }
    Ok(())
}

fn at<T: Copy>(values: &[T], index: usize) -> io::Result<T> {
    values.get(index).copied()
        .ok_or_else(|| invalid("JPEG 2000 encoder internal bounds"))
}

fn put<T>(values: &mut [T], index: usize, value: T) -> io::Result<()> {
    let slot = values.get_mut(index)
        .ok_or_else(|| invalid("JPEG 2000 encoder internal bounds"))?;
    *slot = value;
    Ok(())
}

fn product(a: usize, b: usize) -> io::Result<usize> {
    a.checked_mul(b)
        .ok_or_else(|| invalid("JPEG 2000 encoder size overflow"))
}

fn sum(a: usize, b: usize) -> io::Result<usize> {
    a.checked_add(b)
        .ok_or_else(|| invalid("JPEG 2000 encoder size overflow"))
}

fn half(value: usize) -> usize {
    value.saturating_add(1) / 2
}

fn narrow(value: i64) -> io::Result<i32> {
    i32::try_from(value)
        .map_err(|_| invalid("JPEG 2000 coefficient overflow"))
}

fn u16be(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn u32be(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn marker(out: &mut Vec<u8>, code: u8, payload: &[u8]) -> io::Result<()> {
    let length = u16::try_from(sum(payload.len(), 2)?)
        .map_err(|_| invalid("JPEG 2000 marker too long"))?;
    out.extend_from_slice(&[0xff, code]);
    u16be(out, length);
    out.extend_from_slice(payload);
    Ok(())
}

fn box_bytes(out: &mut Vec<u8>, tag: &[u8; 4], payload: &[u8]) -> io::Result<()> {
    let length = u32::try_from(sum(payload.len(), 8)?)
        .map_err(|_| invalid("JP2 box too long"))?;
    limit("jpx_encoder_bytes", sum(out.len(), length as usize)?, MAX_BYTES)?;
    u32be(out, length);
    out.extend_from_slice(tag);
    out.extend_from_slice(payload);
    Ok(())
}

struct Plane {
    width: usize,
    height: usize,
    data: Vec<i32>,
}

impl Plane {
    fn index(&self, x: usize, y: usize) -> io::Result<usize> {
        if x >= self.width || y >= self.height {
            return Err(invalid("JPEG 2000 plane coordinates"));
        }
        sum(product(y, self.width)?, x)
    }

    fn sample(&self, x: usize, y: usize) -> io::Result<i32> {
        at(&self.data, self.index(x, y)?)
    }

    fn store(&mut self, x: usize, y: usize, value: i32) -> io::Result<()> {
        let index = self.index(x, y)?;
        put(&mut self.data, index, value)
    }
}

/// Forward lifting with whole-sample symmetric extension and an even origin.
fn lift(line: &[i32]) -> io::Result<Vec<i32>> {
    let length = line.len();
    if length <= 1 {
        return Ok(line.to_vec());
    }
    let mut work = line.to_vec();
    for index in (1..length).step_by(2) {
        let left = at(&work, index.saturating_sub(1))?;
        let right_index = if index.saturating_add(1) < length {
            index.saturating_add(1)
        } else {
            index.saturating_sub(1)
        };
        let right = at(&work, right_index)?;
        let prediction = (i64::from(left) + i64::from(right)) >> 1;
        let value = narrow(i64::from(at(&work, index)?) - prediction)?;
        put(&mut work, index, value)?;
    }
    for index in (0..length).step_by(2) {
        let left_index = if index == 0 { 1 } else { index.saturating_sub(1) };
        let right_index = if index.saturating_add(1) < length {
            index.saturating_add(1)
        } else {
            index.saturating_sub(1)
        };
        let left = at(&work, left_index)?;
        let right = at(&work, right_index)?;
        let update = (i64::from(left) + i64::from(right) + 2) >> 2;
        let value = narrow(i64::from(at(&work, index)?) + update)?;
        put(&mut work, index, value)?;
    }
    let mut out = Vec::with_capacity(length);
    for index in (0..length).step_by(2) {
        out.push(at(&work, index)?);
    }
    for index in (1..length).step_by(2) {
        out.push(at(&work, index)?);
    }
    Ok(out)
}

fn wavelet(plane: &mut Plane, levels: u8) -> io::Result<Vec<(usize, usize)>> {
    let mut width = plane.width;
    let mut height = plane.height;
    let mut sizes = vec![(width, height)];
    for _ in 0..levels {
        // Part 1 analysis applies vertical lifting before horizontal lifting.
        for x in 0..width {
            let mut line = Vec::with_capacity(height);
            for y in 0..height {
                line.push(plane.sample(x, y)?);
            }
            for (y, value) in lift(&line)?.into_iter().enumerate() {
                plane.store(x, y, value)?;
            }
        }
        for y in 0..height {
            let mut line = Vec::with_capacity(width);
            for x in 0..width {
                line.push(plane.sample(x, y)?);
            }
            for (x, value) in lift(&line)?.into_iter().enumerate() {
                plane.store(x, y, value)?;
            }
        }
        width = half(width);
        height = half(height);
        sizes.push((width, height));
    }
    Ok(sizes)
}

// JPEG 2000 Part 1 MQ probability states: Qe, NMPS, NLPS, switch MPS.
const MQ_STATES: [(u32, usize, usize, bool); 47] = [
    (0x5601, 1, 1, true),
    (0x3401, 2, 6, false),
    (0x1801, 3, 9, false),
    (0x0ac1, 4, 12, false),
    (0x0521, 5, 29, false),
    (0x0221, 38, 33, false),
    (0x5601, 7, 6, true),
    (0x5401, 8, 14, false),
    (0x4801, 9, 14, false),
    (0x3801, 10, 14, false),
    (0x3001, 11, 17, false),
    (0x2401, 12, 18, false),
    (0x1c01, 13, 20, false),
    (0x1601, 29, 21, false),
    (0x5601, 15, 14, true),
    (0x5401, 16, 14, false),
    (0x5101, 17, 15, false),
    (0x4801, 18, 16, false),
    (0x3801, 19, 17, false),
    (0x3401, 20, 18, false),
    (0x3001, 21, 19, false),
    (0x2801, 22, 19, false),
    (0x2401, 23, 20, false),
    (0x2201, 24, 21, false),
    (0x1c01, 25, 22, false),
    (0x1801, 26, 23, false),
    (0x1601, 27, 24, false),
    (0x1401, 28, 25, false),
    (0x1201, 29, 26, false),
    (0x1101, 30, 27, false),
    (0x0ac1, 31, 28, false),
    (0x09c1, 32, 29, false),
    (0x08a1, 33, 30, false),
    (0x0521, 34, 31, false),
    (0x0441, 35, 32, false),
    (0x02a1, 36, 33, false),
    (0x0221, 37, 34, false),
    (0x0141, 38, 35, false),
    (0x0111, 39, 36, false),
    (0x0085, 40, 37, false),
    (0x0049, 41, 38, false),
    (0x0025, 42, 39, false),
    (0x0015, 43, 40, false),
    (0x0009, 44, 41, false),
    (0x0005, 45, 42, false),
    (0x0001, 45, 43, false),
    (0x5601, 46, 46, false),
];

struct Mq {
    a: u32,
    c: u32,
    ct: u32,
    bytes: Vec<u8>,
    contexts: [(usize, bool); 19],
}

impl Mq {
    fn new() -> Self {
        let mut contexts = [(0, false); 19];
        contexts[0] = (4, false);
        contexts[17] = (3, false);
        contexts[18] = (46, false);
        Self {
            a: 0x8000,
            c: 0,
            ct: 12,
            // The first byte is a carry sentinel, never part of the stream.
            bytes: vec![0],
            contexts,
        }
    }

    fn byte_out(&mut self) -> io::Result<()> {
        let previous = self.bytes.last_mut()
            .ok_or_else(|| invalid("MQ missing carry sentinel"))?;
        if *previous == 0xff {
            self.bytes.push((self.c >> 20) as u8);
            self.c &= 0x000f_ffff;
            self.ct = 7;
        } else if self.c & 0x0800_0000 == 0 {
            self.bytes.push((self.c >> 19) as u8);
            self.c &= 0x0007_ffff;
            self.ct = 8;
        } else {
            *previous = previous.wrapping_add(1);
            if *previous == 0xff {
                self.c &= 0x07ff_ffff;
                self.bytes.push((self.c >> 20) as u8);
                self.c &= 0x000f_ffff;
                self.ct = 7;
            } else {
                self.bytes.push((self.c >> 19) as u8);
                self.c &= 0x0007_ffff;
                self.ct = 8;
            }
        }
        Ok(())
    }

    fn renormalize(&mut self) -> io::Result<()> {
        while self.a < 0x8000 {
            self.a <<= 1;
            self.c = self.c.wrapping_shl(1);
            self.ct = self.ct.checked_sub(1)
                .ok_or_else(|| invalid("MQ bit counter underflow"))?;
            if self.ct == 0 {
                self.byte_out()?;
            }
        }
        Ok(())
    }

    fn bit(&mut self, context: usize, value: bool) -> io::Result<()> {
        let (state, mps) = at(&self.contexts, context)?;
        let (qe, nmps, nlps, switch) = at(&MQ_STATES, state)?;
        self.a = self.a.checked_sub(qe)
            .ok_or_else(|| invalid("MQ interval underflow"))?;
        if value == mps {
            if self.a < 0x8000 {
                if self.a < qe {
                    self.a = qe;
                } else {
                    self.c = self.c.wrapping_add(qe);
                }
                put(&mut self.contexts, context, (nmps, mps))?;
                self.renormalize()?;
            } else {
                self.c = self.c.wrapping_add(qe);
            }
        } else {
            if self.a < qe {
                self.c = self.c.wrapping_add(qe);
            } else {
                self.a = qe;
            }
            put(&mut self.contexts, context, (nlps, mps ^ switch))?;
            self.renormalize()?;
        }
        Ok(())
    }

    fn finish(mut self) -> io::Result<Vec<u8>> {
        let upper = self.c.wrapping_add(self.a);
        self.c |= 0xffff;
        if self.c >= upper {
            self.c = self.c.wrapping_sub(0x8000);
        }
        self.c = self.c.wrapping_shl(self.ct);
        self.byte_out()?;
        self.c = self.c.wrapping_shl(self.ct);
        self.byte_out()?;
        if self.bytes.last() == Some(&0xff) {
            self.bytes.pop();
        }
        Ok(self.bytes.into_iter().skip(1).collect())
    }
}

#[derive(Clone, Copy)]
struct Sample {
    value: i32,
    significant: bool,
    visited: bool,
    refined: bool,
}

struct Tier1 {
    width: usize,
    height: usize,
    orientation: u8,
    samples: Vec<Sample>,
    mq: Mq,
}

impl Tier1 {
    fn index(&self, x: usize, y: usize) -> io::Result<usize> {
        if x >= self.width || y >= self.height {
            return Err(invalid("JPEG 2000 code-block coordinates"));
        }
        sum(product(y, self.width)?, x)
    }

    fn neighbor(&self, x: isize, y: isize) -> Option<Sample> {
        if x < 0 || y < 0 {
            return None;
        }
        let x = usize::try_from(x).ok()?;
        let y = usize::try_from(y).ok()?;
        if x >= self.width || y >= self.height {
            return None;
        }
        let index = y.checked_mul(self.width)?.checked_add(x)?;
        self.samples.get(index).copied()
    }

    fn significant(&self, x: isize, y: isize) -> u8 {
        u8::from(self.neighbor(x, y).map(|s| s.significant).unwrap_or(false))
    }

    fn signed_neighbor(&self, x: isize, y: isize) -> i32 {
        match self.neighbor(x, y) {
            Some(sample) if sample.significant => {
                if sample.value < 0 { -1 } else { 1 }
            }
            _ => 0,
        }
    }

    fn counts(&self, x: usize, y: usize) -> (u8, u8, u8) {
        // Coordinates here are bounded by a 64x64 constructed code block.
        let x = x as isize;
        let y = y as isize;
        let h = self.significant(x - 1, y) + self.significant(x + 1, y);
        let v = self.significant(x, y - 1) + self.significant(x, y + 1);
        let d = self.significant(x - 1, y - 1)
            + self.significant(x + 1, y - 1)
            + self.significant(x - 1, y + 1)
            + self.significant(x + 1, y + 1);
        (h, v, d)
    }

    fn has_neighbors(&self, x: usize, y: usize) -> bool {
        let (h, v, d) = self.counts(x, y);
        h != 0 || v != 0 || d != 0
    }

    fn zero_context(&self, x: usize, y: usize) -> usize {
        let (mut h, mut v, d) = self.counts(x, y);
        // Table D.1 lists LL and LH; the HL band (horizontally high) swaps h and v.
        if self.orientation == 1 {
            std::mem::swap(&mut h, &mut v);
        }
        if self.orientation == 3 {
            let hv = h + v;
            match d {
                0 => if hv == 0 { 0 } else if hv == 1 { 1 } else { 2 },
                1 => if hv == 0 { 3 } else if hv == 1 { 4 } else { 5 },
                2 => if hv == 0 { 6 } else { 7 },
                _ => 8,
            }
        } else {
            match h {
                0 => match v {
                    0 => if d == 0 { 0 } else if d == 1 { 1 } else { 2 },
                    1 => 3,
                    _ => 4,
                },
                1 => if v == 0 {
                    if d == 0 { 5 } else { 6 }
                } else {
                    7
                },
                _ => 8,
            }
        }
    }

    fn sign(&mut self, x: usize, y: usize) -> io::Result<()> {
        let index = self.index(x, y)?;
        let value = at(&self.samples, index)?.value;
        let x = x as isize;
        let y = y as isize;
        let mut h = (self.signed_neighbor(x - 1, y)
            + self.signed_neighbor(x + 1, y)).clamp(-1, 1);
        let mut v = (self.signed_neighbor(x, y - 1)
            + self.signed_neighbor(x, y + 1)).clamp(-1, 1);
        let prediction = h < 0 || (h == 0 && v < 0);
        if h < 0 {
            h = -h;
            v = -v;
        } else if h == 0 {
            v = v.abs();
        }
        let context = if h == 0 {
            if v == 0 { 9 } else { 10 }
        } else {
            match v {
                -1 => 11,
                0 => 12,
                _ => 13,
            }
        };
        self.mq.bit(context, (value < 0) ^ prediction)?;
        let mut sample = at(&self.samples, index)?;
        sample.significant = true;
        put(&mut self.samples, index, sample)
    }

    fn coefficient_bit(&self, index: usize, plane: u32) -> io::Result<bool> {
        if plane >= 32 {
            return Err(invalid("JPEG 2000 bit plane outside coefficient"));
        }
        Ok((at(&self.samples, index)?.value.unsigned_abs() >> plane) & 1 != 0)
    }

    fn significance_pass(&mut self, plane: u32) -> io::Result<()> {
        for stripe in (0..self.height).step_by(4) {
            let end = stripe.saturating_add(4).min(self.height);
            for x in 0..self.width {
                for y in stripe..end {
                    let index = self.index(x, y)?;
                    let mut sample = at(&self.samples, index)?;
                    if sample.significant || sample.visited || !self.has_neighbors(x, y) {
                        continue;
                    }
                    let bit = self.coefficient_bit(index, plane)?;
                    let context = self.zero_context(x, y);
                    self.mq.bit(context, bit)?;
                    sample.visited = true;
                    put(&mut self.samples, index, sample)?;
                    if bit {
                        self.sign(x, y)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn refinement_pass(&mut self, plane: u32) -> io::Result<()> {
        for stripe in (0..self.height).step_by(4) {
            let end = stripe.saturating_add(4).min(self.height);
            for x in 0..self.width {
                for y in stripe..end {
                    let index = self.index(x, y)?;
                    let mut sample = at(&self.samples, index)?;
                    if !sample.significant || sample.visited {
                        continue;
                    }
                    let context = if sample.refined {
                        16
                    } else if self.has_neighbors(x, y) {
                        15
                    } else {
                        14
                    };
                    let bit = self.coefficient_bit(index, plane)?;
                    self.mq.bit(context, bit)?;
                    sample.refined = true;
                    put(&mut self.samples, index, sample)?;
                }
            }
        }
        Ok(())
    }

    fn cleanup_pass(&mut self, plane: u32) -> io::Result<()> {
        for stripe in (0..self.height).step_by(4) {
            let end = stripe.saturating_add(4).min(self.height);
            for x in 0..self.width {
                let mut aggregate = end.saturating_sub(stripe) == 4;
                for y in stripe..end {
                    let sample = at(&self.samples, self.index(x, y)?)?;
                    aggregate &= !sample.significant
                        && !sample.visited
                        && !self.has_neighbors(x, y);
                }
                let mut start = stripe;
                if aggregate {
                    let mut first = 4usize;
                    for offset in 0..4 {
                        let y = sum(stripe, offset)?;
                        if first == 4 && self.coefficient_bit(self.index(x, y)?, plane)? {
                            first = offset;
                        }
                    }
                    self.mq.bit(17, first != 4)?;
                    if first == 4 {
                        continue;
                    }
                    self.mq.bit(18, first & 2 != 0)?;
                    self.mq.bit(18, first & 1 != 0)?;
                    let y = sum(stripe, first)?;
                    self.sign(x, y)?;
                    start = sum(y, 1)?;
                }
                for y in start..end {
                    let index = self.index(x, y)?;
                    let sample = at(&self.samples, index)?;
                    if sample.significant || sample.visited {
                        continue;
                    }
                    let bit = self.coefficient_bit(index, plane)?;
                    let context = self.zero_context(x, y);
                    self.mq.bit(context, bit)?;
                    if bit {
                        self.sign(x, y)?;
                    }
                }
            }
        }
        for sample in &mut self.samples {
            sample.visited = false;
        }
        Ok(())
    }
}

struct CodeBlock {
    bytes: Vec<u8>,
    passes: u32,
    zero_planes: u32,
}

fn code_block(
    values: Vec<i32>,
    width: usize,
    height: usize,
    orientation: u8,
    nominal_planes: u32,
) -> io::Result<CodeBlock> {
    if values.len() != product(width, height)? {
        return Err(invalid("JPEG 2000 code-block sample count"));
    }
    let maximum = values.iter().map(|v| v.unsigned_abs()).max().unwrap_or(0);
    if maximum == 0 {
        return Ok(CodeBlock {
            bytes: Vec::new(),
            passes: 0,
            zero_planes: u32::MAX,
        });
    }
    let planes = 32u32.saturating_sub(maximum.leading_zeros());
    let zero_planes = nominal_planes.checked_sub(planes)
        .ok_or_else(|| invalid("JPEG 2000 insufficient guard bits"))?;
    let passes = planes.checked_mul(3).and_then(|v| v.checked_sub(2))
        .ok_or_else(|| invalid("JPEG 2000 pass count overflow"))?;
    let mut coder = Tier1 {
        width,
        height,
        orientation,
        samples: values.into_iter().map(|value| Sample {
            value,
            significant: false,
            visited: false,
            refined: false,
        }).collect(),
        mq: Mq::new(),
    };
    let highest = planes.saturating_sub(1);
    coder.cleanup_pass(highest)?;
    for plane in (0..highest).rev() {
        coder.significance_pass(plane)?;
        coder.refinement_pass(plane)?;
        coder.cleanup_pass(plane)?;
    }
    let bytes = coder.mq.finish()?;
    if bytes.is_empty() {
        return Err(invalid("JPEG 2000 empty MQ segment"));
    }
    Ok(CodeBlock { bytes, passes, zero_planes })
}

struct Band {
    columns: usize,
    rows: usize,
    blocks: Vec<CodeBlock>,
}

#[derive(Clone, Copy)]
struct Region {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    origin_x: usize,
    origin_y: usize,
    orientation: u8,
}

fn band(
    plane: &Plane,
    region: Region,
    bit_depth: u8,
    block_size: usize,
) -> io::Result<Band> {
    if region.width == 0 || region.height == 0 {
        return Ok(Band { columns: 0, rows: 0, blocks: Vec::new() });
    }
    let end_x = sum(region.origin_x, region.width)?;
    let end_y = sum(region.origin_y, region.height)?;
    let columns = (end_x.saturating_sub(1) / block_size)
        .saturating_sub(region.origin_x / block_size).saturating_add(1);
    let rows = (end_y.saturating_sub(1) / block_size)
        .saturating_sub(region.origin_y / block_size).saturating_add(1);
    let gain = match region.orientation {
        0 => 0,
        3 => 2,
        _ => 1,
    };
    let nominal_planes = u32::from(bit_depth)
        .saturating_add(gain).saturating_add(GUARD_BITS).saturating_sub(1);
    let mut blocks = Vec::with_capacity(product(columns, rows)?);
    let mut y = region.origin_y;
    while y < end_y {
        let next_y = product(sum(y / block_size, 1)?, block_size)?.min(end_y);
        let mut x = region.origin_x;
        while x < end_x {
            let next_x = product(sum(x / block_size, 1)?, block_size)?.min(end_x);
            let width = next_x.saturating_sub(x);
            let height = next_y.saturating_sub(y);
            let mut values = Vec::with_capacity(product(width, height)?);
            for sample_y in y..next_y {
                for sample_x in x..next_x {
                    values.push(plane.sample(
                        sum(region.x, sample_x.saturating_sub(region.origin_x))?,
                        sum(region.y, sample_y.saturating_sub(region.origin_y))?,
                    )?);
                }
            }
            blocks.push(code_block(
                values, width, height, region.orientation, nominal_planes,
            )?);
            x = next_x;
        }
        y = next_y;
    }
    Ok(Band { columns, rows, blocks })
}

fn component_bands(
    mut plane: Plane,
    origin_x: usize,
    origin_y: usize,
    settings: Settings,
) -> io::Result<Vec<Vec<Band>>> {
    let sizes = wavelet(&mut plane, settings.levels)?;
    let mut resolutions = Vec::new();
    for resolution in 0..=usize::from(settings.levels) {
        let depth = if resolution == 0 {
            usize::from(settings.levels)
        } else {
            usize::from(settings.levels).saturating_sub(resolution).saturating_add(1)
        };
        let (low_width, low_height) = at(&sizes, depth)?;
        let global_x = origin_x >> depth;
        let global_y = origin_y >> depth;
        let mut regions = Vec::new();
        if resolution == 0 {
            regions.push(Region {
                x: 0,
                y: 0,
                width: low_width,
                height: low_height,
                origin_x: global_x,
                origin_y: global_y,
                orientation: 0,
            });
        } else {
            let (width, height) = at(&sizes, depth.saturating_sub(1))?;
            let high_width = width.saturating_sub(low_width);
            let high_height = height.saturating_sub(low_height);
            regions.extend_from_slice(&[
                Region {
                    x: low_width,
                    y: 0,
                    width: high_width,
                    height: low_height,
                    origin_x: global_x,
                    origin_y: global_y,
                    orientation: 1,
                },
                Region {
                    x: 0,
                    y: low_height,
                    width: low_width,
                    height: high_height,
                    origin_x: global_x,
                    origin_y: global_y,
                    orientation: 2,
                },
                Region {
                    x: low_width,
                    y: low_height,
                    width: high_width,
                    height: high_height,
                    origin_x: global_x,
                    origin_y: global_y,
                    orientation: 3,
                },
            ]);
        }
        let mut bands = Vec::new();
        for region in regions {
            bands.push(band(
                &plane, region, settings.bit_depth,
                usize::from(settings.code_block_size),
            )?);
        }
        resolutions.push(bands);
    }
    Ok(resolutions)
}

struct HeaderBits {
    bytes: Vec<u8>,
    current: u8,
    used: u8,
    capacity: u8,
}

impl HeaderBits {
    fn new() -> Self {
        Self { bytes: Vec::new(), current: 0, used: 0, capacity: 8 }
    }

    fn bit(&mut self, value: bool) {
        let shift = self.capacity.saturating_sub(self.used).saturating_sub(1);
        self.current |= u8::from(value) << shift;
        self.used = self.used.saturating_add(1);
        if self.used == self.capacity {
            self.bytes.push(self.current);
            self.capacity = if self.current == 0xff { 7 } else { 8 };
            self.current = 0;
            self.used = 0;
        }
    }

    fn integer(&mut self, value: u32, count: u32) -> io::Result<()> {
        if count > 32 {
            return Err(invalid("JPEG 2000 packet integer too wide"));
        }
        for shift in (0..count).rev() {
            self.bit((value >> shift) & 1 != 0);
        }
        Ok(())
    }

    fn finish(mut self) -> Vec<u8> {
        if self.used != 0 || self.capacity == 7 {
            // This also emits the stuffed zero after a final full 0xff byte.
            self.bytes.push(self.current);
        }
        self.bytes
    }
}

#[derive(Clone, Copy)]
struct Node {
    value: u32,
    low: u32,
    known: bool,
}

struct TreeLevel {
    width: usize,
    nodes: Vec<Node>,
}

struct TagTree {
    levels: Vec<TreeLevel>,
}

impl TagTree {
    fn new(width: usize, height: usize, values: Vec<u32>) -> io::Result<Self> {
        if values.len() != product(width, height)? {
            return Err(invalid("JPEG 2000 tag-tree leaf count"));
        }
        if width == 0 || height == 0 {
            return Ok(Self { levels: Vec::new() });
        }
        let nodes = values.into_iter().map(|value| Node {
            value, low: 0, known: false,
        }).collect();
        let mut levels = vec![TreeLevel { width, nodes }];
        let mut width = width;
        let mut height = height;
        while width > 1 || height > 1 {
            let parent_width = half(width);
            let parent_height = half(height);
            let mut parent = vec![
                Node { value: u32::MAX, low: 0, known: false };
                product(parent_width, parent_height)?
            ];
            let previous = levels.last()
                .ok_or_else(|| invalid("JPEG 2000 missing tag-tree level"))?;
            for y in 0..height {
                for x in 0..width {
                    let child = at(&previous.nodes, sum(product(y, width)?, x)?)?;
                    let index = sum(product(y / 2, parent_width)?, x / 2)?;
                    let mut node = at(&parent, index)?;
                    node.value = node.value.min(child.value);
                    put(&mut parent, index, node)?;
                }
            }
            levels.push(TreeLevel { width: parent_width, nodes: parent });
            width = parent_width;
            height = parent_height;
        }
        Ok(Self { levels })
    }

    fn encode(
        &mut self,
        leaf: usize,
        threshold: u32,
        bits: &mut HeaderBits,
    ) -> io::Result<()> {
        let leaves = self.levels.first()
            .ok_or_else(|| invalid("JPEG 2000 empty tag tree"))?;
        if leaf >= leaves.nodes.len() {
            return Err(invalid("JPEG 2000 tag-tree leaf outside tree"));
        }
        let mut x = leaf % leaves.width;
        let mut y = leaf / leaves.width;
        let mut path = Vec::with_capacity(self.levels.len());
        for level in &self.levels {
            path.push(sum(product(y, level.width)?, x)?);
            x /= 2;
            y /= 2;
        }
        let mut low = 0u32;
        for level_index in (0..self.levels.len()).rev() {
            let index = at(&path, level_index)?;
            let level = self.levels.get_mut(level_index)
                .ok_or_else(|| invalid("JPEG 2000 missing tag-tree level"))?;
            let node = level.nodes.get_mut(index)
                .ok_or_else(|| invalid("JPEG 2000 tag-tree path outside tree"))?;
            low = low.max(node.low);
            while low < threshold {
                if low >= node.value {
                    if !node.known {
                        bits.bit(true);
                        node.known = true;
                    }
                    break;
                }
                bits.bit(false);
                low = low.saturating_add(1);
            }
            node.low = low;
        }
        Ok(())
    }
}

fn pass_count(bits: &mut HeaderBits, passes: u32) -> io::Result<()> {
    match passes {
        1 => bits.bit(false),
        2 => bits.integer(0b10, 2)?,
        3..=5 => {
            bits.integer(0b11, 2)?;
            bits.integer(passes.saturating_sub(3), 2)?;
        }
        6..=36 => {
            bits.integer(0b1111, 4)?;
            bits.integer(passes.saturating_sub(6), 5)?;
        }
        37..=164 => {
            bits.integer(0b1_1111_1111, 9)?;
            bits.integer(passes.saturating_sub(37), 7)?;
        }
        _ => return Err(invalid("unsupported:jpx_encoder_passes")),
    }
    Ok(())
}

fn packet(out: &mut Vec<u8>, bands: &[Band]) -> io::Result<()> {
    let mut bits = HeaderBits::new();
    let present = bands.iter().any(|band| band.blocks.iter().any(|b| b.passes != 0));
    bits.bit(present);
    if present {
        for band in bands {
            if band.blocks.is_empty() {
                continue;
            }
            let mut inclusion = TagTree::new(
                band.columns,
                band.rows,
                band.blocks.iter().map(|b| u32::from(b.passes == 0)).collect(),
            )?;
            let mut zero_planes = TagTree::new(
                band.columns,
                band.rows,
                band.blocks.iter().map(|b| b.zero_planes).collect(),
            )?;
            for (index, block) in band.blocks.iter().enumerate() {
                inclusion.encode(index, 1, &mut bits)?;
                if block.passes == 0 {
                    continue;
                }
                zero_planes.encode(
                    index, block.zero_planes.saturating_add(1), &mut bits,
                )?;
                pass_count(&mut bits, block.passes)?;
                let length = u32::try_from(block.bytes.len())
                    .map_err(|_| invalid("JPEG 2000 MQ segment too long"))?;
                let pass_log = 31u32.saturating_sub(block.passes.leading_zeros());
                let length_bits = 32u32.saturating_sub(length.leading_zeros());
                let initial = 3u32.saturating_add(pass_log);
                let increment = length_bits.saturating_sub(initial);
                for _ in 0..increment {
                    bits.bit(true);
                }
                bits.bit(false);
                bits.integer(length, initial.saturating_add(increment))?;
            }
        }
    }
    let header = bits.finish();
    let mut requested = sum(out.len(), header.len())?;
    if present {
        for band in bands {
            for block in &band.blocks {
                requested = sum(requested, block.bytes.len())?;
            }
        }
    }
    limit("jpx_encoder_bytes", requested, MAX_BYTES)?;
    out.extend_from_slice(&header);
    if present {
        for band in bands {
            for block in &band.blocks {
                out.extend_from_slice(&block.bytes);
            }
        }
    }
    Ok(())
}

fn tile(
    width: usize,
    components: &[&[u16]],
    origin_x: usize,
    origin_y: usize,
    tile_width: usize,
    tile_height: usize,
    settings: Settings,
) -> io::Result<Vec<u8>> {
    let count = product(tile_width, tile_height)?;
    let shift = 1i32 << settings.bit_depth.saturating_sub(1);
    let mut planes = Vec::with_capacity(components.len());
    for source in components {
        let mut data = Vec::with_capacity(count);
        for y in origin_y..sum(origin_y, tile_height)? {
            for x in origin_x..sum(origin_x, tile_width)? {
                let value = at(source, sum(product(y, width)?, x)?)?;
                data.push(i32::from(value).checked_sub(shift)
                    .ok_or_else(|| invalid("JPEG 2000 level shift overflow"))?);
            }
        }
        planes.push(Plane { width: tile_width, height: tile_height, data });
    }
    if settings.rct {
        let [red, green, blue] = planes.as_mut_slice() else {
            return Err(invalid("unsupported:jpx_encoder_rct"));
        };
        for ((r, g), b) in red.data.iter_mut().zip(&mut green.data).zip(&mut blue.data) {
            let old_r = *r;
            let old_g = *g;
            let old_b = *b;
            *r = narrow((i64::from(old_r) + 2 * i64::from(old_g)
                + i64::from(old_b)) >> 2)?;
            *g = old_b.checked_sub(old_g)
                .ok_or_else(|| invalid("JPEG 2000 RCT overflow"))?;
            *b = old_r.checked_sub(old_g)
                .ok_or_else(|| invalid("JPEG 2000 RCT overflow"))?;
        }
    }
    let mut encoded = Vec::with_capacity(planes.len());
    for plane in planes {
        encoded.push(component_bands(plane, origin_x, origin_y, settings)?);
    }
    let mut out = Vec::new();
    // LRCP with one layer and one precinct per resolution.
    for resolution in 0..=usize::from(settings.levels) {
        for component in &encoded {
            let bands = component.get(resolution)
                .ok_or_else(|| invalid("JPEG 2000 missing resolution"))?;
            packet(&mut out, bands)?;
        }
    }
    Ok(out)
}

fn wrap_jp2(
    stream: &[u8],
    width: u32,
    height: u32,
    components: usize,
    settings: Settings,
) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    box_bytes(&mut out, b"jP  ", &[0x0d, 0x0a, 0x87, 0x0a])?;
    box_bytes(&mut out, b"ftyp", b"jp2\0\0\0\0\0jp2\0")?;
    let mut header = Vec::new();
    let mut ihdr = Vec::new();
    u32be(&mut ihdr, height);
    u32be(&mut ihdr, width);
    u16be(&mut ihdr, u16::try_from(components)
        .map_err(|_| invalid("JP2 component count overflow"))?);
    ihdr.extend_from_slice(&[settings.bit_depth.saturating_sub(1), 7, 0, 0]);
    box_bytes(&mut header, b"ihdr", &ihdr)?;
    let mut colr = vec![1, 0, 0];
    u32be(&mut colr, if components == 1 { 17 } else { 16 });
    box_bytes(&mut header, b"colr", &colr)?;
    if settings.alpha {
        let mut cdef = Vec::new();
        u16be(&mut cdef, 4);
        // Keep definitions in codestream order, including opacity when first.
        let opacity = if settings.alpha_first { 0 } else { 3 };
        for channel in 0..4u16 {
            let is_opacity = channel == opacity;
            let association = if is_opacity {
                0
            } else if settings.alpha_first {
                channel
            } else {
                channel.saturating_add(1)
            };
            u16be(&mut cdef, channel);
            u16be(&mut cdef, u16::from(is_opacity));
            u16be(&mut cdef, association);
        }
        box_bytes(&mut header, b"cdef", &cdef)?;
    }
    box_bytes(&mut out, b"jp2h", &header)?;
    box_bytes(&mut out, b"jp2c", stream)?;
    Ok(out)
}

fn coding_markers(out: &mut Vec<u8>, settings: Settings) -> io::Result<()> {
    let block_exponent = if settings.code_block_size == 32 { 3 } else { 4 };
    marker(out, 0x52, &[
        0, 0, 0, 1, u8::from(settings.rct),
        settings.levels, block_exponent, block_exponent, 0, 1,
    ])?;
    // Reversible quantization exponents are precision plus subband gain.
    // Four guard bits cover two lifting levels, including transformed chroma.
    let mut qcd = vec![(GUARD_BITS as u8) << 5, settings.bit_depth << 3];
    for _ in 0..settings.levels {
        qcd.extend_from_slice(&[
            settings.bit_depth.saturating_add(1) << 3,
            settings.bit_depth.saturating_add(1) << 3,
            settings.bit_depth.saturating_add(2) << 3,
        ]);
    }
    marker(out, 0x5c, &qcd)
}

/// Encode row-major planar samples as a raw codestream or a JP2 file.
///
/// Supports one, three, or four 8-bit components and one 12- or 16-bit gray
/// component. JP2 four-component output requires `alpha`. RCT requires three
/// components. Dimensions, samples, tiles, and bytes have fixture limits.
pub fn encode(
    width: u32,
    height: u32,
    components: &[&[u16]],
    settings: Settings,
) -> io::Result<Vec<u8>> {
    encode_with_tiles(width, height, components, settings, &[])
}

/// Encode with explicit tile-part COD and QCD overrides for every tile.
///
/// An empty slice inherits the main header. Otherwise the slice must contain
/// exactly one entry per tile in row-major order. Each tile origin must align
/// with its own decomposition grid. Sample precision and block size are shared.
pub fn encode_with_tiles(
    width: u32,
    height: u32,
    components: &[&[u16]],
    settings: Settings,
    tile_coding: &[TileCoding],
) -> io::Result<Vec<u8>> {
    let w = usize::try_from(width)
        .map_err(|_| invalid("JPEG 2000 width conversion"))?;
    let h = usize::try_from(height)
        .map_err(|_| invalid("JPEG 2000 height conversion"))?;
    if w == 0 || h == 0 {
        return Err(invalid("JPEG 2000 dimensions must be nonzero"));
    }
    limit("jpx_encoder_width", w, MAX_SIDE)?;
    limit("jpx_encoder_height", h, MAX_SIDE)?;
    if !matches!(components.len(), 1 | 3 | 4) {
        return Err(invalid("unsupported:jpx_encoder_components"));
    }
    if settings.bit_depth != 8
        && !(matches!(settings.bit_depth, 12 | 16) && components.len() == 1)
    {
        return Err(invalid("unsupported:jpx_encoder_precision"));
    }
    limit("jpx_encoder_levels", usize::from(settings.levels), 2)?;
    if !matches!(settings.code_block_size, 32 | 64) {
        return Err(invalid("unsupported:jpx_encoder_code_block"));
    }
    if settings.rct && components.len() != 3 {
        return Err(invalid("unsupported:jpx_encoder_rct"));
    }
    if settings.alpha && (!settings.jp2 || components.len() != 4) {
        return Err(invalid("unsupported:jpx_encoder_alpha"));
    }
    if settings.alpha_first && !settings.alpha {
        return Err(invalid("unsupported:jpx_encoder_alpha_first"));
    }
    if settings.jp2 && components.len() == 4 && !settings.alpha {
        return Err(invalid("unsupported:jpx_encoder_jp2_color"));
    }
    let pixels = product(w, h)?;
    limit("jpx_encoder_samples", product(pixels, components.len())?, MAX_SAMPLES)?;
    let maximum = (1u32 << settings.bit_depth).saturating_sub(1);
    for component in components {
        if component.len() != pixels {
            return Err(invalid("JPEG 2000 component sample count"));
        }
        if component.iter().any(|sample| u32::from(*sample) > maximum) {
            return Err(invalid("JPEG 2000 sample exceeds precision"));
        }
    }
    let tw = if settings.tile_width == 0 {
        w
    } else {
        usize::try_from(settings.tile_width)
            .map_err(|_| invalid("JPEG 2000 tile width conversion"))?
    };
    let th = if settings.tile_height == 0 {
        h
    } else {
        usize::try_from(settings.tile_height)
            .map_err(|_| invalid("JPEG 2000 tile height conversion"))?
    };
    if tw == 0 || th == 0 || tw > w || th > h {
        return Err(invalid("unsupported:jpx_encoder_tile_dimensions"));
    }
    let alignment = 1usize << settings.levels;
    if tile_coding.is_empty()
        && ((tw < w && tw % alignment != 0) || (th < h && th % alignment != 0))
    {
        return Err(invalid("unsupported:jpx_encoder_tile_alignment"));
    }
    let columns = sum(w, tw.saturating_sub(1))? / tw;
    let rows = sum(h, th.saturating_sub(1))? / th;
    let tile_count = product(columns, rows)?;
    limit("jpx_encoder_tiles", tile_count, MAX_TILES)?;
    if !tile_coding.is_empty() && tile_coding.len() != tile_count {
        return Err(invalid("JPEG 2000 tile coding count"));
    }
    for (index, coding) in tile_coding.iter().enumerate() {
        limit("jpx_encoder_levels", usize::from(coding.levels), 2)?;
        if coding.rct && components.len() != 3 {
            return Err(invalid("unsupported:jpx_encoder_rct"));
        }
        let grid = 1usize << coding.levels;
        let x = product(index % columns, tw)?;
        let y = product(index / columns, th)?;
        if x % grid != 0 || y % grid != 0 {
            return Err(invalid("unsupported:jpx_encoder_tile_alignment"));
        }
    }

    let mut out = vec![0xff, 0x4f];
    let mut siz = Vec::new();
    u16be(&mut siz, 0);
    for value in [width, height, 0, 0, tw as u32, th as u32, 0, 0] {
        u32be(&mut siz, value);
    }
    u16be(&mut siz, u16::try_from(components.len())
        .map_err(|_| invalid("JPEG 2000 component count overflow"))?);
    for _ in components {
        siz.extend_from_slice(&[settings.bit_depth.saturating_sub(1), 1, 1]);
    }
    marker(&mut out, 0x51, &siz)?;
    coding_markers(&mut out, settings)?;

    for row in 0..rows {
        for column in 0..columns {
            let x = product(column, tw)?;
            let y = product(row, th)?;
            let index = sum(product(row, columns)?, column)?;
            let mut local = settings;
            let mut header = Vec::new();
            if !tile_coding.is_empty() {
                let coding = at(tile_coding, index)?;
                local.levels = coding.levels;
                local.rct = coding.rct;
                coding_markers(&mut header, local)?;
            }
            let data = tile(
                w, components, x, y, tw.min(w.saturating_sub(x)),
                th.min(h.saturating_sub(y)), local,
            )?;
            let tile_id = u16::try_from(index)
                .map_err(|_| invalid("JPEG 2000 tile index overflow"))?;
            let length = u32::try_from(sum(sum(data.len(), header.len())?, 14)?)
                .map_err(|_| invalid("JPEG 2000 tile-part length overflow"))?;
            limit(
                "jpx_encoder_bytes",
                sum(sum(out.len(), length as usize)?, 2)?,
                MAX_BYTES,
            )?;
            let mut sot = Vec::new();
            u16be(&mut sot, tile_id);
            u32be(&mut sot, length);
            sot.extend_from_slice(&[0, 1]);
            marker(&mut out, 0x90, &sot)?;
            out.extend_from_slice(&header);
            out.extend_from_slice(&[0xff, 0x93]);
            out.extend_from_slice(&data);
        }
    }
    out.extend_from_slice(&[0xff, 0xd9]);
    if settings.jp2 {
        wrap_jp2(&out, width, height, components.len(), settings)
    } else {
        Ok(out)
    }
}
