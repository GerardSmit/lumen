//! Last remembered sizes (CSS Sizing 4): the content-box size of an element
//! with `contain-intrinsic-*: auto`, recorded at each rendering update while
//! its contents are not skipped, and used in place of the fallback size
//! while they are.
use crate::{css, session::RenderSession, Document, NodeId};
use alloc::collections::{BTreeMap, BTreeSet};

const MAX_ENTRIES: usize = 65536;

#[derive(Default)]
pub(crate) struct RememberedSizes {
    sizes: BTreeMap<u128, (f32, f32)>,
    interest: BTreeSet<u128>,
}

impl Document {
    pub(crate) fn remembered_intrinsic_size(&self, node: NodeId) -> Option<(f32, f32)> {
        let store = self.remembered_sizes.borrow();
        if store.sizes.is_empty() {
            return None;
        }
        store.sizes.get(&node.key()).copied()
    }

    /// Layout calls this for each element whose size a rendering update may
    /// have to record. A box without `auto` forgets its size at once.
    pub(crate) fn note_remembered_size_interest(&self, node: NodeId, style: &css::Style) {
        if style.has_auto_intrinsic_size() {
            let mut store = self.remembered_sizes.borrow_mut();
            if store.interest.len() < MAX_ENTRIES {
                store.interest.insert(node.key());
            }
        } else if style.display != css::Display::None {
            let remembers = !self.remembered_sizes.borrow().sizes.is_empty();
            if remembers {
                self.remembered_sizes.borrow_mut().sizes.remove(&node.key());
            }
        }
    }
}

impl RenderSession {
    /// Records the current layout's content-box sizes. Hosts call this once
    /// per rendering update, after the frame was laid out.
    pub fn update_remembered_sizes(&mut self) {
        let interest = {
            let mut store = self.document().remembered_sizes.borrow_mut();
            core::mem::take(&mut store.interest)
        };
        for key in interest {
            let node = NodeId::from_key(key);
            if self.document().kind(node).is_err() {
                self.document().remembered_sizes.borrow_mut().sizes.remove(&key);
                continue;
            }
            let Ok(style) = self.used_box_style(node) else {
                continue;
            };
            if style.display == css::Display::None {
                continue;
            }
            if !style.has_auto_intrinsic_size() {
                self.document().remembered_sizes.borrow_mut().sizes.remove(&key);
                continue;
            }
            if style.skips_contents() {
                continue;
            }
            let Some(rect) = self.layout_rect(node) else {
                continue;
            };
            let border = style.used_border_widths();
            let padding = style.padding_sides();
            let width = (rect.width - border[1] - border[3] - padding[1] - padding[3]).max(0.0);
            let height = (rect.height - border[0] - border[2] - padding[0] - padding[2]).max(0.0);
            let logical = style.logical_axis_pair((width, height));
            let mut store = self.document().remembered_sizes.borrow_mut();
            if store.sizes.len() < MAX_ENTRIES || store.sizes.contains_key(&key) {
                store.sizes.insert(key, logical);
            }
        }
    }
}
