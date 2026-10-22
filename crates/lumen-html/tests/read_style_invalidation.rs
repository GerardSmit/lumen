//! Live computed-style reads after DOM mutations must match a fresh session.
use lumen_html::{html, selector, session::RenderSession};

fn color(s: &mut RenderSession, id: &str) -> String {
    let n = { let d = s.document(); selector::query_selector(d, d.root(), &format!("#{id}")).unwrap().unwrap() };
    format!("{:?}", s.computed_style(n).unwrap().color)
}

#[test]
fn sibling_combinators_inside_is_reach_has_anchor() {
    let src = r#"<style>div{color:grey}.green:has(#descendant:is(.p + .c_has_scope ~ .d .e)){color:green}</style>
<div><div class="p"></div><div id="parent_previous"></div><div id="parent" class="d k"><div class="p"></div><div id="previous"></div><div class="p"></div>
<div id="has_scope" class="d green"><div class="p"></div><div id="child_previous"></div><div id="child" class="d"><div id="descendant" class="b e o"></div></div></div></div></div>"#;
    let mut s = RenderSession::new(html::parse(src, 100_000).unwrap());
    assert!(color(&mut s, "has_scope").contains("r: 128"));
    let n = { let d = s.document(); selector::query_selector(d, d.root(), "#previous").unwrap().unwrap() };
    s.document_mut().set_attribute(n, "class", "c_has_scope").unwrap();
    let after = color(&mut s, "has_scope");
    let fresh = { let src2 = src.replace(r#"id="previous""#, r#"id="previous" class="c_has_scope""#); let mut f = RenderSession::new(html::parse(&src2, 100_000).unwrap()); color(&mut f, "has_scope") };
    assert_eq!(after, fresh);
}

#[test]
fn id_attribute_selector_reaches_next_sibling() {
    let src = r#"<style>div{color:gray}[id=a2]{color:blue}[id=a2] + #d2{color:yellow}</style><div><div id="b2"></div></div><div id="d2"></div>"#;
    let mut s = RenderSession::new(html::parse(src, 100_000).unwrap());
    let a2 = { let d = s.document(); let b = selector::query_selector(d, d.root(), "#b2").unwrap().unwrap(); d.parent(b).unwrap().unwrap() };
    let _ = color(&mut s, "d2");
    s.document_mut().set_attribute(a2, "id", "x-a2").unwrap();
    let _ = color(&mut s, "d2");
    s.document_mut().set_attribute(a2, "id", "a2").unwrap();
    let after = color(&mut s, "d2");
    assert!(after.contains("g: 255"), "{after}");
}

fn mutate_and_compare(src: &str, watch: &str, target: &str, name: &str, value: Option<&str>) {
    let mut s = RenderSession::new(html::parse(src, 100_000).unwrap());
    let _ = color(&mut s, watch);
    let n = { let d = s.document(); selector::query_selector(d, d.root(), &format!("#{target}")).unwrap().unwrap() };
    match value {
        Some(value) => s.document_mut().set_attribute(n, name, value).unwrap(),
        None => s.document_mut().remove_attribute(n, name).unwrap(),
    }
    let after = color(&mut s, watch);
    let fresh_src = match value {
        Some(value) => src.replacen(&format!(r#"id="{target}""#), &format!(r#"id="{target}" {name}="{value}""#), 1),
        None => src.to_string(),
    };
    let mut f = RenderSession::new(html::parse(&fresh_src, 100_000).unwrap());
    assert_eq!(after, color(&mut f, watch), "{target} {name}={value:?}");
}

#[test]
fn nested_complex_parent_reaches_has_anchor() {
    let src = r#"<style>div{color:grey}.ancestor_prev ~ div .descendant{.anchor:has(&){color:red}}</style>
<div><div id="prev"></div><div id="mid"><div id="p"><div id="anchor" class="anchor"><div><div class="descendant"></div></div></div></div></div></div>"#;
    mutate_and_compare(src, "anchor", "prev", "class", Some("ancestor_prev"));
}

#[test]
fn headingoffset_reaches_heading_in_has() {
    let src = r#"<style>div{color:grey}#ancestor:has(:heading(2)) ~ #subject{color:orange}</style>
<div id="ancestor"><h1 id="h1"></h1></div><div id="subject"></div>"#;
    mutate_and_compare(src, "subject", "h1", "headingoffset", Some("1"));
    mutate_and_compare(src, "subject", "ancestor", "headingoffset", Some("1"));
}
