//! Auto and hint popover stacks: which popovers close when another opens, closes or is
//! light-dismissed. The stacks are the typed `Kind::Popover` entries of the top layer.

use crate::top_layer::{Kind, MAX_ANCESTOR_STEPS, PopoverMode, StateError};
use crate::{Document, NodeId};
use alloc::vec::Vec;

/// The popovers of one stack, bottom first.
pub fn stack(document: &Document, mode: PopoverMode) -> Vec<NodeId> {
    document
        .top_layer_entries()
        .filter(|(_, kind, _)| *kind == Kind::Popover(mode))
        .map(|(node, _, _)| node)
        .collect()
}

fn nearest_inclusive(
    document: &Document,
    start: NodeId,
    positions: &[NodeId],
) -> Result<Option<usize>, StateError> {
    let mut current = start;
    for _ in 0..MAX_ANCESTOR_STEPS {
        if let Some(index) = positions.iter().position(|node| *node == current) {
            return Ok(Some(index));
        }
        let Some(parent) = document.composed_parent(current).map_err(StateError::Dom)? else {
            return Ok(None);
        };
        current = parent;
    }
    Err(StateError::ResourceLimit)
}

/// The topmost popover of `list` that is an ancestor of the invoker or of the popover being
/// shown. The new popover itself counts as the top of the list, so an invoker inside it
/// yields `new_popover`.
pub fn topmost_ancestor(
    document: &Document,
    list: &[NodeId],
    new_popover: Option<NodeId>,
    invoker: Option<NodeId>,
) -> Result<Option<NodeId>, StateError> {
    let mut positions = list.to_vec();
    positions.extend(new_popover);
    let parent = match new_popover {
        Some(node) => document.composed_parent(node).map_err(StateError::Dom)?,
        None => None,
    };
    let mut best: Option<usize> = None;
    for start in [invoker, parent].into_iter().flatten() {
        if let Some(index) = nearest_inclusive(document, start, &positions)? {
            best = Some(best.map_or(index, |best| best.max(index)));
        }
    }
    Ok(best.map(|index| positions[index]))
}

/// Entries of `list` above `ancestor`, topmost first. With no ancestor the whole list.
fn above(list: &[NodeId], ancestor: Option<NodeId>, skip: NodeId) -> Vec<NodeId> {
    if ancestor == Some(skip) {
        return Vec::new();
    }
    let first = ancestor
        .and_then(|ancestor| list.iter().position(|node| *node == ancestor))
        .map_or(0, |index| index + 1);
    list[first..].iter().rev().copied().collect()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShowPlan {
    pub stack: PopoverMode,
    pub close: Vec<NodeId>,
}

/// Popovers to close, topmost first, and the stack to join, when `node` opens with `invoker`.
pub fn plan_show(
    document: &Document,
    node: NodeId,
    mode: PopoverMode,
    invoker: Option<NodeId>,
) -> Result<ShowPlan, StateError> {
    if mode == PopoverMode::Manual {
        return Ok(ShowPlan { stack: mode, close: Vec::new() });
    }
    let hints = stack(document, PopoverMode::Hint);
    let hint_ancestor = topmost_ancestor(document, &hints, Some(node), invoker)?;
    if mode == PopoverMode::Hint || hint_ancestor.is_some_and(|ancestor| ancestor != node) {
        return Ok(ShowPlan {
            stack: PopoverMode::Hint,
            close: above(&hints, hint_ancestor, node),
        });
    }
    let autos = stack(document, PopoverMode::Auto);
    let auto_ancestor = topmost_ancestor(document, &autos, Some(node), invoker)?;
    let mut close = above(&hints, None, node);
    close.extend(above(&autos, auto_ancestor, node));
    Ok(ShowPlan { stack: PopoverMode::Auto, close })
}

/// Popovers that close together with `node` when it is hidden, topmost first, `node` excluded.
pub fn plan_hide(document: &Document, node: NodeId) -> Result<Vec<NodeId>, StateError> {
    match document.top_layer_kind(node) {
        Some(Kind::Popover(PopoverMode::Hint)) => {
            let hints = stack(document, PopoverMode::Hint);
            Ok(above(&hints, Some(node), node))
        }
        Some(Kind::Popover(PopoverMode::Auto)) => {
            let autos = stack(document, PopoverMode::Auto);
            let Some(index) = autos.iter().position(|auto| *auto == node) else {
                return Ok(Vec::new());
            };
            let closing = &autos[index..];
            let mut plan = Vec::new();
            for hint in stack(document, PopoverMode::Hint).into_iter().rev() {
                let ancestor = topmost_ancestor(
                    document,
                    &autos,
                    Some(hint),
                    document.popover_trigger(hint),
                )?;
                if ancestor.is_some_and(|ancestor| ancestor != hint && closing.contains(&ancestor)) {
                    plan.push(hint);
                }
            }
            plan.extend(autos[index + 1..].iter().rev().copied());
            Ok(plan)
        }
        _ => Ok(Vec::new()),
    }
}

/// The popover a button names through `popovertarget`, or through `commandfor` with a popover
/// command, whether or not it was the one that opened the popover.
fn invoked_popover(document: &Document, node: NodeId) -> Option<NodeId> {
    if !crate::invokers::is_button(document, node) || crate::forms::is_disabled(document, node) {
        return None;
    }
    let attribute = |name| document.get_attribute_ns_ref(node, None, name).ok().flatten();
    let id = match attribute("popovertarget") {
        Some(id) => id,
        None => {
            let command = crate::invokers::Command::parse(attribute("command")?);
            if !matches!(
                command,
                crate::invokers::Command::TogglePopover
                    | crate::invokers::Command::ShowPopover
                    | crate::invokers::Command::HidePopover
            ) {
                return None;
            }
            attribute("commandfor")?
        }
    };
    if id.is_empty() {
        return None;
    }
    let root = document.root_node(node, false).ok()?;
    crate::selector::get_element_by_id(document, root, id).ok().flatten()
}

/// The nearest showing popover of one stack containing `node`, or `node` itself. An invoking
/// button counts as part of the popover it invokes.
fn nearest_open(
    document: &Document,
    node: NodeId,
    mode: PopoverMode,
) -> Result<Option<NodeId>, StateError> {
    let list = stack(document, mode);
    let mut current = node;
    for _ in 0..MAX_ANCESTOR_STEPS {
        if let Some(index) = list.iter().position(|entry| *entry == current) {
            return Ok(Some(list[index]));
        }
        if let Some(index) = invoked_popover(document, current)
            .and_then(|target| list.iter().position(|entry| *entry == target))
        {
            return Ok(Some(list[index]));
        }
        let Some(parent) = document.composed_parent(current).map_err(StateError::Dom)? else {
            return Ok(None);
        };
        current = parent;
    }
    Err(StateError::ResourceLimit)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Endpoints {
    pub hint: Option<NodeId>,
    pub auto: Option<NodeId>,
}

impl Endpoints {
    /// The endpoint that is highest in the top layer; this is what pointerdown and pointerup
    /// compare.
    pub fn topmost(self, document: &Document) -> Option<NodeId> {
        let index = |node: NodeId| document.top_layer_entries().position(|(entry, _, _)| entry == node);
        match (self.hint, self.auto) {
            (Some(hint), Some(auto)) => Some(if index(hint) > index(auto) { hint } else { auto }),
            (hint, auto) => hint.or(auto),
        }
    }
}

pub fn endpoints(document: &Document, node: NodeId) -> Result<Endpoints, StateError> {
    Ok(Endpoints {
        hint: nearest_open(document, node, PopoverMode::Hint)?,
        auto: nearest_open(document, node, PopoverMode::Auto)?,
    })
}

pub fn has_light_dismissable(document: &Document) -> bool {
    document.top_layer_entries().any(|(_, kind, _)| {
        matches!(kind, Kind::Popover(PopoverMode::Auto | PopoverMode::Hint))
    })
}

/// Popovers a light dismiss with these endpoints hides, topmost first, hints before autos.
pub fn plan_light_dismiss(document: &Document, endpoints: Endpoints) -> Vec<NodeId> {
    let mut plan = Vec::new();
    for (mode, endpoint) in [(PopoverMode::Hint, endpoints.hint), (PopoverMode::Auto, endpoints.auto)] {
        let list = stack(document, mode);
        let first = endpoint
            .and_then(|endpoint| list.iter().position(|node| *node == endpoint))
            .map_or(0, |index| index + 1);
        plan.extend(list[first..].iter().rev().copied());
    }
    plan
}

/// Whether `popover` still lies above its stack's endpoint; adapters re-check this after each
/// synchronous `beforetoggle` callback.
pub fn should_hide_until(document: &Document, popover: NodeId, endpoints: Endpoints) -> bool {
    let (mode, endpoint) = match document.top_layer_kind(popover) {
        Some(Kind::Popover(PopoverMode::Auto)) => (PopoverMode::Auto, endpoints.auto),
        Some(Kind::Popover(PopoverMode::Hint)) => (PopoverMode::Hint, endpoints.hint),
        _ => return false,
    };
    let list = stack(document, mode);
    let Some(index) = list.iter().position(|node| *node == popover) else {
        return false;
    };
    endpoint
        .and_then(|endpoint| list.iter().position(|node| *node == endpoint))
        .is_none_or(|endpoint| index > endpoint)
}
