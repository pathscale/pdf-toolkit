pub(super) fn apply(key: &[u8], bytes: &mut [u8]) -> bool {
    if key.is_empty() || key.len() > 256 {
        return false;
    }
    let mut state = [0u8; 256];
    for (index, slot) in state.iter_mut().enumerate() {
        *slot = index as u8;
    }
    let mut j = 0u8;
    for i in 0..256usize {
        j = j.wrapping_add(state[i]).wrapping_add(key[i % key.len()]);
        state.swap(i, usize::from(j));
    }
    let mut i = 0u8;
    j = 0;
    for byte in bytes {
        i = i.wrapping_add(1);
        j = j.wrapping_add(state[usize::from(i)]);
        state.swap(usize::from(i), usize::from(j));
        let index = state[usize::from(i)].wrapping_add(state[usize::from(j)]);
        *byte ^= state[usize::from(index)];
    }
    true
}
