use super::*;
use std::sync::Arc;
use tiny_skia::{PathBuilder, Transform};

const W: u32 = 24;
const H: u32 = 16;

fn pattern_image(width: u32, height: u32, alpha: u8) -> Arc<Rgba8Image> {
    let mut pixels = Vec::new();
    for y in 0..height {
        for x in 0..width {
            pixels.extend_from_slice(&[(x * 37 % 256) as u8, (y * 91 % 256) as u8, ((x + y) * 13 % 256) as u8, alpha]);
        }
    }
    Arc::new(Rgba8Image { width, height, pixels })
}

fn gradient_image(width: u32, height: u32) -> Arc<Rgba8Image> {
    let mut pixels = Vec::new();
    for y in 0..height {
        for x in 0..width {
            pixels.extend_from_slice(&[(x * 11) as u8, (y * 17) as u8, 200, ((x * 255) / width.max(1)) as u8]);
        }
    }
    Arc::new(Rgba8Image { width, height, pixels })
}

fn recorded() -> CanvasSurface {
    let mut surface = CanvasSurface::new_recorded(W, H).unwrap();
    assert!(surface.is_recorded());
    surface.set_recording_limits(usize::MAX, usize::MAX);
    surface
}

fn immediate() -> CanvasSurface {
    CanvasSurface::new(W, H).unwrap()
}

fn same_pixels(scenario: impl Fn(&mut CanvasSurface)) -> (CanvasSurface, CanvasSurface) {
    let mut deferred = recorded();
    let mut eager = immediate();
    scenario(&mut deferred);
    scenario(&mut eager);
    let expected = eager.snapshot();
    let actual = deferred.snapshot();
    assert_eq!(actual.width, expected.width);
    assert_eq!(actual.pixels, expected.pixels);
    (deferred, eager)
}

fn scaled(x: f32, y: f32, width: f32, height: f32) -> SourceGeometry {
    SourceGeometry::Scaled { x, y, width, height }
}

fn circle() -> tiny_skia::Path {
    PathBuilder::from_circle(10.0, 8.0, 6.0).unwrap()
}

fn star() -> tiny_skia::Path {
    let mut builder = PathBuilder::new();
    builder.move_to(2.0, 2.0);
    builder.line_to(20.0, 5.0);
    builder.line_to(8.0, 14.0);
    builder.line_to(12.0, 1.0);
    builder.close();
    builder.finish().unwrap()
}

#[test]
fn rectangles_match_immediate_mode() {
    let (deferred, _) = same_pixels(|canvas| {
        canvas.state_mut().fill = [200, 30, 60, 255];
        canvas.fill_rect(1.0, 1.0, 10.0, 7.0).unwrap();
        canvas.state_mut().stroke = [10, 220, 30, 200];
        canvas.stroke_path(&PathBuilder::from_rect(Rect::from_xywh(3.0, 3.0, 12.0, 9.0).unwrap())).unwrap();
        canvas.clear_rect(4.0, 4.0, 3.0, 3.0);
    });
    assert_eq!(deferred.pending_operations(), 0);
}

#[test]
fn paths_match_immediate_mode() {
    same_pixels(|canvas| {
        canvas.state_mut().fill = [20, 120, 220, 180];
        canvas.fill_path(&circle(), FillRule::Winding).unwrap();
        canvas.state_mut().stroke = [250, 120, 20, 255];
        canvas.state_mut().line.width = 2.5;
        canvas.stroke_path(&star()).unwrap();
        canvas.state_mut().fill = [0, 0, 0, 255];
        canvas.fill_path(&star(), FillRule::EvenOdd).unwrap();
    });
}

#[test]
fn state_combinations_match_immediate_mode() {
    let blends = [
        BlendMode::SourceOver,
        BlendMode::Multiply,
        BlendMode::Screen,
        BlendMode::Xor,
        BlendMode::DestinationOut,
        BlendMode::SourceAtop,
        BlendMode::Lighten,
        BlendMode::Difference,
    ];
    for blend in blends {
        for alpha in [1.0f32, 0.4] {
            for transformed in [false, true] {
                for clipped in [false, true] {
                    for shadow in [false, true] {
                        same_pixels(|canvas| {
                            canvas.state_mut().fill = [255, 90, 10, 255];
                            canvas.fill_rect(0.0, 0.0, W as f32, H as f32).unwrap();
                            canvas.save();
                            if clipped {
                                canvas.clip_path(&circle(), FillRule::Winding);
                            }
                            let state = canvas.state_mut();
                            state.alpha = alpha;
                            state.blend = blend;
                            if transformed {
                                state.transform = Transform::from_rotate(17.0).post_translate(6.0, 1.0);
                            }
                            if shadow {
                                state.shadow_color = [0, 0, 0, 160];
                                state.shadow_blur = 3.0;
                                state.shadow_offset_x = 2.0;
                                state.shadow_offset_y = 1.0;
                            }
                            state.fill = [20, 200, 90, 220];
                            canvas.fill_path(&star(), FillRule::Winding).unwrap();
                            let image = CanvasSource::Pixels(gradient_image(6, 5));
                            canvas.draw_source(&image, scaled(3.0, 2.0, 12.0, 8.0), true).unwrap();
                            canvas.restore();
                        });
                    }
                }
            }
        }
    }
}

#[test]
fn images_match_immediate_mode_in_all_argument_forms() {
    let image = pattern_image(8, 6, 255);
    let translucent = gradient_image(8, 6);
    let source = CanvasSource::Pixels(image.clone());
    let translucent_source = CanvasSource::Pixels(translucent.clone());
    let mut deferred = recorded();
    let mut eager = immediate();
    deferred.draw_source(&source, scaled(1.0, 1.0, 8.0, 6.0), true).unwrap();
    eager.draw_image(&image, 1.0, 1.0, 8.0, 6.0).unwrap();
    deferred.draw_source(&translucent_source, scaled(4.5, 2.25, 13.0, 9.0), true).unwrap();
    eager.draw_image(&translucent, 4.5, 2.25, 13.0, 9.0).unwrap();
    let crop = SourceGeometry::Crop { sx: 1.0, sy: 1.0, sw: 5.0, sh: 4.0, dx: 12.0, dy: 3.0, dw: 10.0, dh: 8.0 };
    deferred.draw_source(&translucent_source, crop, true).unwrap();
    eager.draw_image_crop(&translucent, 1.0, 1.0, 5.0, 4.0, 12.0, 3.0, 10.0, 8.0).unwrap();
    assert_eq!(deferred.snapshot().pixels, eager.snapshot().pixels);
}

#[test]
fn put_pixels_ignore_state_and_match_immediate_mode() {
    same_pixels(|canvas| {
        canvas.state_mut().fill = [10, 10, 200, 255];
        canvas.fill_rect(0.0, 0.0, W as f32, H as f32).unwrap();
        canvas.state_mut().alpha = 0.3;
        canvas.state_mut().blend = BlendMode::Multiply;
        let image = gradient_image(10, 8);
        canvas.put_pixels(image, 5, 4, 1, 1, 8, 6).unwrap();
    });
}

#[test]
fn patterns_and_gradients_match_and_are_detached_from_later_mutation() {
    let mut deferred = recorded();
    let mut eager = immediate();
    for canvas in [&mut deferred, &mut eager] {
        let pattern = CanvasPattern::new(&pattern_image(3, 3, 255), CanvasPatternRepetition::Repeat).unwrap();
        let gradient = CanvasGradient::new(CanvasGradientKind::Linear {
            start: [0.0, 0.0],
            end: [W as f32, 0.0],
            transform: Transform::identity(),
        });
        gradient.add_color_stop(0.0, [255, 0, 0, 255]).unwrap();
        gradient.add_color_stop(1.0, [0, 0, 255, 255]).unwrap();
        canvas.state_mut().fill_pattern = Some(pattern.clone());
        canvas.fill_rect(0.0, 0.0, 12.0, H as f32).unwrap();
        canvas.state_mut().fill_pattern = None;
        canvas.state_mut().fill_gradient = Some(gradient.clone());
        canvas.fill_rect(12.0, 0.0, 12.0, H as f32).unwrap();
        pattern.set_transform(Transform::from_translate(1.0, 1.0));
        gradient.add_color_stop(0.5, [0, 255, 0, 255]).unwrap();
    }
    assert_eq!(deferred.snapshot().pixels, eager.snapshot().pixels);
}

#[test]
fn operations_stay_pending_until_read() {
    let mut canvas = recorded();
    canvas.state_mut().fill = [1, 2, 3, 128];
    canvas.fill_rect(0.0, 0.0, 4.0, 4.0).unwrap();
    canvas.fill_path(&circle(), FillRule::Winding).unwrap();
    assert_eq!(canvas.pending_operations(), 2);
    assert!(!canvas.has_pixmap());
    canvas.snapshot();
    assert_eq!(canvas.pending_operations(), 0);
}

#[test]
fn full_clear_drops_the_list_and_the_pixmap_content() {
    let mut canvas = recorded();
    canvas.state_mut().fill = [255, 0, 0, 255];
    canvas.fill_rect(0.0, 0.0, 5.0, 5.0).unwrap();
    canvas.snapshot();
    canvas.fill_rect(2.0, 2.0, 5.0, 5.0).unwrap();
    assert_eq!(canvas.pending_operations(), 1);
    canvas.clear_rect(0.0, 0.0, W as f32, H as f32);
    assert_eq!(canvas.pending_operations(), 0);
    assert!(canvas.snapshot().pixels.iter().all(|byte| *byte == 0));
}

#[test]
fn opaque_full_cover_fill_drops_earlier_operations() {
    let mut canvas = recorded();
    canvas.fill_rect(0.0, 0.0, 3.0, 3.0).unwrap();
    canvas.fill_path(&circle(), FillRule::Winding).unwrap();
    canvas.state_mut().fill = [9, 8, 7, 255];
    canvas.fill_rect(0.0, 0.0, W as f32, H as f32).unwrap();
    assert_eq!(canvas.pending_operations(), 1);
    let pixels = canvas.snapshot().pixels;
    assert!(pixels.chunks(4).all(|pixel| pixel == [9, 8, 7, 255]));
}

#[test]
fn opaque_full_cover_image_drops_earlier_operations() {
    let mut canvas = recorded();
    canvas.fill_rect(0.0, 0.0, 3.0, 3.0).unwrap();
    let image = pattern_image(W, H, 255);
    canvas.state_mut().fill = [5, 5, 5, 255];
    canvas.fill_rect(0.0, 0.0, 2.0, 2.0).unwrap();
    canvas.draw_source(&CanvasSource::Pixels(image.clone()), scaled(0.0, 0.0, W as f32, H as f32), true).unwrap();
    assert!(canvas.pending_operations() <= 1);
    assert_eq!(canvas.snapshot().pixels, image.pixels);
}

fn pending_after_cover(setup: impl Fn(&mut CanvasSurface), alpha: u8, fill_rect_cover: bool) -> usize {
    let mut canvas = recorded();
    canvas.state_mut().fill = [200, 0, 0, 255];
    canvas.fill_rect(0.0, 0.0, 2.0, 2.0).unwrap();
    canvas.save();
    setup(&mut canvas);
    if fill_rect_cover {
        canvas.state_mut().fill = [0, 0, 200, alpha];
        canvas.fill_rect(0.0, 0.0, W as f32, H as f32).unwrap();
    } else {
        let image = CanvasSource::Pixels(pattern_image(W, H, alpha));
        canvas.draw_source(&image, scaled(0.0, 0.0, W as f32, H as f32), true).unwrap();
    }
    canvas.restore();
    canvas.pending_operations()
}

#[test]
fn covers_that_must_not_drop_earlier_operations() {
    for fill_rect_cover in [true, false] {
        let nop = |_: &mut CanvasSurface| {};
        let dropped = pending_after_cover(nop, 255, fill_rect_cover);
        assert_eq!(dropped, if fill_rect_cover { 1 } else { 0 }, "opaque cover baseline {fill_rect_cover}");
        assert_eq!(pending_after_cover(nop, 128, fill_rect_cover), 2, "transparent source {fill_rect_cover}");
        assert_eq!(
            pending_after_cover(|c| c.state_mut().alpha = 0.5, 255, fill_rect_cover),
            2,
            "globalAlpha {fill_rect_cover}"
        );
        assert_eq!(
            pending_after_cover(|c| c.state_mut().blend = BlendMode::Multiply, 255, fill_rect_cover),
            2,
            "blend {fill_rect_cover}"
        );
        assert_eq!(
            pending_after_cover(|c| c.state_mut().transform = Transform::from_rotate(10.0), 255, fill_rect_cover),
            2,
            "rotation {fill_rect_cover}"
        );
        assert_eq!(
            pending_after_cover(|c| c.clip_path(&circle(), FillRule::Winding), 255, fill_rect_cover),
            2,
            "clip {fill_rect_cover}"
        );
        assert_eq!(
            pending_after_cover(
                |c| {
                    let state = c.state_mut();
                    state.shadow_color = [0, 0, 0, 255];
                    state.shadow_offset_x = 1.0;
                },
                255,
                fill_rect_cover
            ),
            2,
            "shadow {fill_rect_cover}"
        );
    }
}

#[test]
fn partial_cover_does_not_drop() {
    let mut canvas = recorded();
    canvas.fill_rect(0.0, 0.0, 2.0, 2.0).unwrap();
    canvas.state_mut().fill = [0, 0, 255, 255];
    canvas.fill_rect(0.0, 0.0, W as f32 - 1.0, H as f32).unwrap();
    assert_eq!(canvas.pending_operations(), 2);
}

#[test]
fn clear_inside_an_open_layer_or_clip_does_not_drop() {
    let mut canvas = recorded();
    canvas.fill_rect(0.0, 0.0, 2.0, 2.0).unwrap();
    canvas.save();
    canvas.clip_path(&circle(), FillRule::Winding);
    canvas.clear_rect(0.0, 0.0, W as f32, H as f32);
    canvas.restore();
    let pending = canvas.pending_operations();
    assert!(pending >= 1);
    let mut eager = immediate();
    eager.fill_rect(0.0, 0.0, 2.0, 2.0).unwrap();
    eager.save();
    eager.clip_path(&circle(), FillRule::Winding);
    eager.clear_rect(0.0, 0.0, W as f32, H as f32);
    eager.restore();
    assert_eq!(canvas.snapshot().pixels, eager.snapshot().pixels);
}

#[test]
fn resize_discards_recording_and_content() {
    let mut canvas = recorded();
    canvas.fill_rect(0.0, 0.0, 6.0, 6.0).unwrap();
    canvas.resize(W, H).unwrap();
    assert_eq!(canvas.pending_operations(), 0);
    assert!(canvas.snapshot().pixels.iter().all(|byte| *byte == 0));
    canvas.state_mut().fill = [1, 1, 1, 255];
    canvas.fill_rect(0.0, 0.0, 1.0, 1.0).unwrap();
    canvas.resize(8, 8).unwrap();
    let snapshot = canvas.snapshot();
    assert_eq!((snapshot.width, snapshot.height), (8, 8));
    assert!(snapshot.pixels.iter().all(|byte| *byte == 0));
}

#[test]
fn operation_byte_limit_rasterizes() {
    let mut canvas = CanvasSurface::new_recorded(W, H).unwrap();
    canvas.set_recording_limits(256, usize::MAX);
    let mut path = PathBuilder::new();
    path.move_to(0.0, 0.0);
    for step in 0..200 {
        path.line_to(step as f32 % 20.0, (step * 7) as f32 % 15.0);
    }
    path.close();
    let path = path.finish().unwrap();
    canvas.state_mut().fill = [3, 4, 5, 255];
    canvas.fill_path(&path, FillRule::Winding).unwrap();
    assert_eq!(canvas.pending_operations(), 0);
    assert!(canvas.has_pixmap());
}

#[test]
fn pinned_image_limit_rasterizes() {
    let mut canvas = CanvasSurface::new_recorded(W, H).unwrap();
    canvas.set_recording_limits(usize::MAX, 1000);
    let image = CanvasSource::Pixels(pattern_image(20, 20, 128));
    canvas.draw_source(&image, scaled(0.0, 0.0, 20.0, 20.0), true).unwrap();
    assert_eq!(canvas.pending_operations(), 0);
    let mut eager = immediate();
    eager.draw_image(&pattern_image(20, 20, 128), 0.0, 0.0, 20.0, 20.0).unwrap();
    assert_eq!(canvas.snapshot().pixels, eager.snapshot().pixels);
}

#[test]
fn source_canvas_mutated_after_being_recorded_into_another_keeps_the_old_pixels() {
    let mut source = recorded();
    source.state_mut().fill = [255, 0, 0, 255];
    source.fill_rect(0.0, 0.0, 8.0, 8.0).unwrap();
    let before = source.source();

    let mut target = recorded();
    target.draw_source(&before, scaled(2.0, 2.0, 8.0, 8.0), true).unwrap();
    assert_eq!(target.pending_operations(), 1);

    source.state_mut().fill = [0, 255, 0, 255];
    source.fill_rect(0.0, 0.0, 8.0, 8.0).unwrap();
    source.snapshot();

    let mut eager = immediate();
    eager.state_mut().fill = [255, 0, 0, 255];
    eager.fill_rect(0.0, 0.0, 8.0, 8.0).unwrap();
    let frozen = eager.snapshot();
    let mut expected = immediate();
    expected.draw_image(&frozen, 2.0, 2.0, 8.0, 8.0).unwrap();
    assert_eq!(target.snapshot().pixels, expected.snapshot().pixels);
}

#[test]
fn unchanged_canvas_hands_out_one_snapshot() {
    let mut canvas = recorded();
    canvas.fill_rect(0.0, 0.0, 3.0, 3.0).unwrap();
    let (CanvasSource::Premultiplied(first), CanvasSource::Premultiplied(second)) = (canvas.source(), canvas.source()) else {
        panic!("a drawn canvas yields pixels");
    };
    assert!(Arc::ptr_eq(&first, &second));
    canvas.state_mut().fill = [0, 0, 0, 255];
    canvas.fill_rect(0.0, 0.0, 1.0, 1.0).unwrap();
    let CanvasSource::Premultiplied(third) = canvas.source() else { panic!() };
    assert!(!Arc::ptr_eq(&first, &third));
}

#[test]
fn drawing_a_canvas_into_itself_uses_its_prior_contents() {
    let scenario = |canvas: &mut CanvasSurface, recorded: bool| {
        canvas.state_mut().fill = [250, 40, 40, 255];
        canvas.fill_rect(0.0, 0.0, 10.0, 8.0).unwrap();
        canvas.state_mut().fill = [40, 40, 250, 128];
        canvas.fill_rect(5.0, 4.0, 10.0, 8.0).unwrap();
        if recorded {
            let own = canvas.source();
            canvas.draw_source(&own, scaled(8.0, 3.0, W as f32, H as f32), true).unwrap();
        } else {
            let own = canvas.snapshot();
            canvas.draw_image(&own, 8.0, 3.0, W as f32, H as f32).unwrap();
        }
        canvas.state_mut().fill = [0, 250, 0, 255];
        canvas.fill_rect(0.0, 12.0, 4.0, 4.0).unwrap();
    };
    let mut deferred = recorded();
    let mut eager = immediate();
    scenario(&mut deferred, true);
    scenario(&mut eager, false);
    assert_eq!(deferred.snapshot().pixels, eager.snapshot().pixels);
}

#[test]
fn self_draw_stays_recorded_until_read() {
    let mut canvas = recorded();
    canvas.fill_rect(0.0, 0.0, 4.0, 4.0).unwrap();
    let own = canvas.source();
    canvas.draw_source(&own, scaled(4.0, 4.0, W as f32, H as f32), true).unwrap();
    assert_eq!(canvas.pending_operations(), 1);
}

fn aliasing_canvas(has_alpha: bool) -> CanvasSurface {
    let mut canvas = recorded();
    canvas.set_has_alpha(has_alpha);
    canvas
}

fn full(image: &Arc<Rgba8Image>) -> (CanvasSource, SourceGeometry) {
    (CanvasSource::Pixels(image.clone()), scaled(0.0, 0.0, image.width as f32, image.height as f32))
}

#[test]
fn first_one_to_one_draw_aliases_the_image() {
    let image = gradient_image(W, H);
    let mut canvas = aliasing_canvas(true);
    let (source, geometry) = full(&image);
    canvas.draw_source(&source, geometry, true).unwrap();
    assert!(canvas.is_image_alias());
    assert!(!canvas.has_pixmap());
    assert_eq!(canvas.pending_operations(), 0);
    let mut eager = immediate();
    eager.draw_image(&image, 0.0, 0.0, W as f32, H as f32).unwrap();
    assert_eq!(canvas.snapshot().pixels, eager.snapshot().pixels);
    assert!(!canvas.has_pixmap());
}

#[test]
fn alias_readback_region_matches_image_without_a_pixmap() {
    let image = gradient_image(W, H);
    let mut canvas = aliasing_canvas(true);
    let (source, geometry) = full(&image);
    canvas.draw_source(&source, geometry, true).unwrap();
    let mut eager = immediate();
    eager.draw_image(&image, 0.0, 0.0, W as f32, H as f32).unwrap();
    assert_eq!(canvas.snapshot().pixels, eager.snapshot().pixels);
    assert!(canvas.is_image_alias());
    assert!(!canvas.has_pixmap());
}

#[test]
fn mutating_an_alias_leaves_the_image_and_other_aliases_unchanged() {
    let image = pattern_image(W, H, 255);
    let original = image.pixels.clone();
    let mut first = aliasing_canvas(true);
    let mut second = aliasing_canvas(true);
    let (source, geometry) = full(&image);
    first.draw_source(&source, geometry, true).unwrap();
    second.draw_source(&source, geometry, true).unwrap();
    assert!(first.is_image_alias() && second.is_image_alias());

    first.state_mut().fill = [255, 255, 255, 255];
    first.fill_rect(0.0, 0.0, 5.0, 5.0).unwrap();
    let mutated = first.snapshot();
    assert!(!first.is_image_alias());
    assert_eq!(&mutated.pixels[..4], &[255, 255, 255, 255]);
    assert_eq!(image.pixels, original);
    assert!(second.is_image_alias());
    assert_eq!(second.snapshot().pixels, original);
}

#[test]
fn alias_followed_by_translucent_draw_matches_immediate_mode() {
    same_pixels(|canvas| {
        canvas.set_has_alpha(true);
        let image = gradient_image(W, H);
        let (source, geometry) = full(&image);
        canvas.draw_source(&source, geometry, true).unwrap();
        canvas.state_mut().fill = [10, 200, 30, 90];
        canvas.fill_path(&circle(), FillRule::Winding).unwrap();
        let other = CanvasSource::Pixels(pattern_image(5, 5, 100));
        canvas.draw_source(&other, scaled(3.0, 3.0, 5.0, 5.0), true).unwrap();
    });
}

#[test]
fn alias_resized_or_cleared_becomes_blank() {
    let image = pattern_image(W, H, 255);
    let (source, geometry) = full(&image);
    let mut canvas = aliasing_canvas(true);
    canvas.draw_source(&source, geometry, true).unwrap();
    canvas.clear_rect(0.0, 0.0, W as f32, H as f32);
    assert!(canvas.snapshot().pixels.iter().all(|byte| *byte == 0));
    let mut canvas = aliasing_canvas(true);
    canvas.draw_source(&source, geometry, true).unwrap();
    canvas.resize(W, H).unwrap();
    assert!(!canvas.is_image_alias());
    assert!(canvas.snapshot().pixels.iter().all(|byte| *byte == 0));
}

#[test]
fn opaque_canvas_does_not_alias_a_transparent_image() {
    let transparent = pattern_image(W, H, 90);
    let (source, geometry) = full(&transparent);
    let mut canvas = aliasing_canvas(false);
    canvas.draw_source(&source, geometry, true).unwrap();
    assert!(!canvas.is_image_alias());
    let mut eager = immediate();
    eager.set_has_alpha(false);
    eager.draw_image(&transparent, 0.0, 0.0, W as f32, H as f32).unwrap();
    assert_eq!(canvas.snapshot().pixels, eager.snapshot().pixels);
}

#[test]
fn opaque_canvas_aliases_an_opaque_image() {
    let opaque = pattern_image(W, H, 255);
    let (source, geometry) = full(&opaque);
    let mut canvas = aliasing_canvas(false);
    canvas.draw_source(&source, geometry, true).unwrap();
    assert_eq!(canvas.is_image_alias(), source.is_opaque() == Some(true));
    assert_eq!(canvas.snapshot().pixels, opaque.pixels);
}

#[test]
fn alias_conditions_that_must_fail() {
    let image = pattern_image(W, H, 255);
    let (source, geometry) = full(&image);
    let fails = |setup: &dyn Fn(&mut CanvasSurface), geometry: SourceGeometry| {
        let mut canvas = aliasing_canvas(true);
        setup(&mut canvas);
        canvas.draw_source(&source, geometry, true).unwrap();
        assert!(!canvas.is_image_alias());
    };
    fails(&|c| c.state_mut().alpha = 0.5, geometry);
    fails(&|c| c.state_mut().blend = BlendMode::Multiply, geometry);
    fails(&|c| c.state_mut().transform = Transform::from_translate(1.0, 0.0), geometry);
    fails(&|c| c.clip_path(&circle(), FillRule::Winding), geometry);
    fails(
        &|c| {
            c.state_mut().shadow_color = [0, 0, 0, 255];
            c.state_mut().shadow_offset_x = 1.0;
        },
        geometry,
    );
    fails(&|_| {}, scaled(1.0, 0.0, W as f32, H as f32));
    fails(&|_| {}, scaled(0.0, 0.0, W as f32 / 2.0, H as f32));

    let mut canvas = aliasing_canvas(true);
    canvas.fill_rect(0.0, 0.0, 1.0, 1.0).unwrap();
    let translucent = pattern_image(W, H, 90);
    let (translucent_source, translucent_geometry) = full(&translucent);
    canvas.draw_source(&translucent_source, translucent_geometry, true).unwrap();
    assert!(!canvas.is_image_alias());

    let mut canvas = aliasing_canvas(true);
    canvas.fill_rect(0.0, 0.0, 1.0, 1.0).unwrap();
    canvas.draw_source(&source, geometry, true).unwrap();
    let mut eager = immediate();
    eager.fill_rect(0.0, 0.0, 1.0, 1.0).unwrap();
    eager.draw_image(&image, 0.0, 0.0, W as f32, H as f32).unwrap();
    assert_eq!(canvas.snapshot().pixels, eager.snapshot().pixels);

    let smaller = pattern_image(W - 1, H, 255);
    let mut canvas = aliasing_canvas(true);
    canvas.draw_source(&CanvasSource::Pixels(smaller), scaled(0.0, 0.0, (W - 1) as f32, H as f32), true).unwrap();
    assert!(!canvas.is_image_alias());
}

#[test]
fn premultiply_helpers_agree_with_tiny_skia() {
    for alpha in 0..=255u8 {
        for channel in 0..=255u8 {
            let expected = tiny_skia::ColorU8::from_rgba(channel, channel, channel, alpha).premultiply();
            assert_eq!(premultiply_channel(channel, alpha), expected.red(), "c={channel} a={alpha}");
        }
    }
}

#[test]
fn premultiply_round_trip_is_idempotent() {
    for alpha in 0..=255u8 {
        for channel in (0..=255u8).step_by(5) {
            let once = stored_pixel(&[channel, channel, channel, alpha]);
            let twice = stored_pixel(&once);
            assert_eq!(once, twice, "c={channel} a={alpha}");
        }
    }
}

#[test]
fn disabled_recording_draws_immediately() {
    let mut canvas = CanvasSurface::new(W, H).unwrap();
    assert!(!canvas.is_recorded());
    canvas.fill_rect(0.0, 0.0, 2.0, 2.0).unwrap();
    assert_eq!(canvas.pending_operations(), 0);
    assert!(canvas.has_pixmap());
}

#[test]
fn premultiplied_storage_survives_straight_round_trip() {
    for alpha in 0..=255u8 {
        for channel in 0..=alpha {
            let color = tiny_skia::PremultipliedColorU8::from_rgba(channel, channel, channel, alpha).unwrap();
            let straight = color.demultiply();
            assert_eq!(premultiply_channel(straight.red(), alpha), channel, "c={channel} a={alpha}");
        }
    }
}

fn painted_source() -> CanvasSurface {
    let mut surface = immediate();
    surface.state_mut().fill = [200, 30, 90, 77];
    surface.fill_path(&circle(), FillRule::Winding).unwrap();
    surface.state_mut().fill = [10, 220, 40, 200];
    surface.fill_rect(2.0, 3.0, 9.0, 7.0).unwrap();
    surface
}

#[test]
fn premultiplied_canvas_source_draws_like_straight_pixels() {
    let geometries = [
        scaled(1.0, 2.0, 20.0, 11.0),
        scaled(3.5, 1.5, 7.25, 9.0),
        scaled(10.0, 8.0, -9.0, -6.0),
        SourceGeometry::Crop { sx: 2.0, sy: 3.0, sw: 11.0, sh: 9.0, dx: 4.0, dy: 2.0, dw: 15.0, dh: 12.0 },
        SourceGeometry::Crop { sx: -3.0, sy: 1.0, sw: 40.0, sh: 30.0, dx: 0.0, dy: 0.0, dw: 24.0, dh: 16.0 },
    ];
    for geometry in geometries {
        for recording in [false, true] {
            let mut source = painted_source();
            let premultiplied = source.source();
            assert!(matches!(premultiplied, CanvasSource::Premultiplied(_)));
            let straight = CanvasSource::Pixels(Arc::new(source.snapshot()));
            let mut left = if recording { recorded() } else { immediate() };
            let mut right = if recording { recorded() } else { immediate() };
            for target in [&mut left, &mut right] {
                target.state_mut().fill = [5, 5, 200, 255];
                target.fill_rect(0.0, 0.0, W as f32, H as f32).unwrap();
            }
            left.draw_source(&premultiplied, geometry, true).unwrap();
            right.draw_source(&straight, geometry, true).unwrap();
            assert_eq!(left.snapshot().pixels, right.snapshot().pixels, "{geometry:?} recording={recording}");
        }
    }
}

#[test]
fn premultiplied_source_is_shared_until_the_canvas_changes() {
    let mut source = painted_source();
    let (CanvasSource::Premultiplied(first), CanvasSource::Premultiplied(second)) = (source.source(), source.source()) else {
        panic!("expected premultiplied sources");
    };
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(first.straight().pixels, source.snapshot().pixels);
}
