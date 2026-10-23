//! Unclamped colour samples for float16 canvases. The canvas surface stores 8-bit pixels; a
//! sample here remembers the exact value written to a pixel so reads and exports can convert it
//! in float precision. A sample only applies while the surface pixel still holds the bytes it
//! was recorded with.
use super::CanvasColorSpace;
use lumen_common::pixel_color;
use lumen_html_image::Rgba8Image;
use std::collections::HashMap;

const MAX_SAMPLES: usize = 1 << 18;

#[derive(Default)]
pub(super) struct ExtendedPixels {
    dimensions: (u32, u32),
    samples: HashMap<(u32, u32), ([u8; 4], [f32; 3])>,
}

fn is_exact(rgb: [f32; 3], bytes: [u8; 4]) -> bool {
    (0..3).all(|index| (rgb[index] * 255.0 - f32::from(bytes[index])).abs() < 1e-3)
}

impl ExtendedPixels {
    pub(super) fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    fn sync(&mut self, dimensions: (u32, u32)) {
        if self.dimensions != dimensions {
            self.samples.clear();
            self.dimensions = dimensions;
        }
    }

    pub(super) fn record(&mut self, dimensions: (u32, u32), x: u32, y: u32, bytes: [u8; 4], rgb: [f32; 3]) {
        self.sync(dimensions);
        if bytes[3] != 255 || x >= dimensions.0 || y >= dimensions.1 {
            return;
        }
        if is_exact(rgb, bytes) {
            self.samples.remove(&(x, y));
        } else if self.samples.len() < MAX_SAMPLES {
            self.samples.insert((x, y), (bytes, rgb));
        }
    }

    pub(super) fn record_rect(
        &mut self,
        dimensions: (u32, u32),
        x0: i64,
        y0: i64,
        x1: i64,
        y1: i64,
        bytes: [u8; 4],
        rgb: [f32; 3],
    ) {
        for y in y0.max(0)..y1.min(i64::from(dimensions.1)) {
            for x in x0.max(0)..x1.min(i64::from(dimensions.0)) {
                self.record(dimensions, x as u32, y as u32, bytes, rgb);
            }
        }
    }

    /// Converts `image`, which covers the canvas region starting at (`x0`, `y0`) and holds
    /// canvas-space bytes, into `to`; recorded samples are converted from their float values.
    pub(super) fn convert(
        &self,
        image: &mut Rgba8Image,
        dimensions: (u32, u32),
        origin: (i64, i64),
        from: CanvasColorSpace,
        to: CanvasColorSpace,
    ) {
        let mut overrides = Vec::new();
        if self.dimensions == dimensions && !self.samples.is_empty() {
            let width = i64::from(image.width);
            let height = i64::from(image.height);
            for (&(x, y), &(bytes, rgb)) in &self.samples {
                let column = i64::from(x) - origin.0;
                let row = i64::from(y) - origin.1;
                if column < 0 || row < 0 || column >= width || row >= height {
                    continue;
                }
                let offset = (row * width + column) as usize * 4;
                if image.pixels.get(offset..offset + 4) != Some(&bytes[..]) {
                    continue;
                }
                let converted = pixel_color::convert_rgb_f32(rgb, from.working(), to.working());
                overrides.push((offset, converted.map(pixel_color::byte)));
            }
        }
        pixel_color::convert_rgba8(&mut image.pixels, from.working(), to.working());
        for (offset, rgb) in overrides {
            image.pixels[offset..offset + 3].copy_from_slice(&rgb);
        }
    }
}
