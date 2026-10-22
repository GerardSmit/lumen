//! `:host(...)` as the left side of a combinator inside shadow stylesheets.
use lumen_html::{css::Display, html, selector, session::RenderSession, NodeId};

fn display_of(src: &str, host_selector: &str, inner: &str) -> Display {
    let document = html::parse_with_declarative_shadow_roots(src, 100_000, true).unwrap();
    let host = selector::query_selector(&document, document.root(), host_selector).unwrap().unwrap();
    let root = document.shadow_root(host).unwrap().expect("declarative shadow root");
    let target: NodeId = selector::query_selector(&document, root, inner).unwrap().unwrap();
    RenderSession::new(document).computed_style(target).unwrap().display
}

#[test]
fn host_descendant_rule_matches_elements_in_the_shadow_tree() {
    let src = "<x-menu defer-hydration><template shadowrootmode=open>\
        <style>:host(:not([loaded],:focus-within)) slot[name=panel]{display:none}</style>\
        <slot name=button></slot><slot name=panel></slot></template></x-menu>";
    assert_eq!(display_of(src, "x-menu", "slot[name=panel]"), Display::None);
    assert_eq!(display_of(src, "x-menu", "slot[name=button]"), Display::Contents);
}

#[test]
fn host_rule_stops_matching_when_its_argument_fails() {
    let src = "<x-menu loaded><template shadowrootmode=open>\
        <style>:host(:not([loaded])) slot{display:none}</style><slot></slot></template></x-menu>";
    assert_eq!(display_of(src, "x-menu", "slot"), Display::Contents);
}

#[test]
fn host_child_rule_matches_top_level_shadow_children_only() {
    let src = "<x-box><template shadowrootmode=open>\
        <style>:host > .a{display:none}</style>\
        <div class=a></div><section><div class=a id=deep></div></section></template></x-box>";
    assert_eq!(display_of(src, "x-box", ".a"), Display::None);
    assert_eq!(display_of(src, "x-box", "#deep"), Display::Block);
}

#[test]
fn nested_shadow_hosts_match_their_own_host_rules() {
    let src = "<x-outer><template shadowrootmode=open>\
        <x-inner defer-hydration><template shadowrootmode=open>\
        <style>:host(:not([loaded])) slot{display:none}</style><slot></slot></template></x-inner>\
        </template></x-outer>";
    let document = html::parse_with_declarative_shadow_roots(src, 100_000, true).unwrap();
    let outer = selector::query_selector(&document, document.root(), "x-outer").unwrap().unwrap();
    let outer_root = document.shadow_root(outer).unwrap().unwrap();
    let inner = selector::query_selector(&document, outer_root, "x-inner").unwrap().unwrap();
    let inner_root = document.shadow_root(inner).unwrap().unwrap();
    let slot = selector::query_selector(&document, inner_root, "slot").unwrap().unwrap();
    assert_eq!(RenderSession::new(document).computed_style(slot).unwrap().display, Display::None);
}
