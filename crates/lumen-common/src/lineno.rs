//! Line-number bookkeeping shared by the languages' tracers and debuggers: the ranges of a
//! per-instruction line table and the decision of when stepping onto an instruction is a new
//! "line event" (CPython's `sys.settrace` / `sys.monitoring` LINE rule). Instruction indices are
//! engine-defined; a line of 0 means "no line".

/// The `(start, end, line)` ranges of consecutive instructions sharing one line, `end` exclusive.
pub fn line_ranges(lines: &[u32]) -> Vec<(usize, usize, u32)> {
    let mut out = Vec::new();
    let mut start = 0;
    for i in 1..=lines.len() {
        if i == lines.len() || lines[i] != lines[start] {
            out.push((start, i, lines[start]));
            start = i;
        }
    }
    out
}

/// The instructions a line event can fire on: the first one, each one whose line differs from its
/// predecessor's, and each jump target (`targets[i]`, may be shorter than `lines`).
pub fn line_starts(lines: &[u32], targets: &[bool]) -> Vec<bool> {
    (0..lines.len()).map(|i| i == 0 || lines[i] != lines[i - 1] || targets.get(i).copied().unwrap_or(false)).collect()
}

/// Whether executing an instruction on line `cur` is a line event. `candidate` is the
/// instruction's [`line_starts`] entry, `prev` the line of the instruction run before it (0 at
/// the start of a frame), `backward` whether it was reached by a backward jump (which fires even
/// on the same line).
#[inline]
pub fn line_event_due(candidate: bool, prev: u32, cur: u32, backward: bool) -> bool {
    candidate && (prev == 0 || prev != cur || backward)
}

/// The first instruction whose line is `line`, else the first one on a later line (the line
/// table's first code-owning line at or after `line`); `None` when no instruction is on a line
/// at or after it.
pub fn first_instruction_at_or_after(lines: &[u32], line: u32) -> Option<usize> {
    lines.iter().position(|&l| l == line).or_else(|| {
        let next = lines.iter().copied().filter(|&l| l > line).min()?;
        lines.iter().position(|&l| l == next)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_group_equal_lines() {
        assert_eq!(line_ranges(&[1, 1, 2, 3, 3]), vec![(0, 2, 1), (2, 3, 2), (3, 5, 3)]);
        assert!(line_ranges(&[]).is_empty());
    }

    #[test]
    fn starts_and_events() {
        let s = line_starts(&[1, 1, 2, 2], &[false, false, false, true]);
        assert_eq!(s, vec![true, false, true, true]);
        assert!(line_event_due(true, 1, 2, false));
        assert!(!line_event_due(true, 2, 2, false));
        assert!(line_event_due(true, 2, 2, true));
        assert!(!line_event_due(false, 1, 2, false));
        assert!(line_event_due(true, 0, 5, false));
    }

    #[test]
    fn first_instruction() {
        assert_eq!(first_instruction_at_or_after(&[1, 2, 4], 2), Some(1));
        assert_eq!(first_instruction_at_or_after(&[1, 2, 4], 3), Some(2));
        assert_eq!(first_instruction_at_or_after(&[1, 2, 4], 5), None);
    }
}
