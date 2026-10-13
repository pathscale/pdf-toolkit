use super::{finish, Decoded, FilterError, FilterKind, Output};

const TABLE_SIZE: usize = 4096;
const CLEAR: u16 = 256;
const EOD: u16 = 257;
const FIRST_FREE: usize = 258;

pub(super) fn decode(
    input: &[u8],
    early_change: bool,
    max_output: u64,
) -> Result<Decoded, FilterError> {
    let mut reader = BitReader::new(input);
    let mut table = Table::new();
    let mut scratch = [0u8; TABLE_SIZE];
    let mut output = Output::new(max_output);
    let mut width = 9u8;
    let mut next = FIRST_FREE;
    let mut previous = None;
    let early = if early_change { 1usize } else { 0usize };

    loop {
        let offset = reader.offset();
        let Some(code) = reader.read(width) else {
            return finish(output.into_vec(), true, FilterKind::Lzw, input.len());
        };

        if code == CLEAR {
            width = 9;
            next = FIRST_FREE;
            previous = None;
            continue;
        }
        if code == EOD {
            return finish(output.into_vec(), false, FilterKind::Lzw, offset);
        }

        // A full table may be followed by a clear or EOD, but cannot grow
        // further or silently freeze while processing another data code.
        if next >= TABLE_SIZE {
            return finish(output.into_vec(), true, FilterKind::Lzw, offset);
        }

        let code_index = usize::from(code);
        let first;
        if code_index < next {
            let Some(word) = table.word(code, &mut scratch) else {
                return finish(output.into_vec(), true, FilterKind::Lzw, offset);
            };
            let Some(byte) = word.first().copied() else {
                return finish(output.into_vec(), true, FilterKind::Lzw, offset);
            };
            first = byte;
            output.extend(word)?;
        } else if code_index == next {
            // KwKwK: the new entry is the preceding word plus its first byte.
            let Some(previous_code) = previous else {
                return finish(output.into_vec(), true, FilterKind::Lzw, offset);
            };
            let Some(word) = table.word(previous_code, &mut scratch) else {
                return finish(output.into_vec(), true, FilterKind::Lzw, offset);
            };
            let Some(byte) = word.first().copied() else {
                return finish(output.into_vec(), true, FilterKind::Lzw, offset);
            };
            first = byte;
            output.extend(word)?;
            output.push(first)?;
        } else {
            return finish(output.into_vec(), true, FilterKind::Lzw, offset);
        }

        if let Some(previous_code) = previous {
            if table.insert(next, previous_code, first).is_none() {
                return finish(output.into_vec(), true, FilterKind::Lzw, offset);
            }
            next = next.saturating_add(1);
            if width < 12 && next.saturating_add(early) == (1usize << width) {
                width = width.saturating_add(1);
            }
        }
        previous = Some(code);
    }
}

struct Table {
    prefix: [u16; TABLE_SIZE],
    suffix: [u8; TABLE_SIZE],
    length: [u16; TABLE_SIZE],
}

impl Table {
    fn new() -> Self {
        let mut table = Self {
            prefix: [0; TABLE_SIZE],
            suffix: [0; TABLE_SIZE],
            length: [0; TABLE_SIZE],
        };
        for (index, (suffix, length)) in table
            .suffix
            .iter_mut()
            .zip(table.length.iter_mut())
            .take(256)
            .enumerate()
        {
            *suffix = index as u8;
            *length = 1;
        }
        table
    }

    fn insert(&mut self, index: usize, prefix: u16, suffix: u8) -> Option<()> {
        if usize::from(prefix) >= index {
            return None;
        }
        let length = self
            .length
            .get(usize::from(prefix))
            .copied()?
            .checked_add(1)?;
        if usize::from(length) > TABLE_SIZE {
            return None;
        }
        *self.prefix.get_mut(index)? = prefix;
        *self.suffix.get_mut(index)? = suffix;
        *self.length.get_mut(index)? = length;
        Some(())
    }

    fn word<'a>(&self, code: u16, scratch: &'a mut [u8; TABLE_SIZE]) -> Option<&'a [u8]> {
        let size = usize::from(*self.length.get(usize::from(code))?);
        if size == 0 {
            return None;
        }
        let word = scratch.get_mut(..size)?;
        let mut current = code;
        let mut rooted = false;

        for (count, slot) in word.iter_mut().rev().enumerate() {
            let index = usize::from(current);
            *slot = *self.suffix.get(index)?;
            if current < CLEAR {
                if count.saturating_add(1) != size {
                    return None;
                }
                rooted = true;
            } else {
                let parent = *self.prefix.get(index)?;
                if parent >= current {
                    return None;
                }
                current = parent;
            }
        }

        if rooted {
            Some(word)
        } else {
            None
        }
    }
}

struct BitReader<'a> {
    remaining: &'a [u8],
    consumed: usize,
    reservoir: u32,
    bits: u8,
}

impl<'a> BitReader<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self {
            remaining: input,
            consumed: 0,
            reservoir: 0,
            bits: 0,
        }
    }

    fn offset(&self) -> usize {
        self.consumed
            .saturating_sub(usize::from(self.bits.saturating_add(7) / 8))
    }

    fn read(&mut self, width: u8) -> Option<u16> {
        while self.bits < width {
            let (&byte, rest) = self.remaining.split_first()?;
            self.remaining = rest;
            self.consumed = self.consumed.saturating_add(1);
            self.reservoir = (self.reservoir << 8) | u32::from(byte);
            self.bits = self.bits.saturating_add(8);
        }

        self.bits = self.bits.saturating_sub(width);
        let code = (self.reservoir >> self.bits) & ((1u32 << width) - 1);
        self.reservoir &= (1u32 << self.bits) - 1;
        Some(code as u16)
    }
}
