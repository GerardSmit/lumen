//! Language-neutral custom highlight paint inputs. Range endpoints remain DOM
//! UTF-16 boundaries; hosts refresh snapshots before rendering opportunities.
use alloc::{string::String, vec::Vec};
use crate::{Document, NodeId, ranges::{self, Boundary}};
use core::cmp::Ordering;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HighlightRange { pub start: Boundary, pub end: Boundary }
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Highlight { pub name: String, pub priority: i32, pub order: usize, pub ranges: Vec<HighlightRange> }

impl HighlightRange {
    /// Intersection with a text node, using the shared DOM boundary ordering.
    /// Disconnected, reversed and invalid static ranges do not paint.
    pub fn text_offsets(&self, document: &Document, node: NodeId) -> Option<(usize, usize)> {
        let length = ranges::length(document, node).ok()?;
        if self.start.offset > ranges::length(document, self.start.container).ok()?
            || self.end.offset > ranges::length(document, self.end.container).ok()?
            || ranges::compare(document, self.start, self.end).ok()? != Some(Ordering::Less)
            || ranges::compare(document, Boundary { container: node, offset: length }, self.start).ok()? != Some(Ordering::Greater)
            || ranges::compare(document, Boundary { container: node, offset: 0 }, self.end).ok()? != Some(Ordering::Less) { return None; }
        let start = if self.start.container == node { self.start.offset } else { 0 };
        let end = if self.end.container == node { self.end.offset } else { length };
        (end > start).then_some((start, end))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{html, selector, session::RenderSession, paint::*};
    use alloc::vec;
    struct Text;
    impl TextShaper for Text {
        fn shape(&self, text: &str, _: f32) -> Result<ShapedRun, ()> {
            Ok(ShapedRun { width: text.chars().count() as f32 * 10.0,
                glyphs: text.char_indices().enumerate().map(|(position, (cluster, ch))| Glyph {
                    id: ch as u16, face: 0, cluster: cluster as u32, caps_expansion: 0,
                    x: position as f32 * 10.0, y: 0.0, size_scale: 1.0,
                }).collect::<Vec<_>>().into() })
        }
        fn shape_resolved_with_cluster_advances(&self, text: &str, size: f32, _: bool, _: &FontSpec) -> Result<Option<ShapedRunWithClusterAdvances>, ()> {
            Ok(Some(ShapedRunWithClusterAdvances { run: self.shape(text, size)?,
                clusters: text.char_indices().map(|(start, ch)| ShapedClusterAdvance { source: start..start + ch.len_utf8(), advance: 10.0 }).collect::<Vec<_>>().into() }))
        }
        fn ascent(&self, _: f32) -> f32 { 10.0 }
        fn line_height(&self, _: f32) -> f32 { 20.0 }
    }
    #[test]
    fn custom_highlight_named_cascade_clips_original_glyphs_and_invalidates_paint() {
        let document = html::parse("<style>::highlight(first){color:red;background:lime}::highlight(second){color:blue}p::highlight(first){text-decoration:underline}</style><p>abcd</p>", 128).unwrap();
        let p = selector::query_selector(&document, document.root(), "p").unwrap().unwrap();
        let node = document.first_child(p).unwrap().unwrap();
        let mut session = RenderSession::new(document);
        let range = HighlightRange { start: Boundary { container: node, offset: 1 }, end: Boundary { container: node, offset: 3 } };
        session.set_highlights(vec![Highlight { name: "first".into(), priority: 0, order: 0, ranges: vec![range.clone(), range] }]);
        let list = session.display_list(200, 100, &Text).unwrap();
        assert!(list.0.iter().any(|command| matches!(command, Command::FillRect { rect, color } if color.g == 255 && color.r == 0 && rect.width == 20.0)));
        let red = list.0.iter().filter_map(|command| if let Command::GlyphRun { color, glyphs, .. } = command { (color.r == 255 && color.b == 0).then_some(glyphs) } else { None }).collect::<Vec<_>>();
        assert_eq!(red.len(), 1, "overlapping ranges in one highlight form a union");
        assert_eq!(red[0].len(), 4, "clip reuses the original shaped glyphs");
        let revision = session.paint_revision();
        session.set_highlights(Vec::new());
        assert!(session.paint_revision() > revision);
        let list = session.display_list(200, 100, &Text).unwrap();
        assert!(!list.0.iter().any(|command| matches!(command, Command::GlyphRun { color, .. } if color.r == 255 && color.b == 0)));
    }

    #[test]
    fn custom_highlight_partial_ranges_share_whitespace_and_case_projection() {
        for (markup, start, end, expected) in [
            ("<p>a   bc</p>", 1, 4, 10.0),
            ("<p style='text-transform:uppercase'>aßb</p>", 1, 2, 20.0),
            ("<p>A😀B</p>", 1, 3, 10.0),
        ] {
            let document = html::parse(&alloc::format!("<style>::highlight(selected){{background:lime}}</style>{markup}"), 128).unwrap();
            let p = selector::query_selector(&document, document.root(), "p").unwrap().unwrap();
            let node = document.first_child(p).unwrap().unwrap();
            let mut session = RenderSession::new(document);
            session.set_highlights(vec![Highlight { name: "selected".into(), priority: 0, order: 0,
                ranges: vec![HighlightRange { start: Boundary { container: node, offset: start }, end: Boundary { container: node, offset: end } }] }]);
            let list = session.display_list(200, 100, &Text).unwrap();
            assert!(list.0.iter().any(|command| matches!(command, Command::FillRect { rect, color } if color.g == 255 && color.r == 0 && rect.width == expected)), "source projection failed for {markup}");
        }
    }
}
