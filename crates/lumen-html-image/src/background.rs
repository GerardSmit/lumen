use super::{Raster, composite, gradient::Prepared};
use lumen_html::paint::{BackgroundPaint, BackgroundRepeat, ImageData, Rect, Rgba};

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
    rect: Rect,
    radius: f32,
    positioning_rect: Rect,
    image_rect: Rect,
    repeat: [BackgroundRepeat; 2],
    image: &BackgroundPaint,
) {
    if image_rect.width <= 0.0 || image_rect.height <= 0.0 {
        return;
    }
    let Some(visible) = raster.clips.last().and_then(|clip| clip.intersection(rect)) else {
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
    let gradient = match image {
        BackgroundPaint::Gradient(g) => Prepared::new(g, tile),
        _ => None,
    };
    if matches!(image, BackgroundPaint::Gradient(_)) && gradient.is_none() {
        return;
    }
    let scale = raster.scale;
    let (x0, y0) = (
        (visible.x * scale).floor().max(0.0) as u32,
        (visible.y * scale).floor().max(0.0) as u32,
    );
    let (x1, y1) = (
        ((visible.x + visible.width) * scale)
            .ceil()
            .min(raster.image.width as f32) as u32,
        ((visible.y + visible.height) * scale)
            .ceil()
            .min(raster.image.height as f32) as u32,
    );
    for y in y0..y1 {
        let Some(py) = yaxis.sample((y as f32 + 0.5) / scale) else {
            continue;
        };
        for x in x0..x1 {
            let Some(px) = xaxis.sample((x as f32 + 0.5) / scale) else {
                continue;
            };
            let color = match image {
                BackgroundPaint::Gradient(_) => gradient.as_ref().unwrap().color(px, py),
                BackgroundPaint::Image(image) => {
                    sample_image(image, px / xaxis.tile, py / yaxis.tile)
                }
            };
            let coverage = if raster.antialias {
                super::coverage::rounded(
                    x as f32,
                    y as f32,
                    rect.x * scale,
                    rect.y * scale,
                    (rect.x + rect.width) * scale,
                    (rect.y + rect.height) * scale,
                    radius * scale,
                )
            } else {
                rect.contains_rounded((x as f32 + 0.5) / scale, (y as f32 + 0.5) / scale, radius)
                    as u8 as f32
            };
            if coverage == 0.0 {
                continue;
            }
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

fn sample_image(image: &ImageData, x: f32, y: f32) -> Rgba {
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
