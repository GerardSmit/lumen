//! Allocation-free alignment of a target interval with a scrolling interval.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Alignment {
    Start,
    Center,
    End,
    Nearest,
    /// No scroll for a wholly visible target, `Nearest` for a partly visible one and `Center`
    /// for a hidden one (what browsers do for `focus()`).
    CenterIfNeeded,
}

/// Physical displacement needed to align an interval. `reverse` maps logical
/// start/end to the opposite physical edges; nearest remains physical.
pub fn alignment_delta(
    target_start: f64,
    target_end: f64,
    port_start: f64,
    port_end: f64,
    alignment: Alignment,
    reverse: bool,
) -> f64 {
    let start = target_start - port_start;
    let end = target_end - port_end;
    let target_size = target_end - target_start;
    let port_size = port_end - port_start;
    match alignment {
        Alignment::Start => {
            if reverse {
                end
            } else {
                start
            }
        }
        Alignment::End => {
            if reverse {
                start
            } else {
                end
            }
        }
        Alignment::Center => start + (target_size - port_size) / 2.0,
        Alignment::CenterIfNeeded => {
            if target_end <= port_start || target_start >= port_end {
                start + (target_size - port_size) / 2.0
            } else {
                alignment_delta(
                    target_start,
                    target_end,
                    port_start,
                    port_end,
                    Alignment::Nearest,
                    reverse,
                )
            }
        }
        Alignment::Nearest => {
            let before = target_start < port_start;
            let after = target_end > port_end;
            if before && after {
                0.0
            } else if (before && target_size < port_size) || (after && target_size > port_size) {
                start
            } else if (before && target_size > port_size) || (after && target_size < port_size) {
                end
            } else {
                0.0
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_preserves_visible_or_spanning_targets_and_uses_the_nearer_edge() {
        let nearest = |a, b| alignment_delta(a, b, 0.0, 100.0, Alignment::Nearest, false);
        assert_eq!(nearest(20.0, 40.0), 0.0);
        assert_eq!(nearest(-20.0, 120.0), 0.0);
        assert_eq!(nearest(-20.0, 10.0), -20.0);
        assert_eq!(nearest(90.0, 120.0), 20.0);
        assert_eq!(nearest(-120.0, -10.0), -110.0);
        assert_eq!(nearest(110.0, 220.0), 110.0);
        assert_eq!(
            alignment_delta(20.0, 40.0, 0.0, 100.0, Alignment::Start, true),
            -60.0
        );
    }

    #[test]
    fn center_if_needed_keeps_visible_targets_and_centers_hidden_ones() {
        let align = |a, b| alignment_delta(a, b, 0.0, 100.0, Alignment::CenterIfNeeded, false);
        assert_eq!(align(20.0, 40.0), 0.0);
        assert_eq!(align(90.0, 120.0), 20.0);
        assert_eq!(align(-20.0, 10.0), -20.0);
        assert_eq!(align(200.0, 220.0), 160.0);
        assert_eq!(align(-220.0, -200.0), -260.0);
    }
}
