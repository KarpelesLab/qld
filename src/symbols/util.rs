//! Small helpers shared by the table and the resolution driver.

/// Returns mutable references to `slice[i]` for each `i` in `indices`, which
/// must be strictly increasing and in bounds. Costs `O(indices.len())`.
///
/// # Panics
///
/// Panics if `indices` is not strictly increasing or an index is out of
/// bounds.
pub(crate) fn select_mut<'s, T>(slice: &'s mut [T], indices: &[usize]) -> Vec<&'s mut T> {
    let mut out = Vec::with_capacity(indices.len());
    let mut rest = slice;
    let mut offset = 0usize;
    for &index in indices {
        let skip = index
            .checked_sub(offset)
            .expect("select_mut: indices must be strictly increasing");
        let (item, tail) = rest[skip..]
            .split_first_mut()
            .expect("select_mut: index out of bounds");
        out.push(item);
        rest = tail;
        offset = index + 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_disjoint_items() {
        let mut values = [0, 1, 2, 3, 4, 5];
        for item in select_mut(&mut values, &[0, 2, 5]) {
            *item += 10;
        }
        assert_eq!(values, [10, 1, 12, 3, 4, 15]);
        assert!(select_mut(&mut values, &[]).is_empty());
    }

    #[test]
    #[should_panic(expected = "strictly increasing")]
    fn rejects_repeated_indices() {
        let mut values = [0, 1];
        let _ = select_mut(&mut values, &[1, 1]);
    }
}
