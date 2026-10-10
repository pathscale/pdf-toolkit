use super::{finish, is_whitespace, Decoded, FilterError, FilterKind, Output};

pub(super) fn decode(input: &[u8], max_output: u64) -> Result<Decoded, FilterError> {
    let mut output = Output::new(max_output);
    let mut input_bytes = input.iter().copied().enumerate();
    let mut value = 0u32;
    let mut count = 0u8;
    let mut group_start = 0usize;
    let mut ended = false;

    while let Some((offset, byte)) = input_bytes.next() {
        if is_whitespace(byte) {
            continue;
        }
        if byte == b'~' {
            if !matches!(input_bytes.next(), Some((_, b'>'))) {
                return finish(output.into_vec(), true, FilterKind::Ascii85, offset);
            }
            ended = true;
            break;
        }
        if byte == b'z' {
            if count != 0 {
                return finish(output.into_vec(), true, FilterKind::Ascii85, offset);
            }
            output.repeat(0, 4)?;
            continue;
        }
        if !(b'!'..=b'u').contains(&byte) {
            return finish(output.into_vec(), true, FilterKind::Ascii85, offset);
        }

        if count == 0 {
            group_start = offset;
        }
        let Some(next_value) = value
            .checked_mul(85)
            .and_then(|value| value.checked_add(u32::from(byte - b'!')))
        else {
            return finish(output.into_vec(), true, FilterKind::Ascii85, offset);
        };
        value = next_value;
        count = count.saturating_add(1);

        if count == 5 {
            output.extend(&value.to_be_bytes())?;
            value = 0;
            count = 0;
        }
    }

    if count == 1 {
        return finish(
            output.into_vec(),
            true,
            FilterKind::Ascii85,
            group_start,
        );
    }
    if count > 1 {
        for _ in count..5 {
            let Some(next_value) = value
                .checked_mul(85)
                .and_then(|value| value.checked_add(84))
            else {
                return finish(
                    output.into_vec(),
                    true,
                    FilterKind::Ascii85,
                    group_start,
                );
            };
            value = next_value;
        }
        let word = value.to_be_bytes();
        let Some(partial) = word.get(..usize::from(count.saturating_sub(1))) else {
            return finish(
                output.into_vec(),
                true,
                FilterKind::Ascii85,
                group_start,
            );
        };
        output.extend(partial)?;
    }

    finish(
        output.into_vec(),
        !ended,
        FilterKind::Ascii85,
        input.len(),
    )
}
