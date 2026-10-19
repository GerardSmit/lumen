//! Ordered, allocation-free sRGB color filter primitives. CSS grammar belongs
//! to the caller. Spatial filters and resource-backed filters are separate.
use crate::color::{Color, ColorSpace};
use alloc::sync::Arc;
pub mod resource;

pub const MAX_COLOR_FILTERS: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ColorFilter {
    Brightness(f32), Contrast(f32), Grayscale(f32),
    /// Unnormalized degrees; only trigonometric evaluation reduces the angle.
    HueRotate(f32),
    Invert(f32), Opacity(f32), Saturate(f32), Sepia(f32),
}

impl ColorFilter {
    pub fn is_valid(self) -> bool {
        let (value, angle) = match self {
            Self::Brightness(v) | Self::Contrast(v) | Self::Grayscale(v)
            | Self::Invert(v) | Self::Opacity(v) | Self::Saturate(v) | Self::Sepia(v) => (v, false),
            Self::HueRotate(v) => (v, true),
        };
        value.is_finite() && (angle || value >= 0.0)
    }

    /// Filter Effects 1 §13.1: each operation consumes the previous clamped
    /// primitive result. Keep matrices separate; composing them loses clamps.
    pub fn matrix(self) -> ColorMatrix {
        let mut m = ColorMatrix::IDENTITY.0;
        match self {
            Self::Brightness(v) | Self::Contrast(v) => {
                let intercept = if matches!(self, Self::Contrast(_)) { 0.5 - 0.5 * v } else { 0.0 };
                for (i, row) in m.iter_mut().take(3).enumerate() { row[i] = v; row[4] = intercept; }
            }
            Self::Invert(v) => {
                let v = v.min(1.0);
                for (i, row) in m.iter_mut().take(3).enumerate() { row[i] = 1.0 - 2.0 * v; row[4] = v; }
            }
            Self::Opacity(v) => m[3][3] = v.min(1.0),
            Self::Grayscale(v) | Self::Saturate(v) => {
                let s = if matches!(self, Self::Grayscale(_)) { 1.0 - v.min(1.0) } else { v };
                // feColorMatrix uses these luminance coefficients, including
                // its specified decimal precision.
                let luminance = [0.213, 0.715, 0.072];
                for (i, row) in m.iter_mut().take(3).enumerate() {
                    for j in 0..3 { row[j] = luminance[j] * (1.0 - s) + if i == j { s } else { 0.0 }; }
                }
                if matches!(self, Self::Grayscale(_)) {
                    // The shorthand's explicit matrix has distinct precision.
                    let v = v.min(1.0);
                    let luminance = [0.2126, 0.7152, 0.0722];
                    for (i, row) in m.iter_mut().take(3).enumerate() {
                        for j in 0..3 { row[j] = luminance[j] * v + if i == j { 1.0 - v } else { 0.0 }; }
                    }
                }
            }
            Self::Sepia(v) => {
                let v = v.min(1.0);
                let sepia = [[0.393, 0.769, 0.189], [0.349, 0.686, 0.168], [0.272, 0.534, 0.131]];
                for (i, row) in m.iter_mut().take(3).enumerate() {
                    for j in 0..3 { row[j] = sepia[i][j] * v + if i == j { 1.0 - v } else { 0.0 }; }
                }
            }
            Self::HueRotate(degrees) => {
                let radians = libm::fmodf(degrees, 360.0) * (core::f32::consts::PI / 180.0);
                let c = libm::cosf(radians); let s = libm::sinf(radians);
                let base = [[0.213, 0.715, 0.072]; 3];
                let cosine = [[0.787, -0.715, -0.072], [-0.213, 0.285, -0.072], [-0.213, -0.715, 0.928]];
                let sine = [[-0.213, -0.715, 0.928], [0.143, 0.140, -0.283], [-0.787, 0.715, 0.072]];
                for (i, row) in m.iter_mut().take(3).enumerate() {
                    for j in 0..3 { row[j] = base[i][j] + c * cosine[i][j] + s * sine[i][j]; }
                }
            }
        }
        ColorMatrix(m)
    }
}

/// CSS local fragment references retain their local URL flag even when the
/// computed href has been made absolute in its declaring stylesheet context.
#[derive(Clone,Debug,PartialEq)]
pub struct UrlReference {pub href:Arc<str>,pub local:bool}

/// One resolved primitive in an ordered Filter Effects function list.
/// Spatial amounts are local CSS lengths; consumers choose operating scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DropShadowFilter { pub offset: [f32; 2], pub sigma: f32, pub color: Color }

#[derive(Clone, Debug, PartialEq)]
pub enum FilterOperation {
    Color(ColorFilter),
    Blur(f32),
    /// Computed URL tokens retain canonical declaring-resource resolution.
    /// They must be bound to a used resource before issuing paint commands.
    Url(Arc<UrlReference>),
    Resource(Arc<resource::Use>),
    // Keep the color operations compact; only shadows allocate their larger
    // immutable parameter block, shared by styles and paint commands.
    DropShadow(Arc<DropShadowFilter>),
}
impl From<ColorFilter> for FilterOperation {
    fn from(value: ColorFilter) -> Self { Self::Color(value) }
}
impl FilterOperation {
    pub fn is_valid(&self) -> bool {
        match self {
            Self::Color(filter) => filter.is_valid(),
            Self::Blur(sigma) => sigma.is_finite() && *sigma >= 0.0,
            Self::Url(url)=>url.href.len()<=8192,
            Self::Resource(resource)=>resource.valid(usize::MAX),
            Self::DropShadow(shadow) => shadow.offset.iter().all(|v|v.is_finite())
                && shadow.sigma.is_finite() && shadow.sigma >= 0.0 && shadow.color.alpha.is_finite()
                && shadow.color.components.iter().all(|v|v.is_finite()),
        }
    }
    pub fn is_spatial(&self) -> bool { !matches!(self, Self::Color(_)) }
    pub fn payload_bytes(&self) -> usize {
        match self {
            Self::DropShadow(_)=>core::mem::size_of::<DropShadowFilter>()+2*core::mem::size_of::<usize>(),
            Self::Url(url)=>url.href.len()+core::mem::size_of::<UrlReference>()+4*core::mem::size_of::<usize>(),
            Self::Resource(resource)=>core::mem::size_of::<resource::Use>()+4*core::mem::size_of::<usize>()+resource.url.len()+resource.program.bytes(),
            _=>0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColorMatrix([[f32; 5]; 4]);
impl ColorMatrix {
    pub const IDENTITY: Self = Self([
        [1.0, 0.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 0.0, 1.0, 0.0],
    ]);
    pub const fn coefficients(self)->[[f32;5];4] {self.0}
    pub fn from_coefficients(values:[[f32;5];4])->Self {Self(values)}
    pub fn apply(self, source: Color) -> Color {self.apply_in_space(source,ColorSpace::Srgb)}
    /// SVG filter primitives choose their working color space independently
    /// of CSS filter functions, which always use sRGB.
    pub fn apply_in_space(self,source:Color,space:ColorSpace)->Color {
        let source = source.to(space);
        let input = [source.components[0], source.components[1], source.components[2], source.alpha];
        let out: [f32; 4] = core::array::from_fn(|i| {
            let row = self.0[i];
            let sum=row[0]*input[0]+row[1]*input[1]+row[2]*input[2]+row[3]*input[3]+row[4];
            // Preserve the existing ordinary f32 path. Finite but extreme SVG
            // coefficients can otherwise cancel overflowing terms to NaN.
            let sum=if sum.is_nan(){(f64::from(row[0])*f64::from(input[0])+f64::from(row[1])*f64::from(input[1])+f64::from(row[2])*f64::from(input[2])+f64::from(row[3])*f64::from(input[3])+f64::from(row[4])) as f32}else{sum};
            sum.clamp(0.0,1.0)
        });
        Color::new(space, [out[0], out[1], out[2]], out[3], 0)
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ordered_primitives_clamp_without_quantizing_intermediate_samples() {
        let input = Color::rgba8([255, 0, 255, 128]);
        let inverted = ColorFilter::Invert(1.0).matrix().apply(input);
        assert_eq!(inverted.components, [0.0, 1.0, 0.0]);
        assert_eq!(inverted.alpha, input.alpha);
        let white = Color::new(ColorSpace::Srgb, [0.75; 3], 1.0, 0);
        let bright = ColorFilter::Brightness(2.0).matrix().apply(white);
        assert_eq!(ColorFilter::Brightness(0.5).matrix().apply(bright).components, [0.5; 3]);
        assert_eq!(ColorFilter::Opacity(0.25).matrix().apply(inverted).alpha, input.alpha * 0.25);
        assert_eq!(ColorFilter::Invert(2.0).matrix(), ColorFilter::Invert(1.0).matrix());
        assert!(!ColorFilter::Brightness(-1.0).is_valid());
        assert!(!ColorFilter::HueRotate(f32::NAN).is_valid());
    }
}
