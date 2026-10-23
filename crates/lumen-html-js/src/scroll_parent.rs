//! `HTMLElement.scrollParent`: the nearest scroll container along the containing-block chain.
use super::*;
use lumen_html::css::{Display, Position};

#[derive(Clone, Copy)]
struct Step {
    position: Position,
    display: Display,
    forms_fixed_containing_block: bool,
    scroll_container: bool,
}

impl Step {
    fn of(style: &lumen_html::css::Style) -> Self {
        let contained = (style.contain_layout || style.contain_paint) && style.display != Display::Contents;
        Self {
            position: style.position,
            display: style.display,
            forms_fixed_containing_block: style.has_transform() || contained,
            scroll_container: style.display != Display::Contents
                && (style.overflow_x.scroll_container() || style.overflow_y.scroll_container()),
        }
    }

    fn forms_containing_block(&self, fixed: bool) -> bool {
        self.forms_fixed_containing_block || (!fixed && self.position != Position::Static)
    }
}

/// Computed-style summaries of the elements visited by one batch of
/// `scroll_parent` queries.
#[derive(Default)]
pub(crate) struct StepCache(std::collections::HashMap<NodeId, Option<Step>>);

pub(crate) fn scroll_parent(realm: &Rc<DomRealm>, node: NodeId) -> OpResult<Option<NodeId>> {
    realm.flush_layout()?;
    let scrolling = scrolling::document_scrolling_element(realm)?;
    scroll_parent_cached(realm, node, scrolling, &mut StepCache::default())
}

/// `scroll_parent` for a fresh layout and a known scrolling element.
pub(crate) fn scroll_parent_cached(
    realm: &Rc<DomRealm>,
    node: NodeId,
    scrolling: Option<NodeId>,
    cache: &mut StepCache,
) -> OpResult<Option<NodeId>> {
    let mut session = realm.session.borrow_mut();
    if session.client_fragment_boxes(node).is_empty() || Some(node) == scrolling {
        return Ok(None);
    }
    let root = lumen_html::selector::document_element(session.document());
    if Some(node) == root {
        return Ok(None);
    }
    let mut style_of = |id: NodeId, session: &mut RenderSession| -> Option<Step> {
        *cache.0.entry(id).or_insert_with(|| session.computed_style(id).ok().map(|style| Step::of(&style)))
    };
    let Some(start) = style_of(node, &mut session) else { return Ok(None) };
    let mut position = start.position;
    let mut current = node;
    for _ in 0..4096 {
        let fixed = position == Position::Fixed;
        let absolute = position == Position::Absolute;
        let mut candidate = session.document().flat_tree_parent(current).map_err(dom_error)?;
        let mut found = None;
        while let Some(id) = candidate {
            if !matches!(session.document().kind(id), Ok(NodeKind::Element { .. })) {
                candidate = session.document().flat_tree_parent(id).map_err(dom_error)?;
                continue;
            }
            if Some(id) == root || Some(id) == scrolling {
                found = Some((id, None));
                break;
            }
            let Some(step) = style_of(id, &mut session) else { return Ok(None) };
            if step.display == Display::Contents || step.display == Display::None {
                candidate = session.document().flat_tree_parent(id).map_err(dom_error)?;
                continue;
            }
            if (fixed || absolute) && !step.forms_containing_block(fixed) {
                candidate = session.document().flat_tree_parent(id).map_err(dom_error)?;
                continue;
            }
            found = Some((id, Some(step)));
            break;
        }
        let Some((id, step)) = found else {
            return Ok(if fixed { None } else { scrolling });
        };
        let Some(step) = step else {
            return Ok(if fixed { None } else { scrolling });
        };
        if step.scroll_container {
            return Ok(Some(id));
        }
        position = step.position;
        current = id;
    }
    Ok(None)
}
