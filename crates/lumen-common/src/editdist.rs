//! The edit cost behind "Did you mean" suggestions: Levenshtein distance where a move costs 2 and
//! changing only the case of a letter costs 1.

const MOVE_COST: usize = 2;
const CASE_COST: usize = 1;

fn substitution_cost(a: u8, b: u8) -> usize {
    if a == b {
        0
    } else if a.to_ascii_lowercase() == b.to_ascii_lowercase() {
        CASE_COST
    } else {
        MOVE_COST
    }
}

/// Edit cost between `a` and `b`. With `max_cost` set, any result above it is reported as
/// `max_cost + 1` (the search stops early); `None` means no limit.
pub fn edit_cost(a: &[u8], b: &[u8], max_cost: Option<usize>) -> usize {
    let max_cost = max_cost.unwrap_or_else(|| MOVE_COST * a.len().max(b.len()));
    if a == b {
        return 0;
    }
    let (mut a, mut b) = (a, b);
    while let (Some(x), Some(y)) = (a.first(), b.first()) {
        if x != y {
            break;
        }
        a = &a[1..];
        b = &b[1..];
    }
    while let (Some(x), Some(y)) = (a.last(), b.last()) {
        if x != y {
            break;
        }
        a = &a[..a.len() - 1];
        b = &b[..b.len() - 1];
    }
    if a.is_empty() || b.is_empty() {
        return (a.len() + b.len()) * MOVE_COST;
    }
    if b.len() > a.len() {
        std::mem::swap(&mut a, &mut b);
    }
    if (a.len() - b.len()) * MOVE_COST > max_cost {
        return max_cost + 1;
    }
    let mut row: Vec<usize> = (0..a.len()).map(|i| (i + 1) * MOVE_COST).collect();
    let mut result = 0;
    for (b_index, &code) in b.iter().enumerate() {
        let mut distance = b_index * MOVE_COST;
        result = distance;
        let mut minimum = usize::MAX;
        for (index, &ch) in a.iter().enumerate() {
            let substitute = distance + substitution_cost(code, ch);
            distance = row[index];
            let insert_delete = result.min(distance) + MOVE_COST;
            result = insert_delete.min(substitute);
            row[index] = result;
            minimum = minimum.min(result);
        }
        if minimum > max_cost {
            return max_cost + 1;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::edit_cost;

    #[test]
    fn known_costs() {
        for (a, b, n) in [
            ("", "", 0),
            ("", "a", 2),
            ("a", "A", 1),
            ("Apple", "Aple", 2),
            ("Banana", "B@n@n@", 6),
            ("abc", "y", 6),
            ("CPython", "pypy", 11),
            ("AbstractFoobarManager", "abstract_foobar_manager", 7),
            ("ABA", "AAB", 4),
        ] {
            assert_eq!(edit_cost(a.as_bytes(), b.as_bytes(), None), n, "{a} {b}");
        }
    }

    #[test]
    fn threshold_is_exceeded() {
        assert!(edit_cost(b"Python", b"Java", Some(3)) > 3);
        assert_eq!(edit_cost(b"Python", b"Java", Some(25)), 12);
    }
}
