use super::{finish, Decoded, FilterError, FilterKind, Output};

pub(super) fn decode(input: &[u8], max_output: u64) -> Result<Decoded, FilterError> {
    let mut output = Output::new(max_output);
    let mut remaining = input;

    while let Some((&control, rest)) = remaining.split_first() {
        let offset = input.len().saturating_sub(remaining.len());
        remaining = rest;

        match control {
            128 => {
                return finish(output.into_vec(), false, FilterKind::RunLength, offset);
            }
            0..=127 => {
                let count = usize::from(control).saturating_add(1);
                let Some(literal) = remaining.get(..count) else {
                    output.extend(remaining)?;
                    return finish(output.into_vec(), true, FilterKind::RunLength, input.len());
                };
                output.extend(literal)?;
                let Some(rest) = remaining.get(count..) else {
                    return finish(output.into_vec(), true, FilterKind::RunLength, offset);
                };
                remaining = rest;
            }
            _ => {
                let Some((&byte, rest)) = remaining.split_first() else {
                    return finish(output.into_vec(), true, FilterKind::RunLength, input.len());
                };
                remaining = rest;
                let count = 257usize.saturating_sub(usize::from(control));
                output.repeat(byte, count)?;
            }
        }
    }

    finish(output.into_vec(), true, FilterKind::RunLength, input.len())
}
