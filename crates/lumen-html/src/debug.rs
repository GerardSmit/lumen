//! Deterministic inspection of the document and its most recently rendered frame.
//! Node indices are local to the arena; process-global document identities and
//! allocation addresses are deliberately absent. Output streams to the caller.
use crate::{NodeKind, session::RenderSession};
use alloc::{sync::Arc, vec, vec::Vec};
use core::fmt::{self, Write};

#[derive(Debug)]
pub enum DumpError {
    InvalidTree,
    Unrendered,
    Style(crate::layout::LayoutError),
    Output(fmt::Error),
}

impl From<fmt::Error> for DumpError {
    fn from(error: fmt::Error) -> Self {
        Self::Output(error)
    }
}

/// Write ordinary DOM, shadow roots and template contents, computed styles,
/// retained layout boxes, and paint commands. Render the session first.
/// The writer controls the output budget and may stop with `fmt::Error`.
pub fn write_session(
    session: &mut RenderSession,
    output: &mut impl Write,
) -> Result<(), DumpError> {
    let mut list = session
        .cached_display_list()
        .ok_or(DumpError::Unrendered)?
        .clone();
    // Font provider identities are process-local too. Preserve distinct faces
    // with ordinals in first-use order, including glyphs inside paint masks.
    fn normalize(command: &mut crate::paint::Command, faces: &mut Vec<u64>) {
        match command {
            crate::paint::Command::GlyphRun { glyphs, .. } => {
                for glyph in Arc::make_mut(glyphs) {
                    if glyph.face == 0 {
                        continue;
                    }
                    let index = faces
                        .iter()
                        .position(|id| *id == glyph.face)
                        .unwrap_or_else(|| {
                            faces.push(glyph.face);
                            faces.len() - 1
                        });
                    glyph.face = index as u64 + 1;
                }
            }
            crate::paint::Command::MaskedBackground(mask) => {
                normalize(&mut mask.paint, faces);
                for command in &mut Arc::make_mut(&mut mask.mask).0 {
                    normalize(command, faces);
                }
            }
            _ => {}
        }
    }
    let mut faces = Vec::new();
    for command in &mut list.0 {
        normalize(command, &mut faces);
    }
    writeln!(output, "DOM / COMPUTED STYLE / LAYOUT (CSS px)")?;
    let mut pending = vec![(session.document().root(), 0usize, "node")];
    while let Some((node, depth, edge)) = pending.pop() {
        let kind = session
            .document()
            .kind(node)
            .map_err(|_| DumpError::InvalidTree)?
            .clone();
        writeln!(
            output,
            "{indent}{edge} #{} {kind:?}",
            node.index(),
            indent = "  ".repeat(depth)
        )?;
        if matches!(kind, NodeKind::Element { .. }) {
            let style = session.computed_style(node).map_err(DumpError::Style)?;
            writeln!(
                output,
                "{indent}style {style:?}",
                indent = "  ".repeat(depth + 1)
            )?;
            writeln!(
                output,
                "{indent}layout {:?}",
                session.layout_rect(node),
                indent = "  ".repeat(depth + 1)
            )?;
        }
        let document = session.document();
        let mut children = vec![];
        let mut child = document
            .first_child(node)
            .map_err(|_| DumpError::InvalidTree)?;
        while let Some(id) = child {
            children.push((id, depth + 1, "node"));
            child = document
                .next_sibling(id)
                .map_err(|_| DumpError::InvalidTree)?;
        }
        if let Some(id) = document
            .template_content(node)
            .map_err(|_| DumpError::InvalidTree)?
        {
            children.push((id, depth + 1, "template-content"));
        }
        if let Some(id) = document
            .shadow_root(node)
            .map_err(|_| DumpError::InvalidTree)?
        {
            children.push((id, depth + 1, "shadow-root"));
        }
        pending.extend(children.into_iter().rev());
    }
    writeln!(output, "DISPLAY LIST (CSS px)")?;
    for (index, command) in list.0.iter().enumerate() {
        writeln!(output, "{index}: {command:?}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;
    struct NoText(u64);
    impl crate::paint::TextShaper for NoText {
        fn shape(&self, text: &str, _: f32) -> Result<crate::paint::ShapedRun, ()> {
            let glyphs = if text.is_empty() {
                vec![]
            } else {
                vec![crate::paint::Glyph {
                    id: 1,
                    face: self.0,
                    cluster: 0,
                    caps_expansion: 0,
                    x: 0.0,
                    y: 0.0,
                    size_scale: 1.0,
                }]
            };
            Ok(crate::paint::ShapedRun {
                glyphs: glyphs.into(),
                width: 5.0,
            })
        }
        fn ascent(&self, _: f32) -> f32 {
            2.0
        }
        fn line_height(&self, _: f32) -> f32 {
            6.0
        }
    }
    #[test]
    fn dump_includes_detached_template_and_shadow_trees_without_global_ids() {
        fn render(face_id: u64) -> String {
            let mut document = crate::html::parse(
                "<body><template><i>detached</i></template><div id=host></div></body>",
                64,
            )
            .unwrap();
            let host = crate::selector::query_selector(&document, document.root(), "#host")
                .unwrap()
                .unwrap();
            let root = document
                .attach_shadow(host, crate::ShadowMode::Closed)
                .unwrap();
            let children = crate::html::parse_fragment(&mut document, "<b>shadow</b>").unwrap();
            document.append(root, children).unwrap();
            let mut session = RenderSession::new(document);
            assert!(matches!(
                write_session(&mut session, &mut String::new()),
                Err(DumpError::Unrendered)
            ));
            session.display_list(80, 80, &NoText(face_id)).unwrap();
            let mut output = String::new();
            write_session(&mut session, &mut output).unwrap();
            session
                .document_mut()
                .set_attribute(host, "class", "changed")
                .unwrap();
            assert!(
                matches!(
                    write_session(&mut session, &mut String::new()),
                    Err(DumpError::Unrendered)
                ),
                "a dump must not pair fresh styles with stale paint commands"
            );
            output
        }
        let output = render(41);
        assert!(output.contains("template-content"));
        assert!(output.contains("shadow-root"));
        assert!(output.contains("detached"));
        assert!(output.contains("shadow"));
        assert!(!output.contains("NodeId { document:"));
        assert!(output.contains("face: 1"));
        assert_eq!(output, render(83));
    }
}
