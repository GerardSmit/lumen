//! Snapping of programmatic scroll destinations to the snap positions of a
//! scroll container (CSS Scroll Snap 1 §6.1).
use super::*;
use crate::scrolling::{scroll_edge_lengths, target_bounds_in_scroll_space};
use lumen_common::scroll::{alignment_delta, Alignment};
use lumen_html::css::{Direction, WritingMode};

const MAX_VISITED: usize = 50_000;
const PROXIMITY_RATIO: f32 = 1.0 / 3.0;

struct Axes {
    snap_x: bool,
    snap_y: bool,
    mandatory: bool,
}

fn parse_type(value: &str, vertical: bool) -> Option<Axes> {
    let mut words = value.split_ascii_whitespace();
    let axis = words.next()?;
    let mandatory = words.next().is_some_and(|word| word.eq_ignore_ascii_case("mandatory"));
    let (snap_x, snap_y) = match axis.to_ascii_lowercase().as_str() {
        "x" => (true, false),
        "y" => (false, true),
        "both" => (true, true),
        "inline" => (!vertical, vertical),
        "block" => (vertical, !vertical),
        _ => return None,
    };
    Some(Axes { snap_x, snap_y, mandatory })
}

fn parse_align(word: &str) -> Option<Alignment> {
    match word.to_ascii_lowercase().as_str() {
        "start" => Some(Alignment::Start),
        "center" => Some(Alignment::Center),
        "end" => Some(Alignment::End),
        _ => None,
    }
}

/// The physical (horizontal, vertical) alignments of a `scroll-snap-align`
/// value, whose first word is the block axis and second the inline axis.
fn physical_alignments(value: &str, vertical: bool) -> (Option<Alignment>, Option<Alignment>) {
    let mut words = value.split_ascii_whitespace();
    let block = words.next().and_then(parse_align);
    let inline = match words.next() {
        Some(word) => parse_align(word),
        None => block,
    };
    if vertical { (block, inline) } else { (inline, block) }
}

/// The scroll container that `candidate`'s snap area belongs to: the nearest
/// scroll container along its containing-block chain, or the viewport.
fn snap_container(
    realm: &Rc<DomRealm>,
    candidate: NodeId,
    root: NodeId,
    scrolling: Option<NodeId>,
    cache: &mut crate::scroll_parent::StepCache,
) -> OpResult<Option<NodeId>> {
    Ok(crate::scroll_parent::scroll_parent_cached(realm, candidate, scrolling, cache)?
        .map(|id| if Some(id) == scrolling { root } else { id }))
}

struct Interval {
    low: f32,
    high: f32,
}

fn nearest(intervals: &[Interval], target: f32) -> Option<(f32, f32)> {
    let mut best: Option<(f32, f32)> = None;
    for interval in intervals {
        let position = target.clamp(interval.low, interval.high);
        let distance = (position - target).abs();
        if best.is_none_or(|(_, current)| distance < current) {
            best = Some((position, distance));
        }
    }
    best
}

/// Snap `to` (already clamped to `limits`: min x, max x, min y, max y) for a
/// scroll of `node` from `from`. Returns `to` unchanged when the container
/// does not snap or has no snap area on an axis.
pub(crate) fn snap_destination(
    realm: &Rc<DomRealm>,
    node: NodeId,
    _from: (f32, f32),
    to: (f32, f32),
    limits: (f32, f32, f32, f32),
) -> OpResult<(f32, f32)> {
    realm.flush_layout()?;
    let scrolling = scrolling::document_scrolling_element(realm)?;
    let (root, document_element) = {
        let session = realm.session.borrow();
        let root = session.document().root();
        (root, selector::document_element(session.document()))
    };
    let style_node = if node == root { document_element.unwrap_or(node) } else { node };
    let (writing_mode, direction) = {
        let mut session = realm.session.borrow_mut();
        let style = session.computed_style(style_node).map_err(|error| {
            OpError::new("InvalidStateError", format!("scroll style failed: {error:?}"))
        })?;
        (style.writing_mode, style.direction)
    };
    let vertical = writing_mode != WritingMode::HorizontalTb;
    let snap_type = crate::style::computed_property_value(realm, style_node, "scroll-snap-type")?;
    let from = realm.session.borrow().scroll_offset(node);
    let Some(axes) = parse_type(&snap_type, vertical) else { return Ok(to) };
    let Some((port, inverse)) = realm.session.borrow().scrollport_coordinate_space(node) else { return Ok(to) };
    let padding = scroll_edge_lengths(realm, style_node, "scroll-padding", (port.width, port.height))?;
    let snapport = lumen_html::paint::Rect {
        x: port.x + padding[3],
        y: port.y + padding[0],
        width: (port.width - padding[1] - padding[3]).max(0.0),
        height: (port.height - padding[0] - padding[2]).max(0.0),
    };
    let rtl = direction == Direction::Rtl;
    let (x_reverse, y_reverse) = match writing_mode {
        WritingMode::HorizontalTb => (rtl, false),
        WritingMode::VerticalRl | WritingMode::SidewaysRl => (true, rtl),
        WritingMode::VerticalLr => (false, rtl),
        WritingMode::SidewaysLr => (false, !rtl),
    };

    let mut x_positions: Vec<Interval> = Vec::new();
    let mut y_positions: Vec<Interval> = Vec::new();
    let mut pending: Vec<NodeId> = {
        let session = realm.session.borrow();
        let mut children = session.document().composed_children(node).map_err(dom_error)?;
        children.reverse();
        children
    };
    let mut visited = 0usize;
    let mut step_cache = crate::scroll_parent::StepCache::default();
    while let Some(candidate) = pending.pop() {
        visited += 1;
        if visited > MAX_VISITED {
            break;
        }
        let is_element = matches!(realm.session.borrow().document().kind(candidate), Ok(NodeKind::Element { .. }));
        if !is_element {
            continue;
        }
        {
            let session = realm.session.borrow();
            let mut children = session.document().composed_children(candidate).map_err(dom_error)?;
            children.reverse();
            pending.extend(children);
        }
        let align_value = {
            let mut session = realm.session.borrow_mut();
            let style = session.computed_style(candidate).map_err(|error| {
                OpError::new("InvalidStateError", format!("scroll style failed: {error:?}"))
            })?;
            style
                .computed_css_value("scroll-snap-align", lumen_html::css::computed_values::ComputedValueContext::default())
                .unwrap_or_default()
        };
        let (x_align, y_align) = physical_alignments(&align_value, vertical);
        if x_align.is_none() && y_align.is_none() {
            continue;
        }
        if (x_align.is_none() || !axes.snap_x) && (y_align.is_none() || !axes.snap_y) {
            continue;
        }
        if snap_container(realm, candidate, root, scrolling, &mut step_cache)? != Some(node) {
            continue;
        }
        let Some(bounds) = target_bounds_in_scroll_space(realm, candidate, realm, inverse, &[])? else { continue };
        let margin = scroll_edge_lengths(realm, candidate, "scroll-margin", (0.0, 0.0))?;
        let area = lumen_html::paint::Rect {
            x: bounds.x - margin[3],
            y: bounds.y - margin[0],
            width: bounds.width + margin[1] + margin[3],
            height: bounds.height + margin[0] + margin[2],
        };
        if let (true, Some(align)) = (axes.snap_x, x_align) {
            if let Some(interval) = interval_for(area.x, area.width, snapport.x, snapport.width, align, x_reverse, from.0, limits.0, limits.1) {
                x_positions.push(interval);
            }
        }
        if let (true, Some(align)) = (axes.snap_y, y_align) {
            if let Some(interval) = interval_for(area.y, area.height, snapport.y, snapport.height, align, y_reverse, from.1, limits.2, limits.3) {
                y_positions.push(interval);
            }
        }
    }

    let choose = |intervals: &[Interval], target: f32, extent: f32, lower: f32, upper: f32| -> f32 {
        match nearest(intervals, target) {
            Some((position, distance)) if axes.mandatory || distance <= extent * PROXIMITY_RATIO => position.clamp(lower, upper),
            _ => target,
        }
    };
    Ok((
        choose(&x_positions, to.0, snapport.width, limits.0, limits.1),
        choose(&y_positions, to.1, snapport.height, limits.2, limits.3),
    ))
}

/// Scroll positions at which an area aligns with the snapport on one axis. An
/// area larger than the snapport covers it over a whole range of positions.
fn interval_for(
    area_start: f32,
    area_size: f32,
    port_start: f32,
    port_size: f32,
    align: Alignment,
    reverse: bool,
    origin: f32,
    lower: f32,
    upper: f32,
) -> Option<Interval> {
    let area_end = area_start + area_size;
    let port_end = port_start + port_size;
    if area_size > port_size {
        let covering_start = origin + (area_start - port_start);
        let covering_end = origin + (area_end - port_end);
        let (low, high) = if covering_start <= covering_end { (covering_start, covering_end) } else { (covering_end, covering_start) };
        return Some(Interval { low: low.clamp(lower, upper), high: high.clamp(lower, upper) });
    }
    let delta = alignment_delta(
        f64::from(area_start),
        f64::from(area_end),
        f64::from(port_start),
        f64::from(port_end),
        align,
        reverse,
    ) as f32;
    let position = (origin + delta).clamp(lower, upper);
    Some(Interval { low: position, high: position })
}
