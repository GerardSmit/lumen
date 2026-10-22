//! Flex shrinking with margins and shrink-wrapped letter-spaced text.
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

fn commands(markup: &str, width: u32, height: u32) -> Vec<Command> {
    let document = html::parse(markup, 64).unwrap();
    lumen_html::layout::display_list(&document, width, height, &Text).unwrap().0
}

#[test]
fn flex_shrink_weights_use_the_inner_base_size_not_the_margin_box() {
    let list = commands(
        "<style>body{margin:0}#row{display:flex;width:200px}#row>div{width:100%;height:4px;min-width:0;background:red}\
         #row>div+div{margin:0 10px;background:green}#row>div+div+div{margin:0;background:red}</style>\
         <div id=row><div></div><div></div><div></div></div>",
        200,
        50,
    );
    let boxes: Vec<(f32, f32)> = list
        .into_iter()
        .filter_map(|command| match command {
            Command::FillRect { rect, color } if color.a == 255 && rect.height == 4.0 => Some((rect.x, rect.width)),
            _ => None,
        })
        .collect();
    assert_eq!(boxes, [(0.0, 60.0), (70.0, 60.0), (140.0, 60.0)]);
}

#[test]
fn shrink_wrapped_letter_spaced_heading_stays_on_one_line() {
    let list = commands(
        "<style>body{margin:0}#wrap{display:inline-block}h2{margin:0;letter-spacing:1px;font-size:10px}</style>\
         <div id=wrap><h2>\n        Why Rust?\n      </h2></div>",
        400,
        100,
    );
    let mut baselines: Vec<f32> = list
        .into_iter()
        .filter_map(|command| match command {
            Command::GlyphRun { baseline_y, .. } => Some(baseline_y),
            _ => None,
        })
        .collect();
    baselines.dedup();
    assert_eq!(baselines.len(), 1, "heading wrapped: {baselines:?}");
}

#[test]
fn four_percentage_floats_summing_to_the_container_share_one_row() {
    let list = commands(
        "<style>body{margin:0}#row{width:1152px}.w{width:23.40426%;float:left;margin-right:2.12766%;height:4px;background:red}\
         .w.last{float:right;margin-right:0}</style>\
         <div id=row><div class=w></div><div class=w></div><div class=w></div><div class='w last'></div></div>",
        1280,
        100,
    );
    let ys: Vec<f32> = list
        .into_iter()
        .filter_map(|command| match command {
            Command::FillRect { rect, color } if color.a == 255 && rect.height == 4.0 => Some(rect.y),
            _ => None,
        })
        .collect();
    assert_eq!(ys, [0.0, 0.0, 0.0, 0.0]);
}

fn clips_over_glyphs(list: &[Command]) -> Vec<Vec<lumen_html::paint::Rect>> {
    let mut stack = Vec::new();
    let mut found = Vec::new();
    for command in list {
        match command {
            Command::PushClip(rect) | Command::PushBoxClip(rect) => stack.push(*rect),
            Command::PopClip => {
                stack.pop();
            }
            Command::GlyphRun { .. } => found.push(stack.clone()),
            _ => {}
        }
    }
    found
}

#[test]
fn legacy_clip_rect_hides_an_absolutely_positioned_box() {
    let list = commands(
        "<style>body{margin:0}#a{position:absolute;left:10px;top:10px;width:80px;height:20px;clip:rect(0 0 0 0)}</style>\
         <div id=a>hidden</div>",
        200,
        100,
    );
    let runs = clips_over_glyphs(&list);
    assert_eq!(runs.len(), 1);
    assert!(runs[0].iter().any(|rect| rect.width == 0.0 || rect.height == 0.0), "clip stack: {:?}", runs[0]);
}

#[test]
fn legacy_clip_rect_offsets_are_relative_to_the_border_box() {
    let list = commands(
        "<style>body{margin:0}#a{position:absolute;left:10px;top:20px;width:80px;height:40px;clip:rect(5px 30px auto 4px)}</style>\
         <div id=a>shown</div>",
        200,
        100,
    );
    let runs = clips_over_glyphs(&list);
    let clip = runs[0].last().copied().expect("clip over glyphs");
    assert_eq!((clip.x, clip.y, clip.width, clip.height), (14.0, 25.0, 26.0, 35.0));
}

#[test]
fn legacy_clip_rect_ignores_statically_positioned_boxes() {
    let list = commands("<style>body{margin:0}#a{clip:rect(0 0 0 0)}</style><div id=a>shown</div>", 200, 100);
    assert!(clips_over_glyphs(&list)[0].is_empty());
}
