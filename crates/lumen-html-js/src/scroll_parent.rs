//! `HTMLElement.scrollParent`: the nearest scroll container along the containing-block chain.
use super::*;
use lumen_html::css::{Display, Position};

struct Step {
    style: lumen_html::css::Style,
}

impl Step {
    fn forms_containing_block(&self, fixed: bool) -> bool {
        let style = &self.style;
        let contained = (style.contain_layout || style.contain_paint) && style.display != Display::Contents;
        style.has_transform() || contained || (!fixed && style.position != Position::Static)
    }

    fn scroll_container(&self) -> bool {
        self.style.display != Display::Contents
            && (self.style.overflow_x.scroll_container() || self.style.overflow_y.scroll_container())
    }
}

pub(crate) fn scroll_parent(realm: &Rc<DomRealm>, node: NodeId) -> OpResult<Option<NodeId>> {
    realm.flush_layout()?;
    let scrolling = scrolling::document_scrolling_element(realm)?;
    let mut session = realm.session.borrow_mut();
    if session.client_fragment_boxes(node).is_empty() || Some(node) == scrolling {
        return Ok(None);
    }
    let root = lumen_html::selector::document_element(session.document());
    if Some(node) == root {
        return Ok(None);
    }
    let style_of = |id: NodeId, session: &mut RenderSession| -> Option<Step> {
        session.computed_style(id).ok().map(|style| Step { style })
    };
    let Some(start) = style_of(node, &mut session) else { return Ok(None) };
    let mut position = start.style.position;
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
            if step.style.display == Display::Contents || step.style.display == Display::None {
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
        if step.scroll_container() {
            return Ok(Some(id));
        }
        position = step.style.position;
        current = id;
    }
    Ok(None)
}
