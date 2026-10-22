//! SVG stroke cap, join and miter-limit presentation do not make an SVG image undecodable.
use lumen_html::{html, session::RenderSession};

#[test]
fn stroke_line_shape_properties_are_not_reported_unsupported() {
    let source = "<svg width=20 height=20><path d='M2 2h10v10z' fill='none' stroke='#000' stroke-width='3' \
        stroke-linecap='round' stroke-linejoin='round' stroke-miterlimit='2' \
        style='stroke-linecap:square;stroke-linejoin:bevel'/></svg>";
    let mut session = RenderSession::new(html::parse(source, 100_000).unwrap());
    assert!(session.unsupported_svg_features().unwrap().is_empty());
}

#[test]
fn dash_properties_remain_reported_unsupported() {
    let source = "<svg width=20 height=20><path d='M2 2h10' stroke='#000' stroke-dasharray='2'/></svg>";
    let mut session = RenderSession::new(html::parse(source, 100_000).unwrap());
    assert!(!session.unsupported_svg_features().unwrap().is_empty());
}
