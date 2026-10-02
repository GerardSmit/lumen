use lumen_html::{html, layout, paint::{Command, Rect, ShapedRun, TextShaper}};

struct Font;
impl TextShaper for Font {
    fn shape(&self, text: &str, _: f32) -> Result<ShapedRun, ()> { Ok(ShapedRun { glyphs: Vec::new(), width: text.len() as f32 * 8.0 }) }
    fn ascent(&self, _: f32) -> f32 { 12.0 }
    fn line_height(&self, _: f32) -> f32 { 16.0 }
}
fn boxes(source: &str) -> Vec<Rect> {
    let doc = html::parse(source, 128).unwrap();
    layout::display_list(&doc, 320, 240, &Font).unwrap().0.into_iter().filter_map(|command| match command {
        Command::FillRect { rect, color } if color.r == 255 && color.g == 0 => Some(rect), _ => None,
    }).collect()
}
#[test]
fn positioned_boxes_leave_normal_flow() {
    let rects = boxes("<style>body{margin:0}section{position:relative;width:100px;height:80px}div{background:red;width:20px;height:10px}#a{position:absolute;left:30px;top:15px}#b{position:fixed;right:10px;bottom:10px}</style><section><div id=a></div><div></div></section><div id=b></div>");
    assert_eq!(rects, vec![Rect{x:0.0,y:0.0,width:20.0,height:10.0}, Rect { x:30.0,y:15.0,width:20.0,height:10.0 }, Rect{x:290.0,y:220.0,width:20.0,height:10.0}]);
}
#[test]
fn absolute_bottom_uses_auto_height_ancestor() {
    let rects = boxes("<style>body{margin:0}section{position:relative;width:100px}div{background:red;width:20px;height:40px}#a{position:absolute;bottom:5px;right:5px;height:10px}</style><section><div id=a></div><div></div></section>");
    assert_eq!(rects[1], Rect { x:75.0,y:25.0,width:20.0,height:10.0 });
}
#[test]
fn float_sides_and_clear() {
    let rects = boxes("<style>body{margin:0}div{background:red;height:30px;width:40px}#a{float:left}#b{float:right}#c{clear:both}</style><div id=a></div><div id=b></div><div id=c></div>");
    assert_eq!(rects[0].x, 0.0); assert_eq!(rects[1].x, 280.0); assert_eq!(rects[2].y, 30.0);
}
#[test]
fn asymmetric_boxes_and_sibling_margins() {
    let rects = boxes("<style>body{margin:0}div{background:red;width:20px;height:10px;margin:10px 20px 30px 40px;padding:1px 2px 3px 4px}</style><div></div><div></div>");
    assert_eq!(rects[0], Rect{x:40.0,y:10.0,width:26.0,height:14.0});
    assert_eq!(rects[1].y, 54.0);
}

#[test]
fn fixed_bottom_right_uses_viewport() {
    let doc = html::parse(include_str!("../../lumen-html-image/tests/render/position.html"),128).unwrap();
    let commands = layout::display_list(&doc,64,64,&Font).unwrap();
    let rect = commands.0.iter().find_map(|command| match command {
        Command::FillRect{rect,color} if color.g == 255 && color.r == 0 => Some(*rect), _=>None,
    }).unwrap();
    assert_eq!(rect,Rect{x:14.0,y:34.0,width:30.0,height:20.0});
}

#[test]
fn flex_align_self_overrides_parent() {
    let rects = boxes("<style>body{margin:0}main{display:flex;height:80px;width:100px}div{width:20px;height:20px;background:red}#a{align-self:center}#b{align-self:flex-end}</style><main><div id=a></div><div id=b></div></main>");
    assert_eq!(rects[0].y,30.0); assert_eq!(rects[1].y,60.0);
}

#[test]
fn absolute_flex_child_uses_alignment_without_consuming_space() {
    let rects=boxes("<style>body{margin:0}main{position:relative;display:flex;width:100px;height:80px;justify-content:center;align-items:center}div{position:absolute;width:20px;height:10px;background:red}</style><main><div></div></main>");
    assert_eq!(rects[0],Rect{x:40.0,y:35.0,width:20.0,height:10.0});
}

#[test]
fn flex_order_and_auto_margins() {
    let rects=boxes("<style>body{margin:0}main{display:flex;width:100px;height:80px}div{width:20px;height:20px;background:red}#a{order:1;margin-left:auto;margin-top:auto;margin-bottom:auto}</style><main><div id=a></div><div></div></main>");
    assert_eq!(rects[0],Rect{x:0.0,y:0.0,width:20.0,height:20.0});
    assert_eq!(rects[1],Rect{x:80.0,y:30.0,width:20.0,height:20.0});
}

#[test]
fn flex_line_distribution_and_wrap_reverse() {
    for (property, first, last) in [("align-content:space-between",0.0,80.0),("flex-wrap:wrap-reverse",80.0,30.0)] {
        let source=format!("<style>body{{margin:0}}main{{display:flex;flex-wrap:wrap;width:30px;height:100px;{property}}}div{{width:20px;height:20px;background:red}}</style><main><div></div><div></div></main>");
        let rects=boxes(&source);
        assert_eq!(rects[0].y,first); assert_eq!(rects[1].y,last);
    }
}

#[test]
fn percentage_sizes_use_parent_content_box() {
    let rects=boxes("<style>body{margin:0}main{width:200px;height:100px}div{width:50%;height:50%;background:red;padding-left:10%}</style><main><div></div></main>");
    assert_eq!(rects[0],Rect{x:0.0,y:0.0,width:120.0,height:50.0});
}

#[test]
fn percentage_flex_basis_and_height_constraints() {
    for flex in ["flex-grow:1;flex-shrink:1;flex-basis:20%","flex:1 1 20%"] {
        let source=format!("<style>body{{margin:0}}main{{display:flex;flex-direction:column;width:100px;height:100px}}div{{{flex};max-height:30px;background:red;width:20px}}</style><main><div></div><div></div></main>");
        let rects=boxes(&source);
        assert_eq!(rects[0].height,30.0);assert_eq!(rects[1].height,30.0);assert_eq!(rects[1].y,30.0);
    }
}

#[test]
fn gradient_preserves_background_color_and_final_bounds() {
    let doc=html::parse("<style>body{margin:0}div{width:80px;height:40px;background-color:red;background-image:linear-gradient(to right,transparent,blue)}</style><div></div>",128).unwrap();
    let commands=layout::display_list(&doc,320,240,&Font).unwrap();
    let gradient=commands.0.iter().position(|command| matches!(command,Command::FillLinearGradient{..})).unwrap();
    assert!(matches!(commands.0[gradient-1],Command::FillRect{color,..} if color.r==255 && color.g==0));
    assert!(matches!(commands.0[gradient],Command::FillLinearGradient{rect,..} if rect.width==80.0 && rect.height==40.0));
}

fn text_runs(source:&str)->Vec<(f32,f32)> {
    let doc=html::parse(source,128).unwrap();
    layout::display_list(&doc,320,240,&Font).unwrap().0.into_iter().filter_map(|command|match command {Command::GlyphRun{origin_x,baseline_y,..}=>Some((origin_x,baseline_y)),_=>None}).collect()
}
#[test]
fn text_alignment_and_whitespace_modes() {
    for (align,x) in [("left",0.0),("center",32.0),("right",64.0),("end",64.0)] {
        let source=format!("<style>body{{margin:0}}div{{width:80px;text-align:{align}}}</style><div>ab</div>");
        assert_eq!(text_runs(&source)[0].0,x);
    }
    for whitespace in ["pre","pre-wrap","pre-line","break-spaces"] {
        let source=format!("<style>body{{margin:0}}div{{width:80px;white-space:{whitespace}}}</style><div>a\nb</div>");
        let runs=text_runs(&source);assert_eq!(runs.len(),2);assert_eq!(runs[1].1-runs[0].1,16.0);
    }
    assert_eq!(text_runs("<style>body{margin:0}div{width:16px;white-space:nowrap}</style><div>a b c</div>").len(),1);
}

#[test]
fn ancestor_text_decoration_reaches_inline_text() {
    let rects=boxes("<style>body{margin:0}div{color:red;text-decoration:underline line-through}span{color:blue}</style><div><span>ab</span></div>");
    assert_eq!(rects.len(),2);
    assert_eq!(rects[0].width,16.0);assert_eq!(rects[1].width,16.0);
    assert!(rects[1].y<rects[0].y);
}
