use super::{blend_colors, composite, SourceCache, BLEND_RUN, gradient::{GradientAxis, Prepared}, Raster};
use lumen_html::paint::{BackgroundPaint, BackgroundRepeat, ImageData, Rect, Rgba};

enum MixedPaint<'a> {
    Solid(lumen_html::paint::Rgba),
    Border(&'a lumen_html::paint::BorderImagePaint,Box<MixedPaint<'a>>),
    Image(&'a ImageData),
    Gradient(Prepared<'a>, usize),
    Mix(Vec<(MixedPaint<'a>, f32)>),
}
impl<'a> MixedPaint<'a> {
    fn new(image: &'a BackgroundPaint, tile: Rect) -> Option<Self> {
        Some(match image {
            BackgroundPaint::Solid(color) => Self::Solid(*color),
            BackgroundPaint::Border(border)=>Self::Border(border,Box::new(Self::new(&border.image,Rect{x:0.0,y:0.0,width:border.source_size[0],height:border.source_size[1]})?)),
            BackgroundPaint::Image(image) => Self::Image(image),
            BackgroundPaint::Worklet(image) => match &image.pixels {
                Some(pixels) => Self::Image(&pixels.image),
                None => Self::Solid(Rgba { r: 0, g: 0, b: 0, a: 0 }),
            },
            BackgroundPaint::Gradient(g) => Self::Gradient(Prepared::new(g, tile)?, 0),
            BackgroundPaint::CrossFade(items) => Self::Mix(
                items
                    .iter()
                    .map(|(i, w)| Some((Self::new(i, tile)?, *w)))
                    .collect::<Option<Vec<_>>>()?,
            ),
        })
    }
    fn premultiplied(&mut self, x: f32, y: f32, width: f32, height: f32) -> [f32; 4] {self.premultiplied_clipped(x,y,width,height,None)}
    fn premultiplied_clipped(&mut self, x: f32, y: f32, width: f32, height: f32, crop:Option<[f32;4]>) -> [f32; 4] {
        let color = match self {
            Self::Solid(c) => *c,
            Self::Border(border,paint)=>{
                let Some(([x,y],crop))=border.source_coordinate(x,y,width,height) else{return [0.0;4];};
                return paint.premultiplied_clipped(x,y,border.source_size[0],border.source_size[1],Some(crop));
            },
            Self::Image(i) => {
                if i.is_valid() {
                    let mut x=x/width;let mut y=y/height;
                    if let Some(crop)=crop {
                        let clamp=|value:f32,start:f32,end:f32,extent:f32,pixels:u32|{let center=0.5/pixels as f32;let lo=start/extent+center;let hi=end/extent-center;if lo<=hi{value.clamp(lo,hi)}else{(start+end)/(2.0*extent)}};
                        x=clamp(x,crop[0],crop[2],width,i.width);y=clamp(y,crop[1],crop[3],height,i.height);
                    }
                    sample_image(i,x,y)
                } else {
                    lumen_html::paint::Rgba {
                        r: 0,
                        g: 0,
                        b: 0,
                        a: 0,
                    }
                }
            }
            Self::Gradient(g, hint) => g.color_at(x, g.row_term(y), hint),
            Self::Mix(items) => {
                let mut rgba = [0.0f32; 4];
                for (image, weight) in items {
                    let c = image.premultiplied_clipped(x, y, width, height,crop);
                    for channel in 0..4 {
                        rgba[channel] += c[channel] * *weight;
                    }
                }
                return rgba;
            }
        };
        let alpha = color.a as f32 / 255.0;
        [
            color.r as f32 * alpha,
            color.g as f32 * alpha,
            color.b as f32 * alpha,
            alpha,
        ]
    }
    fn sample(&mut self, x: f32, y: f32, width: f32, height: f32) -> lumen_html::paint::Rgba {
        use lumen_html::paint::Rgba;
        let rgba = self.premultiplied(x, y, width, height);
        if rgba[3] <= 0.0 {
            return Rgba {
                r: 0,
                g: 0,
                b: 0,
                a: 0,
            };
        }
        Rgba {
            r: (rgba[0] / rgba[3]).round().clamp(0.0, 255.0) as u8,
            g: (rgba[1] / rgba[3]).round().clamp(0.0, 255.0) as u8,
            b: (rgba[2] / rgba[3]).round().clamp(0.0, 255.0) as u8,
            a: (rgba[3] * 255.0).round().clamp(0.0, 255.0) as u8,
        }
    }
}

struct Axis {
    start: f32,
    tile: f32,
    step: f32,
    count: Option<f32>,
}
impl Axis {
    fn new(start: f32, tile: f32, origin: f32, length: f32, repeat: BackgroundRepeat) -> Self {
        match repeat {
            BackgroundRepeat::Round => {
                let count = (length / tile).round().max(1.0);
                let tile = length / count;
                Self {
                    start,
                    tile,
                    step: tile,
                    count: None,
                }
            }
            BackgroundRepeat::Space => {
                let count = (length / tile).floor();
                if count >= 2.0 {
                    Self {
                        start: origin,
                        tile,
                        step: (length - tile) / (count - 1.0),
                        count: Some(count),
                    }
                } else {
                    Self {
                        start,
                        tile,
                        step: tile,
                        count: Some(1.0),
                    }
                }
            }
            BackgroundRepeat::NoRepeat => Self {
                start,
                tile,
                step: tile,
                count: Some(1.0),
            },
            BackgroundRepeat::Repeat => Self {
                start,
                tile,
                step: tile,
                count: None,
            },
        }
    }
    fn sample(&self, value: f32) -> Option<f32> {
        let relative = value - self.start;
        let index = (relative / self.step).floor();
        if self
            .count
            .is_some_and(|count| index < 0.0 || index >= count)
        {
            return None;
        }
        let offset = relative - index * self.step;
        (offset >= 0.0 && offset < self.tile).then_some(offset)
    }
}

pub(super) fn fill(
    raster: &mut Raster<'_>,
    corners: Option<&[[f32; 2]; 4]>,
    rect: Rect,
    radius: f32,
    positioning_rect: Rect,
    image_rect: Rect,
    repeat: [BackgroundRepeat; 2],
    image: &BackgroundPaint,
) {
    if let BackgroundPaint::Border(border)=image {
        if !border.image.is_available() {
            let mut fallback=border.fallback.clone();
            fallback.rect.x+=image_rect.x;fallback.rect.y+=image_rect.y;
            super::border::draw_box(raster,&fallback);
            return;
        }
    }
    if image_rect.width <= 0.0 || image_rect.height <= 0.0 {
        return;
    }
    let Some(visible) = raster.visible_rect(rect) else {
        return;
    };
    let xaxis = Axis::new(
        image_rect.x,
        image_rect.width,
        positioning_rect.x,
        positioning_rect.width,
        repeat[0],
    );
    let yaxis = Axis::new(
        image_rect.y,
        image_rect.height,
        positioning_rect.y,
        positioning_rect.height,
        repeat[1],
    );
    if xaxis.tile <= 0.0
        || yaxis.tile <= 0.0
        || ![xaxis.tile, xaxis.step, yaxis.tile, yaxis.step]
            .iter()
            .all(|v| v.is_finite())
    {
        return;
    }
    let tile = Rect {
        x: 0.0,
        y: 0.0,
        width: xaxis.tile,
        height: yaxis.tile,
    };
    let mut mixed = if matches!(image, BackgroundPaint::CrossFade(_)|BackgroundPaint::Border(_)) {
        MixedPaint::new(image, tile)
    } else {
        None
    };
    let gradient = match image {
        BackgroundPaint::Gradient(g) => Prepared::new(g, tile),
        _ => None,
    };
    if matches!(image, BackgroundPaint::Gradient(_)) && gradient.is_none() {
        return;
    }
    let scale = raster.scale;
    let (x0, y0, x1, y1) = raster.pixel_span(visible);
    let columns: Vec<Option<f32>> = (x0..x1)
        .map(|x| xaxis.sample(raster.css_x(x as f32 + 0.5)))
        .collect();
    let (left, top, right, bottom) = (
        raster.device_x(rect.x),
        raster.device_y(rect.y),
        raster.device_x(rect.x + rect.width),
        raster.device_y(rect.y + rect.height),
    );
    let radii = match corners {
        Some(corners) => corners.map(|r| [r[0] * scale, r[1] * scale]),
        None => {
            let radius = (radius * scale).max(0.0).min((right - left) * 0.5).min((bottom - top) * 0.5);
            [[radius; 2]; 4]
        }
    };
    let solid_interior = matches!(image, BackgroundPaint::Solid(_)) && columns.iter().all(Option::is_some);
    let mut hint = 0;
    let mut lut = gradient
        .as_ref()
        .map(|g| g.lut((x1.saturating_sub(x0)) as usize * (y1.saturating_sub(y0)) as usize));
    let axis = gradient.as_ref().map_or(GradientAxis::Free, |g| g.axis());
    let transparent = Rgba { r: 0, g: 0, b: 0, a: 0 };
    let column_colors: Vec<Rgba> = match (&gradient, &mut lut) {
        (Some(g), Some(lut)) if axis == GradientAxis::Horizontal => {
            let row = g.row_term(0.0);
            columns
                .iter()
                .map(|px| px.map_or(transparent, |px| g.color_lut(lut, px, row, &mut hint)))
                .collect()
        }
        _ => Vec::new(),
    };
    for y in y0..y1 {
        let Some(py) = yaxis.sample(raster.css_y(y as f32 + 0.5)) else {
            continue;
        };
        let row = gradient.as_ref().map_or(0.0, |g| g.row_term(py));
        let row_color = match (&gradient, &mut lut) {
            (Some(g), Some(lut)) if axis == GradientAxis::Vertical => {
                Some(g.color_lut(lut, columns.iter().flatten().next().copied().unwrap_or(0.0), row, &mut hint))
            }
            _ => None,
        };
        let (full_start, full_end) = super::coverage::full_span(y as f32, left, top, right, bottom, radii);
        let interior = (full_start.max(i64::from(x0)).min(i64::from(x1)) as u32)
            ..(full_end.min(i64::from(x1)).max(i64::from(x0)) as u32);
        let mut leading = x0..x1;
        let mut trailing = x1..x1;
        if solid_interior && interior.start < interior.end {
            let row_start = y as usize * raster.image.width as usize;
            let span = (row_start + interior.start as usize) * 4..(row_start + interior.end as usize) * 4;
            if let BackgroundPaint::Solid(color) = image {
                if color.a == 255 {
                    let pixel = [color.r, color.g, color.b, 255];
                    for target in raster.image.pixels[span].chunks_exact_mut(4) {
                        target.copy_from_slice(&pixel);
                    }
                } else {
                    super::blend_solid_span(&mut raster.image.pixels[span], *color);
                }
            }
            leading = x0..interior.start;
            trailing = interior.end..x1;
        } else if interior.start < interior.end {
            let row_start = y as usize * raster.image.width as usize;
            let span = (row_start + interior.start as usize) * 4..(row_start + interior.end as usize) * 4;
            let covered = &columns[(interior.start - x0) as usize..(interior.end - x0) as usize];
            let base = (interior.start - x0) as usize;
            let target = &mut raster.image.pixels[span];
            let handled = match image {
                BackgroundPaint::Gradient(_) => match axis {
                    GradientAxis::Horizontal => {
                        fill_span(target, covered, |i, _| column_colors[base + i]);
                        true
                    }
                    GradientAxis::Vertical => {
                        let color = row_color.unwrap();
                        fill_span(target, covered, |_, _| color);
                        true
                    }
                    GradientAxis::Free => {
                        let g = gradient.as_ref().unwrap();
                        let lut = lut.as_mut().unwrap();
                        fill_span_runs(target, covered, |_, columns, out| g.color_lut_run(lut, columns, row, &mut hint, out));
                        true
                    }
                },
                BackgroundPaint::Image(image) => {
                    fill_span(target, covered, |_, px| sample_image(image, px / xaxis.tile, py / yaxis.tile));
                    true
                }
                BackgroundPaint::Worklet(image) => match &image.pixels {
                    Some(pixels) => {
                        fill_span(target, covered, |_, px| {
                            sample_image(&pixels.image, px / xaxis.tile, py / yaxis.tile)
                        });
                        true
                    }
                    None => false,
                },
                _ => false,
            };
            if handled {
                leading = x0..interior.start;
                trailing = interior.end..x1;
            }
        }
        for x in leading.chain(trailing) {
            let px = &columns[(x - x0) as usize];
            let Some(px) = *px else {
                continue;
            };
            let coverage = if (x as i64) >= full_start && (x as i64) < full_end {
                1.0
            } else if let Some(corners) = corners {
                if raster.antialias {
                    super::coverage::rounded_corners(
                        x as f32,
                        y as f32,
                        raster.device_x(rect.x),
                        raster.device_y(rect.y),
                        raster.device_x(rect.x + rect.width),
                        raster.device_y(rect.y + rect.height),
                        corners.map(|r| [r[0] * scale, r[1] * scale]),
                    )
                } else {
                    rect.contains_corners(
                        raster.css_x(x as f32 + 0.5),
                        raster.css_y(y as f32 + 0.5),
                        corners,
                    ) as u8 as f32
                }
            } else if raster.antialias {
                super::coverage::rounded(
                    x as f32,
                    y as f32,
                    raster.device_x(rect.x),
                    raster.device_y(rect.y),
                    raster.device_x(rect.x + rect.width),
                    raster.device_y(rect.y + rect.height),
                    radius * scale,
                )
            } else {
                rect.contains_rounded(raster.css_x(x as f32 + 0.5), raster.css_y(y as f32 + 0.5), radius)
                    as u8 as f32
            };
            if coverage == 0.0 {
                continue;
            }
            let color = match image {
                BackgroundPaint::Solid(color) => *color,
                BackgroundPaint::CrossFade(_)|BackgroundPaint::Border(_) => {
                    let Some(paint) = mixed.as_mut() else {
                        continue;
                    };
                    paint.sample(px, py, xaxis.tile, yaxis.tile)
                }
                BackgroundPaint::Gradient(_) => match axis {
                    GradientAxis::Horizontal => column_colors[(x - x0) as usize],
                    GradientAxis::Vertical => row_color.unwrap(),
                    GradientAxis::Free => gradient.as_ref().unwrap().color_lut(lut.as_mut().unwrap(), px, row, &mut hint),
                },
                BackgroundPaint::Image(image) => {
                    sample_image(image, px / xaxis.tile, py / yaxis.tile)
                }
                BackgroundPaint::Worklet(image) => match &image.pixels {
                    Some(pixels) => sample_image(&pixels.image, px / xaxis.tile, py / yaxis.tile),
                    None => continue,
                },
            };
            let offset = (y as usize * raster.image.width as usize + x as usize) * 4;
            let pixel = &mut raster.image.pixels[offset..offset + 4];
            if coverage == 1.0 && color.a == 255 {
                pixel.copy_from_slice(&[color.r, color.g, color.b, 255]);
            } else {
                composite(pixel, color, coverage);
            }
        }
    }
}

fn fill_span(target: &mut [u8], columns: &[Option<f32>], mut color: impl FnMut(usize, f32) -> Rgba) {
    fill_span_runs(target, columns, |start, columns, out| {
        for (index, (slot, px)) in out.iter_mut().zip(columns).enumerate() {
            *slot = match *px {
                Some(px) => color(start + index, px),
                None => TRANSPARENT,
            };
        }
    });
}

/// `colors` receives the run's start column, its sample positions and the colours to fill
/// (transparent where a position is `None`).
fn fill_span_runs(
    target: &mut [u8],
    columns: &[Option<f32>],
    mut colors: impl FnMut(usize, &[Option<f32>], &mut [Rgba]),
) {
    let mut cache = SourceCache::new();
    let mut staged = [TRANSPARENT; BLEND_RUN];
    for (chunk, (pixels, columns)) in target.chunks_mut(BLEND_RUN * 4).zip(columns.chunks(BLEND_RUN)).enumerate() {
        let out = &mut staged[..columns.len()];
        colors(chunk * BLEND_RUN, columns, out);
        blend_colors(pixels, out, &mut cache);
    }
}

const TRANSPARENT: Rgba = Rgba { r: 0, g: 0, b: 0, a: 0 };

fn sample_image(image: &ImageData, x: f32, y: f32) -> Rgba {
    if !image.is_valid() {
        return Rgba {
            r: 0,
            g: 0,
            b: 0,
            a: 0,
        };
    }
    let (x, y) = (x * image.width as f32 - 0.5, y * image.height as f32 - 0.5);
    let (ix, iy) = (x.floor(), y.floor());
    let (tx, ty) = (x - ix, y - iy);
    let mut sum = [0.0; 4];
    for (dx, dy, weight) in [
        (0.0, 0.0, (1.0 - tx) * (1.0 - ty)),
        (1.0, 0.0, tx * (1.0 - ty)),
        (0.0, 1.0, (1.0 - tx) * ty),
        (1.0, 1.0, tx * ty),
    ] {
        let (xx, yy) = (
            (ix + dx).clamp(0.0, image.width as f32 - 1.0) as usize,
            (iy + dy).clamp(0.0, image.height as f32 - 1.0) as usize,
        );
        let offset = (yy * image.width as usize + xx) * 4;
        let pixel = &image.pixels[offset..offset + 4];
        let alpha = pixel[3] as f32;
        sum[3] += alpha * weight;
        for channel in 0..3 {
            sum[channel] += pixel[channel] as f32 * alpha * weight;
        }
    }
    if sum[3] == 0.0 {
        return Rgba {
            r: 0,
            g: 0,
            b: 0,
            a: 0,
        };
    }
    Rgba {
        r: (sum[0] / sum[3]).round() as u8,
        g: (sum[1] / sum[3]).round() as u8,
        b: (sum[2] / sum[3]).round() as u8,
        a: sum[3].round() as u8,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unresolved_background_image_does_not_panic() {
        for image in [
            ImageData {
                width: 0,
                height: 0,
                pixels: vec![],
            },
            ImageData {
                width: 1,
                height: 1,
                pixels: vec![],
            },
        ] {
            assert_eq!(
                sample_image(&image, 0.5, 0.5),
                Rgba {
                    r: 0,
                    g: 0,
                    b: 0,
                    a: 0
                }
            );
        }
    }
}
