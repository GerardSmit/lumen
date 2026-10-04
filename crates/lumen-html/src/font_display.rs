//! CSS font-display decisions driven by the embedding renderer's monotonic clock.
use crate::css::FontDisplay;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DisplayPhase {
    Block,
    Swap,
    Loaded,
    Failure,
}

/// User-agent durations: a short block is 100 ms and a long block/swap is 3 s.
/// A face's timer starts on rendering demand, independently of resource loading.
#[derive(Clone, Debug)]
pub struct DisplayTimeline {
    started: u64,
    block_ms: u64,
    failure_ms: Option<u64>,
    phase: DisplayPhase,
}

impl DisplayTimeline {
    pub fn new(display: FontDisplay, now_ms: u64) -> Self {
        let (block_ms, failure_ms) = match display {
            FontDisplay::Auto | FontDisplay::Block => (3000, None),
            FontDisplay::Swap => (0, None),
            FontDisplay::Fallback => (100, Some(3100)),
            FontDisplay::Optional => (100, Some(100)),
        };
        Self {
            started: now_ms,
            block_ms,
            failure_ms,
            phase: if block_ms == 0 {
                DisplayPhase::Swap
            } else {
                DisplayPhase::Block
            },
        }
    }

    pub fn phase(&self) -> DisplayPhase {
        self.phase
    }

    /// Loading after the failure deadline does not swap this face into the page.
    /// A previously loaded face remains usable after all deadlines pass.
    pub fn update(&mut self, now_ms: u64, loaded: bool, failed: bool) -> bool {
        let before = self.phase;
        if matches!(before, DisplayPhase::Loaded | DisplayPhase::Failure) {
            return false;
        }
        let elapsed = now_ms.saturating_sub(self.started);
        self.phase = if failed || self.failure_ms.is_some_and(|end| elapsed >= end) {
            DisplayPhase::Failure
        } else if loaded {
            DisplayPhase::Loaded
        } else if elapsed < self.block_ms {
            DisplayPhase::Block
        } else {
            DisplayPhase::Swap
        };
        before != self.phase
    }

    pub fn next_delay_ms(&self, now_ms: u64) -> Option<u64> {
        let elapsed = now_ms.saturating_sub(self.started);
        match self.phase {
            DisplayPhase::Block => Some(self.block_ms.saturating_sub(elapsed)),
            DisplayPhase::Swap => self.failure_ms.map(|end| end.saturating_sub(elapsed)),
            DisplayPhase::Loaded | DisplayPhase::Failure => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn font_display_deadlines_and_late_loads() {
        let mut block = DisplayTimeline::new(FontDisplay::Block, 10);
        assert_eq!(block.next_delay_ms(10), Some(3000));
        assert!(!block.update(3009, false, false));
        assert!(block.update(3010, false, false));
        assert_eq!(block.phase(), DisplayPhase::Swap);
        assert_eq!(block.next_delay_ms(3010), None);
        assert!(block.update(10_000, true, false));
        assert_eq!(block.phase(), DisplayPhase::Loaded);
        let mut optional = DisplayTimeline::new(FontDisplay::Optional, 0);
        assert!(optional.update(100, true, false));
        assert_eq!(optional.phase(), DisplayPhase::Failure);
        assert!(!optional.update(101, true, false));
        let mut timely = DisplayTimeline::new(FontDisplay::Optional, 0);
        timely.update(99, true, false);
        timely.update(100_000, false, false);
        assert_eq!(timely.phase(), DisplayPhase::Loaded);
        let mut fallback = DisplayTimeline::new(FontDisplay::Fallback, 0);
        fallback.update(100, false, false);
        assert_eq!(fallback.phase(), DisplayPhase::Swap);
        assert_eq!(fallback.next_delay_ms(100), Some(3000));
        fallback.update(3100, false, false);
        assert_eq!(fallback.phase(), DisplayPhase::Failure);
        let mut swap = DisplayTimeline::new(FontDisplay::Swap, 0);
        assert_eq!(swap.phase(), DisplayPhase::Swap);
        swap.update(0, false, true);
        assert_eq!(swap.phase(), DisplayPhase::Failure);
    }
}
