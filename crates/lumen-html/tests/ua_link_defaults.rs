//! HTML rendering defaults for links and edit/strike phrasing elements.
use lumen_html::{html, selector, session::RenderSession};

fn style_of(src: &str, selector_text: &str) -> (String, Option<String>) {
    let mut session = RenderSession::new(html::parse(src, 100_000).unwrap());
    let node = {
        let document = session.document();
        selector::query_selector(document, document.root(), selector_text).unwrap().unwrap()
    };
    let style = session.computed_style(node).unwrap();
    let line = style.computed_css_value("text-decoration-line", Default::default());
    (format!("{:?}", style.color), line)
}

#[test]
fn links_use_link_text_and_underline_until_authors_override() {
    let (color, line) = style_of("<a href=https://example.com>link</a>", "a");
    assert!(color.contains("b: 238"), "{color}");
    assert_eq!(line.as_deref(), Some("underline"));
    let (color, line) = style_of("<style>a{color:red;text-decoration:none}</style><a href=x>link</a>", "a");
    assert!(color.contains("r: 255"), "{color}");
    assert_eq!(line.as_deref(), Some("none"));
    let (_, line) = style_of("<a>placeholder</a>", "a");
    assert_eq!(line.as_deref(), Some("none"));
}

#[test]
fn edit_and_strike_elements_are_decorated() {
    assert_eq!(style_of("<u>u</u>", "u").1.as_deref(), Some("underline"));
    assert_eq!(style_of("<ins>i</ins>", "ins").1.as_deref(), Some("underline"));
    assert_eq!(style_of("<del>d</del>", "del").1.as_deref(), Some("line-through"));
    assert_eq!(style_of("<s>s</s>", "s").1.as_deref(), Some("line-through"));
}

#[test]
fn link_pseudo_classes_match_hyperlinks_and_override_the_ua_defaults() {
    let sheet = "<style>a:link{color:#000000;text-decoration:none}a:visited{color:red}</style>";
    let (color, line) = style_of(&format!("{sheet}<a href=x>link</a>"), "a");
    assert!(color.contains("r: 0") && color.contains("b: 0"), "{color}");
    assert_eq!(line.as_deref(), Some("none"));
    let (color, line) = style_of(&format!("<style>a:any-link{{color:#112233}}</style><a href=x>link</a>"), "a");
    assert!(color.contains("r: 17"), "{color}");
    assert_eq!(line.as_deref(), Some("underline"));
    let (_, line) = style_of(&format!("<style>:link{{text-decoration:overline}}</style><a>anchor</a><area href=y>"), "a");
    assert_eq!(line.as_deref(), Some("none"));
    let (color, _) = style_of(&format!("{sheet}<a href=x>link</a>"), "a");
    assert!(!color.contains("r: 255"), "{color}");
}
