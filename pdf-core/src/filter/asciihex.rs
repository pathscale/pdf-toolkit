use super::{finish, is_whitespace, Decoded, FilterError, FilterKind, Output};

pub(super) fn decode(input: &[u8], max_output: u64) -> Result<Decoded, FilterError> {
    let mut output = Output::new(max_output);
    let mut high = None;
    let mut ended = false;

    for (offset, byte) in input.iter().copied().enumerate() {
        if is_whitespace(byte) {
            continue;
        }
        if byte == b'>' {
            ended = true;
            break;
        }

        let nibble = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => {
                return finish(output.into_vec(), true, FilterKind::AsciiHex, offset);
            }
        };
        match high.take() {
            Some(high) => output.push((high << 4) | nibble)?,
            None => high = Some(nibble),
        }
    }

    if let Some(high) = high {
        output.push(high << 4)?;
    }

    finish(output.into_vec(), !ended, FilterKind::AsciiHex, input.len())
}
