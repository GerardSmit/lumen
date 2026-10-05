use super::*;
use std::{
    collections::{HashSet, VecDeque},
    mem::size_of,
};

pub(super) const MAX_CONTROL_BYTES: usize = 64 * 1024;
const MAX_HISTORY_BYTES: usize = 1024 * 1024;
const MAX_HISTORY_CONTROLS: usize = 16;
const MAX_HISTORY_ENTRIES_PER_CONTROL: usize = 32;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct EditSnapshot {
    pub value: String,
    pub start: usize,
    pub end: usize,
    pub direction: String,
}

impl EditSnapshot {
    fn retained_bytes(&self) -> usize {
        size_of::<Self>() + self.value.capacity() + self.direction.capacity()
    }
}

#[derive(Clone)]
struct EditStep {
    before: EditSnapshot,
    after: EditSnapshot,
    sequence: u64,
}

impl EditStep {
    fn retained_bytes(&self) -> usize {
        size_of::<Self>() + self.before.retained_bytes() + self.after.retained_bytes()
    }
}

struct ControlHistory {
    node: NodeId,
    undo: VecDeque<EditStep>,
    redo: VecDeque<EditStep>,
    touched: u64,
}

impl ControlHistory {
    fn new(node: NodeId) -> Self {
        Self {
            node,
            undo: VecDeque::new(),
            redo: VecDeque::new(),
            touched: 0,
        }
    }
    fn entry_count(&self) -> usize {
        self.undo.len() + self.redo.len()
    }
    fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            + self.undo.capacity() * size_of::<EditStep>()
            + self.redo.capacity() * size_of::<EditStep>()
            + self
                .undo
                .iter()
                .map(EditStep::retained_bytes)
                .sum::<usize>()
            + self
                .redo
                .iter()
                .map(EditStep::retained_bytes)
                .sum::<usize>()
    }
}

#[derive(Clone)]
pub(super) struct Composition {
    pub node: NodeId,
    pub before: EditSnapshot,
    pub mark_start: usize,
    pub mark_end: usize,
}

impl Composition {
    fn retained_bytes(&self) -> usize {
        size_of::<Self>() + self.before.retained_bytes()
    }
}

/// Bounded editing history and the one active composition for a realm.
/// The byte budget includes retained string capacities and container slots,
/// rather than only visible UTF-8 lengths.
#[derive(Default)]
pub(super) struct EditingState {
    controls: Vec<ControlHistory>,
    composition: Option<Composition>,
    pending_select_events: HashSet<NodeId>,
    sequence: u64,
}

impl EditingState {
    fn retained_bytes(&self) -> usize {
        size_of::<Self>()
            + self.pending_select_events.capacity() * size_of::<NodeId>()
            + self.controls.capacity() * size_of::<ControlHistory>()
            + self
                .controls
                .iter()
                .map(ControlHistory::retained_bytes)
                .sum::<usize>()
            + self
                .composition
                .as_ref()
                .map_or(0, Composition::retained_bytes)
    }

    fn tick(&mut self) -> u64 {
        self.sequence = self.sequence.wrapping_add(1);
        self.sequence
    }

    fn control_index(&self, node: NodeId) -> Option<usize> {
        self.controls
            .iter()
            .position(|control| control.node == node)
    }

    fn ensure_control(&mut self, node: NodeId) -> Option<usize> {
        if let Some(index) = self.control_index(node) {
            return Some(index);
        }
        if self.controls.len() == MAX_HISTORY_CONTROLS {
            let oldest = self
                .controls
                .iter()
                .enumerate()
                .min_by_key(|(_, control)| control.touched)
                .map(|(index, _)| index)?;
            self.controls.remove(oldest);
        }
        if self.controls.try_reserve(1).is_err() {
            return None;
        }
        self.controls.push(ControlHistory::new(node));
        Some(self.controls.len() - 1)
    }

    pub fn record(&mut self, node: NodeId, before: EditSnapshot, after: EditSnapshot) {
        if before == after {
            return;
        }
        let Some(index) = self.ensure_control(node) else {
            return;
        };
        let sequence = self.tick();
        let control = &mut self.controls[index];
        control.touched = sequence;
        control.redo.clear();
        while control.entry_count() >= MAX_HISTORY_ENTRIES_PER_CONTROL {
            match (control.undo.front(), control.redo.front()) {
                (Some(left), Some(right)) if left.sequence <= right.sequence => {
                    control.undo.pop_front();
                }
                (Some(_), Some(_)) => {
                    control.redo.pop_front();
                }
                (Some(_), None) => {
                    control.undo.pop_front();
                }
                (None, Some(_)) => {
                    control.redo.pop_front();
                }
                (None, None) => break,
            }
        }
        if control.undo.try_reserve(1).is_err() {
            return;
        }
        control.undo.push_back(EditStep {
            before,
            after,
            sequence,
        });
        self.trim_to_budget();
    }

    pub fn target(
        &mut self,
        node: NodeId,
        current: &EditSnapshot,
        redo: bool,
    ) -> Option<EditSnapshot> {
        let index = self.control_index(node)?;
        let matches = {
            let control = &self.controls[index];
            let step = if redo {
                control.redo.back()
            } else {
                control.undo.back()
            }?;
            let expected = if redo { &step.before } else { &step.after };
            // Selection is intentionally not part of the stale-value test:
            // moving the caret after an edit must not make native Undo vanish.
            expected.value == current.value
        };
        if !matches {
            self.controls.remove(index);
            return None;
        }
        let touched = self.tick();
        self.controls[index].touched = touched;
        let control = &self.controls[index];
        let step = if redo {
            control.redo.back()?
        } else {
            control.undo.back()?
        };
        Some(if redo {
            step.after.clone()
        } else {
            step.before.clone()
        })
    }

    pub fn commit_history(&mut self, node: NodeId, redo: bool) {
        let Some(index) = self.control_index(node) else {
            return;
        };
        let control = &mut self.controls[index];
        let (from, to) = if redo {
            (&mut control.redo, &mut control.undo)
        } else {
            (&mut control.undo, &mut control.redo)
        };
        let Some(step) = from.pop_back() else {
            return;
        };
        if to.try_reserve(1).is_ok() {
            to.push_back(step);
        }
        self.trim_to_budget();
    }

    pub fn invalidate(&mut self, node: NodeId) {
        self.controls.retain(|control| control.node != node);
        if self
            .composition
            .as_ref()
            .is_some_and(|composition| composition.node == node)
        {
            self.composition = None;
        }
    }

    pub fn begin_select_event(&mut self, node: NodeId) -> Result<bool, ()> {
        if self.pending_select_events.contains(&node) {
            return Ok(false);
        }
        self.pending_select_events.try_reserve(1).map_err(|_| ())?;
        self.pending_select_events.insert(node);
        Ok(true)
    }

    pub fn finish_select_event(&mut self, node: NodeId) {
        self.pending_select_events.remove(&node);
    }

    pub fn composition(&self, node: NodeId) -> Option<Composition> {
        self.composition
            .as_ref()
            .filter(|composition| composition.node == node)
            .cloned()
    }

    pub fn active_composition_node(&self) -> Option<NodeId> {
        self.composition
            .as_ref()
            .map(|composition| composition.node)
    }

    pub fn begin_composition(&mut self, composition: Composition) {
        self.composition = Some(composition);
        self.trim_to_budget();
    }

    pub fn update_composition(&mut self, node: NodeId, mark_start: usize, mark_end: usize) {
        if let Some(composition) = self.composition.as_mut().filter(|entry| entry.node == node) {
            composition.mark_start = mark_start;
            composition.mark_end = mark_end;
        }
    }

    pub fn finish_composition(&mut self, node: NodeId) -> Option<Composition> {
        if self
            .composition
            .as_ref()
            .is_some_and(|entry| entry.node == node)
        {
            self.composition.take()
        } else {
            None
        }
    }

    fn trim_to_budget(&mut self) {
        while self.retained_bytes() > MAX_HISTORY_BYTES {
            let oldest = self
                .controls
                .iter()
                .enumerate()
                .flat_map(|(index, control)| {
                    [control.undo.front(), control.redo.front()]
                        .into_iter()
                        .flatten()
                        .map(move |step| (index, step.sequence))
                })
                .min_by_key(|(_, sequence)| *sequence);
            if let Some((index, _)) = oldest {
                let control = &mut self.controls[index];
                match (control.undo.front(), control.redo.front()) {
                    (Some(left), Some(right)) if left.sequence <= right.sequence => {
                        control.undo.pop_front();
                    }
                    (Some(_), Some(_)) => {
                        control.redo.pop_front();
                    }
                    (Some(_), None) => {
                        control.undo.pop_front();
                    }
                    (None, Some(_)) => {
                        control.redo.pop_front();
                    }
                    (None, None) => {}
                }
            } else if self.composition.take().is_some() {
                // An oversized, long-lived composition cannot consume the realm
                // budget indefinitely. Its current DOM value remains intact.
                break;
            } else {
                break;
            }
        }
        self.controls
            .retain(|control| !control.undo.is_empty() || !control.redo.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(value: &str) -> EditSnapshot {
        let end = value.encode_utf16().count();
        EditSnapshot {
            value: value.into(),
            start: end,
            end,
            direction: "none".into(),
        }
    }

    fn nodes(count: usize) -> Vec<NodeId> {
        let source = (0..count)
            .map(|index| format!("<input id=i{index}>"))
            .collect::<String>();
        let document = html::parse(&source, count + 8).unwrap();
        (0..count)
            .map(|index| {
                selector::query_selector(&document, document.root(), &format!("#i{index}"))
                    .unwrap()
                    .unwrap()
            })
            .collect()
    }

    #[test]
    fn histories_are_bounded_and_stale_values_are_rejected() {
        let mut state = EditingState::default();
        let node = nodes(1)[0];
        let mut before = snapshot("");
        for index in 0..40 {
            let after = snapshot(&"x".repeat(index + 1));
            state.record(node, before, after.clone());
            before = after;
        }
        let control = &state.controls[0];
        assert_eq!(control.entry_count(), MAX_HISTORY_ENTRIES_PER_CONTROL);
        assert!(state.retained_bytes() <= MAX_HISTORY_BYTES);
        assert!(state.target(node, &snapshot("stale"), false).is_none());
        assert!(state.controls.is_empty());
    }

    #[test]
    fn history_keeps_at_most_sixteen_controls_and_counts_composition_memory() {
        let mut state = EditingState::default();
        let nodes = nodes(20);
        for node in nodes {
            state.record(node, snapshot("a"), snapshot("b"));
        }
        assert!(state.controls.len() <= MAX_HISTORY_CONTROLS);
        let node = state.controls[0].node;
        state.begin_composition(Composition {
            node,
            before: snapshot("a"),
            mark_start: 0,
            mark_end: 1,
        });
        assert!(state.retained_bytes() <= MAX_HISTORY_BYTES);
    }

    #[test]
    fn moving_selection_does_not_make_history_stale() {
        let node = nodes(1)[0];
        let mut state = EditingState::default();
        state.record(node, snapshot("before"), snapshot("after"));
        let mut current = snapshot("after");
        current.start = 0;
        current.end = 0;
        assert_eq!(
            state.target(node, &current, false),
            Some(snapshot("before"))
        );
    }
}
