//! SVG titles closing before their siblings and grid line names derived from `-start` / `-end` lines.
use lumen_html::{
    html,
    paint::{Command, Glyph, ShapedRun, TextShaper},
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

fn commands(markup: &str, width: u32, height: u32) -> Vec<Command> {
    let document = html::parse(markup, 64).unwrap();
    lumen_html::layout::display_list(&document, width, height, &Text).unwrap().0
}

#[test]
fn svg_title_end_tag_closes_the_title_so_siblings_are_painted() {
    let with_title = commands(
        "<style>body{margin:0}</style><svg width=20 height=20 viewBox='0 0 20 20'><title>logo</title>\
         <path d='M0 0H20V20H0Z' fill='red'/></svg>",
        100,
        50,
    );
    let without_title = commands(
        "<style>body{margin:0}</style><svg width=20 height=20 viewBox='0 0 20 20'>\
         <path d='M0 0H20V20H0Z' fill='red'/></svg>",
        100,
        50,
    );
    let empty = commands("<style>body{margin:0}</style><svg width=20 height=20 viewBox='0 0 20 20'></svg>", 100, 50);
    assert!(without_title.len() > empty.len());
    assert_eq!(with_title.len(), without_title.len());
}

fn red_boxes(list: Vec<Command>) -> Vec<(f32, f32)> {
    list.into_iter()
        .filter_map(|command| match command {
            Command::FillRect { rect, color } if color.a == 255 && rect.height == 4.0 => Some((rect.x, rect.width)),
            _ => None,
        })
        .collect()
}

#[test]
fn bare_grid_column_ident_uses_the_start_and_end_named_lines() {
    let list = commands(
        "<style>body{margin:0}#g{display:grid;width:300px;\
         grid-template-columns:[full-start] 20px [content-start] 100px [content-end] 20px [full-end]}\
         #g>div{grid-column:content;height:4px;background:red}</style><div id=g><div></div></div>",
        300,
        50,
    );
    assert_eq!(red_boxes(list), [(20.0, 100.0)]);
}

#[test]
fn bare_grid_column_ident_spans_from_one_area_to_another() {
    let list = commands(
        "<style>body{margin:0}#g{display:grid;width:300px;\
         grid-template-columns:[full-start] 20px [content-start] 100px [content-end] 20px [full-end]}\
         #g>div{grid-column:full;height:4px;background:red}</style><div id=g><div></div></div>",
        300,
        50,
    );
    assert_eq!(red_boxes(list), [(0.0, 140.0)]);
}
