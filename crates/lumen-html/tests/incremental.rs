use lumen_html::{
    html,
    paint::{DisplayList, Glyph, ImageData, ShapedRun, TextShaper},
    selector,
    session::RenderSession,
    Name, Namespace, NodeId, NodeKind, ShadowMode,
};
use std::sync::Arc;

struct Shaper;
impl TextShaper for Shaper {
    fn shape(&self, text: &str, _: f32) -> Result<ShapedRun, ()> {
        Ok(ShapedRun {
            glyphs: text
                .char_indices()
                .enumerate()
                .map(|(i, (cluster, c))| Glyph {
                    id: c as u16,
                    face: 0,
                    cluster: cluster as u32,
                    caps_expansion: 0, x_offset: 0.0,
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

const MAX_NODES: usize = 400_000;
const VIEW: (u32, u32) = (800, 600);

type Step = fn(&mut RenderSession);

fn q(s: &RenderSession, sel: &str) -> NodeId {
    let d = s.document();
    selector::query_selector(d, d.root(), sel)
        .unwrap()
        .unwrap_or_else(|| panic!("no node matches {sel}"))
}

fn all(s: &RenderSession, sel: &str) -> Vec<NodeId> {
    let d = s.document();
    selector::query_selector_all(d, d.root(), sel).unwrap()
}

fn attr(s: &mut RenderSession, sel: &str, name: &str, value: &str) {
    let node = q(s, sel);
    s.document_mut().set_attribute(node, name, value).unwrap();
}

fn unattr(s: &mut RenderSession, sel: &str, name: &str) {
    let node = q(s, sel);
    s.document_mut().remove_attribute(node, name).unwrap();
}

fn text(s: &mut RenderSession, sel: &str, value: &str) {
    let node = q(s, sel);
    let d = s.document_mut();
    match d.first_child(node).unwrap() {
        Some(child) if matches!(d.kind(child), Ok(NodeKind::Text(_))) => {
            d.replace_data(child, value).unwrap()
        }
        _ => {
            let child = d.create(NodeKind::Text(value.into())).unwrap();
            d.append(node, child).unwrap();
        }
    }
}

fn add(
    s: &mut RenderSession,
    parent: &str,
    tag: &str,
    attrs: &[(&str, &str)],
    body: Option<&str>,
    before: Option<&str>,
) -> NodeId {
    let parent = q(s, parent);
    let before = before.map(|b| q(s, b));
    let d = s.document_mut();
    let el = d
        .create(NodeKind::Element {
            namespace: Namespace::Html,
            name: Name::new(tag),
            attributes: Vec::new(),
        })
        .unwrap();
    for (k, v) in attrs {
        d.set_attribute(el, k, v).unwrap();
    }
    if let Some(body) = body {
        let t = d.create(NodeKind::Text(body.into())).unwrap();
        d.append(el, t).unwrap();
    }
    d.insert_before(parent, el, before).unwrap();
    el
}

fn del(s: &mut RenderSession, sel: &str) {
    let node = q(s, sel);
    s.document_mut().remove(node).unwrap();
}

fn mv(s: &mut RenderSession, node: &str, parent: &str, before: Option<&str>) {
    let node = q(s, node);
    let parent = q(s, parent);
    let before = before.map(|b| q(s, b));
    s.document_mut().insert_before(parent, node, before).unwrap();
}

fn render<'a>(s: &'a mut RenderSession) -> &'a DisplayList {
    s.display_list(VIEW.0, VIEW.1, &Shaper).unwrap()
}

fn set_view(s: &mut RenderSession, size: (u32, u32)) {
    s.set_media_environment(lumen_html::css::MediaEnvironment {
        width: size.0 as f32,
        height: size.1 as f32,
        ..Default::default()
    })
    .unwrap();
}

fn render_at(s: &mut RenderSession, size: (u32, u32)) -> DisplayList {
    set_view(s, size);
    s.display_list(size.0, size.1, &Shaper).unwrap().clone()
}

fn paras(n: usize) -> String {
    (0..n).map(|i| format!("<p id=p{i}>paragraph {i}</p>")).collect()
}

fn assert_same(name: &str, mode: &str, actual: &DisplayList, expected: &DisplayList) {
    if actual.approx_eq(expected, 1e-3) {
        return;
    }
    let index = actual
        .0
        .iter()
        .zip(expected.0.iter())
        .position(|(a, b)| a != b)
        .unwrap_or(actual.0.len().min(expected.0.len()));
    panic!(
        "{name} [{mode}]: incremental and fresh display lists differ ({} vs {} commands), first difference at {index}:\nincremental: {:?}\nfresh: {:?}",
        actual.0.len(),
        expected.0.len(),
        actual.0.get(index),
        expected.0.get(index)
    );
}

fn run(name: &str, source: &str, resize: Option<(u32, u32)>, steps: &[Step]) {
    let expected = {
        let mut fresh = RenderSession::new(html::parse(source, MAX_NODES).unwrap());
        for step in steps {
            step(&mut fresh);
        }
        render_at(&mut fresh, resize.unwrap_or(VIEW))
    };
    for per_step in [true, false] {
        let mode = if per_step { "render after each step" } else { "one render after all steps" };
        let mut session = RenderSession::new(html::parse(source, MAX_NODES).unwrap());
        session.set_verify_incremental(true);
        render_at(&mut session, VIEW);
        for (index, step) in steps.iter().enumerate() {
            step(&mut session);
            if per_step {
                render_at(&mut session, VIEW);
                assert_eq!(session.invalidation_stats().verify_mismatches, 0, "{name}: verify mode found a mismatch after step {index}");
            }
        }
        let actual = render_at(&mut session, resize.unwrap_or(VIEW));
        assert_same(name, mode, &actual, &expected);
        let stats = session.invalidation_stats();
        assert_eq!(stats.verify_mismatches, 0, "{name} [{mode}]: verify mode found mismatches");
    }
}

macro_rules! diff {
    ($name:ident, $html:expr, [$($step:expr),* $(,)?]) => {
        #[test]
        fn $name() {
            run(stringify!($name), &$html, None, &[$($step),*]);
        }
    };
    ($name:ident, $html:expr, resize $size:expr, [$($step:expr),* $(,)?]) => {
        #[test]
        fn $name() {
            run(stringify!($name), &$html, Some($size), &[$($step),*]);
        }
    };
}

diff!(
    svg_resource_definitions_invalidate_referencing_elements,
    "<style>body{margin:0}#host{width:60px;height:40px;background:#369}\
     #lg{width:60px;height:30px;fill:url(#grad)}#cl{clip-path:url(#clip);width:60px;height:40px;background:#c00}</style>\
     <svg style=display:none><defs>\
     <filter id=paint><feFlood id=flood flood-color=green/></filter>\
     <linearGradient id=grad><stop id=stop offset=0 stop-color=red /><stop offset=1 stop-color=blue /></linearGradient>\
     <clipPath id=clip clipPathUnits=userSpaceOnUse><rect id=cr x=0 y=0 width=30 height=20 /></clipPath>\
     </defs></svg>\
     <div id=host style=\"filter:url(#paint)\">x</div>\
     <svg id=lg width=60 height=30><rect width=60 height=30 style=\"fill:url(#grad)\"/></svg>\
     <div id=cl>y</div>",
    [
        |s| attr(s, "#flood", "style", "flood-color:red"),
        |s| attr(s, "#flood", "style", "flood-color:blue"),
        |s| attr(s, "#flood", "style", "flood-color:red"),
        |s| attr(s, "#stop", "stop-color", "lime"),
        |s| attr(s, "#stop", "style", "stop-color:yellow"),
        |s| attr(s, "#cr", "width", "50"),
        |s| attr(s, "#cr", "style", "height:10px"),
    ]
);

diff!(
    class_toggle,
    "<style>body{margin:0}p{margin:4px;height:20px}.on{color:red;background:#eee;height:40px}.big{font-size:24px}</style>\
     <div id=w><p id=a>one</p><p id=b>two</p><p id=c>three</p></div>",
    [
        |s| attr(s, "#b", "class", "on"),
        |s| attr(s, "#a", "class", "on big"),
        |s| attr(s, "#b", "class", ""),
        |s| unattr(s, "#a", "class"),
    ]
);

diff!(
    id_and_inline_style,
    "<style>body{margin:0}#x{background:blue;height:30px}#y{background:green;height:50px}</style>\
     <div id=x style='margin-left:10px'>a</div><div id=z>b</div>",
    [
        |s| attr(s, "#x", "id", "y"),
        |s| attr(s, "#y", "style", "margin-left:40px;color:red"),
        |s| attr(s, "#z", "style", "height:100px;background:#abc"),
        |s| unattr(s, "#y", "style"),
    ]
);

diff!(
    attribute_selectors,
    "<style>body{margin:0}div{height:20px}[data-k=x]{background:red}[data-k^=ab]{color:blue;height:30px}\
     a[href$='.pdf']{color:green;font-size:20px}[hidden]{display:none}[lang|=en]{margin-left:12px}[data-k~=w]{border:2px solid black}</style>\
     <div id=a data-k=x>a</div><div id=b data-k=abc>b</div><div id=c data-k='u w'>c</div>\
     <a id=l href='f.txt'>link</a><div id=d lang=en-US>d</div>",
    [
        |s| attr(s, "#a", "data-k", "abz"),
        |s| attr(s, "#b", "data-k", "x"),
        |s| attr(s, "#l", "href", "f.pdf"),
        |s| attr(s, "#c", "hidden", ""),
        |s| attr(s, "#d", "lang", "de"),
        |s| unattr(s, "#c", "hidden"),
        |s| attr(s, "#c", "data-k", "w"),
    ]
);

diff!(
    has_selector,
    "<style>body{margin:0}.card{height:30px;margin:2px}.card:has(.flag){border:3px solid red;height:50px}\
     li:has(+ li.sel){color:blue;font-size:18px}ul:has(> li:only-child){background:#ccc}</style>\
     <div class=card id=c1><span>x</span></div><div class=card id=c2><span>y</span></div>\
     <ul id=u><li id=l1>1</li><li id=l2>2</li><li id=l3>3</li></ul>",
    [
        |s| {
            add(s, "#c1", "b", &[("class", "flag")], Some("f"), None);
        },
        |s| attr(s, "#l3", "class", "sel"),
        |s| attr(s, "#l2", "class", "sel"),
        |s| del(s, "#c1 .flag"),
        |s| del(s, "#l2"),
        |s| del(s, "#l1"),
        |s| attr(s, "#c2 span", "class", "flag"),
    ]
);

diff!(
    nth_child,
    "<style>body{margin:0}li{height:16px}li:nth-child(odd){color:red;background:#eee}li:nth-last-child(2){height:30px}\
     li:first-child{margin-top:5px}li:last-of-type{border-bottom:2px solid blue}li:nth-child(3n+1 of .k){background:yellow}</style>\
     <ul id=u><li id=a class=k>a</li><li id=b>b</li><li id=c class=k>c</li><li id=d>d</li><li id=e class=k>e</li></ul>",
    [
        |s| {
            add(s, "#u", "li", &[("id", "n0")], Some("new first"), Some("#a"));
        },
        |s| del(s, "#c"),
        |s| {
            add(s, "#u", "li", &[("id", "n1"), ("class", "k")], Some("new last"), None);
        },
        |s| attr(s, "#b", "class", "k"),
        |s| mv(s, "#e", "#u", Some("#a")),
        |s| del(s, "#n0"),
    ]
);

diff!(
    sibling_combinators,
    "<style>body{margin:0}h2{margin:0;height:20px}h2 + p{margin-left:20px;color:red}h2 ~ p{background:#def;height:24px}\
     p.x + p.y{font-size:20px}</style>\
     <p id=p0>before</p><h2 id=h>head</h2><p id=p1>one</p><p id=p2>two</p><p id=p3 class=y>three</p>",
    [
        |s| {
            add(s, "body", "h2", &[("id", "h2")], Some("second"), Some("#p3"));
        },
        |s| attr(s, "#p2", "class", "x"),
        |s| del(s, "#h"),
        |s| del(s, "#h2"),
        |s| {
            add(s, "body", "p", &[("id", "p4"), ("class", "x")], Some("four"), Some("#p3"));
        },
        |s| mv(s, "#p0", "body", None),
    ]
);

diff!(
    structural_edits,
    "<style>body{margin:0}.box{padding:4px;border:1px solid #333}.box .in{margin:3px;color:#444}div > span{display:block}</style>\
     <div class=box id=b1><div class=in id=i1>a</div><div class=in id=i2>b</div></div><div class=box id=b2></div>",
    [
        |s| {
            add(s, "#b2", "div", &[("class", "in")], Some("moved in"), None);
        },
        |s| mv(s, "#i1", "#b2", None),
        |s| del(s, "#i2"),
        |s| {
            add(s, "#b1", "span", &[], Some("span"), None);
        },
        |s| mv(s, "#b2", "#b1", None),
        |s| del(s, "#b2"),
    ]
);

diff!(
    text_edits,
    "<style>body{margin:0;width:200px}p{margin:2px}</style>\
     <p id=a>short</p><p id=b>a longer paragraph of text that wraps over several lines in this narrow body</p><p id=c><em>em</em> and <b id=bold>bold</b> tail</p>",
    [
        |s| text(s, "#a", "now it is a much longer text that must wrap onto more than one line here"),
        |s| text(s, "#b", ""),
        |s| text(s, "#bold", "bolder text"),
        |s| text(s, "#b", "x"),
        |s| text(s, "#a", "s"),
    ]
);

diff!(
    stylesheet_append,
    "<style id=base>body{margin:0}p{height:20px;margin:2px}</style><p id=a>a</p><p id=b class=m>b</p><div id=d>d</div>",
    [
        |s| {
            add(s, "head", "style", &[("id", "s1")], Some(".m{color:red;height:40px}"), None);
        },
        |s| {
            add(s, "body", "style", &[("id", "s2")], Some("#d{background:blue;height:60px}p{margin-left:10px}"), None);
        },
        |s| {
            add(s, "head", "style", &[("id", "s3")], Some(".nothing-matches{color:red}"), None);
        },
        |s| {
            add(s, "head", "style", &[("id", "s4")], Some("p{height:30px !important}"), None);
        },
    ]
);

diff!(
    stylesheet_edit,
    "<style id=a>body{margin:0}.m{height:20px;color:red}</style><style id=b>p{margin:3px}</style><p class=m id=p>p</p><p id=q>q</p>",
    [
        |s| text(s, "#a", "body{margin:0}.m{height:50px;color:blue}"),
        |s| text(s, "#b", "p{margin:10px}#q{height:30px;background:#ccc}"),
        |s| text(s, "#a", "body{margin:0}"),
        |s| text(s, "#b", ""),
        |s| text(s, "#a", "p{color:green;font-size:22px}"),
    ]
);

diff!(
    stylesheet_remove,
    "<style id=a>body{margin:0}.m{height:20px;color:red}</style><style id=b>p{margin:8px}</style><style id=c>#q{background:#abc;height:44px}</style>\
     <p class=m id=p>p</p><p id=q>q</p>",
    [
        |s| del(s, "#b"),
        |s| del(s, "#c"),
        |s| {
            add(s, "head", "style", &[("id", "d")], Some("p{margin:1px}"), None);
        },
        |s| del(s, "#a"),
    ]
);

diff!(
    media_queries_resize,
    "<style>body{margin:0}.a{height:30px;background:red}@media(max-width:400px){.a{display:none}.b{height:10px}}\
     @media(min-width:600px){.b{height:50px;color:blue}}@media(min-height:500px){.c{background:green;height:20px}}</style>\
     <div class=a>a</div><div class=b>b</div><div class=c>c</div>",
    resize(300, 200),
    []
);

diff!(
    media_queries_resize_with_edits,
    "<style>body{margin:0}.a{height:30px;background:red}@media(max-width:400px){.a{height:5px}.b{margin-left:10px}}</style>\
     <div class=a id=a>a</div><div class=b id=b>b</div>",
    resize(320, 480),
    [
        |s| attr(s, "#b", "class", "b a"),
        |s| {
            add(s, "head", "style", &[], Some("@media(max-width:400px){#b{color:red}}"), None);
        },
    ]
);

diff!(
    counters_and_lists,
    "<style>body{margin:0;counter-reset:n}li{height:18px}ol{counter-reset:k}li::before{content:counter(k) '. '}li{counter-increment:k}\
     h3{counter-increment:n;margin:0;height:20px}h3::before{content:'Section ' counter(n) ': '}</style>\
     <h3 id=h1>t1</h3><ol id=o><li id=l1>a</li><li id=l2>b</li><li id=l3>c</li></ol><h3 id=h2>t2</h3>",
    [
        |s| {
            add(s, "#o", "li", &[("id", "l0")], Some("new"), Some("#l1"));
        },
        |s| attr(s, "#l2", "style", "counter-increment:k 5"),
        |s| del(s, "#h1"),
        |s| attr(s, "#o", "start", "7"),
        |s| attr(s, "#l3", "value", "20"),
        |s| attr(s, "#o", "reversed", ""),
        |s| {
            add(s, "body", "h3", &[("id", "h3")], Some("t3"), None);
        },
        |s| attr(s, "#h2", "style", "display:none"),
    ]
);

diff!(
    quotes_and_generated_content,
    "<style>body{margin:0}q{quotes:'<' '>'}.t::after{content:' [' attr(data-n) ']'}.t{height:18px}</style>\
     <p>a <q id=q1>b <q>c</q></q></p><div class=t id=t data-n=1>t</div>",
    [
        |s| attr(s, "#t", "data-n", "22"),
        |s| attr(s, "#t", "class", ""),
        |s| attr(s, "#t", "class", "t"),
        |s| {
            add(s, "#q1", "q", &[], Some("deep"), None);
        },
    ]
);

diff!(
    tables,
    "<style>body{margin:0}table{border-collapse:collapse}td,th{border:1px solid #000;padding:2px}.w{width:120px}.hl td{background:#fe0}</style>\
     <table id=t><thead><tr><th>h1</th><th>h2</th></tr></thead><tbody id=tb><tr id=r1><td>a</td><td>b</td></tr><tr id=r2><td>c</td><td id=c2>d</td></tr></tbody></table>",
    [
        |s| {
            add(s, "#tb", "tr", &[("id", "r3")], None, None);
            add(s, "#r3", "td", &[], Some("e"), None);
            add(s, "#r3", "td", &[], Some("f"), None);
        },
        |s| attr(s, "#r1", "class", "hl"),
        |s| attr(s, "#c2", "colspan", "2"),
        |s| attr(s, "#c2", "class", "w"),
        |s| del(s, "#r2"),
        |s| attr(s, "#t", "style", "border-collapse:separate;border-spacing:6px"),
        |s| text(s, "#r1 td", "long cell content that forces the column to be wider than before"),
    ]
);

diff!(
    flexbox,
    "<style>body{margin:0}#f{display:flex;width:400px;height:100px;gap:4px}#f > div{flex:1 1 0;background:#ccd;min-width:0}\
     .col{flex-direction:column}.wrap{flex-wrap:wrap}.g2{flex-grow:2}.fixed{flex:0 0 150px}</style>\
     <div id=f><div id=a>a</div><div id=b class=g2>b</div><div id=c>c</div></div>",
    [
        |s| attr(s, "#a", "class", "fixed"),
        |s| {
            add(s, "#f", "div", &[("id", "d")], Some("d"), None);
        },
        |s| attr(s, "#f", "class", "wrap"),
        |s| attr(s, "#f", "class", "col"),
        |s| del(s, "#b"),
        |s| attr(s, "#c", "style", "order:-1;align-self:flex-end"),
        |s| attr(s, "#f", "style", "justify-content:space-between;width:600px"),
    ]
);

diff!(
    grid,
    "<style>body{margin:0}#g{display:grid;grid-template-columns:100px 1fr;gap:5px;width:400px}#g > div{background:#cdc;min-height:20px}\
     .three{grid-template-columns:repeat(3,1fr)}.span{grid-column:span 2}.areas{grid-template-areas:'a b' 'c c'}</style>\
     <div id=g><div id=a>a</div><div id=b>b</div><div id=c>c</div></div>",
    [
        |s| attr(s, "#g", "class", "three"),
        |s| attr(s, "#a", "class", "span"),
        |s| {
            add(s, "#g", "div", &[("id", "d")], Some("dd"), Some("#b"));
        },
        |s| attr(s, "#g", "class", "areas"),
        |s| attr(s, "#a", "style", "grid-area:a"),
        |s| del(s, "#a"),
        |s| text(s, "#c", "tall content that wraps inside a narrow grid cell and grows the row"),
    ]
);

diff!(
    positioned_sticky_fixed,
    "<style>body{margin:0}#rel{position:relative;left:10px;top:5px;height:50px}#abs{position:absolute;left:20px;top:30px;width:50px;height:20px;background:red}\
     #fix{position:fixed;right:5px;bottom:5px;width:40px;height:40px;background:blue}#stk{position:sticky;top:0;height:20px;background:#fe0}\
     .tall{height:1500px}.z{z-index:3}.static{position:static}</style>\
     <div id=rel><div id=abs>abs</div></div><div id=stk>sticky</div><div id=fix>fix</div><div class=tall id=tall></div>",
    [
        |s| attr(s, "#abs", "style", "left:60px;top:10px"),
        |s| attr(s, "#rel", "class", "static"),
        |s| attr(s, "#abs", "class", "z"),
        |s| attr(s, "#fix", "style", "bottom:50px"),
        |s| attr(s, "#stk", "style", "top:10px;height:35px"),
        |s| del(s, "#rel"),
        |s| attr(s, "#tall", "style", "height:10px"),
    ]
);

diff!(
    display_visibility_overflow_float,
    "<style>body{margin:0}.box{width:100px;height:60px;background:#abc;margin:4px}.fl{float:left}.hid{visibility:hidden}.none{display:none}\
     .clip{overflow:hidden;height:20px}.sc{overflow:auto;height:20px}.ib{display:inline-block}</style>\
     <div class=box id=a>a<span>content content content content</span></div><div class='box fl' id=b>b</div><div class=box id=c>c</div>",
    [
        |s| attr(s, "#a", "class", "box hid"),
        |s| attr(s, "#b", "class", "box"),
        |s| attr(s, "#c", "class", "box clip"),
        |s| attr(s, "#a", "class", "box none"),
        |s| attr(s, "#c", "class", "box sc ib"),
        |s| attr(s, "#a", "class", "box fl"),
        |s| attr(s, "#b", "class", "box ib"),
    ]
);

diff!(
    inherited_properties,
    "<style>body{margin:0}#root{color:#333;font-size:16px;line-height:1.2}.pad{padding:1em}.w{width:10em;height:2em;background:#eee}</style>\
     <div id=root><div class=pad><p id=p1>text <span class=w id=w>inner</span></p><ul><li>item <b>bold</b></li></ul></div><p id=p2 style='font-size:20px'>other</p></div>",
    [
        |s| attr(s, "#root", "style", "color:red"),
        |s| attr(s, "#root", "style", "color:red;font-size:24px"),
        |s| attr(s, "#p2", "style", "font-size:inherit;color:inherit"),
        |s| attr(s, "#root", "style", "font-family:serif;letter-spacing:2px;line-height:2"),
        |s| attr(s, "#root", "style", "text-transform:uppercase;white-space:pre"),
        |s| unattr(s, "#root", "style"),
        |s| attr(s, "#w", "style", "font-size:30px"),
    ]
);

diff!(
    custom_properties,
    "<style>body{margin:0}:root{--c:red;--h:20px}.a{color:var(--c);height:var(--h);background:var(--bg,#eee)}</style>\
     <div id=w><div class=a id=a>a</div><div class=a id=b>b</div></div>",
    [
        |s| attr(s, "#w", "style", "--c:blue"),
        |s| attr(s, "#b", "style", "--h:50px;--bg:yellow"),
        |s| attr(s, "#w", "style", "--c:green;--h:35px"),
        |s| unattr(s, "#w", "style"),
        |s| {
            add(s, "head", "style", &[], Some(":root{--c:purple}"), None);
        },
    ]
);

diff!(
    dir_and_lang,
    "<style>body{margin:0;width:300px}div{padding:4px}div:dir(rtl){background:#fde;margin-left:30px}:lang(fr){color:blue;quotes:'<<' '>>'}</style>\
     <div id=a>abc def</div><div id=b dir=rtl>abc def ghi</div><div id=c lang=fr><q>quoted</q></div><p id=d>plain</p>",
    [
        |s| attr(s, "#a", "dir", "rtl"),
        |s| attr(s, "#b", "dir", "ltr"),
        |s| attr(s, "#c", "lang", "en"),
        |s| attr(s, "#d", "lang", "fr"),
        |s| attr(s, "body", "dir", "rtl"),
        |s| unattr(s, "#a", "dir"),
        |s| attr(s, "#b", "dir", "auto"),
    ]
);

diff!(
    pseudo_elements,
    "<style>body{margin:0}.p::before{content:'>> ';color:red}.p::after{content:attr(data-s);background:#eee}.q::first-line{font-weight:bold}\
     .q::first-letter{font-size:30px}</style>\
     <p class=p id=a data-s=end>one</p><p id=b class=q>second line of text here that is long enough</p>",
    [
        |s| attr(s, "#a", "data-s", "changed"),
        |s| attr(s, "#a", "class", ""),
        |s| attr(s, "#b", "class", ""),
        |s| attr(s, "#a", "class", "p q"),
        |s| attr(s, "#b", "class", "q p"),
    ]
);

diff!(
    selector_lists_not_is_where,
    "<style>body{margin:0}.a:not(.b){color:red;height:20px}:is(.a,.c) > span{background:#eee;height:30px}:where(.a) .d{margin-left:9px}\
     .a .b .d{color:blue}</style>\
     <div class=a id=x><span id=s>s</span><div class=b id=y><p class=d id=d>d</p></div></div><div class=c id=z><span>z</span></div>",
    [
        |s| attr(s, "#x", "class", "a b"),
        |s| attr(s, "#z", "class", "a"),
        |s| attr(s, "#y", "class", ""),
        |s| attr(s, "#y", "class", "b"),
        |s| del(s, "#y"),
    ]
);

diff!(
    form_controls_and_pseudo_classes,
    "<style>body{margin:0}input{width:100px;height:20px}input:checked + span{color:red}input:disabled{opacity:.5}input:not([disabled]) + span{font-size:20px}</style>\
     <input type=checkbox id=c checked><span id=cs>cs</span><br><input id=t type=text value=v><span id=ts>ts</span>",
    [
        |s| unattr(s, "#c", "checked"),
        |s| attr(s, "#t", "disabled", ""),
        |s| attr(s, "#t", "value", "other value"),
        |s| attr(s, "#c", "checked", ""),
        |s| unattr(s, "#t", "disabled"),
    ]
);

diff!(
    images,
    "<style>body{margin:0}img{display:block;margin:2px}.small{width:20px}</style>\
     <img id=i src=a.png><img id=j src=b.png width=40 height=10><p id=p>after</p>",
    [
        |s| {
            let node = q(s, "#i");
            let image = Arc::new(ImageData { width: 4, height: 2, pixels: vec![255; 4 * 2 * 4].into() });
            s.set_node_bitmap(node, Some(image)).unwrap();
        },
        |s| attr(s, "#i", "class", "small"),
        |s| {
            let node = q(s, "#j");
            let image = Arc::new(ImageData { width: 8, height: 8, pixels: vec![128; 8 * 8 * 4].into() });
            s.set_node_bitmap(node, Some(image)).unwrap();
        },
        |s| attr(s, "#j", "width", "80"),
        |s| {
            let node = q(s, "#i");
            let image = Arc::new(ImageData { width: 10, height: 30, pixels: vec![10; 10 * 30 * 4].into() });
            s.set_node_bitmap(node, Some(image)).unwrap();
        },
        |s| {
            let node = q(s, "#j");
            s.set_node_bitmap(node, None).unwrap();
        },
        |s| del(s, "#i"),
    ]
);

diff!(
    linked_stylesheet,
    "<link id=l rel=stylesheet href=a.css><style>body{margin:0}</style><p class=m id=p>p</p><p id=q>q</p>",
    [
        |s| {
            let node = q(s, "#l");
            s.set_linked_stylesheet(node, Some(".m{height:40px;color:red}".into())).unwrap();
        },
        |s| {
            let node = q(s, "#l");
            s.set_linked_stylesheet(node, Some("p{margin:9px}.m{color:blue}".into())).unwrap();
        },
        |s| attr(s, "#l", "href", "b.css"),
        |s| {
            let node = q(s, "#l");
            s.set_linked_stylesheet(node, Some("#q{height:70px}".into())).unwrap();
        },
    ]
);

diff!(
    shadow_dom,
    "<style>body{margin:0}.host{display:block;border:1px solid #333;padding:3px}.lit{color:green}</style>\
     <div class=host id=h><span class=lit id=lit>light</span></div><div class=host id=h2>second</div>",
    [
        |s| {
            let host = q(s, "#h");
            let d = s.document_mut();
            let root = d.attach_shadow(host, ShadowMode::Open).unwrap();
            let style = d
                .create(NodeKind::Element { namespace: Namespace::Html, name: Name::new("style"), attributes: Vec::new() })
                .unwrap();
            let css = d.create(NodeKind::Text("p{color:red;margin:2px}::slotted(span){background:#ffd}".into())).unwrap();
            d.append(style, css).unwrap();
            d.append(root, style).unwrap();
            let p = d
                .create(NodeKind::Element { namespace: Namespace::Html, name: Name::new("p"), attributes: Vec::new() })
                .unwrap();
            let t = d.create(NodeKind::Text("shadow text".into())).unwrap();
            d.append(p, t).unwrap();
            d.append(root, p).unwrap();
            let slot = d
                .create(NodeKind::Element { namespace: Namespace::Html, name: Name::new("slot"), attributes: Vec::new() })
                .unwrap();
            d.append(root, slot).unwrap();
        },
        |s| attr(s, "#h", "class", "host lit"),
        |s| {
            let host = q(s, "#h");
            let d = s.document_mut();
            let root = d.shadow_root(host).unwrap().unwrap();
            let style = d.first_child(root).unwrap().unwrap();
            let css = d.first_child(style).unwrap().unwrap();
            d.replace_data(css, "p{color:blue;margin:10px}::slotted(span){background:#fdd;font-size:20px}").unwrap();
        },
        |s| {
            add(s, "#h", "span", &[("id", "lit2")], Some("another light child"), None);
        },
        |s| del(s, "#lit"),
        |s| {
            let host = q(s, "#h");
            let d = s.document_mut();
            let root = d.shadow_root(host).unwrap().unwrap();
            let p = d.next_sibling(d.first_child(root).unwrap().unwrap()).unwrap().unwrap();
            d.set_attribute(p, "style", "height:50px;background:#eee").unwrap();
        },
        |s| attr(s, "#h2", "class", "host lit"),
    ]
);

diff!(
    bulk_mutations_in_one_batch,
    format!("<style>body{{margin:0}}p{{margin:1px;height:12px}}.odd{{color:red;height:20px}}.third{{display:none}}.e{{font-size:20px}}</style><div id=w>{}</div>", paras(3000)),
    [
        |s| {
            for (i, node) in all(s, "p").into_iter().enumerate() {
                if i % 2 == 1 {
                    s.document_mut().set_attribute(node, "class", "odd").unwrap();
                }
            }
        },
        |s| {
            for (i, node) in all(s, "p").into_iter().enumerate() {
                if i % 3 == 0 {
                    s.document_mut().remove(node).unwrap();
                }
            }
        },
        |s| {
            for i in 0..2200 {
                let id = format!("n{i}");
                add(s, "#w", "p", &[("id", id.as_str()), ("class", if i % 2 == 0 { "e" } else { "third" })], Some("new"), None);
            }
        },
        |s| {
            for node in all(s, "p.odd") {
                s.document_mut().remove_attribute(node, "class").unwrap();
            }
        },
    ]
);

diff!(
    mutation_between_forced_layouts,
    "<style>body{margin:0}p{margin:2px;height:20px}.t{height:40px;color:red}</style><p id=a>a</p><p id=b>b</p><p id=c>c</p>",
    [
        |s| {
            attr(s, "#a", "class", "t");
            s.with_forced_layout(|s| {
                render(s);
                let node = q(s, "#b");
                s.computed_style(node).unwrap();
            });
            attr(s, "#b", "class", "t");
            render(s);
            attr(s, "#c", "style", "margin-left:30px");
            let node = q(s, "#c");
            s.computed_style(node).unwrap();
            s.layout_rect(node);
        },
        |s| {
            del(s, "#a");
            render(s);
            add(s, "body", "p", &[("id", "d")], Some("d"), None);
            attr(s, "#d", "class", "t");
            render(s);
            del(s, "#d");
            attr(s, "#b", "class", "");
            render(s);
        },
    ]
);

diff!(
    hover_free_deep_tree_ancestor_changes,
    "<style>body{margin:0}.l1 .l3{color:red}.l2 > .l3 + .l3{height:30px}.hot .l2{padding:5px}</style>\
     <div class=l1 id=r><div class=l2 id=m><div class=l3 id=a>a</div><div class=l3 id=b>b</div></div></div>",
    [
        |s| attr(s, "#r", "class", "l1 hot"),
        |s| attr(s, "#m", "class", "l2 l0"),
        |s| del(s, "#a"),
        |s| {
            add(s, "#m", "div", &[("class", "l3"), ("id", "c")], Some("c"), Some("#b"));
        },
        |s| mv(s, "#b", "body", None),
    ]
);

struct Counts {
    styles: u64,
    boxes: u64,
    full: u64,
}

fn counts(s: &RenderSession) -> Counts {
    let stats = s.invalidation_stats();
    Counts { styles: stats.styles_computed, boxes: stats.boxes_laid_out, full: stats.full_restyles }
}

fn large_session(n: usize) -> RenderSession {
    let source = format!(
        "<style>body{{margin:0}}p{{margin:1px;height:14px}}.on{{color:red;background:#eee}}.nomatch{{color:blue}}</style><div id=w>{}</div>",
        paras(n)
    );
    let mut session = RenderSession::new(html::parse(&source, MAX_NODES).unwrap());
    session.set_verify_incremental(false);
    render(&mut session);
    session
}

#[test]
fn leaf_class_toggle_restyles_few_nodes() {
    let mut session = large_session(2000);
    let before = counts(&session);
    attr(&mut session, "#p1000", "class", "on");
    render(&mut session);
    let after = counts(&session);
    assert!(after.styles - before.styles < 50, "styles computed: {}", after.styles - before.styles);
    assert!(after.boxes - before.boxes < 300, "boxes laid out: {}", after.boxes - before.boxes);
    assert_eq!(after.full, before.full, "class toggle fell back to a full restyle");
}

#[test]
fn leaf_text_edit_restyles_few_nodes() {
    let mut session = large_session(2000);
    let before = counts(&session);
    text(&mut session, "#p500", "edited");
    render(&mut session);
    let after = counts(&session);
    assert!(after.styles - before.styles < 50, "styles computed: {}", after.styles - before.styles);
    assert!(after.boxes - before.boxes < 300, "boxes laid out: {}", after.boxes - before.boxes);
    assert_eq!(after.full, before.full);
}

#[test]
fn inserted_paragraph_does_not_restyle_document() {
    let mut session = large_session(2000);
    let before = counts(&session);
    add(&mut session, "#w", "p", &[("id", "fresh")], Some("fresh"), Some("#p1500"));
    render(&mut session);
    let after = counts(&session);
    assert!(after.styles - before.styles < 100, "styles computed: {}", after.styles - before.styles);
    assert_eq!(after.full, before.full);
}

#[test]
fn non_matching_stylesheet_append_restyles_nothing() {
    let mut session = large_session(2000);
    let before = counts(&session);
    add(&mut session, "head", "style", &[], Some(".never-used{color:red}#absent{height:1px}"), None);
    render(&mut session);
    let after = counts(&session);
    assert!(after.styles - before.styles < 50, "styles computed: {}", after.styles - before.styles);
    assert!(after.boxes - before.boxes < 300, "boxes laid out: {}", after.boxes - before.boxes);
    assert_eq!(after.full, before.full);
}

#[test]
fn matching_stylesheet_append_restyles_only_matches() {
    let mut session = large_session(2000);
    let before = counts(&session);
    add(&mut session, "head", "style", &[], Some("#p42{color:green;height:30px}"), None);
    render(&mut session);
    let after = counts(&session);
    assert!(after.styles - before.styles < 100, "styles computed: {}", after.styles - before.styles);
    assert_eq!(after.full, before.full);
}

#[test]
fn verify_mode_counts_no_mismatch_on_large_document() {
    let mut session = large_session(500);
    session.set_verify_incremental(true);
    attr(&mut session, "#p10", "class", "on");
    render(&mut session);
    add(&mut session, "#w", "p", &[("id", "x")], Some("x"), Some("#p200"));
    render(&mut session);
    del(&mut session, "#p300");
    render(&mut session);
    add(&mut session, "head", "style", &[], Some("#p5{height:50px}"), None);
    render(&mut session);
    assert_eq!(session.invalidation_stats().verify_mismatches, 0);
}

#[test]
fn frame_damage_is_reported_after_incremental_render() {
    let mut session = large_session(50);
    attr(&mut session, "#p10", "class", "on");
    render(&mut session);
    if let Some(rects) = session.frame_damage() {
        assert!(!rects.is_empty(), "a visible change must damage some area");
    }
}



diff!(
    blocks_in_table_cells_and_inline_blocks,
    "<style>body{margin:0}td{vertical-align:middle;padding:2px}td div{height:18px;margin:2px 0}.big{height:40px}\
     .ib{display:inline-block;width:120px;vertical-align:top}.ib p{margin:3px;height:14px}caption div{height:10px}</style>\
     <table><caption><div id=cap>c</div></caption><tr><td id=t1><div id=a>a</div><div id=b>b</div><div id=c>c</div></td>\
     <td id=t2><div id=d>d</div></td></tr><tr><td><div id=e>e</div></td><td><div id=f>f</div></td></tr></table>\
     <span class=ib id=i1><p id=p1>1</p><p id=p2>2</p><p id=p3>3</p></span><span class=ib><p id=p4>4</p></span>",
    [
        |s| attr(s, "#b", "class", "big"),
        |s| text(s, "#d", "longer text in the second cell"),
        |s| attr(s, "#p1", "class", "big"),
        |s| attr(s, "#cap", "class", "big"),
        |s| attr(s, "#b", "class", ""),
        |s| del(s, "#p2"),
        |s| attr(s, "#e", "style", "height:60px"),
    ]
);

diff!(
    root_class_reaches_keyed_descendants,
    "<style>body{margin:0}p{height:20px;margin:2px}.on .x{height:40px;background:red}.on .y span{color:blue;font-size:20px}\
     .card:has(.flag){border:2px solid green}.card{margin:3px}</style>\
     <div id=w><p class=x id=a>a</p><p id=b>b</p><p class=y id=c><span>s</span></p>\
     <div class=card id=k><section><i id=deep>i</i></section></div><div class=card><b>b</b></div></div>",
    [
        |s| attr(s, "html", "class", "on"),
        |s| attr(s, "#b", "class", "x"),
        |s| attr(s, "#deep", "class", "flag"),
        |s| attr(s, "html", "class", ""),
        |s| attr(s, "#deep", "class", ""),
    ]
);

diff!(
    form_state_in_and_out_of_forms,
    "<style>body{margin:0}input{width:60px}:invalid{outline:2px solid red}form:invalid{background:#fdd;padding:4px}\
     fieldset:disabled p{color:gray;height:30px}:checked{margin-left:8px}input[type=radio]{width:20px}</style>\
     <form id=f><fieldset id=fs><p>p</p><input id=i1 name=a></fieldset><input id=r1 type=radio name=g><input id=r2 type=radio name=g checked></form>\
     <input id=r3 type=radio name=h><input id=r4 type=radio name=h>",
    [
        |s| attr(s, "#i1", "required", ""),
        |s| attr(s, "#fs", "disabled", ""),
        |s| attr(s, "#r1", "checked", ""),
        |s| attr(s, "#r3", "checked", ""),
        |s| attr(s, "#i1", "type", "number"),
        |s| unattr(s, "#fs", "disabled"),
    ]
);

fn float_page() -> String {
    let body: String = (0..30)
        .map(|i| format!("<p id=p{i}><b>{i}</b> paragraph {i}</p>"))
        .collect();
    format!(
        "<style>body{{margin:0}}#fl{{float:right;width:200px;height:120px;background:#ccd}}p{{margin:2px;height:20px}}\
         .tall{{height:60px}}.wide{{padding:0 40px}}.ov b{{float:left;width:30px;height:80px}}.clr{{clear:both}}.neg{{margin-top:-30px}}\
         .root{{display:flow-root}}</style><div id=w><div id=fl>box</div>{body}</div>"
    )
}

diff!(
    retained_boxes_below_an_earlier_float,
    float_page(),
    [
        |s| attr(s, "#p20", "class", "tall"),
        |s| attr(s, "#p2", "class", "tall"),
        |s| attr(s, "#p25", "class", "wide"),
        |s| attr(s, "#fl", "style", "height:300px"),
        |s| attr(s, "#fl", "style", "height:10px"),
        |s| attr(s, "#fl", "style", "height:121px"),
        |s| attr(s, "#p28", "class", "ov"),
        |s| attr(s, "#p29", "class", "tall"),
        |s| attr(s, "#p28", "class", ""),
        |s| attr(s, "#p26", "class", "clr"),
        |s| attr(s, "#fl", "style", "height:200px"),
    ]
);

diff!(
    float_ending_where_a_retained_box_starts,
    float_page(),
    [
        |s| attr(s, "#fl", "style", "height:118px"),
        |s| attr(s, "#p10", "class", "tall"),
        |s| attr(s, "#fl", "style", "height:122px"),
        |s| attr(s, "#fl", "style", "height:240px"),
        |s| attr(s, "#p3", "class", "tall"),
        |s| attr(s, "#fl", "style", "height:60px"),
    ]
);

diff!(
    negative_margin_reaches_back_to_a_float,
    float_page(),
    [
        |s| attr(s, "#p12", "class", "neg"),
        |s| attr(s, "#fl", "style", "height:260px"),
        |s| attr(s, "#p12", "class", ""),
        |s| attr(s, "#p6", "class", "neg tall"),
        |s| attr(s, "#fl", "style", "height:140px"),
        |s| attr(s, "#p6", "class", ""),
    ]
);

diff!(
    formatting_roots_beside_and_below_a_float,
    float_page(),
    [
        |s| attr(s, "#p3", "class", "root"),
        |s| attr(s, "#p20", "class", "root"),
        |s| attr(s, "#fl", "style", "height:400px"),
        |s| attr(s, "#p20", "class", "root tall"),
        |s| attr(s, "#fl", "style", "height:20px"),
        |s| attr(s, "#p3", "class", ""),
    ]
);

diff!(
    left_and_right_floats_with_clearing_paragraphs,
    "<style>body{margin:0}.l{float:left;width:100px;height:90px}.r{float:right;width:120px;height:140px}p{margin:2px;height:20px}\
     .tall{height:60px}.cl{clear:left}.cr{clear:right}.cb{clear:both}</style>\
     <div id=w><div class=l id=fl></div><div class=r id=fr></div><p id=a>a</p><p id=b>b</p><p id=c class=cl>c</p><p id=d>d</p>\
     <p id=e class=cr>e</p><p id=f>f</p><p id=g>g</p><p id=h class=cb>h</p><p id=i>i</p><p id=j>j</p></div>",
    [
        |s| attr(s, "#j", "class", "tall"),
        |s| attr(s, "#fr", "style", "height:300px"),
        |s| attr(s, "#f", "class", "tall"),
        |s| attr(s, "#fl", "style", "height:200px"),
        |s| attr(s, "#fr", "style", "height:20px"),
        |s| attr(s, "#fl", "style", "height:10px"),
    ]
);

fn stacked_page() -> String {
    let body: String = (0..40)
        .map(|i| {
            if i % 5 == 0 {
                format!("<p id=p{i}>paragraph {i}<b class=zc id=c{i}>z</b></p>")
            } else {
                format!("<p id=p{i}>paragraph {i}</p>")
            }
        })
        .collect();
    format!(
        "<style>body{{margin:0}}#hd{{position:sticky;top:0;z-index:2;height:30px;background:#eee}}\
         #rel{{position:relative;z-index:1;height:20px;background:#dde}}p{{margin:2px;height:20px}}\
         .zc{{position:relative;z-index:3;display:block;height:10px;background:#fcc}}.off{{display:none}}.tall{{height:60px}}\
         .neg{{z-index:-1}}</style><div id=w><div id=hd>head</div><div id=rel>rel</div>{body}</div>"
    )
}

diff!(
    retained_boxes_after_stacking_escapes,
    stacked_page(),
    [
        |s| attr(s, "#p31", "class", "tall"),
        |s| attr(s, "#p10", "class", "tall"),
        |s| attr(s, "#rel", "style", "z-index:4"),
        |s| attr(s, "#c15", "class", "zc off"),
        |s| attr(s, "#hd", "style", "z-index:0"),
        |s| attr(s, "#p36", "class", "tall"),
        |s| attr(s, "#c15", "class", "zc"),
        |s| attr(s, "#hd", "style", "z-index:5;top:10px"),
        |s| attr(s, "#c20", "class", "zc neg"),
        |s| unattr(s, "#rel", "style"),
        |s| del(s, "#p22"),
    ]
);

diff!(
    stacking_escapes_with_floats,
    "<style>body{margin:0}#hd{position:sticky;top:0;z-index:2;height:30px}#fl{float:left;width:90px;height:70px}\
     p{margin:2px;height:20px}.zc{position:relative;z-index:3;display:block;height:10px}.tall{height:50px}</style>\
     <div id=w><div id=hd>h</div><div id=fl>f</div><p id=a>a<b class=zc>z</b></p><p id=b>b</p><p id=c>c</p><p id=d>d</p>\
     <p id=e>e<b class=zc>z</b></p><p id=f>f</p><p id=g>g</p><p id=h>h</p></div>",
    [
        |s| attr(s, "#h", "class", "tall"),
        |s| attr(s, "#fl", "style", "height:20px"),
        |s| attr(s, "#b", "class", "tall"),
        |s| attr(s, "#fl", "style", "height:200px"),
        |s| attr(s, "#hd", "style", "z-index:6"),
    ]
);

fn reuse_session(n: usize, extra_style: &str, head: &str) -> RenderSession {
    let source = format!(
        "<style>body{{margin:0}}p{{margin:1px;height:14px}}.on{{height:30px;background:#eee}}{extra_style}</style><div id=w>{head}{}</div>",
        paras(n)
    );
    let mut session = RenderSession::new(html::parse(&source, MAX_NODES).unwrap());
    session.set_verify_incremental(false);
    render(&mut session);
    session
}

fn assert_reuse(mut session: RenderSession, n: usize, target: &str) {
    let before = session.invalidation_stats();
    attr(&mut session, target, "class", "on");
    render(&mut session);
    let after = session.invalidation_stats();
    let laid_out = after.boxes_laid_out - before.boxes_laid_out;
    assert!(laid_out < (n / 3) as u64, "boxes laid out: {laid_out} of {n} paragraphs");
    assert!(after.fragments_replayed > before.fragments_replayed, "no fragment was replayed");
}

#[test]
fn leaf_toggle_far_below_a_float_reuses_fragments() {
    let session = reuse_session(300, "#fl{float:right;width:200px;height:120px}", "<div id=fl>box</div>");
    assert_reuse(session, 300, "#p200");
}

#[test]
fn leaf_toggle_below_a_z_indexed_sticky_header_reuses_fragments() {
    let session = reuse_session(
        300,
        "#hd{position:sticky;top:0;z-index:2;height:30px}#rel{position:relative;z-index:1;height:20px}",
        "<div id=hd>head</div><div id=rel>rel</div>",
    );
    assert_reuse(session, 300, "#p200");
}

#[test]
fn leaf_toggle_below_float_and_sticky_header_reuses_fragments() {
    let session = reuse_session(
        300,
        "#fl{float:right;width:200px;height:120px}#hd{position:sticky;top:0;z-index:2;height:30px}\
         #rel{position:relative;z-index:1;height:20px}",
        "<div id=hd>head</div><div id=rel>rel</div><div id=fl>box</div>",
    );
    assert_reuse(session, 300, "#p200");
}

#[test]
fn float_growing_over_retained_boxes_relays_them_out() {
    let mut session = reuse_session(300, "#fl{float:right;width:200px;height:120px}#fl.grow{height:4000px}", "<div id=fl>box</div>");
    let before = session.invalidation_stats();
    attr(&mut session, "#fl", "class", "grow");
    render(&mut session);
    let after = session.invalidation_stats();
    assert!(
        after.boxes_laid_out - before.boxes_laid_out >= 150,
        "boxes laid out: {}",
        after.boxes_laid_out - before.boxes_laid_out
    );
}

diff!(
    stylesheet_change_with_svg_use_mutation_in_one_frame,
    "<style id=st>body{margin:0}.t{height:20px;background:#eee}</style>\
     <svg width=40 height=40><defs><circle id=c cx=10 cy=10 r=5 fill=red></circle></defs><use href='#c'></use></svg>\
     <div class=t id=d>x</div>",
    [
        |s| {
            text(s, "#st", "body{margin:0}.t{height:60px;background:#abc}");
            attr(s, "#c", "fill", "blue");
        },
    ]
);

diff!(
    svg_use_in_another_svg_sees_class_change_in_symbol,
    "<style>body{margin:0}.r{fill:red;width:10px;height:10px}.big{fill:blue;width:30px;height:30px}</style>\
     <svg width=60 height=60><symbol id=s><rect class=r x=0 y=0 width=10 height=10></rect></symbol></svg>\
     <svg width=60 height=60><use href='#s'></use></svg><div id=d>after</div>",
    [
        |s| attr(s, ".r", "class", "big"),
        |s| attr(s, ".big", "class", "r"),
    ]
);

fn multicol_source(items: usize) -> String {
    let list: String = (0..items).map(|i| format!("<li id=i{i}>reference item number {i} with some text</li>")).collect();
    format!(
        "<style>body{{margin:0}}.refs{{columns:3 10em}}.far{{height:10px}}.on{{height:30px}}.wide{{columns:2}}</style>\
         <div class=far id=far>x</div><div style='position:relative'><div id=wrap style='display:flex'>\
         <div class=refs id=refs><ol>{list}</ol></div></div></div><div id=after>tail</div>"
    )
}

fn measured_after(session: &mut RenderSession, step: impl FnOnce(&mut RenderSession)) -> (u64, u64) {
    let before = session.invalidation_stats();
    step(session);
    render(session);
    let after = session.invalidation_stats();
    (after.intrinsics_measured - before.intrinsics_measured, after.intrinsics_reused - before.intrinsics_reused)
}

fn grid_multicol_source(columns: &str, rows: &str, items: usize) -> String {
    let list: String = (0..items).map(|i| format!("<li id=i{i}>reference item number {i} with some text</li>")).collect();
    format!(
        "<style>body{{margin:0}}.refs{{columns:3 10em}}</style>\
         <div style='display:grid;width:600px;grid-template-columns:{columns};grid-template-rows:{rows}'>\
         <div class=refs><ol>{list}</ol></div><div>side</div></div>"
    )
}

#[test]
fn grid_with_definite_fixed_and_zero_minimum_tracks_measures_no_items() {
    let measured = |columns: &str, rows: &str| {
        let mut session = RenderSession::new(html::parse(&grid_multicol_source(columns, rows, 120), MAX_NODES).unwrap());
        session.set_verify_incremental(false);
        measured_after(&mut session, |_| {}).0
    };
    let content_based = measured("1fr 100px", "60px");
    let zero_minimum = measured("minmax(0,1fr) 100px", "60px");
    let fixed = measured("400px 100px", "60px");
    let auto_rows = measured("400px 100px", "auto");
    assert!(content_based >= 120, "a content-minimum track measured {content_based}");
    assert!(auto_rows >= 120, "an auto row measured {auto_rows}");
    assert!(zero_minimum < 12, "minmax(0,1fr) measured {zero_minimum} boxes");
    assert!(fixed < 12, "fixed tracks measured {fixed} boxes");
}

#[test]
fn intrinsic_sizes_survive_a_shift_of_the_positioned_ancestor() {
    let mut session = RenderSession::new(html::parse(&multicol_source(120), MAX_NODES).unwrap());
    session.set_verify_incremental(false);
    let (first, _) = measured_after(&mut session, |_| {});
    assert!(first >= 120, "first layout measured {first}");
    let (shifted, _) = measured_after(&mut session, |s| attr(s, "#far", "style", "height:90px"));
    assert_eq!(shifted, 0, "a shift above re-measured {shifted} boxes");
    let (restyled, _) = measured_after(&mut session, |s| attr(s, "#wrap", "style", "display:flex;margin-left:3px"));
    assert_eq!(restyled, 0, "a restyled parent re-measured {restyled} boxes");
    let (edited, reused) = measured_after(&mut session, |s| text(s, "#i60", "one edited reference item with much longer text"));
    assert!(edited < 12 && reused > 0, "an edit re-measured {edited} boxes, reused {reused}");
}

diff!(
    multicol_intrinsic_content_changes,
    multicol_source(24),
    [
        |s| text(s, "#i5", "a much longer reference item that changes the max-content width of the whole column set"),
        |s| {
            add(s, "#refs ol", "li", &[("id", "extra")], Some("added item"), Some("#i3"));
        },
        |s| del(s, "#i7"),
        |s| attr(s, "#refs", "class", "refs wide"),
        |s| attr(s, "#far", "style", "height:70px"),
        |s| attr(s, "#wrap", "style", "display:flex;margin-left:9px"),
        |s| unattr(s, "#refs", "class"),
    ]
);

diff!(
    inherited_font_size_change_remeasures_intrinsic_child,
    "<style>body{margin:0}.box{float:left;border:1px solid red}.big{font-size:30px}</style>\
     <div id=far>x</div><div class=box id=box><div id=parent style='display:flex'><span id=child>inherited width text</span></div></div><p id=after>tail</p>",
    [
        |s| attr(s, "#parent", "class", "big"),
        |s| attr(s, "#box", "class", "box big"),
        |s| unattr(s, "#parent", "class"),
        |s| attr(s, "#far", "style", "height:20px"),
        |s| attr(s, "#box", "class", "box"),
    ]
);

fn table_source() -> String {
    let rows: String = (0..12).map(|i| format!("<tr><th>head {i}</th><td id=c{i}>cell value {i}</td></tr>")).collect();
    format!(
        "<style>body{{margin:0}}.box{{float:left;border:1px solid red}}table{{border-collapse:collapse}}td{{padding:2px}}\
         .wide td{{padding:12px}}</style><div id=far>x</div><div class=box id=box><table id=t>{rows}</table></div><p id=after>tail text</p>"
    )
}

diff!(
    table_cell_text_changes_intrinsic_width,
    table_source(),
    [
        |s| text(s, "#c4", "a very long cell value that makes the shrink-to-fit table much wider than before"),
        |s| text(s, "#c4", "short"),
        |s| attr(s, "#t", "class", "wide"),
        |s| {
            add(s, "#t", "tr", &[("id", "nr")], Some(""), None);
        },
        |s| attr(s, "#far", "style", "height:40px"),
        |s| del(s, "#c2"),
    ]
);

fn grid_source() -> String {
    let cells: String = (0..9).map(|i| format!("<div id=g{i}>grid item {i}</div>")).collect();
    format!(
        "<style>body{{margin:0}}.grid{{display:grid;grid-template-columns:auto auto 1fr;gap:3px;width:600px}}\
         .fit{{display:inline-grid;grid-template-columns:min-content max-content}}</style>\
         <div id=far>x</div><div class=grid id=gr>{cells}</div>"
    )
}

diff!(
    grid_intrinsic_tracks_follow_item_changes,
    grid_source(),
    [
        |s| text(s, "#g1", "a considerably longer grid item label"),
        |s| text(s, "#g4", "x"),
        |s| attr(s, "#gr", "class", "grid fit"),
        |s| text(s, "#g0", "another much longer first item"),
        |s| attr(s, "#far", "style", "height:25px"),
        |s| del(s, "#g2"),
    ]
);

fn quotes_source() -> String {
    "<style>body{margin:0}q{color:#336}.plain q::before,.plain q::after{content:none}\
     .fr{quotes:'<<' '>>' '<' '>'}</style>\
     <p id=p1>say <q id=q1>outer <q id=q2>inner</q> end</q> then <q id=q3>solo</q>.</p>\
     <p id=p2 lang=de>sag <q id=q4>aussen <q id=q5>innen</q></q></p><div id=d>after</div>"
        .to_string()
}

diff!(
    quotes_nest_and_toggle_with_classes,
    quotes_source(),
    [
        |s| attr(s, "#p1", "class", "plain"),
        |s| attr(s, "#p1", "class", ""),
        |s| attr(s, "#q1", "class", "fr"),
        |s| attr(s, "#p2", "lang", "fr"),
        |s| {
            add(s, "#d", "q", &[("id", "q6")], Some("late quote"), None);
        },
        |s| del(s, "#q2"),
        |s| attr(s, "#p1", "class", "plain"),
        |s| unattr(s, "#q1", "class"),
    ]
);

#[test]
fn quote_positions_are_reused_until_a_change_can_move_them() {
    let mut session = RenderSession::new(html::parse(&quotes_source(), MAX_NODES).unwrap());
    session.set_verify_incremental(true);
    render(&mut session);
    attr(&mut session, "#d", "style", "height:30px");
    render(&mut session);
    assert_eq!(session.invalidation_stats().verify_mismatches, 0);
    attr(&mut session, "#p1", "class", "plain");
    let toggled = render(&mut session).clone();
    attr(&mut session, "#p1", "class", "");
    let restored = render(&mut session).clone();
    assert_ne!(toggled, restored, "removing the quotes changed nothing");
    assert_eq!(session.invalidation_stats().verify_mismatches, 0);
}

use lumen_html::session::TransitionSnapshot;

fn snapshot_after(s: &mut RenderSession, previous: Option<&Arc<TransitionSnapshot>>) -> Arc<TransitionSnapshot> {
    s.transition_snapshot_after(Some(&Shaper), previous).unwrap()
}

fn assert_snapshot_matches_fresh(session: &mut RenderSession, snapshot: &TransitionSnapshot) {
    let fresh = session.transition_snapshot_with_text(Some(&Shaper)).unwrap();
    assert_eq!(snapshot.nodes.len(), fresh.nodes.len());
    for (built, expected) in snapshot.nodes.iter().zip(&fresh.nodes) {
        assert!(built.node == expected.node && built.pseudo == expected.pseudo && built.parent == expected.parent);
        assert!(*built.style == *expected.style, "style of {:?} differs from a fresh snapshot", built.node);
        assert_eq!(built.was_rendered(), expected.was_rendered());
    }
}

fn snapshot_session(n: usize, extra: &str) -> RenderSession {
    let source = format!(
        "<style>body{{margin:0}}.on{{color:red}}.hot{{opacity:.5}}p:last-child{{margin-left:3px}}#w{{color:green}}#w.warm{{color:blue}}{extra}</style><div id=w>{}</div>",
        paras(n)
    );
    let mut session = RenderSession::new(html::parse(&source, MAX_NODES).unwrap());
    render(&mut session);
    session
}

#[test]
fn transition_snapshot_styles_only_the_changed_element() {
    let mut session = snapshot_session(2000, "");
    let first = snapshot_after(&mut session, None);
    assert_eq!(session.invalidation_stats().transition_snapshots_full, 1);
    attr(&mut session, "#p500", "class", "on");
    let before = session.invalidation_stats();
    let second = snapshot_after(&mut session, Some(&first));
    let after = session.invalidation_stats();
    assert_eq!(after.transition_snapshots_incremental, before.transition_snapshots_incremental + 1);
    assert_eq!(after.transition_snapshots_full, before.transition_snapshots_full);
    assert!(after.transition_styles_computed - before.transition_styles_computed < 20,
        "styles computed: {}", after.transition_styles_computed - before.transition_styles_computed);
    assert!(after.transition_styles_reused - before.transition_styles_reused > 1900);
    let node = q(&session, "#p500");
    let changed = second.nodes.iter().find(|input| input.node == node && input.pseudo.is_none()).unwrap();
    let unchanged = first.nodes.iter().find(|input| input.node == node && input.pseudo.is_none()).unwrap();
    assert!(*changed.style != *unchanged.style);
    assert_snapshot_matches_fresh(&mut session, &second);
}

#[test]
fn transition_snapshot_restyles_inheriting_descendants_of_a_changed_parent() {
    let mut session = snapshot_session(200, "");
    let first = snapshot_after(&mut session, None);
    attr(&mut session, "#w", "class", "warm");
    let before = session.invalidation_stats();
    let second = snapshot_after(&mut session, Some(&first));
    let after = session.invalidation_stats();
    assert_eq!(after.transition_snapshots_incremental, before.transition_snapshots_incremental + 1);
    assert!(after.transition_styles_computed - before.transition_styles_computed >= 201);
    let child = q(&session, "#p7");
    let color = |snapshot: &TransitionSnapshot| snapshot.nodes.iter()
        .find(|input| input.node == child && input.pseudo.is_none()).unwrap().style.color;
    assert!(color(&first) != color(&second), "the child inherits the parent's new color");
    assert_snapshot_matches_fresh(&mut session, &second);
}

#[test]
fn transition_snapshot_computes_nothing_when_no_style_input_changed() {
    let mut session = snapshot_session(500, "");
    let first = snapshot_after(&mut session, None);
    text(&mut session, "#p3", "edited");
    let before = session.invalidation_stats();
    let second = snapshot_after(&mut session, Some(&first));
    let after = session.invalidation_stats();
    assert_eq!(after.transition_snapshots_incremental, before.transition_snapshots_incremental + 1);
    assert_eq!(after.transition_styles_computed, before.transition_styles_computed);
    assert_snapshot_matches_fresh(&mut session, &second);
}

#[test]
fn transition_snapshot_follows_sibling_index_after_an_earlier_insert() {
    let mut session = snapshot_session(6, "p{width:calc(sibling-index() * 10px)}");
    let mut snapshot = snapshot_after(&mut session, None);
    let steps: [Step; 3] = [
        |s| { add(s, "#w", "p", &[("id", "first")], Some("new"), Some("#p0")); },
        |s| del(s, "#p2"),
        |s| attr(s, "#p4", "class", "on"),
    ];
    for step in steps {
        step(&mut session);
        snapshot = snapshot_after(&mut session, Some(&snapshot));
        assert_snapshot_matches_fresh(&mut session, &snapshot);
    }
}

#[test]
fn transition_snapshot_follows_sibling_index_after_an_insert_and_a_render() {
    let mut session = snapshot_session(6, "p{width:calc(sibling-index() * 10px)}");
    let mut snapshot = snapshot_after(&mut session, None);
    add(&mut session, "#w", "p", &[("id", "first")], Some("new"), Some("#p0"));
    render(&mut session);
    snapshot = snapshot_after(&mut session, Some(&snapshot));
    assert_snapshot_matches_fresh(&mut session, &snapshot);
}

#[test]
fn transition_snapshot_follows_counters_after_an_earlier_insert() {
    let mut session = snapshot_session(6, "#w{counter-reset:c}p{counter-increment:c}p::before{content:counter(c) '. '}");
    let mut snapshot = snapshot_after(&mut session, None);
    for step in [
        (|s: &mut RenderSession| { add(s, "#w", "p", &[("id", "first")], Some("new"), Some("#p0")); }) as Step,
        |s| del(s, "#p2"),
    ] {
        step(&mut session);
        snapshot = snapshot_after(&mut session, Some(&snapshot));
        assert_snapshot_matches_fresh(&mut session, &snapshot);
    }
}

#[test]
fn transition_snapshot_follows_structural_changes_and_rule_changes() {
    let mut session = snapshot_session(100, "");
    let mut snapshot = snapshot_after(&mut session, None);
    let steps: [Step; 5] = [
        |s| { add(s, "#w", "p", &[("id", "extra")], Some("new"), None); },
        |s| del(s, "#p10"),
        |s| mv(s, "#p20", "#w", Some("#p2")),
        |s| { add(s, "#w", "style", &[], Some("#p30{color:red}"), None); },
        |s| attr(s, "#p40", "style", "color:red"),
    ];
    for (index, step) in steps.into_iter().enumerate() {
        step(&mut session);
        if index % 2 == 1 {
            render(&mut session);
        }
        snapshot = snapshot_after(&mut session, Some(&snapshot));
        assert_snapshot_matches_fresh(&mut session, &snapshot);
    }
    let stats = session.invalidation_stats();
    assert!(stats.transition_snapshots_incremental >= 3);
    assert!(stats.transition_snapshots_full >= 2, "a new stylesheet rebuilds the snapshot");
}

#[test]
fn transition_declarations_are_found_in_rules_keyframes_and_inline_styles() {
    let mut session = snapshot_session(20, "");
    assert!(!session.may_declare_transitions());
    attr(&mut session, "#p3", "style", "color:red");
    assert!(!session.may_declare_transitions());
    attr(&mut session, "#p3", "style", "opacity:0;Transition:opacity 1s");
    assert!(session.may_declare_transitions());
    assert!(snapshot_session(20, "p{transition:opacity 1s}").may_declare_transitions());
    assert!(snapshot_session(20, "p{transition-duration:var(--d)}").may_declare_transitions());
    assert!(snapshot_session(20, "@keyframes spin{to{opacity:0}}").may_declare_transitions());
    let mut added = snapshot_session(20, "");
    assert!(!added.may_declare_transitions());
    add(&mut added, "#w", "p", &[("style", "transition:none")], Some("new"), None);
    assert!(added.may_declare_transitions());
    let mut sheet = snapshot_session(20, "");
    add(&mut sheet, "#w", "style", &[], Some("p{transition-duration:1s}"), None);
    render(&mut sheet);
    assert!(sheet.may_declare_transitions());
}

#[test]
fn transition_snapshot_after_a_stylesheet_change_restyles_only_the_rule_subjects() {
    let mut session = snapshot_session(1000, "");
    let first = snapshot_after(&mut session, None);
    add(&mut session, "#w", "style", &[], Some("#p300{color:red}"), None);
    let before = session.invalidation_stats();
    let second = snapshot_after(&mut session, Some(&first));
    let after = session.invalidation_stats();
    assert_eq!(after.transition_snapshots_incremental, before.transition_snapshots_incremental + 1);
    assert_eq!(after.transition_snapshots_full, before.transition_snapshots_full);
    assert!(after.transition_styles_computed - before.transition_styles_computed < 20,
        "styles computed: {}", after.transition_styles_computed - before.transition_styles_computed);
    let node = q(&session, "#p300");
    let color = |snapshot: &TransitionSnapshot| snapshot.nodes.iter()
        .find(|input| input.node == node && input.pseudo.is_none()).unwrap().style.color;
    assert!(color(&first) != color(&second));
    assert_snapshot_matches_fresh(&mut session, &second);
}

#[test]
fn transition_snapshot_after_two_stylesheet_rebuilds_stays_incremental() {
    let mut session = snapshot_session(500, "");
    let first = snapshot_after(&mut session, None);
    add(&mut session, "#w", "style", &[], Some("#p10{color:red}"), None);
    session.computed_style(q(&session, "#p10")).unwrap();
    add(&mut session, "#w", "style", &[], Some("#p20{color:blue}"), None);
    session.computed_style(q(&session, "#p20")).unwrap();
    let before = session.invalidation_stats();
    let third = snapshot_after(&mut session, Some(&first));
    let after = session.invalidation_stats();
    assert_eq!(after.transition_snapshots_full, before.transition_snapshots_full);
    assert!(after.transition_styles_computed - before.transition_styles_computed < 20);
    assert_snapshot_matches_fresh(&mut session, &third);
}

diff!(
    ancestor_selector_filter_follows_ancestor_changes,
    "<style>body{margin:0}p{height:20px}.on .inner p{color:red;height:40px}#box > .inner > p{background:#eee}\
     DIV.mark span{font-size:24px}.off .inner p{height:60px}</style>\
     <div id=box><div class=inner><p id=a>one</p><span id=s>s</span></div></div><div class=inner><p id=b>two</p></div>",
    [
        |s| attr(s, "#box", "class", "on"),
        |s| attr(s, "#box", "class", "off"),
        |s| attr(s, "#box", "id", "other"),
        |s| attr(s, "#other", "class", "on mark"),
        |s| mv(s, "#b", ".inner", None),
        |s| { add(s, "#other", "section", &[("class", "on")], None, None); },
        |s| mv(s, ".inner", "section", None),
        |s| unattr(s, "section", "class"),
    ]
);

fn flex_chain_source(depth: usize) -> String {
    let mut open = String::new();
    let mut close = String::new();
    for level in 0..depth {
        open.push_str(&format!("<div class=f id=f{level}><p id=p{level}>level {level} <b>bold</b> trailing words</p><span id=s{level}>tag {level}</span>"));
        close.push_str("</div>");
    }
    format!(
        "<style>body{{margin:0}}.f{{display:flex;flex-wrap:wrap}}p{{margin:0}}</style><div id=far style='height:10px'>x</div>{open}{close}<div id=after>tail</div>"
    )
}

#[test]
fn nested_flex_chain_measures_linearly_and_reuses_across_relayouts() {
    let depth = 12;
    let mut session = RenderSession::new(html::parse(&flex_chain_source(depth), MAX_NODES).unwrap());
    session.set_verify_incremental(false);
    let (first, _) = measured_after(&mut session, |_| {});
    assert!(first >= depth as u64 && first <= 24 * depth as u64, "first layout measured {first}");
    let (shifted, _) = measured_after(&mut session, |s| attr(s, "#far", "style", "height:44px"));
    assert_eq!(shifted, 0, "a shift above re-measured {shifted} boxes");
    let (edited, _) = measured_after(&mut session, |s| text(s, "#p11", "an edited paragraph with quite a few more words"));
    assert!(edited > 0 && edited <= first, "an edit re-measured {edited} of {first}");
    let (restyled, _) = measured_after(&mut session, |s| attr(s, "#s6", "style", "padding:0 9px"));
    assert!(restyled > 0 && restyled <= first, "a restyle re-measured {restyled} of {first}");
}

diff!(
    flex_wrap_grid_percent_aspect_and_keyword_sizes_follow_edits,
    "<style>body{margin:0}.row{display:flex;flex-wrap:wrap;width:140px;gap:3px}.row>*{border:1px solid red}\
     .g{display:grid;grid-template-columns:auto 1fr min-content;width:220px}.pc{display:flex;width:300px}.pc>div{padding:2%}\
     .ar{display:flex}.ar>div{border:1px solid blue}</style>\
     <div id=far style='height:5px'>x</div>\
     <div class=row id=row><span id=a>alpha beta gamma</span><p id=b style='margin:0'>delta <b>epsilon</b> zeta eta</p><span id=c>x</span>\
     <div id=d style='flex:1 1 30px'>theta iota kappa</div></div>\
     <div class=g id=grid><div id=g0>one two three</div><div id=g1>four <i>five</i> six</div><div id=g2>seven eight</div><div id=g3>nine</div></div>\
     <div class=pc id=pc><div id=h style='width:50%'>half width text here</div><div id=r style='flex:1'>rest <b>of</b> the row</div></div>\
     <div class=ar id=ar><div id=t style='aspect-ratio:2/1;height:30px'>tall <i>t</i></div><div id=u style='aspect-ratio:1;width:20px'>words words</div></div>\
     <div id=mc style='width:min-content'>min content <i>wraps</i> words</div>\
     <div id=fc style='float:left;width:fit-content'>fit <b>content</b> float text</div>\
     <div id=xc style='display:flex;width:max-content'><div id=x0>max <b>content</b> row</div><div id=x1>second</div></div>",
    [
        |s| text(s, "#a", "a much longer first item that has to wrap inside the flex row"),
        |s| text(s, "#g1", "four five six seven"),
        |s| attr(s, "#row", "style", "width:90px"),
        |s| text(s, "#h", "short"),
        |s| attr(s, "#t", "style", "aspect-ratio:3/1;height:20px"),
        |s| text(s, "#mc", "min"),
        |s| attr(s, "#xc", "style", "display:flex;width:min-content"),
        |s| attr(s, "#far", "style", "height:25px"),
        |s| text(s, "#a", "alpha"),
        |s| attr(s, "#grid", "class", "g fit"),
    ]
);

fn animate(s: &mut RenderSession, sel: &str, declarations: &[(&str, &str)]) {
    let node = q(s, sel);
    let declarations = declarations.iter().map(|(name, value)| (name.to_string(), value.to_string())).collect();
    s.set_animation_declarations(node, declarations).unwrap();
}

const MARQUEE: &str = "<style>body{margin:0}#c{display:flex;overflow:hidden visible;width:300px}\
     .g{display:flex;gap:10px;min-width:100%;transform:translateX(0)}.g i{display:block;width:40px;height:20px;background:green}\
     #free{transform:translate(0,0);width:50px;height:20px;background:blue}</style>\
     <div id=c><div class=g id=a><i></i><i></i><i></i></div><div class=g id=b><i></i><i></i><i></i></div></div>\
     <p>after</p><div id=free></div>";

diff!(
    clipped_transform_samples_match_fresh_layout,
    MARQUEE,
    [
        |s| { animate(s, "#a", &[("transform", "translateX(-10px)")]); animate(s, "#b", &[("transform", "translateX(-10px)")]); },
        |s| { animate(s, "#a", &[("transform", "translateX(-25.5px)")]); animate(s, "#b", &[("transform", "translateX(-25.5px)")]); },
        |s| animate(s, "#a", &[("transform", "translateX(-40px)")]),
        |s| animate(s, "#free", &[("transform", "translate(30px,5px)")]),
        |s| animate(s, "#a", &[("transform", "translateX(-40px) rotate(10deg)")]),
    ]
);

#[test]
fn clipped_transform_sample_patches_the_frame_without_layout() {
    let mut session = RenderSession::new(html::parse(MARQUEE, MAX_NODES).unwrap());
    session.set_verify_incremental(false);
    render(&mut session);
    animate(&mut session, "#a", &[("transform", "translateX(-10px)")]);
    render(&mut session);
    let passes = session.invalidation_stats().layout_passes;
    for offset in [20, 30, 40] {
        let value = format!("translateX(-{offset}px)");
        animate(&mut session, "#a", &[("transform", &value)]);
        render(&mut session);
    }
    assert_eq!(session.invalidation_stats().layout_passes, passes, "a clipped transform sample moves the painted frame");
    animate(&mut session, "#free", &[("transform", "translate(30px,5px)")]);
    render(&mut session);
    assert_eq!(session.invalidation_stats().layout_passes, passes + 1, "an unclipped transform moves scrollable overflow");
}

diff!(
    clipped_transform_patched_frame_matches_fresh_layout,
    MARQUEE,
    [
        |s| { animate(s, "#a", &[("transform", "translateX(-10px)")]); animate(s, "#b", &[("transform", "translateX(-10px)")]); },
        |s| { animate(s, "#a", &[("transform", "translateX(-33.25px)")]); animate(s, "#b", &[("transform", "translateX(-33.25px)")]); },
    ]
);
