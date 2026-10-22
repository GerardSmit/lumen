//! Timing probe for custom-property substitution in the cascade. Ignored by default; run with
//! `cargo test --profile fast -p lumen-html --test var_cascade_bench -- --ignored --nocapture`.

use lumen_html::{
    html,
    paint::{Glyph, ShapedRun, TextShaper},
    selector,
    session::RenderSession,
};
use std::time::Instant;

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

const RULES: usize = 120;
const CARDS: usize = 3000;

fn stylesheet() -> String {
    let mut css = String::from(":root{--fg:#123;--bg:#fafafa;--space:4px;--radius:6px;--font:14px}");
    for i in 0..RULES {
        css.push_str(&format!(
            ".c{i}{{color:var(--fg);background-color:var(--bg);padding:var(--space) calc(var(--space) * 2);\
             border-radius:var(--radius);font-size:var(--font);margin:var(--m{i}, var(--space));\
             border:1px solid var(--line, var(--fg))}}"
        ));
    }
    css
}

fn page(scoped: bool) -> String {
    let mut body = String::new();
    for i in 0..CARDS {
        let style = if scoped {
            format!(" style=\"--space:{}px;--fg:#{:03x}\"", i % 7 + 2, i % 4096)
        } else {
            String::new()
        };
        body.push_str(&format!(
            "<div class=\"c{} c{}\"{style}><span class=c{}>card {i}</span></div>",
            i % RULES,
            (i * 7) % RULES,
            (i * 13) % RULES
        ));
    }
    format!("<style>{}</style><div id=root>{body}</div>", stylesheet())
}

fn time(label: &str, scoped: bool) {
    let source = page(scoped);
    let start = Instant::now();
    let mut session = RenderSession::new(html::parse(&source, 400_000).unwrap());
    session.display_list(800, 600, &Shaper).unwrap();
    let cold = start.elapsed();
    let root = {
        let d = session.document();
        selector::query_selector(d, d.root(), "#root").unwrap().unwrap()
    };
    let mut restyle = std::time::Duration::ZERO;
    for round in 0..5 {
        session
            .document_mut()
            .set_attribute(root, "style", &format!("--fg:#{round}{round}{round};--space:{}px", round + 3))
            .unwrap();
        let start = Instant::now();
        session.display_list(800, 600, &Shaper).unwrap();
        restyle += start.elapsed();
    }
    println!("{label}: cold {:.1} ms, restyle {:.1} ms/round", cold.as_secs_f64() * 1e3, restyle.as_secs_f64() * 1e3 / 5.);
}

#[test]
#[ignore]
fn custom_property_cascade_timing() {
    time("shared custom properties", false);
    time("per-element custom properties", true);
}
