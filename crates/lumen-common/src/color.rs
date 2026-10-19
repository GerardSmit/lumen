//! Unclipped CSS Color 4 coordinates and allocation-free color interpolation.
//! Syntax and serialization belong to the caller; conversions are shared by
//! CSS colors, color-mix and image gradient preparation.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ColorSpace {
    Srgb, SrgbLinear, DisplayP3, DisplayP3Linear, A98Rgb, ProphotoRgb,
    Rec2020, XyzD65, XyzD50, Lab, Lch, Oklab, Oklch, Hsl, Hwb,
}
impl ColorSpace {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Srgb => "srgb", Self::SrgbLinear => "srgb-linear",
            Self::DisplayP3 => "display-p3", Self::DisplayP3Linear => "display-p3-linear",
            Self::A98Rgb => "a98-rgb", Self::ProphotoRgb => "prophoto-rgb",
            Self::Rec2020 => "rec2020", Self::XyzD65 => "xyz-d65", Self::XyzD50 => "xyz-d50",
            Self::Lab => "lab", Self::Lch => "lch", Self::Oklab => "oklab", Self::Oklch => "oklch",
            Self::Hsl => "hsl", Self::Hwb => "hwb",
        }
    }
    pub fn named(name: &str) -> Option<Self> {
        Some(match name {
            "srgb" => Self::Srgb, "srgb-linear" => Self::SrgbLinear,
            "display-p3" => Self::DisplayP3, "display-p3-linear" => Self::DisplayP3Linear,
            "a98-rgb" => Self::A98Rgb, "prophoto-rgb" => Self::ProphotoRgb,
            "rec2020" => Self::Rec2020, "xyz" | "xyz-d65" => Self::XyzD65, "xyz-d50" => Self::XyzD50,
            "lab" => Self::Lab, "lch" => Self::Lch, "oklab" => Self::Oklab, "oklch" => Self::Oklch,
            "hsl" => Self::Hsl, "hwb" => Self::Hwb, _ => return None,
        })
    }
    pub const fn hue(self) -> Option<usize> {
        match self { Self::Hsl | Self::Hwb => Some(0), Self::Lch | Self::Oklch => Some(2), _ => None }
    }
    // CSS Color 4 analogous component categories. Zero denotes an unmatched
    // component: those components participate in the analogous-set rule.
    const fn categories(self) -> [u8; 3] {
        match self {
            Self::Lab | Self::Oklab => [4, 7, 8],
            Self::Lch | Self::Oklch => [4, 5, 6],
            Self::Hsl => [6, 5, 4], Self::Hwb => [6, 0, 0],
            _ => [1, 2, 3],
        }
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u8)]
pub enum HueInterpolation { #[default] Shorter, Longer, Increasing, Decreasing }
impl HueInterpolation {
    pub const fn name(self) -> &'static str {
        match self {Self::Shorter => "shorter", Self::Longer => "longer", Self::Increasing => "increasing", Self::Decreasing => "decreasing"}
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InterpolationMethod { pub space: ColorSpace, pub hue: HueInterpolation }
impl Default for InterpolationMethod {
    fn default() -> Self { Self { space: ColorSpace::Oklab, hue: HueInterpolation::Shorter } }
}
/// Hue coordinates are turns. HSL/HWB saturation, lightness, whiteness and
/// blackness are fractions. Lab lightness remains in its 0..100 scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color {
    pub space: ColorSpace, pub components: [f32; 3], pub alpha: f32,
    /// Bits 0..2 are missing coordinates, bit 3 is missing alpha.
    pub missing: u8,
}
impl Color {
    pub const fn new(space: ColorSpace, components: [f32; 3], alpha: f32, missing: u8) -> Self {
        Self { space, components, alpha, missing }
    }
    pub fn rgba8(value: [u8; 4]) -> Self {
        Self::new(ColorSpace::Srgb, [value[0] as f32 / 255.0, value[1] as f32 / 255.0, value[2] as f32 / 255.0], value[3] as f32 / 255.0, 0)
    }
    pub fn is_finite(self) -> bool { self.components.iter().all(|x|x.is_finite()) && self.alpha.is_finite() && self.missing & !15 == 0 }
    pub fn to(self, space: ColorSpace) -> Self {
        if self.space == space { return self; }
        let mut source=self;source.convert_powerless();
        let mut values = source.components;
        for (index, value) in values.iter_mut().enumerate() { if source.missing & (1 << index) != 0 { *value = 0.0; } }
        {
            let coordinates=match (self.space,space) {
                (ColorSpace::Srgb,ColorSpace::Hsl)=>rgb_to_hsl(values),
                (ColorSpace::Hsl,ColorSpace::Srgb)=>hsl_to_rgb(values),
                (ColorSpace::Hwb,ColorSpace::Srgb)=>hwb_to_rgb(values),
                (ColorSpace::Srgb,ColorSpace::SrgbLinear)=>values.map(srgb_to_linear),
                (ColorSpace::SrgbLinear,ColorSpace::Srgb)=>values.map(linear_to_srgb),
                (ColorSpace::Srgb,ColorSpace::Oklab)=>linear_rgb_to_oklab(values.map(srgb_to_linear)),
                (ColorSpace::Srgb,ColorSpace::Oklch)=>polar(linear_rgb_to_oklab(values.map(srgb_to_linear))),
                (ColorSpace::Oklab,ColorSpace::Srgb)=>oklab_to_linear_rgb(values).map(linear_to_srgb),
                (ColorSpace::Oklch,ColorSpace::Srgb)=>oklab_to_linear_rgb(cartesian(values)).map(linear_to_srgb),
                (ColorSpace::Oklch,ColorSpace::Oklab)|(ColorSpace::Lch,ColorSpace::Lab)=>cartesian(values),
                (ColorSpace::Oklab,ColorSpace::Oklch)|(ColorSpace::Lab,ColorSpace::Lch)=>polar(values),
                _=>from_xyz(to_xyz(values,self.space),space),
            };
            let mut result=Self::new(space,coordinates,self.alpha,self.missing&8);
            result.convert_powerless();result
        }
    }
    fn powerless(self) -> u8 {
        match self.space {
            ColorSpace::Hsl if self.components[1] <= 0.00001 => 1,
            ColorSpace::Hwb if self.components[1] + self.components[2] >= 0.99999 => 1,
            ColorSpace::Lch if self.components[1] <= 0.0015 => 4,
            ColorSpace::Oklch if self.components[1] <= 0.000004 => 4,
            _ => 0,
        }
    }
    fn convert_powerless(&mut self) {
        // Only the powerless component becomes missing. Tiny positive chroma,
        // saturation and whiteness/blackness remain the actual coordinates.
        self.missing|=self.powerless();
    }
    /// Convert while carrying analogous missing components, as required by
    /// interpolation and relative-color origin conversion.
    pub fn to_with_missing(self, target: ColorSpace) -> Self {
        if self.space == target { return self; }
        // Carry is classified from the original missing mask before conversion
        // performs its source and destination powerless-component handling.
        let src = self.space.categories(); let dst = target.categories();
        let mut matched_src = 0u8; let mut matched_dst = 0u8; let mut carry = self.missing & 8;
        for (i, category) in src.iter().enumerate() {
            if *category == 0 { continue; }
            if let Some(j) = dst.iter().position(|other| other == category) {
                matched_src |= 1 << i; matched_dst |= 1 << j;
                if self.missing & (1 << i) != 0 { carry |= 1 << j; }
            }
        }
        let remaining_src = 7 & !matched_src;
        if remaining_src != 0 && self.missing & remaining_src == remaining_src { carry |= 7 & !matched_dst; }
        let mut result=self.to(target);result.missing=carry | result.missing;
        result

    }
    pub fn srgb(self) -> [f32; 3] { self.to(ColorSpace::Srgb).components }
    pub fn to_rgba8(self) -> [u8; 4] {
        let rgb = gamut_map_srgb(self);
        let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        [byte(rgb[0]), byte(rgb[1]), byte(rgb[2]), if self.missing & 8 != 0 {0} else {byte(self.alpha)}]
    }
}
/// Prepared once per adjacent endpoint pair. No syntax, allocation, coordinate
/// conversion or missing-component search occurs while interpolating a pixel.
#[derive(Clone, Copy, Debug)]
pub struct PreparedPair { method: InterpolationMethod, left: [f32; 4], right: [f32; 4], missing: u8 }
impl PreparedPair {
    pub fn new(left: Color, right: Color, method: InterpolationMethod) -> Self {
        let left = left.to_with_missing(method.space);
        let right = right.to_with_missing(method.space);
        let mut a = [left.components[0], left.components[1], left.components[2], left.alpha];
        let mut b = [right.components[0], right.components[1], right.components[2], right.alpha];
        for i in 0..4 {
            if left.missing & (1 << i) != 0 { a[i] = if right.missing & (1 << i) != 0 {0.0} else {b[i]}; }
            if right.missing & (1 << i) != 0 { b[i] = a[i]; }
        }
        if let Some(i) = method.space.hue() {
            a[i] = a[i].rem_euclid(1.0); b[i] = b[i].rem_euclid(1.0);
            let delta = b[i] - a[i];
            match method.hue {
                HueInterpolation::Shorter if delta > 0.5 => a[i] += 1.0,
                HueInterpolation::Shorter if delta < -0.5 => b[i] += 1.0,
                HueInterpolation::Longer if delta > 0.0 && delta < 0.5 => a[i] += 1.0,
                HueInterpolation::Longer if delta > -0.5 && delta <= 0.0 => b[i] += 1.0,
                HueInterpolation::Increasing if b[i] < a[i] => b[i] += 1.0,
                HueInterpolation::Decreasing if a[i] < b[i] => a[i] += 1.0,
                _ => {},
            }
        }
        let missing=left.missing & right.missing;
        if missing&8==0 {for i in 0..3 {if method.space.hue()!=Some(i) {a[i]*=a[3];b[i]*=b[3];}}}
        Self {method, left: a, right: b, missing}
    }
    pub fn sample(self, t: f32) -> Color {
        let alpha = self.left[3]*(1.0-t)+self.right[3]*t;
        let mut values = [0.0; 3];
        for (i, value) in values.iter_mut().enumerate() {
            *value = self.left[i]*(1.0-t)+self.right[i]*t;
            if self.method.space.hue() == Some(i) { *value = value.rem_euclid(1.0); }
            else if self.missing&8==0 && alpha != 0.0 { *value /= alpha; }
        }
        Color::new(self.method.space, values, alpha, self.missing)
    }
}

pub fn srgb_to_linear(value: f32) -> f32 {
    if value.abs() <= 0.04045 {
        value / 12.92
    } else {
        value.signum() * libm::powf((value.abs() + 0.055) / 1.055, 2.4)
    }
}

pub fn linear_to_srgb(value: f32) -> f32 {
    if value.abs() <= 0.0031308 {
        value * 12.92
    } else {
        value.signum() * (1.055 * libm::powf(value.abs(), 1.0 / 2.4) - 0.055)
    }
}

pub fn linear_rgb_to_xyz(rgb: [f32; 3]) -> [f32; 3] {
    [
        0.4123908 * rgb[0] + 0.3575843 * rgb[1] + 0.1804808 * rgb[2],
        0.2126390 * rgb[0] + 0.7151687 * rgb[1] + 0.0721923 * rgb[2],
        0.0193308 * rgb[0] + 0.1191948 * rgb[1] + 0.9505322 * rgb[2],
    ]
}

pub fn xyz_to_linear_rgb(xyz: [f32; 3]) -> [f32; 3] {
    [
        3.2409699 * xyz[0] - 1.5373832 * xyz[1] - 0.4986108 * xyz[2],
        -0.9692436 * xyz[0] + 1.8759675 * xyz[1] + 0.0415551 * xyz[2],
        0.0556301 * xyz[0] - 0.2039770 * xyz[1] + 1.0569715 * xyz[2],
    ]
}

pub fn xyz_d65_to_d50(xyz: [f32; 3]) -> [f32; 3] {
    [
        1.0479298 * xyz[0] + 0.022946871 * xyz[1] - 0.050192267 * xyz[2],
        0.02962781 * xyz[0] + 0.9904344 * xyz[1] - 0.017073799 * xyz[2],
        -0.009243041 * xyz[0]  + 0.015055192 * xyz[1] + 0.75187427 * xyz[2],
    ]
}

pub fn xyz_d50_to_d65(xyz: [f32; 3]) -> [f32; 3] {
    [
        0.9554734 * xyz[0] - 0.023098455 * xyz[1] + 0.063259244 * xyz[2],
        -0.02836971 * xyz[0] + 1.0099953 * xyz[1] + 0.021041442 * xyz[2],
        0.012314015 * xyz[0] - 0.020507649 * xyz[1] + 1.3303659 * xyz[2],
    ]
}

pub fn lab_f(value: f32) -> f32 {
    if value > 216.0 / 24389.0 {
        libm::cbrtf(value)
    } else {
        value * (24389.0 / (27.0 * 116.0)) + 16.0 / 116.0
    }
}

pub fn lab_f_inv(value: f32) -> f32 {
    if value > 6.0 / 29.0 {
        value * value * value
    } else {
        3.0 * (6.0f32 / 29.0).powi(2) * (value - 4.0 / 29.0)
    }
}

pub fn xyz_d65_to_lab(xyz: [f32; 3]) -> [f32; 3] {
    let xyz = xyz_d65_to_d50(xyz);
    let x = lab_f(xyz[0] / (0.3457 / 0.3585));
    let y = lab_f(xyz[1]);
    let z = lab_f(xyz[2] / ((1.0 - 0.3457 - 0.3585) / 0.3585));
    [116.0 * y - 16.0, 500.0 * (x - y), 200.0 * (y - z)]
}

pub fn lab_to_xyz_d50(lab: [f32; 3]) -> [f32; 3] {
    let fy = (lab[0] + 16.0) / 116.0;
    let fx = fy + lab[1] / 500.0;
    let fz = fy - lab[2] / 200.0;
    [
        (0.3457 / 0.3585) * lab_f_inv(fx),
        lab_f_inv(fy),
        ((1.0 - 0.3457 - 0.3585) / 0.3585) * lab_f_inv(fz),
    ]
}

pub fn linear_rgb_to_oklab(rgb: [f32; 3]) -> [f32; 3] {
    let l = libm::cbrtf(0.4122214708 * rgb[0] + 0.5363325363 * rgb[1] + 0.0514459929 * rgb[2]);
    let m = libm::cbrtf(0.2119034982 * rgb[0] + 0.6806995451 * rgb[1] + 0.1073969566 * rgb[2]);
    let s = libm::cbrtf(0.0883024619 * rgb[0] + 0.2817188376 * rgb[1] + 0.6299787005 * rgb[2]);
    [
        0.2104542553 * l + 0.7936177850 * m - 0.0040720468 * s,
        1.9779984951 * l - 2.4285922050 * m + 0.4505937099 * s,
        0.0259040371 * l + 0.7827717662 * m - 0.8086757660 * s,
    ]
}

pub fn oklab_to_linear_rgb(lab: [f32; 3]) -> [f32; 3] {
    let l = (lab[0] + 0.3963377774 * lab[1] + 0.2158037573 * lab[2]).powi(3);
    let m = (lab[0] - 0.1055613458 * lab[1] - 0.0638541728 * lab[2]).powi(3);
    let s = (lab[0] - 0.0894841775 * lab[1] - 1.2914855480 * lab[2]).powi(3);
    [
        4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
        -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
        -0.0041960863 * l - 0.7034186147 * m + 1.7076147010 * s,
    ]
}

fn profile_to_xyz(encoded: [f32; 3], space: ColorSpace) -> [f32; 3] {
    let linear = match space {
        ColorSpace::DisplayP3 => encoded.map(srgb_to_linear),
        ColorSpace::A98Rgb => encoded.map(|v| v.signum() * libm::powf(v.abs(), 563.0 / 256.0)),
        ColorSpace::ProphotoRgb => encoded.map(|v| {
            if v.abs() <= 1.0 / 32.0 {
                v / 16.0
            } else {
                v.signum() * libm::powf(v.abs(), 1.8)
            }
        }),
        ColorSpace::Rec2020 => encoded.map(|v|v.signum()*libm::powf(v.abs(),2.4)),
        ColorSpace::DisplayP3Linear => encoded,
        _ => unreachable!(),
    };
    let xyz = match space {
        ColorSpace::DisplayP3 | ColorSpace::DisplayP3Linear => [
            0.48657095 * linear[0] + 0.26566769 * linear[1] + 0.19821729 * linear[2],
            0.22897456 * linear[0] + 0.69173852 * linear[1] + 0.07928691 * linear[2],
            0.0 * linear[0] + 0.04511338 * linear[1] + 1.04394437 * linear[2],
        ],
        ColorSpace::A98Rgb => [
            (573536.0 / 994567.0) * linear[0] + (263643.0 / 1420810.0) * linear[1] + (187206.0 / 994567.0) * linear[2],
            (591459.0 / 1989134.0) * linear[0] + (6239551.0 / 9945670.0) * linear[1] + (374412.0 / 4972835.0) * linear[2],
            (53769.0 / 1989134.0) * linear[0] + (351524.0 / 4972835.0) * linear[1] + (4929758.0 / 4972835.0) * linear[2],
        ],
        ColorSpace::ProphotoRgb => xyz_d50_to_d65([
            0.79776664 * linear[0] + 0.13518130 * linear[1] + 0.03134773 * linear[2],
            0.28807483 * linear[0] + 0.71183523 * linear[1] + 0.00008994 * linear[2],
            0.0 * linear[0] + 0.0 * linear[1] + 0.82510460 * linear[2],
        ]),
        ColorSpace::Rec2020 => [
            0.63695805 * linear[0] + 0.14461690 * linear[1] + 0.16888098 * linear[2],
            0.26270021 * linear[0] + 0.67799807 * linear[1] + 0.05930172 * linear[2],
            0.0 * linear[0] + 0.02807269 * linear[1] + 1.06098506 * linear[2],
        ],
        _ => unreachable!(),
    };
    xyz
}

fn xyz_to_profile(xyz: [f32; 3], space: ColorSpace) -> [f32; 3] {
    let linear = match space {
        ColorSpace::SrgbLinear => xyz_to_linear_rgb(xyz),
        ColorSpace::DisplayP3 | ColorSpace::DisplayP3Linear => [
            2.4934969 * xyz[0] - 0.9313836 * xyz[1] - 0.4027108 * xyz[2],
            -0.8294890 * xyz[0] + 1.7626641 * xyz[1] + 0.0236247 * xyz[2],
            0.0358458 * xyz[0] - 0.0761724 * xyz[1] + 0.9568845 * xyz[2],
        ],
        ColorSpace::A98Rgb => [
            (1829569.0 / 896150.0) * xyz[0] - (506331.0 / 896150.0) * xyz[1] - (308931.0 / 896150.0) * xyz[2],
            (-851781.0 / 878810.0) * xyz[0] + (1648619.0 / 878810.0) * xyz[1] + (36519.0 / 878810.0) * xyz[2],
            (16779.0 / 1248040.0) * xyz[0] - (147721.0 / 1248040.0) * xyz[1] + (1266979.0 / 1248040.0) * xyz[2],
        ],
        ColorSpace::ProphotoRgb => {
            let d50 = xyz_d65_to_d50(xyz);
            [
                1.3457869 * d50[0] - 0.25557208 * d50[1] - 0.051101863 * d50[2],
                -0.5446307 * d50[0] + 1.5082477 * d50[1] + 0.020527447 * d50[2],
                1.2119676 * d50[2],
            ]
        }
        ColorSpace::Rec2020 => [
            1.7166512 * xyz[0] - 0.3556708 * xyz[1] - 0.2533663 * xyz[2],
            -0.6666844 * xyz[0] + 1.6164812 * xyz[1] + 0.0157685 * xyz[2],
            0.0176399 * xyz[0] - 0.0427706 * xyz[1] + 0.9421031 * xyz[2],
        ],
        _ => return [0.0; 3],
    };
    match space {
        ColorSpace::SrgbLinear | ColorSpace::DisplayP3Linear => linear,
        ColorSpace::DisplayP3 => linear.map(linear_to_srgb),
        ColorSpace::A98Rgb => linear.map(|v| v.signum() * libm::powf(v.abs(), 256.0 / 563.0)),
        ColorSpace::ProphotoRgb => linear.map(|v| {
            if v.abs() <= 1.0 / 512.0 {
                v * 16.0
            } else {
                v.signum() * libm::powf(v.abs(), 1.0 / 1.8)
            }
        }),
        ColorSpace::Rec2020 => linear.map(|v|v.signum()*libm::powf(v.abs(),1.0/2.4)),
        _ => unreachable!(),
    }
}

fn rgb_to_hsl(rgb: [f32; 3]) -> [f32; 3] {
    let [r,g,b] = rgb; let max = r.max(g).max(b); let min = r.min(g).min(b);
    let delta = max - min; let lightness = (max + min) * 0.5;
    if delta == 0.0 { return [0.0, 0.0, lightness]; }
    let denominator = 1.0 - (2.0 * lightness - 1.0).abs();
    let mut saturation = if denominator == 0.0 {0.0} else {delta / denominator};
    let mut hue = (if max == r { ((g-b)/delta).rem_euclid(6.0) } else if max == g { (b-r)/delta+2.0 } else { (r-g)/delta+4.0 }) / 6.0;
    // Extended sRGB can produce negative saturation; rotate hue before making
    // saturation positive, as prescribed by CSS Color's conversion algorithm.
    if saturation < 0.0 { saturation = -saturation; hue += 0.5; }
    [hue.rem_euclid(1.0), saturation, lightness]
}
fn hsl_to_rgb(hsl: [f32; 3]) -> [f32; 3] {
    let [h,s,l] = hsl; let a = s * l.min(1.0-l);
    let channel = |n: f32| { let k = (n+h*12.0).rem_euclid(12.0); l-a*(-1.0f32).max((k-3.0).min(9.0-k).min(1.0)) };
    [channel(0.0), channel(8.0), channel(4.0)]
}
fn hwb_to_rgb(hwb: [f32; 3]) -> [f32; 3] {
    let [h,w,b] = hwb; let sum = w+b;
    if sum >= 1.0 { return [w/sum;3]; }
    hsl_to_rgb([h,1.0,0.5]).map(|v|v*(1.0-sum)+w)
}
fn cartesian(polar: [f32; 3]) -> [f32; 3] {
    let angle=polar[2].rem_euclid(1.0)*core::f32::consts::TAU;
    [polar[0], polar[1]*libm::cosf(angle), polar[1]*libm::sinf(angle)]
}
fn polar(cartesian: [f32; 3]) -> [f32; 3] {
    [cartesian[0],libm::hypotf(cartesian[1],cartesian[2]),libm::atan2f(cartesian[2],cartesian[1]).rem_euclid(core::f32::consts::TAU)/core::f32::consts::TAU]
}
fn to_xyz(values: [f32; 3], space: ColorSpace) -> [f32; 3] {
    match space {
        ColorSpace::Srgb => linear_rgb_to_xyz(values.map(srgb_to_linear)),
        ColorSpace::SrgbLinear => linear_rgb_to_xyz(values),
        ColorSpace::XyzD65 => values, ColorSpace::XyzD50 => xyz_d50_to_d65(values),
        ColorSpace::Lab => xyz_d50_to_d65(lab_to_xyz_d50(values)),
        ColorSpace::Lch => xyz_d50_to_d65(lab_to_xyz_d50(cartesian(values))),
        ColorSpace::Oklab => linear_rgb_to_xyz(oklab_to_linear_rgb(values)),
        ColorSpace::Oklch => linear_rgb_to_xyz(oklab_to_linear_rgb(cartesian(values))),
        ColorSpace::Hsl => linear_rgb_to_xyz(hsl_to_rgb(values).map(srgb_to_linear)),
        ColorSpace::Hwb => linear_rgb_to_xyz(hwb_to_rgb(values).map(srgb_to_linear)),
        _ => profile_to_xyz(values,space),
    }
}
fn from_xyz(xyz: [f32; 3], space: ColorSpace) -> [f32; 3] {
    match space {
        ColorSpace::Srgb => xyz_to_linear_rgb(xyz).map(linear_to_srgb),
        ColorSpace::SrgbLinear => xyz_to_linear_rgb(xyz),
        ColorSpace::XyzD65 => xyz, ColorSpace::XyzD50 => xyz_d65_to_d50(xyz),
        ColorSpace::Lab => xyz_d65_to_lab(xyz), ColorSpace::Lch => polar(xyz_d65_to_lab(xyz)),
        ColorSpace::Oklab => linear_rgb_to_oklab(xyz_to_linear_rgb(xyz)),
        ColorSpace::Oklch => polar(linear_rgb_to_oklab(xyz_to_linear_rgb(xyz))),
        ColorSpace::Hsl => rgb_to_hsl(xyz_to_linear_rgb(xyz).map(linear_to_srgb)),
        ColorSpace::Hwb => {
            let rgb=xyz_to_linear_rgb(xyz).map(linear_to_srgb);
            [rgb_to_hsl(rgb)[0],rgb[0].min(rgb[1]).min(rgb[2]),1.0-rgb[0].max(rgb[1]).max(rgb[2])]
        },
        _ => xyz_to_profile(xyz,space),
    }
}
/// CSS Color 4 binary-search local-MINDE mapping to the actual sRGB output.
/// In-gamut colors use the allocation-free conversion path without a search.
pub fn gamut_map_srgb(color: Color) -> [f32; 3] {
    let rgb=color.srgb();
    let in_gamut=|rgb:[f32;3]|rgb.iter().all(|v|*v>=0.0 && *v<=1.0);
    if in_gamut(rgb) { return rgb; }
    // Numeric conversion can exceed the output representation even when
    // source coordinates are finite. Never admit infinities into the search.
    let clip=|rgb:[f32;3]|rgb.map(|v|if v.is_nan() {0.0} else {v.clamp(0.0,1.0)});
    let origin=color.to(ColorSpace::Oklch); let [l,mut max,h]=origin.components;
    if !origin.is_finite() {return clip(rgb);}
    if l>=1.0 {return [1.0;3];} if l<=0.0 {return [0.0;3];}
    let difference=|source:Color,clipped:[f32;3]| {
        let lab=source.to(ColorSpace::Oklab).components;
        let other=Color::new(ColorSpace::Srgb,clipped,1.0,0).to(ColorSpace::Oklab).components;
        libm::hypotf(libm::hypotf(lab[0]-other[0],lab[1]-other[1]),lab[2]-other[2])
    };
    let mut clipped=clip(rgb);
    // Local-MINDE first accepts clipping when its perceptual difference is
    // below the JND; reducing chroma would otherwise change the output again.
    if difference(origin,clipped)<0.02 {return clipped;}
    let mut min=0.0; let mut min_in_gamut=true;
    // Lightness and hue stay fixed throughout chroma reduction. Prepare the
    // canonical polar direction once rather than repeating trigonometry.
    let direction=cartesian([l,1.0,h]);
    // 160 halvings cover the entire f32 exponent range and the specified
    // epsilon. The equality check also bounds representable-precision stalls.
    for _ in 0..160 {
        if max-min<=0.0001 {break;}
        let chroma=min*0.5+max*0.5;
        if chroma==min || chroma==max {break;}
        let current=Color::new(ColorSpace::Oklab,[l,chroma*direction[1],chroma*direction[2]],1.0,0);
        let rgb=current.srgb();
        if min_in_gamut && in_gamut(rgb) { min=chroma; continue; }
        clipped=clip(rgb);
        let delta=difference(current,clipped);
        if delta<0.02 {
            if 0.02-delta<0.0001 {return clipped;}
            min_in_gamut=false; min=chroma;
        } else { max=chroma; }
    }
    clipped
}

#[cfg(test)]
mod tests {
    use super::*;
    fn near(a:f32,b:f32) {assert!((a-b).abs()<0.00002,"{a} != {b}");}
    #[test]
    fn specification_color_finite_extremes_keep_gamut_search_and_polar_conversion_bounded() {
        let polar=super::polar([0.5,1.0e30,1.0e30]);
        assert!(polar.iter().all(|value|value.is_finite()));
        assert!(polar[1]>1.0e30);
        for color in [Color::new(ColorSpace::Oklch,[0.5,f32::MAX,0.25],1.0,0),
            Color::new(ColorSpace::Lab,[50.0,f32::MAX,-f32::MAX],1.0,0),
            Color::new(ColorSpace::DisplayP3,[f32::MAX,-f32::MAX,0.5],1.0,0)] {
            let before=color;
            assert!(gamut_map_srgb(color).iter().all(|value|value.is_finite() && (0.0..=1.0).contains(value)));
            assert_eq!(color,before,"display mapping must not clip the retained source");
        }
    }
    #[test]
    fn specification_color_float_conversion_and_prepared_alpha_preserve_unclipped_endpoints() {
        let just_outside=Color::new(ColorSpace::Srgb,[1.00001,0.4,0.3],1.0,0);
        let mapped=gamut_map_srgb(just_outside);
        for (actual,expected) in mapped.into_iter().zip([1.0,0.4,0.3]) {near(actual,expected);}
        assert_eq!(just_outside.components,[1.00001,0.4,0.3],"display mapping preserves source coordinates");
        let mut tiny=Color::new(ColorSpace::Oklch,[0.5,0.000003,0.3],1.0,0);
        tiny.convert_powerless();assert_eq!(tiny.components[1],0.000003);assert_eq!(tiny.missing,4);
        let mut near_gray=Color::new(ColorSpace::Hwb,[0.3,0.2,0.799995],1.0,0);
        near_gray.convert_powerless();assert_eq!(near_gray.components,[0.3,0.2,0.799995]);
        assert_eq!(near_gray.missing,1);
        let method=InterpolationMethod{space:ColorSpace::Srgb,hue:HueInterpolation::Shorter};
        let missing_alpha=PreparedPair::new(Color::new(ColorSpace::Srgb,[1.0,0.0,0.0],0.0,8),
            Color::new(ColorSpace::Srgb,[0.0,0.0,1.0],0.0,8),method).sample(0.5);
        assert_eq!(missing_alpha.components,[0.5,0.0,0.5]);assert_eq!(missing_alpha.missing,8);
        let carried=PreparedPair::new(missing_alpha,Color::rgba8([0,255,0,255]),method).sample(0.5);
        assert_eq!(carried.components,[0.25,0.5,0.25]);assert_eq!(carried.alpha,1.0);
        let p3=Color::new(ColorSpace::DisplayP3,[1.0,0.0,0.0],0.5,0);
        let rgb=p3.to(ColorSpace::Srgb);
        assert!(rgb.components[0]>1.0 && rgb.components[1]<0.0);
        for (actual,expected) in rgb.to(ColorSpace::DisplayP3).components.into_iter().zip(p3.components) {near(actual,expected);}
        let rec=Color::new(ColorSpace::Rec2020,[0.5;3],1.0,0).to(ColorSpace::SrgbLinear);
        for channel in rec.components {near(channel,libm::powf(0.5,2.4));}
        for space in [ColorSpace::ProphotoRgb,ColorSpace::A98Rgb] {
            let white=Color::new(space,[1.0;3],1.0,0).to(ColorSpace::Lab);
            near(white.components[0],100.0);
            assert!(white.components[1].abs()<0.001 && white.components[2].abs()<0.001);
        }
        let lab=Color::new(ColorSpace::Lab,[50.0,0.0,0.0],1.0,0);
        assert_eq!(lab.to_rgba8(),[119,119,119,255]);
        let a=Color::rgba8([0,0,0,255]);let b=Color::rgba8([255,255,255,255]);
        let linear=PreparedPair::new(a,b,InterpolationMethod{space:ColorSpace::SrgbLinear,hue:HueInterpolation::Shorter});
        assert_eq!(linear.sample(0.5).to_rgba8(),[188,188,188,255]);
        let transparent=Color::new(ColorSpace::Srgb,[1.0,0.0,0.0],0.0,0);
        let blue=Color::rgba8([0,0,255,255]);
        let mixed=PreparedPair::new(transparent,blue,InterpolationMethod{space:ColorSpace::Srgb,hue:HueInterpolation::Shorter}).sample(0.5);
        assert_eq!(mixed.to_rgba8(),[0,0,255,128]);
        assert_eq!(p3.components,[1.0,0.0,0.0]);
    }
    #[test]
    fn specification_color_analogous_sets_powerless_hues_and_four_arcs_share_preparation() {
        let method=InterpolationMethod::default();
        let absent=Color::new(ColorSpace::Srgb,[0.0;3],1.0,7);
        let other=Color::new(ColorSpace::Oklab,[0.6,0.1,-0.1],1.0,0);
        let sample=PreparedPair::new(absent,other,method).sample(0.5);
        for (a,b) in sample.components.into_iter().zip(other.components) {near(a,b);}
        let a=Color::new(ColorSpace::Hsl,[0.95,1.0,0.5],0.25,0);
        let b=Color::new(ColorSpace::Hsl,[0.05,1.0,0.5],1.0,0);
        for (hue,expected) in [(HueInterpolation::Shorter,0.0),(HueInterpolation::Longer,0.5),(HueInterpolation::Increasing,0.0),(HueInterpolation::Decreasing,0.5)] {
            let sample=PreparedPair::new(a,b,InterpolationMethod{space:ColorSpace::Hsl,hue}).sample(0.5);
            near(sample.components[0],expected);near(sample.components[2],0.5);near(sample.alpha,0.625);
        }
        let white=Color::rgba8([255,255,255,255]);let green=Color::rgba8([0,255,0,255]);
        let sample=PreparedPair::new(white,green,InterpolationMethod{space:ColorSpace::Hsl,hue:HueInterpolation::Shorter}).sample(0.5);
        near(sample.components[0],1.0/3.0);
        let absent=Color::new(ColorSpace::Lab,[50.0,0.0,0.0],1.0,6);
        let lch=Color::new(ColorSpace::Lch,[50.0,20.0,0.5],1.0,0);
        let sample=PreparedPair::new(absent,lch,InterpolationMethod{space:ColorSpace::Lch,hue:HueInterpolation::Shorter}).sample(0.5);
        near(sample.components[1],20.0);near(sample.components[2],0.5);
    }
}
