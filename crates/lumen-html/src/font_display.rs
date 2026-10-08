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
    display:FontDisplay,
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
            display,
            block_ms,
            failure_ms,
            phase: if block_ms == 0 {
                DisplayPhase::Swap
            } else {
                DisplayPhase::Block
            },
        }
    }

    /// Descriptor changes use the original first-use clock. Resource failure
    /// is independent of a policy deadline and cannot be revived by a new policy.
    pub fn configure(&mut self,display:FontDisplay,now_ms:u64,loaded_at:Option<u64>,failed:bool)->bool {
        if self.display==display {return false;}
        let before=self.phase;
        let configured=Self::new(display,self.started);
        self.display=display;
        self.block_ms=configured.block_ms;
        self.failure_ms=configured.failure_ms;
        self.phase=configured.phase;
        self.update_with_completion(now_ms,loaded_at,failed);
        before!=self.phase
    }

    pub fn phase(&self) -> DisplayPhase {
        self.phase
    }

    /// Loading after the failure deadline does not swap this face into the page.
    /// A previously loaded face remains usable after all deadlines pass.
    pub fn update(&mut self, now_ms: u64, loaded: bool, failed: bool) -> bool {
        self.update_with_completion(now_ms,loaded.then_some(now_ms),failed)
    }

    /// Embedders record the actual completion clock, not the next rendering
    /// opportunity: a timely load remains usable even when painting is delayed.
    pub fn update_with_completion(&mut self,now_ms:u64,loaded_at:Option<u64>,failed:bool)->bool {
        let before = self.phase;
        if matches!(before, DisplayPhase::Loaded | DisplayPhase::Failure) {
            return false;
        }
        let elapsed = now_ms.saturating_sub(self.started);
        let timely=loaded_at.is_some_and(|loaded|self.failure_ms.is_none_or(|end|
            loaded.saturating_sub(self.started)<end));
        self.phase = if failed {
            DisplayPhase::Failure
        } else if timely {
            DisplayPhase::Loaded
        } else if self.failure_ms.is_some_and(|end|elapsed>=end) {
            DisplayPhase::Failure
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
        let mut changed=DisplayTimeline::new(FontDisplay::Optional,0);
        changed.update(100,false,false);
        assert_eq!(changed.phase(),DisplayPhase::Failure);
        assert!(changed.configure(FontDisplay::Block,200,None,false));
        assert_eq!(changed.phase(),DisplayPhase::Block);
        assert_eq!(changed.next_delay_ms(200),Some(2800));
        assert!(changed.configure(FontDisplay::Swap,200,None,false));
        assert_eq!(changed.phase(),DisplayPhase::Swap);
        let mut delayed=DisplayTimeline::new(FontDisplay::Optional,10);
        assert!(delayed.update_with_completion(500,Some(109),false));
        assert_eq!(delayed.phase(),DisplayPhase::Loaded);
        let mut late=DisplayTimeline::new(FontDisplay::Optional,10);
        assert!(late.update_with_completion(500,Some(110),false));
        assert_eq!(late.phase(),DisplayPhase::Failure);
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
