//! Allocation-free alignment of a target interval with a scrolling interval.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Alignment {
    Start,
    Center,
    End,
    Nearest,
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
}
