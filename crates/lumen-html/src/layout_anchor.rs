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

/// The insets `anchor()` resolution replaced, as top, right, bottom, left.
pub(super) fn changed_insets(original: &Style, resolved: &Style) -> Option<[Option<f32>; 4]> {
    let before = [original.top, original.right, original.bottom, original.left];
    let after = [resolved.top, resolved.right, resolved.bottom, resolved.left];
    let changed: [Option<f32>; 4] = core::array::from_fn(|edge| if before[edge] != after[edge] { after[edge] } else { None });
    changed.iter().any(Option::is_some).then_some(changed)
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
        let geometry = self.geometry.as_deref()?;
        let mut index = self.anchor_index.borrow_mut();
        if geometry.hits.len() < index.scanned {
            *index = AnchorIndex::default();
        }
        let start = index.scanned;
        let mut checked: Option<NodeId> = index.last_node;
        for (offset, hit) in geometry.hits[start..].iter().enumerate() {
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
        let mut ancestor = self.document.composed_parent(target).ok().flatten();
        let mut chosen = None;
        for _ in 0..4096 {
            let Some(node) = ancestor else { break };
            if let Some((_, at)) = candidates.iter().rev().find(|(candidate, _)| *candidate == node) {
                if usable(node) {
                    chosen = Some(*at);
                    break;
                }
            }
            ancestor = self.document.composed_parent(node).ok().flatten();
        }
        let at = chosen.or_else(|| candidates.iter().rev().find(|(node, _)| usable(*node)).map(|(_, at)| *at))?;
        geometry.hits.get(at).map(|hit| [hit.rect.x, hit.rect.y, hit.rect.width, hit.rect.height])
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
        let columns = [origin.0, anchor[0], anchor[0] + anchor[2], origin.0 + containing.width];
        let rows = [origin.1, anchor[1], anchor[1] + anchor[3], origin.1 + containing.height];
        let span = |edges: &[f32; 4], (low, high): (u8, u8)| {
            let start = edges[usize::from(low)].clamp(edges[0], edges[3]);
            let end = edges[usize::from(high) + 1].clamp(edges[0], edges[3]);
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
                AreaAlignment::Stretch => continue,
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
    /// fits, when the box as authored overflows.
    pub(super) fn position_fallback_style(&self, id: NodeId, style: &Style, depth: usize) -> Option<Style> {
        if !matches!(style.position, Position::Absolute | Position::Fixed) {
            return None;
        }
        let computed = css::extra_props::computed(style, "position-try-fallbacks").filter(|value| value != "none")?;
        let fallbacks = css::anchor::parse_fallbacks(&computed);
        if fallbacks.is_empty() || self.fits_containing_block(id, style, depth) {
            return None;
        }
        let order = css::extra_props::computed(style, "position-try-order").unwrap_or_default();
        let candidates = fallbacks.iter().map(|fallback| self.fallback_candidate(style, fallback));
        if order == "normal" || order.is_empty() {
            return candidates.into_iter().find(|candidate| self.fits_containing_block(id, candidate, depth));
        }
        let inline_horizontal = matches!(style.logical_sides()[0], 1 | 3);
        let horizontal = match order.as_str() {
            "most-width" => true,
            "most-height" => false,
            "most-inline-size" => inline_horizontal,
            _ => !inline_horizontal,
        };
        let mut best: Option<(f32, Style)> = None;
        for candidate in candidates {
            let (fits, available) = self.containing_block_fit(id, &candidate, depth);
            if !fits {
                continue;
            }
            let space = available[usize::from(!horizontal)];
            if best.as_ref().map_or(true, |(most, _)| space > *most + 0.01) {
                best = Some((space, candidate));
            }
        }
        best.map(|(_, candidate)| candidate)
    }
}
