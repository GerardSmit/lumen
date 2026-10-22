//! Resolution of `anchor()` and `anchor-size()` against laid out anchor boxes.
use super::*;
use css::anchor::{AnchorEnvironment, AreaAlignment};

/// Anchor-name lookup over the hits laid out so far, extended incrementally.
#[derive(Default)]
pub(super) struct AnchorIndex {
    scanned: usize,
    last_node: Option<NodeId>,
    by_name: BTreeMap<String, Vec<(NodeId, usize)>>,
}

type Remembered = (Arc<str>, usize);

/// The fallback each box last fit, by node, with the fallback list it was
/// chosen from. Layouts only stage their outcome in `pending`; a rendering
/// update commits it, so layouts between two updates all see the option
/// recorded at the previous update.
#[derive(Default)]
pub(crate) struct LastSuccessfulState {
    committed: BTreeMap<u128, Remembered>,
    pending: BTreeMap<u128, Option<Remembered>>,
}

impl LastSuccessfulState {
    pub(crate) fn commit(&mut self) {
        for (key, value) in core::mem::take(&mut self.pending) {
            match value {
                Some(value) => {
                    self.committed.insert(key, value);
                }
                None => {
                    self.committed.remove(&key);
                }
            }
        }
    }
}

pub(super) type LastSuccessful = Rc<RefCell<LastSuccessfulState>>;

/// The insets `anchor()` resolution replaced, as top, right, bottom, left.
pub(super) fn changed_insets(original: &Style, resolved: &Style) -> Option<[Option<f32>; 4]> {
    let before = [original.top, original.right, original.bottom, original.left];
    let after = [resolved.top, resolved.right, resolved.bottom, resolved.left];
    let changed: [Option<f32>; 4] = core::array::from_fn(|edge| if before[edge] != after[edge] { after[edge] } else { None });
    changed.iter().any(Option::is_some).then_some(changed)
}

/// Whether an `anchor()` or `anchor-size()` call in the declaration names no
/// anchor, and so queries the default one.
fn calls_without_name(entry: &str) -> bool {
    let lower = entry.to_ascii_lowercase();
    ["anchor(", "anchor-size("].iter().any(|function| {
        lower.match_indices(function).any(|(at, _)| {
            let inner = &lower[at + function.len()..];
            let end = inner.find([',', ')']).unwrap_or(inner.len());
            !inner[..end].split_ascii_whitespace().any(|token| token.starts_with("--"))
        })
    })
}

impl Layout<'_> {
    pub(super) fn resolve_anchored_style(&self, id: NodeId, style: Style, containing: Option<Rect>) -> Style {
        let (Some(entries), Some(containing)) = (style.anchored.clone(), containing) else {
            return style;
        };
        let fixed = style.position == Position::Fixed;
        let container_node = if fixed { self.fixed_containing_block_node } else { self.containing_block_node };
        let scroll = if let Some(node) = container_node {
            self.scrolls.iter().find(|scroll| scroll.node == node).map_or((0.0, 0.0), |scroll| (scroll.x, scroll.y))
        } else if fixed {
            (0.0, 0.0)
        } else {
            self.root_scroll
        };
        let containing_sides = container_node
            .and_then(|node| self.computed_style_from_tree_shared(node).ok())
            .map_or([3, 1, 0, 2], |container| container.logical_sides());
        let default_anchor = css::extra_props::computed(&style, "position-anchor").filter(|name| name.starts_with("--"));
        let lookup = |name: &str| self.anchor_box(id, name);
        let mut resolved = style.clone();
        for (_, entry) in entries.iter() {
            let Some((longhand, text)) = entry.split_once(':') else {
                continue;
            };
            let environment = AnchorEnvironment {
                property: longhand,
                containing: [containing.x - scroll.0, containing.y - scroll.1, containing.width, containing.height],
                self_sides: style.logical_sides(),
                containing_sides,
                default_anchor: default_anchor.as_deref(),
                lookup: &lookup,
            };
            if let Some(value) = css::anchor::resolve(text, &environment) {
                css::anchor::apply_resolved(&mut resolved, longhand, &value);
            }
        }
        resolved
    }

    fn anchor_box(&self, target: NodeId, name: &str) -> Option<[f32; 4]> {
        self.anchor_hit(target, name).map(|(_, rect)| rect)
    }

    fn anchor_hit(&self, target: NodeId, name: &str) -> Option<(NodeId, [f32; 4])> {
        let geometry = self.geometry.as_deref()?;
        let mut index = self.anchor_index.borrow_mut();
        if geometry.hits.len() < index.scanned {
            *index = AnchorIndex::default();
        }
        let start = index.scanned;
        let mut checked: Option<NodeId> = index.last_node;
        for (offset, hit) in geometry.hits[start..].iter().enumerate() {
            if let Some(pseudo @ (css::PseudoElement::Before | css::PseudoElement::After)) = hit.pseudo {
                for candidate in self.pseudo_anchor_names(hit.node, pseudo).unwrap_or_default().split(',').map(str::trim).filter(|candidate| !candidate.is_empty()) {
                    index.by_name.entry(candidate.into()).or_default().push((hit.node, start + offset));
                }
                continue;
            }
            if hit.pseudo.is_some() || hit.virtual_generated || checked == Some(hit.node) {
                continue;
            }
            checked = Some(hit.node);
            let Ok(style) = self.computed_style_from_tree_shared(hit.node) else {
                continue;
            };
            let names = css::extra_props::computed(&style, "anchor-name").unwrap_or_default();
            for candidate in names.split(',').map(str::trim).filter(|candidate| !candidate.is_empty()) {
                index.by_name.entry(candidate.into()).or_default().push((hit.node, start + offset));
            }
        }
        index.last_node = checked;
        index.scanned = geometry.hits.len();
        let target_scope = self.anchor_scope_of(target, name);
        let candidates = index.by_name.get(name)?;
        let usable = |node: NodeId| node != target && self.anchor_scope_of(node, name) == target_scope && self.acceptable_anchor(node, target, geometry, 0);
        let at = candidates.iter().rev().find(|(node, _)| usable(*node)).map(|(_, at)| *at)?;
        geometry.hits.get(at).map(|hit| (hit.node, [hit.rect.x, hit.rect.y, hit.rect.width, hit.rect.height]))
    }

    fn pseudo_anchor_names(&self, origin: NodeId, pseudo: css::PseudoElement) -> Option<String> {
        let origin_style = self.computed_style_from_tree_shared(origin).ok()?;
        let generated = self
            .rules
            .compute_pseudo_cached(self.document, origin, &origin_style, pseudo, Some(self.text), &mut self.style_cache.borrow_mut())
            .ok()??;
        css::extra_props::computed(&generated.style, "anchor-name")
    }

    /// The implicit anchor of a popover: the element that invoked it.
    fn implicit_popover_anchor_box(&self, target: NodeId) -> Option<[f32; 4]> {
        let trigger = self.document.popover_trigger(target)?;
        let geometry = self.geometry.as_deref()?;
        let hit = geometry.hits.iter().rev().find(|hit| hit.node == trigger && hit.pseudo.is_none() && !hit.virtual_generated)?;
        Some([hit.rect.x, hit.rect.y, hit.rect.width, hit.rect.height])
    }

    /// The element generating the containing block of `node`; `None` for the
    /// initial containing block.
    fn anchor_containing_block(&self, node: NodeId) -> Option<NodeId> {
        let style = self.computed_style_from_tree_shared(node).ok()?;
        let mut current = self.document.composed_parent(node).ok().flatten();
        for _ in 0..4096 {
            let candidate = current?;
            let ancestor = self.computed_style_from_tree_shared(candidate).ok()?;
            let establishes = match style.position {
                Position::Absolute => establishes_absolute_containing_block(&ancestor),
                Position::Fixed => establishes_fixed_containing_block(&ancestor),
                _ => !matches!(ancestor.display, Display::Inline | Display::Contents),
            };
            if establishes {
                return Some(candidate);
            }
            current = self.document.composed_parent(candidate).ok().flatten();
        }
        None
    }

    /// css-anchor-position-1 §2.3: the anchor is laid out strictly before
    /// the target, directly or through the element generating its containing block.
    fn acceptable_anchor(&self, anchor: NodeId, target: NodeId, geometry: &LayoutGeometry, depth: usize) -> bool {
        let anchor_block = self.anchor_containing_block(anchor);
        if anchor_block == self.anchor_containing_block(target) {
            let positioned = self.computed_style_from_tree_shared(anchor).is_ok_and(|style| matches!(style.position, Position::Absolute | Position::Fixed));
            return !positioned || geometry.hits.iter().any(|hit| hit.node == anchor && hit.pseudo.is_none());
        }
        match anchor_block {
            Some(block) if depth < 64 => self.acceptable_anchor(block, target, geometry, depth + 1),
            _ => false,
        }
    }

    /// The nearest inclusive flat-tree ancestor whose `anchor-scope` covers
    /// `name`; an anchor is only visible to targets sharing its scope.
    fn anchor_scope_of(&self, node: NodeId, name: &str) -> Option<NodeId> {
        let mut current = Some(node);
        for _ in 0..4096 {
            let candidate = current?;
            if let Ok(style) = self.computed_style_from_tree_shared(candidate) {
                let scope = css::extra_props::computed(&style, "anchor-scope").unwrap_or_default();
                if scope == "all" || scope.split(',').any(|scoped| scoped.trim() == name) {
                    return Some(candidate);
                }
            }
            current = self.document.composed_parent(candidate).ok().flatten();
        }
        None
    }

    /// The containing block and style adjustments `position-area` implies for
    /// an absolutely positioned box: the selected tracks around the default
    /// anchor become the containing block, and `normal` self-alignment faces
    /// the anchor.
    pub(super) fn position_area_placement(&self, id: NodeId, style: &Style) -> Option<(Rect, Style)> {
        if !matches!(style.position, Position::Absolute | Position::Fixed) {
            return None;
        }
        let value = css::extra_props::computed(style, "position-area")?;
        if value == "none" {
            return None;
        }
        let fixed = style.position == Position::Fixed;
        let containing = (if fixed { self.fixed_containing_block } else { self.containing_block }).unwrap_or(self.viewport_rect());
        let container_node = if fixed { self.fixed_containing_block_node } else { self.containing_block_node };
        let scroll = if let Some(node) = container_node {
            self.scrolls.iter().find(|scroll| scroll.node == node).map_or((0.0, 0.0), |scroll| (scroll.x, scroll.y))
        } else if fixed {
            (0.0, 0.0)
        } else {
            self.root_scroll
        };
        let containing_sides = container_node
            .and_then(|node| self.computed_style_from_tree_shared(node).ok())
            .map_or([3, 1, 0, 2], |container| container.logical_sides());
        let anchor = match css::extra_props::computed(style, "position-anchor").filter(|name| name.starts_with("--")) {
            Some(name) => self.anchor_box(id, &name)?,
            None => self.implicit_popover_anchor_box(id)?,
        };
        let own_sides = style.logical_sides();
        let tracks = css::anchor::area_tracks(&value, &own_sides, &containing_sides)?;
        let origin = (containing.x - scroll.0, containing.y - scroll.1);
        let columns = [
            origin.0.min(anchor[0]),
            anchor[0],
            anchor[0] + anchor[2],
            (origin.0 + containing.width).max(anchor[0] + anchor[2]),
        ];
        let rows = [
            origin.1.min(anchor[1]),
            anchor[1],
            anchor[1] + anchor[3],
            (origin.1 + containing.height).max(anchor[1] + anchor[3]),
        ];
        let span = |edges: &[f32; 4], (low, high): (u8, u8)| {
            let start = edges[usize::from(low)];
            let end = edges[usize::from(high) + 1];
            (start, end.max(start))
        };
        let (left, right) = span(&columns, tracks.horizontal);
        let (top, bottom) = span(&rows, tracks.vertical);
        let area = Rect { x: left + scroll.0, y: top + scroll.1, width: right - left, height: bottom - top };
        let mut adjusted = style.clone();
        adjusted.top.get_or_insert(0.0);
        adjusted.right.get_or_insert(0.0);
        adjusted.bottom.get_or_insert(0.0);
        adjusted.left.get_or_insert(0.0);
        let container_style = container_node.and_then(|node| self.computed_style_from_tree_shared(node).ok());
        let container_style = container_style.as_deref();
        for horizontal in [true, false] {
            let range = if horizontal { tracks.horizontal } else { tracks.vertical };
            let normal = if horizontal {
                adjusted.justify_self.is_none() || adjusted.justify_self_keyword == Some(css::AlignmentKeyword::Normal)
            } else {
                adjusted.align_self.is_none() || adjusted.align_self_keyword == Some(css::AlignmentKeyword::Normal)
            };
            if !normal {
                continue;
            }
            let towards_far = match css::anchor::area_alignment(range) {
                AreaAlignment::Stretch => {
                    if horizontal {
                        adjusted.justify_self = Some(AlignItems::Center);
                        adjusted.justify_self_keyword = Some(css::AlignmentKeyword::AnchorCenter);
                    } else {
                        adjusted.align_self = Some(AlignItems::Center);
                        adjusted.align_self_keyword = Some(css::AlignmentKeyword::AnchorCenter);
                    }
                    continue;
                }
                AreaAlignment::Center => None,
                AreaAlignment::TowardsEnd => Some(true),
                AreaAlignment::TowardsStart => Some(false),
            };
            let alignment = match towards_far {
                None => AlignItems::Center,
                Some(far) => {
                    let start_far = container_style.map_or(false, |container| {
                        let sides = container.logical_sides();
                        let start = if (sides[0] == 1 || sides[0] == 3) == horizontal { sides[0] } else { sides[2] };
                        start == 1 || start == 2
                    });
                    if far == start_far { AlignItems::Start } else { AlignItems::End }
                }
            };
            if horizontal {
                adjusted.justify_self = Some(alignment);
                adjusted.justify_self_keyword = None;
            } else {
                adjusted.align_self = Some(alignment);
                adjusted.align_self_keyword = None;
            }
        }
        Some((area, adjusted))
    }

    /// Records the insets that were `auto` and resolved to zero because the
    /// box is placed against its anchor, keeping the insets `anchor()` set.
    pub(super) fn record_filled_insets(&mut self, id: NodeId, filled: [bool; 4]) -> Result<(), LayoutError> {
        if !filled.iter().any(|edge| *edge) {
            return Ok(());
        }
        let mut insets = self.geometry.as_deref().and_then(|geometry| geometry.used_insets_for(id)).unwrap_or([None; 4]);
        for (edge, fill) in filled.iter().enumerate() {
            if *fill && insets[edge].is_none() {
                insets[edge] = Some(0.0);
            }
        }
        self.record_used_insets(id, insets)
    }

    /// Records, for every inset that is `auto` after a fallback was applied,
    /// the distance the laid out box has from the containing block edge.
    pub(super) fn record_fallback_insets(&mut self, id: NodeId, style: &Style) -> Result<(), LayoutError> {
        let fixed = style.position == Position::Fixed;
        let containing = (if fixed { self.fixed_containing_block } else { self.containing_block }).unwrap_or(self.viewport_rect());
        let container_node = if fixed { self.fixed_containing_block_node } else { self.containing_block_node };
        let scroll = if let Some(node) = container_node {
            self.scrolls.iter().find(|scroll| scroll.node == node).map_or((0.0, 0.0), |scroll| (scroll.x, scroll.y))
        } else if fixed {
            (0.0, 0.0)
        } else {
            self.root_scroll
        };
        let Some(rect) = self
            .geometry
            .as_deref()
            .and_then(|geometry| geometry.hits.iter().rev().find(|hit| hit.node == id && hit.pseudo.is_none() && !hit.virtual_generated))
            .map(|hit| hit.rect)
        else {
            return Ok(());
        };
        let margins = self.geometry.as_deref().and_then(|geometry| geometry.used_margins_for(id)).unwrap_or(style.margin_sides);
        let (left, top) = (containing.x - scroll.0, containing.y - scroll.1);
        let measured = [
            rect.y - margins[0] - top,
            left + containing.width - (rect.x + rect.width + margins[1]),
            top + containing.height - (rect.y + rect.height + margins[2]),
            rect.x - margins[3] - left,
        ];
        let mut insets = self.geometry.as_deref().and_then(|geometry| geometry.used_insets_for(id)).unwrap_or([None; 4]);
        let authored = [style.top, style.right, style.bottom, style.left];
        let mut changed = false;
        for edge in 0..4 {
            if insets[edge].is_none() && authored[edge].is_none() {
                insets[edge] = Some(measured[edge]);
                changed = true;
            }
        }
        if changed {
            self.record_used_insets(id, insets)?;
        }
        Ok(())
    }

    /// The default anchor box of `id`: the `position-anchor` name when set,
    /// otherwise the popover invoker.
    pub(super) fn default_anchor_box(&self, id: NodeId, style: &Style) -> Option<[f32; 4]> {
        self.default_anchor_hit(id, style).map(|(_, rect)| rect)
    }

    fn default_anchor_hit(&self, id: NodeId, style: &Style) -> Option<(NodeId, [f32; 4])> {
        match css::extra_props::computed(style, "position-anchor").filter(|name| name.starts_with("--")) {
            Some(name) => self.anchor_hit(id, &name),
            None => {
                let trigger = self.document.popover_trigger(id)?;
                self.implicit_popover_anchor_box(id).map(|rect| (trigger, rect))
            }
        }
    }

    /// Whether `position-visibility` hides the box: it overflows its
    /// containing block, its default anchor is missing while it is used, or
    /// the anchor is clipped away or invisible.
    pub(super) fn position_visibility_hidden(&self, id: NodeId, style: &Style, depth: usize) -> bool {
        if !matches!(style.position, Position::Absolute | Position::Fixed) {
            return false;
        }
        let value = css::extra_props::computed(style, "position-visibility").unwrap_or_default();
        if value == "always" {
            return false;
        }
        let has = |keyword: &str| value.split_ascii_whitespace().any(|token| token == keyword);
        if has("no-overflow") && !self.fits_containing_block(id, style, depth) {
            return true;
        }
        let named = css::extra_props::computed(style, "position-anchor").is_some_and(|name| name.starts_with("--"));
        if has("anchor-valid") && self.uses_default_anchor(style) && self.default_anchor_hit(id, style).is_none() {
            return true;
        }
        if (value.is_empty() || has("anchor-visible")) && named {
            if let Some((anchor, rect)) = self.default_anchor_hit(id, style) {
                return self.anchor_hidden(anchor, rect, style);
            }
        }
        false
    }

    /// Whether the style queries its default anchor: an `anchor()` or
    /// `anchor-size()` without a name, `position-area`, or `anchor-center`.
    fn uses_default_anchor(&self, style: &Style) -> bool {
        if css::extra_props::computed(style, "position-area").is_some_and(|area| area != "none") {
            return true;
        }
        let centered = |keyword: Option<css::AlignmentKeyword>| keyword == Some(css::AlignmentKeyword::AnchorCenter);
        if centered(style.justify_self_keyword) || centered(style.align_self_keyword) {
            return true;
        }
        style.anchored.as_deref().is_some_and(|entries| entries.iter().any(|(_, entry)| calls_without_name(entry)))
    }

    /// Whether the anchor is invisible, or scrolled out of an intervening
    /// clipping box between it and the containing block of the target.
    fn anchor_hidden(&self, anchor: NodeId, rect: [f32; 4], target: &Style) -> bool {
        if self.computed_style_from_tree_shared(anchor).is_ok_and(|style| !style.visibility_visible) {
            return true;
        }
        let Some(geometry) = self.geometry.as_deref() else {
            return false;
        };
        let boundary = if target.position == Position::Fixed { self.fixed_containing_block_node } else { self.containing_block_node };
        let (mut left, mut top, mut right, mut bottom) = (rect[0], rect[1], rect[0] + rect[2], rect[1] + rect[3]);
        let empty = |low: f32, high: f32, extent: f32| high < low || (high == low && extent > 0.0);
        let mut current = self.document.composed_parent(anchor).ok().flatten();
        for _ in 0..4096 {
            let Some(node) = current else { break };
            if Some(node) == boundary {
                break;
            }
            if let Ok(style) = self.computed_style_from_tree_shared(node) {
                if style.overflow_x.clips() || style.overflow_y.clips() {
                    if let Some(hit) = geometry.hits.iter().rev().find(|hit| hit.node == node && hit.pseudo.is_none() && !hit.virtual_generated) {
                        let borders = style.used_border_widths();
                        if style.overflow_x.clips() {
                            let (low, high) = (hit.rect.x + borders[3], hit.rect.x + hit.rect.width - borders[1]);
                            let (new_left, new_right) = (left.max(low), right.min(high));
                            if empty(new_left, new_right, right - left) {
                                return true;
                            }
                            (left, right) = (new_left, new_right);
                        }
                        if style.overflow_y.clips() {
                            let (low, high) = (hit.rect.y + borders[0], hit.rect.y + hit.rect.height - borders[2]);
                            let (new_top, new_bottom) = (top.max(low), bottom.min(high));
                            if empty(new_top, new_bottom, bottom - top) {
                                return true;
                            }
                            (top, bottom) = (new_top, new_bottom);
                        }
                    }
                }
            }
            current = self.document.composed_parent(node).ok().flatten();
        }
        false
    }

    fn containing_sides(&self, style: &Style) -> [usize; 4] {
        let fixed = style.position == Position::Fixed;
        let container_node = if fixed { self.fixed_containing_block_node } else { self.containing_block_node };
        container_node
            .and_then(|node| self.computed_style_from_tree_shared(node).ok())
            .map_or([3, 1, 0, 2], |container| container.logical_sides())
    }

    /// Whether the margin box of the box fits its inset-modified containing
    /// block, or the `position-area` it is placed in.
    fn fits_containing_block(&self, id: NodeId, style: &Style, depth: usize) -> bool {
        self.containing_block_fit(id, style, depth).0
    }

    /// Whether the box fits, with the width and height of its inset-modified
    /// containing block.
    fn containing_block_fit(&self, id: NodeId, style: &Style, depth: usize) -> (bool, [f32; 2]) {
        let fixed = style.position == Position::Fixed;
        let base = (if fixed { self.fixed_containing_block } else { self.containing_block }).unwrap_or(self.viewport_rect());
        let (cb, adjusted) = self.position_area_placement(id, style).unwrap_or((base, style.clone()));
        let resolved = self.resolve_anchored_style(id, adjusted, Some(cb));
        let resolved = self.resolve_style_percentages(&resolved, cb.width, Some(cb.height));
        let available = [
            cb.width - resolved.left.unwrap_or(0.0) - resolved.right.unwrap_or(0.0),
            cb.height - resolved.top.unwrap_or(0.0) - resolved.bottom.unwrap_or(0.0),
        ];
        let width = match (resolved.width, resolved.left.is_some() && resolved.right.is_some()) {
            (Some(width), _) => content_dimension(&resolved, width) + box_edges(&resolved, true),
            (None, true) => box_edges(&resolved, true),
            (None, false) => match self.box_source_intrinsic_size(id, &resolved, depth, true, None) {
                Ok(size) => size.0,
                Err(_) => return (true, available),
            },
        };
        let height = match (resolved.height, resolved.top.is_some() && resolved.bottom.is_some()) {
            (Some(height), _) => content_height_dimension(&resolved, height) + box_edges(&resolved, false),
            (None, true) => box_edges(&resolved, false),
            (None, false) => match self.box_source_intrinsic_size(id, &resolved, depth, false, None) {
                Ok(size) => size.1,
                Err(_) => return (true, available),
            },
        };
        (width <= available[0] + 0.01 && height <= available[1] + 0.01, available)
    }

    fn fallback_candidate(&self, style: &Style, fallback: &css::anchor::Fallback) -> Style {
        let mut candidate = style.clone();
        if let Some(name) = &fallback.rule {
            if let Some(rule) = self.rules.position_try.iter().rev().find(|rule| &*rule.name == name.as_str()) {
                css::anchor::apply_block(&mut candidate, &rule.declarations);
            }
        }
        if let Some(area) = &fallback.area {
            css::anchor::apply_resolved(&mut candidate, "position-area", area);
        }
        let containing_sides = self.containing_sides(style);
        for tactic in &fallback.tactics {
            let own = candidate.logical_sides();
            let permutation = css::anchor::tactic_permutation(*tactic, &own);
            if let Some(value) = css::extra_props::computed(&candidate, "position-area").filter(|value| value != "none") {
                if let Some(tracks) = css::anchor::area_tracks(&value, &own, &containing_sides) {
                    let flipped = css::anchor::permute_tracks(&tracks, &permutation);
                    css::anchor::apply_resolved(&mut candidate, "position-area", &css::anchor::tracks_to_area(&flipped));
                }
            }
            css::anchor::flip_style(&mut candidate, &permutation);
        }
        candidate
    }

    /// The style of the first `position-try-fallbacks` candidate whose box
    /// fits, when the box as authored overflows. When nothing fits, the
    /// candidate that last fit stays in use.
    pub(super) fn position_fallback_style(&self, id: NodeId, style: &Style, depth: usize) -> Option<Style> {
        if !matches!(style.position, Position::Absolute | Position::Fixed) {
            return None;
        }
        let computed = css::extra_props::computed(style, "position-try-fallbacks").filter(|value| value != "none")?;
        let fallbacks = css::anchor::parse_fallbacks(&computed);
        if fallbacks.is_empty() {
            return None;
        }
        if self.fits_containing_block(id, style, depth) {
            self.last_successful.borrow_mut().pending.insert(id.key(), None);
            return None;
        }
        let order = css::extra_props::computed(style, "position-try-order").unwrap_or_default();
        let mut signature = alloc::format!("{computed}|{order}");
        for fallback in &fallbacks {
            let rule = fallback.rule.as_ref().and_then(|name| self.rules.position_try.iter().rev().find(|rule| &*rule.name == name.as_str()));
            if let Some(rule) = rule {
                signature.push('|');
                signature.push_str(&rule.declarations);
            }
        }
        let remember = |index: usize| {
            self.last_successful
                .borrow_mut()
                .pending
                .insert(id.key(), Some((Arc::from(signature.as_str()), index)));
        };
        let chosen = if order == "normal" || order.is_empty() {
            fallbacks.iter().enumerate().find_map(|(index, fallback)| {
                let candidate = self.fallback_candidate(style, fallback);
                self.fits_containing_block(id, &candidate, depth).then_some((index, candidate))
            })
        } else {
            let inline_horizontal = matches!(style.logical_sides()[0], 1 | 3);
            let horizontal = match order.as_str() {
                "most-width" => true,
                "most-height" => false,
                "most-inline-size" => inline_horizontal,
                _ => !inline_horizontal,
            };
            let mut best: Option<(f32, usize, Style)> = None;
            for (index, fallback) in fallbacks.iter().enumerate() {
                let candidate = self.fallback_candidate(style, fallback);
                let (fits, available) = self.containing_block_fit(id, &candidate, depth);
                if !fits {
                    continue;
                }
                let space = available[usize::from(!horizontal)];
                if best.as_ref().map_or(true, |(most, _, _)| space > *most + 0.01) {
                    best = Some((space, index, candidate));
                }
            }
            best.map(|(_, index, candidate)| (index, candidate))
        };
        if let Some((index, candidate)) = chosen {
            remember(index);
            return Some(candidate);
        }
        let kept = self
            .last_successful
            .borrow()
            .committed
            .get(&id.key())
            .filter(|(kept, _)| **kept == *signature)
            .cloned();
        let Some((_, kept)) = kept else {
            self.last_successful.borrow_mut().pending.insert(id.key(), None);
            return None;
        };
        self.last_successful
            .borrow_mut()
            .pending
            .insert(id.key(), Some((Arc::from(signature.as_str()), kept)));
        fallbacks.get(kept).map(|fallback| self.fallback_candidate(style, fallback))
    }
}
