use lumen_html::{
    css, html,
    paint::{Command, Glyph, ShapedRun, TextShaper},
    selector,
    session::RenderSession,
};
struct Text;
impl TextShaper for Text {
    fn shape(&self, text: &str, _: f32) -> Result<ShapedRun, ()> {
        Ok(ShapedRun {
            glyphs: text
                .char_indices()
                .enumerate()
                .map(|(i, (cluster, c))| Glyph {
                    id: c as u16,
                    face: 0,
                    cluster: cluster as u32,
                    caps_expansion: 0,
                    x: i as f32 * 6.,
                    y: 0.,
                    size_scale: 1.,
                })
                .collect::<Vec<_>>()
                .into(),
            width: text.chars().count() as f32 * 6.,
        })
    }
    fn ascent(&self, _: f32) -> f32 {
        10.
    }
    fn line_height(&self, _: f32) -> f32 {
        16.
    }
}
fn text(list: &lumen_html::paint::DisplayList) -> String {
    list.0
        .iter()
        .filter_map(|c| match c {
            Command::GlyphRun { glyphs, .. } => Some(
                glyphs
                    .iter()
                    .filter_map(|g| char::from_u32(g.id as u32))
                    .collect::<String>(),
            ),
            _ => None,
        })
        .collect()
}
#[test]
fn hidden_transformed_relative_descendant_retains_geometry_without_paint() {
    for wrapper in [
        "transform:scale(1)",
        "opacity:0.5",
        "transform:scale(1);opacity:0.5",
    ] {
        let markup = format!("<style>body{{margin:0}}#outer{{position:absolute;visibility:hidden;{wrapper};width:100px;height:100px}}#relative{{position:relative;width:20px;height:30px;background:red}}</style><div id='outer'><div id='relative'></div></div>");
        let document = html::parse(&markup, 32).unwrap();
        let node = selector::query_selector(&document, document.root(), "#relative")
            .unwrap()
            .unwrap();
        let outer = selector::query_selector(&document, document.root(), "#outer")
            .unwrap()
            .unwrap();
        let mut session = RenderSession::new(document);
        let paints_red = |list: &lumen_html::paint::DisplayList| {
            list.0.iter().any(|command| matches!(command,
                Command::FillRect { color, .. }
                    if (color.r, color.g, color.b, color.a) == (255, 0, 0, 255)))
        };
        assert!(!paints_red(session.display_list(200, 200, &Text).unwrap()));
        let rect = session.layout_rect(node).unwrap();
        assert_eq!((rect.width, rect.height), (20.0, 30.0), "{wrapper}");
        assert_ne!(session.hit_test(5.0, 5.0), Some(node));
        // Removing an empty wrapper must not remove geometry or prevent the
        // same subtree from painting and receiving hits after visibility changes.
        session.document_mut().set_attribute(outer, "style", "visibility:visible").unwrap();
        assert!(paints_red(session.display_list(200, 200, &Text).unwrap()));
        assert_eq!(session.hit_test(5.0, 5.0), Some(node));
    }
}
#[test]
fn counter_mutation_invalidates_following_retained_fragments() {
    let markup="<style>body{margin:0;counter-reset:n}p{height:20px;margin:0;counter-increment:n}p::before{content:counter(n)}</style><p id='first'></p><p></p><p></p>";
    let doc = html::parse(markup, 64).unwrap();
    let first = selector::query_selector(&doc, doc.root(), "#first")
        .unwrap()
        .unwrap();
    let mut session = RenderSession::new(doc);
    assert_eq!(text(session.display_list(200, 200, &Text).unwrap()), "123");
    session
        .document_mut()
        .set_attribute(first, "style", "counter-increment:n 2")
        .unwrap();
    let actual = text(session.display_list(200, 200, &Text).unwrap());
    assert_eq!(
        actual,
        "234",
        "cache stats {:?}",
        session.layout_cache_stats()
    );
}
#[test]
fn animation_initial_resets_prior_cascade_value() {
    let doc = html::parse(
        "<style>div{animation-name:fade}div{animation-name:initial}</style><div></div>",
        32,
    )
    .unwrap();
    let node = selector::query_selector(&doc, doc.root(), "div")
        .unwrap()
        .unwrap();
    let style = RenderSession::new(doc).computed_style(node).unwrap();
    assert!(style.animation[0].is_none(), "{:?}", style.animation);
    assert!(css::supports_declaration("animation-name", "initial"));
}
#[test]
fn ordinary_node_kind_stays_compact() {
    assert!(std::mem::size_of::<lumen_html::NodeKind>() <= 64);
}
#[test]
fn animation_snapshot_reuses_unchanged_inputs_and_refreshes_dom_and_media() {
    let doc = html::parse("<style>@keyframes fade{from{opacity:0}to{opacity:1}}main{animation-name:fade}@media(min-width:300px){main{animation-name:none}}</style><main><span></span></main>",64).unwrap();
    let node = selector::query_selector(&doc, doc.root(), "main")
        .unwrap()
        .unwrap();
    let mut session = RenderSession::new(doc);
    session
        .set_media_environment(css::MediaEnvironment {
            width: 128.,
            ..Default::default()
        })
        .unwrap();
    let first = session.animation_snapshot().unwrap();
    assert!(first
        .nodes
        .iter()
        .any(|(id, _, _, style)| *id == node && style[0].as_deref() == Some("fade")));
    session
        .set_animation_declarations(node, vec![("opacity".into(), "0.5".into())])
        .unwrap();
    let same = session.animation_snapshot().unwrap();
    assert!(std::sync::Arc::ptr_eq(&first, &same));
    session
        .document_mut()
        .set_attribute(node, "style", "animation-name:other")
        .unwrap();
    let changed = session.animation_snapshot().unwrap();
    assert_ne!(changed.generation, first.generation);
    assert!(changed
        .nodes
        .iter()
        .any(|(id, _, _, style)| *id == node && style[0].as_deref() == Some("other")));
    session
        .set_media_environment(css::MediaEnvironment {
            width: 400.,
            ..Default::default()
        })
        .unwrap();
    assert_ne!(
        session.animation_snapshot().unwrap().generation,
        changed.generation
    );
}
