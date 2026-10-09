//! Device edge snapping for untransformed CSS rectangular box painting.
//! Geometry and hit testing retain CSS coordinates. Transformed descendants
//! retain antialiased geometry; replay applies their transform afterwards.
use lumen_html::paint::{BackgroundPaint, Command, DisplayList, Rect};

fn rectangle(rect: &mut Rect, scale: f32) {
    let edge = |value: f32| (value * scale + 0.5).floor() / scale;
    let right = edge(rect.x + rect.width);
    let bottom = edge(rect.y + rect.height);
    rect.x = edge(rect.x);
    rect.y = edge(rect.y);
    rect.width = (right - rect.x).max(0.0);
    rect.height = (bottom - rect.y).max(0.0);
}

pub(crate) fn boxes(list: &mut DisplayList, scale: f32) {
    let mut transforms = 0usize;
    for command in &mut list.0 {
        match command {
            Command::PushTransform(_) => transforms += 1,
            Command::PopTransform => transforms = transforms.saturating_sub(1),
            Command::PushBoxClip(rect) | Command::FillRect { rect, .. } | Command::FillRoundedRect { rect, .. }
                | Command::Image { rect, .. } | Command::ReservedImage { rect, .. }
                if transforms == 0 => rectangle(rect, scale),
            // A CSS layer result uses the same box contour as its ordinary
            // background and border. SVG source/path clipping retains user
            // coordinates; a transformed layer retains its fractional contour.
            Command::PushLayer{rect,clip:true,svg_clip:None,..} if transforms==0=>rectangle(rect,scale),
            Command::FillBackground(fill) if transforms == 0 => {
                let origin = (fill.positioning_rect.x, fill.positioning_rect.y);
                rectangle(&mut fill.rect, scale);
                rectangle(&mut fill.positioning_rect, scale);
                if matches!(fill.image, BackgroundPaint::Border(_)) {
                    // The nine-slice area itself is a CSS border box.
                    rectangle(&mut fill.image_rect, scale);
                } else {
                    // A background tile retains its size and position within
                    // the snapped positioning box, including negative offsets.
                    fill.image_rect.x += fill.positioning_rect.x - origin.0;
                    fill.image_rect.y += fill.positioning_rect.y - origin.1;
                }
            }
            Command::StrokeBorder {
                rect,
                radius,
                width,
                ..
            } if transforms == 0 && *radius == 0.0 => {
                rectangle(rect, scale);
                if *width > 0.0 {
                    *width = (*width * scale).floor().max(1.0) / scale;
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_html::paint::{Affine, Rgba};
    // /html/rendering/non-replaced-elements/the-fieldset-and-legend-elements/absolute-fixed-in-legend.html
    // /html/rendering/non-replaced-elements/the-fieldset-and-legend-elements/fieldset-baseline.html
    #[test]
    fn specification_css_layer_clips_share_snapped_boxes_and_preserve_source_modes() {
        use std::sync::Arc;
        use crate::{GlyphCache,RasterizationMode};
        use lumen_html::paint::{SvgClip,SvgLayerClip,SvgGradientUnits};
        let font=crate::default_font().unwrap();let green=Rgba{r:0,g:128,b:0,a:255};
        let canvas=Command::FillRect{rect:Rect{x:0.0,y:0.0,width:32.0,height:32.0},color:Rgba{r:255,g:255,b:255,a:255}};
        for scale in [1.0,1.25,2.0] {for phase in [-0.375,0.125,0.875] {for opacity in [0.25,0.75,1.0] {
            let rect=Rect{x:4.0+phase,y:6.0+phase,width:16.0,height:12.0};
            let layer=Command::PushLayer{rect,radius:0.0,corners:None,opacity,clip:true,svg_clip:None,filters:None};
            let source=DisplayList(vec![canvas.clone(),layer.clone(),Command::FillRect{rect:Rect{x:0.0,y:0.0,width:32.0,height:32.0},color:green},Command::PopLayer]);
            let reference=DisplayList(vec![canvas.clone(),Command::FillRect{rect,color:Rgba{a:(255.0*opacity).round()as u8,..green}}]);
            let draw=|list:&DisplayList,mode|crate::render_with_mode_cached(list,32,32,scale,mode,font,&mut GlyphCache::default()).unwrap();
            assert_eq!(draw(&source,RasterizationMode::CssPixelSnapped),draw(&reference,RasterizationMode::CssPixelSnapped),"CSS result clipping and ordinary boxes share their device contour and opacity");
            assert_ne!(draw(&source,RasterizationMode::Antialiased),draw(&source,RasterizationMode::CssPixelSnapped),"unsnapped antialiased coverage remains an independent supported mode");
            let transformed=DisplayList(vec![canvas.clone(),Command::PushTransform(Affine::IDENTITY),layer.clone(),Command::FillRect{rect:Rect{x:0.0,y:0.0,width:32.0,height:32.0},color:green},Command::PopLayer,Command::PopTransform]);
            assert_eq!(draw(&transformed,RasterizationMode::CssPixelSnapped),draw(&transformed,RasterizationMode::Antialiased),"transform scopes preserve fractional source/result coverage");
            assert!(matches!(source.0[1],Command::PushLayer{rect:original,..} if original==rect),"render mode never changes CSS layout coordinates");
            let mut untouched=DisplayList(vec![Command::PushLayer{rect,radius:0.0,corners:None,opacity,clip:false,svg_clip:None,filters:None},
                Command::PushLayer{rect,radius:0.0,corners:None,opacity,clip:true,svg_clip:Some(Arc::new(SvgLayerClip{transform:Affine::IDENTITY,
                    clip:Arc::new(SvgClip{units:SvgGradientUnits::ObjectBoundingBox,transform:Affine::IDENTITY,shapes:Arc::from([])})})),filters:None}]);
            boxes(&mut untouched,scale);
            assert!(untouched.0.iter().all(|command|matches!(command,Command::PushLayer{rect:original,..} if *original==rect)),"filter metadata and SVG object-box/path contours remain in their source coordinate system");
        }}}
    }

    #[test]
    fn specification_snapped_css_layer_clip_preserves_fractional_source_alpha_across_roi_partitions() {
        use crate::{FilterPixelRegion,Rgba8Image};
        for scale in [1.0,1.25,2.0] {for radius in [0.0,2.7] {for opacity in [0.25,0.75,1.0] {
            let raw=Rect{x:4.125,y:6.875,width:16.33,height:12.27};
            let mut list=DisplayList(vec![Command::PushLayer{rect:raw,radius,corners:None,opacity,clip:true,svg_clip:None,filters:None}]);
            boxes(&mut list,scale);let Command::PushLayer{rect,..}=list.0[0] else{panic!("layer")};
            let source=Rgba8Image{width:64,height:64,pixels:(0..4096).flat_map(|index|[0,128,0,if index%2==0{127}else{255}]).collect()};
            let mut full=source.clone();crate::apply_layer_result_coverage(&mut full,rect,radius,None,opacity,true,None,None,scale,0.0,0.0,0).unwrap();
            let region=FilterPixelRegion{left:0,top:0,width:64,height:64};let mut joined=source.clone();
            for row in (0..64u32).step_by(7) {for column in (0..64u32).step_by(11) {
                let tile_region=FilterPixelRegion{left:i64::from(column),top:i64::from(row),width:11.min(64-column),height:7.min(64-row)};
                let mut tile=crate::crop_filter_region(&source,region,tile_region,0).unwrap();
                crate::apply_layer_result_coverage(&mut tile,rect,radius,None,opacity,true,None,None,scale,column as f32,row as f32,0).unwrap();
                for y in 0..tile.height {let at=((row+y)*64+column)as usize*4;let from=(y*tile.width)as usize*4;
                    joined.pixels[at..at+tile.width as usize*4].copy_from_slice(&tile.pixels[from..from+tile.width as usize*4]);}
            }}
            if let Some((index,(actual,expected)))=joined.pixels.chunks_exact(4).zip(full.pixels.chunks_exact(4)).enumerate().find(|(_, (actual,expected))|actual!=expected) {
                panic!("partitioned clip differs: scale={scale}, radius={radius}, opacity={opacity}, pixel=({},{}), joined={actual:?}, full={expected:?}",index%64,index/64);
            }
            if radius==0.0 {let x=(rect.x*scale)as usize+2;let y=(rect.y*scale)as usize+2;let at=(y*64+x)*4;
                assert_eq!(full.pixels[at+3],(source.pixels[at+3]as f32*opacity).round()as u8,"clip does not erase pre-existing source antialiasing");}
        }}}
    }

    #[test]
    fn specification_css_box_clips_share_device_edges_and_preserve_ink_clips() {
        use crate::{GlyphCache, RasterizationMode};
        let white=Rgba{r:255,g:255,b:255,a:255};
        let red=Rgba{r:255,g:0,b:0,a:255};
        let lime=Rgba{r:0,g:255,b:0,a:255};
        let black=Rgba{r:0,g:0,b:0,a:255};
        let font=crate::default_font().unwrap();
        for scale in [1.0,1.25,2.0] {
            let rect=Rect{x:8.25,y:44.0,width:320.0,height:35.5};
            let canvas=Command::FillRect{rect:Rect{x:0.0,y:0.0,width:340.0,height:100.0},color:white};
            let clipped=DisplayList(vec![canvas.clone(),Command::PushBoxClip(rect),
                Command::FillRect{rect,color:red},Command::FillRect{rect,color:lime},Command::PopClip]);
            let ordinary=DisplayList(vec![canvas.clone(),Command::FillRect{rect,color:lime}]);
            let draw=|list:&DisplayList,mode|crate::render_with_mode_cached(list,340,100,scale,mode,font,&mut GlyphCache::default()).unwrap();
            clipped.validate().unwrap();
            assert_eq!(draw(&clipped,RasterizationMode::CssPixelSnapped),draw(&ordinary,RasterizationMode::CssPixelSnapped),
                "box contour cannot re-antialias backgrounds already snapped to its device edge");
            let border=Command::StrokeBorder{rect,radius:0.0,width:2.0,color:black};
            let clipped_border=DisplayList(vec![canvas.clone(),Command::PushBoxClip(rect),border.clone(),Command::PopClip]);
            let ordinary_border=DisplayList(vec![canvas.clone(),border]);
            assert_eq!(draw(&clipped_border,RasterizationMode::CssPixelSnapped),draw(&ordinary_border,RasterizationMode::CssPixelSnapped),
                "a fieldset border clip without a legend preserves the ordinary border contour");
            let transformed=DisplayList(vec![canvas.clone(),Command::PushTransform(Affine::IDENTITY),
                Command::PushBoxClip(rect),Command::FillRect{rect,color:red},Command::FillRect{rect,color:lime},
                Command::PopClip,Command::PopTransform]);
            assert_eq!(draw(&transformed,RasterizationMode::CssPixelSnapped),draw(&transformed,RasterizationMode::Antialiased),
                "transformed clip and box retain fractional coverage");
            let ink=DisplayList(vec![canvas,Command::PushClip(rect),Command::FillRect{rect,color:lime},Command::PopClip]);
            let mut snapped=ink.clone();boxes(&mut snapped,scale);
            assert!(matches!(snapped.0[1],Command::PushClip(value) if value==rect),
                "generic glyph/SVG clipping keeps exact fractional source edges");
            assert_ne!(draw(&ink,RasterizationMode::CssPixelSnapped),draw(&ordinary,RasterizationMode::CssPixelSnapped),
                "the distinction has observable coverage, not merely an enum tag");
        }
    }

    #[test]
    fn specification_css_pixel_snapping_replaced_png_matches_solid_box_and_preserves_transforms() {
        use std::sync::Arc;
        use lumen_html::paint::ImageData;
        use crate::{GlyphCache, RasterizationMode, Rgba8Image};

        let black = Rgba { r: 0, g: 0, b: 0, a: 255 };
        let white = Rgba { r: 255, g: 255, b: 255, a: 255 };
        let source = Rgba8Image {
            width: 3,
            height: 2,
            pixels: [0, 0, 0, 255].repeat(6),
        };
        let decoded = crate::decode_raster_image_with_limit(&crate::encode_png(&source), 24).unwrap();
        let image = Arc::new(ImageData {
            width: decoded.width,
            height: decoded.height,
            pixels: decoded.pixels,
        });
        let font = crate::default_font().unwrap();
        let canvas = Command::FillRect {
            rect: Rect { x: 0.0, y: 0.0, width: 9.0, height: 6.0 },
            color: white,
        };
        for scale in [1.0, 2.0] {
            for x in [1.25, -0.25] {
                let rect = Rect { x, y: 1.875, width: 5.0, height: 1.0 };
                let solid = DisplayList(vec![canvas.clone(), Command::FillRect { rect, color: black }]);
                let replaced = DisplayList(vec![canvas.clone(), Command::Image { rect, image: image.clone() }]);
                let draw = |list: &DisplayList, mode| crate::render_with_mode_cached(
                    list, 9, 6, scale, mode, font, &mut GlyphCache::default(),
                ).unwrap();
                let snapped = draw(&solid, RasterizationMode::CssPixelSnapped);
                assert_eq!(draw(&replaced, RasterizationMode::CssPixelSnapped), snapped,
                    "replaced PNG and solid CSS box share device edges, including viewport clipping");
                let antialiased = draw(&replaced, RasterizationMode::Antialiased);
                assert_eq!(draw(&solid, RasterizationMode::Antialiased), antialiased);
                assert_ne!(antialiased, snapped, "raw fractional coverage remains available");
                let transformed = DisplayList(vec![
                    canvas.clone(), Command::PushTransform(Affine::IDENTITY),
                    Command::Image { rect, image: image.clone() }, Command::PopTransform,
                ]);
                assert_eq!(draw(&transformed, RasterizationMode::CssPixelSnapped), antialiased,
                    "transformed contents retain fractional image coverage");
                let mut commands = replaced.clone();
                boxes(&mut commands, scale);
                assert!(matches!(&commands.0[1], Command::Image { image: retained, .. }
                    if Arc::ptr_eq(retained, &image)), "snapping retains the shared decoded pixels");
                assert!(matches!(&replaced.0[1], Command::Image { rect: original, .. }
                    if *original == rect), "rendering does not mutate source CSS geometry");
            }
        }
    }
    #[test]
    fn specification_css_pixel_snapping_border_image_matches_solid_edges_and_preserves_source() {
        use std::sync::Arc;
        use lumen_html::paint::{BackgroundFill, BackgroundRepeat, BorderImagePaint, BoxBorder};
        use crate::{GlyphCache, RasterizationMode};
        let green = Rgba { r: 0, g: 128, b: 0, a: 255 };
        let red = Rgba { r: 255, g: 0, b: 0, a: 255 };
        let rect = Rect { x: 8.0, y: 49.875, width: 100.0, height: 100.0 };
        let source = Arc::new(BorderImagePaint {
            image: BackgroundPaint::Solid(green),
            fallback: BoxBorder { rect, radius: 0.0, colors: [red; 4], widths: [40.0; 4],
                pattern: None, side_patterns: None, corners: None },
            source_size: [100.0; 2], slices: [1.0; 4], widths: [40.0; 4],
            repeat: [lumen_html::css::BorderImageRepeat::Stretch; 2], fill: true,
        });
        let canvas = Command::FillRect {
            rect: Rect { x: 0.0, y: 0.0, width: 120.0, height: 170.0 }, color: red,
        };
        let border = Command::FillBackground(Box::new(BackgroundFill {
            rect, radius: 0.0, corners: None, positioning_rect: rect, image_rect: rect,
            repeat: [BackgroundRepeat::NoRepeat; 2], image: BackgroundPaint::Border(source.clone()),
        }));
        let image = DisplayList(vec![canvas.clone(), border.clone()]);
        let reference = DisplayList(vec![canvas.clone(), Command::FillRect { rect, color: green }]);
        let font = crate::default_font().unwrap();
        for scale in [1.0, 1.25, 2.0] {
            let draw = |list: &DisplayList, mode| crate::render_with_mode_cached(
                list, 120, 170, scale, mode, font, &mut GlyphCache::default(),
            ).unwrap();
            assert_eq!(draw(&image, RasterizationMode::CssPixelSnapped),
                draw(&reference, RasterizationMode::CssPixelSnapped),
                "border image and solid boxes share their device contour");
            let transformed = DisplayList(vec![canvas.clone(), Command::PushTransform(Affine::IDENTITY),
                border.clone(), Command::PopTransform]);
            assert_eq!(draw(&transformed, RasterizationMode::CssPixelSnapped),
                draw(&transformed, RasterizationMode::Antialiased),
                "transformed border geometry retains fractional coverage");
            let mut snapped = image.clone();
            boxes(&mut snapped, scale);
            assert!(matches!(&snapped.0[1], Command::FillBackground(fill)
                if matches!(&fill.image, BackgroundPaint::Border(retained) if Arc::ptr_eq(retained, &source))),
                "device edges reuse the original nine-slice source");
        }
        assert!(matches!(&image.0[1], Command::FillBackground(fill) if fill.rect == rect),
            "CSS layout and hit geometry remain unchanged");
    }

    #[test]
    fn specification_css_pixel_snapping_background_tiles_and_curved_boxes_share_device_origins() {
        use std::sync::Arc;
        use lumen_html::paint::{BackgroundFill, BackgroundRepeat, ImageData};
        use crate::{GlyphCache, RasterizationMode};
        let green = Rgba { r: 0, g: 128, b: 0, a: 255 };
        let red = Rgba { r: 255, g: 0, b: 0, a: 255 };
        let rect = Rect { x: 8.125, y: 49.875, width: 100.0, height: 100.0 };
        let image = Arc::new(ImageData { width: 1, height: 1, pixels: vec![0, 128, 0, 255] });
        let background = Command::FillBackground(Box::new(BackgroundFill {
            rect, radius: 0.0, corners: None, positioning_rect: rect,
            image_rect: Rect { x: rect.x - 16.0, y: rect.y - 32.0, width: 48.0, height: 48.0 },
            repeat: [BackgroundRepeat::Repeat; 2], image: BackgroundPaint::Image(image.clone()),
        }));
        let font = crate::default_font().unwrap();
        for scale in [1.0, 1.25, 2.0] {
            let draw = |list: &DisplayList, mode| crate::render_with_mode_cached(
                list, 120, 170, scale, mode, font, &mut GlyphCache::default(),
            ).unwrap();
            let actual = DisplayList(vec![background.clone()]);
            let reference = DisplayList(vec![Command::FillRect { rect, color: green }]);
            assert_eq!(draw(&actual, RasterizationMode::CssPixelSnapped),
                draw(&reference, RasterizationMode::CssPixelSnapped),
                "free background tiles use the same snapped CSS contour as a solid fill");
            let mixed = DisplayList(vec![Command::FillRoundedRect {
                rect, color: red, radius: 40.0, corners: None,
            }, background.clone()]);
            assert_eq!(draw(&mixed, RasterizationMode::CssPixelSnapped),
                draw(&reference, RasterizationMode::CssPixelSnapped),
                "a rounded background cannot leak beyond its shared opaque box contour");
            let mut snapped = actual.clone();
            boxes(&mut snapped, scale);
            let Command::FillBackground(fill) = &snapped.0[0] else { panic!("background carrier") };
            assert_eq!((fill.image_rect.width, fill.image_rect.height), (48.0, 48.0));
            assert_eq!((fill.image_rect.x - fill.positioning_rect.x,
                fill.image_rect.y - fill.positioning_rect.y), (-16.0, -32.0));
            assert!(matches!(&fill.image, BackgroundPaint::Image(retained) if Arc::ptr_eq(retained, &image)));
            let transformed = DisplayList(vec![Command::PushTransform(Affine::IDENTITY),
                background.clone(), Command::PopTransform]);
            assert_eq!(draw(&transformed, RasterizationMode::CssPixelSnapped),
                draw(&transformed, RasterizationMode::Antialiased));
        }
    }

    #[test]
    fn css_boxes_snap_edges_but_transformed_children_keep_fractional_geometry() {
        let rect = Rect {
            x: 11.5,
            y: 4.25,
            width: 30.0,
            height: 20.0,
        };
        let color = Rgba {
            r: 255,
            g: 255,
            b: 255,
            a: 255,
        };
        let mut list = DisplayList(vec![
            Command::FillRect { rect, color },
            Command::PushTransform(Affine::IDENTITY),
            Command::FillRect { rect, color },
            Command::PopTransform,
        ]);
        boxes(&mut list, 1.0);
        assert!(matches!(
            list.0[0],
            Command::FillRect {
                rect: Rect {
                    x: 12.0,
                    y: 4.0,
                    width: 30.0,
                    height: 20.0
                },
                ..
            }
        ));
        assert!(matches!(list.0[2], Command::FillRect { rect: original, .. } if original == rect));
    }
}
