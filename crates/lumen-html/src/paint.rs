//! CSS-pixel display list shared by the image and window backends.
use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::ops::Range;

/// Maximum encoded SVG path data retained by one paint command. The image
/// backend applies the segment-count limit while parsing this bounded input.
pub const MAX_SVG_PATH_BYTES: usize = 1024 * 1024;
pub const MAX_SVG_CLIP_PATHS: usize = 256;
pub const MAX_SVG_GRADIENT_STOPS: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SvgGradientUnits {
    ObjectBoundingBox,
    UserSpaceOnUse,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SvgGradientStop {
    pub offset: f32,
    pub color: Rgba,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SvgGradientKind {
    Linear {
        start: [f32; 2],
        end: [f32; 2],
    },
    Radial {
        start: [f32; 2],
        end: [f32; 2],
        start_radius: f32,
        end_radius: f32,
    },
}

/// A resolved local SVG paint server. Coordinates are in object-bounding-box
/// space or the referencing element's current user space, as selected by
/// `units`; `transform` is the SVG gradientTransform matrix.
#[derive(Clone, Debug, PartialEq)]
pub struct SvgGradient {
    pub units: SvgGradientUnits,
    pub transform: Affine,
    pub kind: SvgGradientKind,
    pub stops: Arc<[SvgGradientStop]>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SvgPaint {
    Color(Rgba),
    Gradient(Arc<SvgGradient>),
}

pub fn valid_svg_paint(paint: Option<&SvgPaint>) -> bool {
    let Some(SvgPaint::Gradient(gradient)) = paint else {
        return true;
    };
    let finite_point = |point: [f32; 2]| point.into_iter().all(f32::is_finite);
    let valid_kind = match gradient.kind {
        SvgGradientKind::Linear { start, end } => finite_point(start) && finite_point(end),
        SvgGradientKind::Radial {
            start,
            end,
            start_radius,
            end_radius,
        } => {
            finite_point(start)
                && finite_point(end)
                && start_radius.is_finite()
                && end_radius.is_finite()
                && start_radius >= 0.0
                && end_radius >= 0.0
        }
    };
    gradient.transform.is_finite()
        && valid_kind
        && gradient.stops.len() <= MAX_SVG_GRADIENT_STOPS
        && gradient
            .stops
            .iter()
            .all(|stop| stop.offset.is_finite() && (0.0..=1.0).contains(&stop.offset))
}

pub fn svg_paint_has_ink(paint: Option<&SvgPaint>) -> bool {
    match paint {
        Some(SvgPaint::Color(color)) => color.a != 0,
        Some(SvgPaint::Gradient(gradient)) => gradient.stops.iter().any(|stop| stop.color.a != 0),
        None => false,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SvgClipShape {
    pub data: Arc<str>,
    /// Element transform within the clipPath coordinate system.
    pub transform: Affine,
    pub fill_rule: SvgFillRule,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SvgClip {
    pub units: SvgGradientUnits,
    pub transform: Affine,
    pub shapes: Arc<[SvgClipShape]>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Glyph {
    pub id: u16,
    /// Stable face identity; zero selects a legacy provider's default face.
    pub face: u64,
    /// UTF-8 byte offset of the grapheme cluster that produced this glyph.
    pub cluster: u32,
    /// Grapheme ordinal within a synthesized Unicode caps expansion. Source
    /// offsets stay intact while spacing can distinguish e.g. the two S's in ß.
    pub caps_expansion: u8,
    pub x: f32,
    pub y: f32,
    /// Raster scale relative to the computed CSS font size. CSS font fallback
    /// can adjust each selected face independently while shaped advances and
    /// offsets are already expressed at that face's used size.
    pub size_scale: f32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FontStyle {
    #[default]
    Normal,
    Italic,
    Oblique,
    ObliqueAngle(FontAngle),
}

/// Validated CSS angle. Bit storage preserves Eq/cache identity without NaNs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FontAngle(u32);
impl FontAngle {
    pub fn new(degrees: f32) -> Option<Self> {
        (degrees.is_finite() && (-90.0..=90.0).contains(&degrees))
            .then(|| Self(if degrees == 0.0 { 0 } else { degrees.to_bits() }))
    }
    pub fn degrees(self) -> f32 { f32::from_bits(self.0) }
}
impl FontStyle {
    pub fn oblique(degrees: f32) -> Option<Self> {
        let angle = FontAngle::new(degrees)?;
        Some(if angle.degrees() == 0.0 { Self::Normal } else { Self::ObliqueAngle(angle) })
    }
    pub fn angle(self) -> Option<f32> {
        match self {
            Self::Normal => Some(0.0), Self::Italic => None,
            Self::Oblique => Some(14.0), Self::ObliqueAngle(angle) => Some(angle.degrees()),
        }
    }
    /// Stable identity including whether an angle was explicitly specified.
    pub fn cache_key(self) -> (u8, u32) {
        match self {
            Self::Normal => (0, 0), Self::Italic => (1, 0), Self::Oblique => (2, 0),
            Self::ObliqueAngle(angle) => (3, angle.0),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FontMetric {
    ExHeight,
    CapHeight,
    ChWidth,
    IcWidth,
    IcHeight,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FontSizeAdjustValue {
    Number(f32),
    FromFont,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FontSizeAdjust {
    pub metric: FontMetric,
    pub value: FontSizeAdjustValue,
}

/// The `ex` and `ch` unit bases for one computed font selection.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FontRelativeMetrics {
    pub ex: f32,
    pub ch: f32,
}

/// Optional unshaped metrics needed by CSS cap/ch/ic units. Absence asks the
/// computed-value resolver to use CSS Values' specified font-metric fallback.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FontUnitMetrics {
    pub cap: Option<f32>,
    pub ch: Option<f32>,
    pub ic: Option<f32>,
}


/// Actual primary-font character advances for HTML's default control sizes.
/// These are font metadata, independent of letter spacing and shaped contents.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PrimaryCharacterWidths {
    pub average: f32,
    pub maximum: f32,
}

/// Descriptor match order from CSS Fonts §5. Equal ranks are resolved by the
/// caller using the source ordering that applies to its font-face collection.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct FontMatchRank {
    stretch_group: u8,
    stretch_order: u32,
    style_group: u8,
    style_order: u32,
    weight_group: u8,
    weight_order: u16,
}

fn angle_order(angle: f32) -> u32 {
    let bits = if angle == 0.0 { 0 } else { angle.to_bits() };
    if angle < 0.0 { !bits } else { bits | 0x8000_0000 }
}

/// CSS Fonts style search order, before weight. Negative requests mirror the
/// positive search; the 11-degree threshold distinguishes shallow slopes.
fn font_style_rank(wanted: FontStyle, actual: FontStyle) -> (u8, u32) {
    let Some(mut desired) = wanted.angle() else {
        return match actual.angle() {
            None => (0, 0),
            Some(angle) if angle >= 11.0 => (1, angle_order(angle)),
            Some(angle) if angle > 0.0 => (2, u32::MAX - angle_order(angle)),
            Some(angle) => (3, u32::MAX - angle_order(angle)),
        };
    };
    let Some(mut angle) = actual.angle() else { return (if desired < 0.0 { 4 } else { 2 }, 0); };
    if desired == 0.0 {
        return if angle >= 0.0 { (0, angle_order(angle)) }
            else { (3, u32::MAX - angle_order(angle)) };
    }
    if desired < 0.0 { desired = -desired; angle = -angle; }
    if angle <= 0.0 { return (3, u32::MAX - angle_order(angle)); }
    if desired >= 11.0 {
        if angle >= desired { (0, angle_order(angle)) }
        else { (1, u32::MAX - angle_order(angle)) }
    } else if angle <= desired {
        (0, u32::MAX - angle_order(angle))
    } else {
        (1, angle_order(angle))
    }
}

fn font_weight_rank(wanted: u16, actual: u16) -> (u8, u16) {
    if (400..=500).contains(&wanted) {
        if (wanted..=500).contains(&actual) {
            (0, actual)
        } else if actual < wanted {
            (1, u16::MAX - actual)
        } else {
            (2, actual)
        }
    } else if wanted < 400 {
        if actual <= wanted {
            (0, u16::MAX - actual)
        } else {
            (1, actual)
        }
    } else if actual >= wanted {
        (0, actual)
    } else {
        (1, u16::MAX - actual)
    }
}

/// Rank one available face against a requested CSS font selection. Family and
/// Unicode coverage filtering remain with the caller because they depend on
/// its source records and the requested text.
pub fn font_match_rank(
    requested: &FontSpec,
    actual_style: FontStyle,
    actual_weight: u16,
    actual_stretch: f32,
) -> Option<FontMatchRank> {
    if requested.unresolved_style.is_some() || requested.unresolved_stretch.is_some() || !(1..=1000).contains(&requested.weight)
        || !(1..=1000).contains(&actual_weight)
        || !requested.stretch.is_finite()
        || requested.stretch < 0.0
        || !actual_stretch.is_finite()
        || actual_stretch < 0.0
    {
        return None;
    }
    let bits = if actual_stretch == 0.0 {
        0
    } else {
        actual_stretch.to_bits()
    };
    let (stretch_group, stretch_order) = if requested.stretch <= 100.0 {
        if actual_stretch <= requested.stretch {
            (0, u32::MAX - bits)
        } else {
            (1, bits)
        }
    } else if actual_stretch >= requested.stretch {
        (0, bits)
    } else {
        (1, u32::MAX - bits)
    };
    let (weight_group, weight_order) = font_weight_rank(requested.weight, actual_weight);
    let (style_group, style_order) = font_style_rank(requested.style, actual_style);
    Some(FontMatchRank {
        stretch_group,
        stretch_order,
        style_group,
        style_order,
        weight_group,
        weight_order,
    })
}

/// Rank variable-font descriptors that advertise weight and stretch ranges.
/// When the requested value falls inside a range, that face matches at the
/// exact requested value; otherwise the same endpoint preference as ordinary
/// font matching chooses its nearest available descriptor.
pub fn font_match_range_rank(
    requested: &FontSpec,
    actual_style: FontStyle,
    weight_range: [u16; 2],
    stretch_range: [f32; 2],
) -> Option<(FontMatchRank, u16, f32)> {
    let (rank, weight, stretch, _) = font_match_style_range_rank(requested, actual_style,
        None, weight_range, stretch_range)?;
    Some((rank, weight, stretch))
}

/// Shared descriptor selection for CSS loading and decoded face registries.
/// Returns the selected slope as part of the face combination identity.
pub fn font_match_style_range_rank(
    requested: &FontSpec,
    actual_style: FontStyle,
    oblique_range: Option<[f32; 2]>,
    weight_range: [u16; 2],
    stretch_range: [f32; 2],
) -> Option<(FontMatchRank, u16, f32, FontStyle)> {
    if requested.unresolved_style.is_some() || requested.unresolved_stretch.is_some() { return None; }
    if oblique_range.is_some_and(|range| range[0] > range[1]
        || range.iter().any(|angle| FontAngle::new(*angle).is_none())
        || !matches!(actual_style, FontStyle::Oblique | FontStyle::ObliqueAngle(_))) {
        return None;
    }
    let styles = if let Some(range) = oblique_range {
        let desired = requested.style.angle().unwrap_or(11.0);
        if (range[0]..=range[1]).contains(&desired) {
            [FontStyle::oblique(desired), None]
        } else { [FontStyle::oblique(range[0]), FontStyle::oblique(range[1])] }
    } else { [Some(actual_style), None] };
    if weight_range[0] == 0
        || weight_range[0] > weight_range[1]
        || weight_range[1] > 1000
        || stretch_range
            .iter()
            .any(|value| !value.is_finite() || *value < 0.0)
        || stretch_range[0] > stretch_range[1]
    {
        return None;
    }
    let weights = if (weight_range[0]..=weight_range[1]).contains(&requested.weight) {
        [Some(requested.weight), None]
    } else {
        [Some(weight_range[0]), Some(weight_range[1])]
    };
    let stretches = if (stretch_range[0]..=stretch_range[1]).contains(&requested.stretch) {
        [Some(requested.stretch), None]
    } else {
        [Some(stretch_range[0]), Some(stretch_range[1])]
    };
    let mut best = None;
    for style in styles.into_iter().flatten() {
      for weight in weights.into_iter().flatten() {
        for stretch in stretches.into_iter().flatten() {
            let rank = font_match_rank(requested, style, weight, stretch)?;
            if best.is_none_or(|(prior, _, _, _)| rank < prior) {
                best = Some((rank, weight, stretch, style));
            }
        }
      }
    }
    best
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FontVariantCaps {
    #[default]
    Normal,
    SmallCaps,
    AllSmallCaps,
    PetiteCaps,
    AllPetiteCaps,
    Unicase,
    TitlingCaps,
}
impl FontVariantCaps {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::SmallCaps => "small-caps",
            Self::AllSmallCaps => "all-small-caps",
            Self::PetiteCaps => "petite-caps",
            Self::AllPetiteCaps => "all-petite-caps",
            Self::Unicase => "unicase",
            Self::TitlingCaps => "titling-caps",
        }
    }
    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "normal" => Self::Normal,
            "small-caps" => Self::SmallCaps,
            "all-small-caps" => Self::AllSmallCaps,
            "petite-caps" => Self::PetiteCaps,
            "all-petite-caps" => Self::AllPetiteCaps,
            "unicase" => Self::Unicase,
            "titling-caps" => Self::TitlingCaps,
            _ => return None,
        })
    }
}

/// Generic keywords are distinct from family names even when their spelling
/// is identical. A quoted "serif" must select a named face, never a generic.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GenericFontFamily {
    Serif, SansSerif, Monospace, Cursive, Fantasy, SystemUi,
    UiSerif, UiSansSerif, UiMonospace, UiRounded, Emoji, Math, Fangsong,
}
impl GenericFontFamily {
    pub fn parse(value: &str) -> Option<Self> {
        [Self::Serif, Self::SansSerif, Self::Monospace, Self::Cursive,
            Self::Fantasy, Self::SystemUi, Self::UiSerif, Self::UiSansSerif,
            Self::UiMonospace, Self::UiRounded, Self::Emoji, Self::Math, Self::Fangsong]
            .into_iter().find(|family| family.as_str().eq_ignore_ascii_case(value))
    }
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Serif => "serif", Self::SansSerif => "sans-serif",
            Self::Monospace => "monospace", Self::Cursive => "cursive",
            Self::Fantasy => "fantasy", Self::SystemUi => "system-ui",
            Self::UiSerif => "ui-serif", Self::UiSansSerif => "ui-sans-serif",
            Self::UiMonospace => "ui-monospace", Self::UiRounded => "ui-rounded",
            Self::Emoji => "emoji", Self::Math => "math", Self::Fangsong => "fangsong",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FontFamily {
    Named(Arc<str>),
    Generic(GenericFontFamily),
}
impl FontFamily {
    pub const fn is_generic(&self) -> bool { matches!(self, Self::Generic(_)) }
    pub fn named(&self) -> Option<&Arc<str>> {
        match self { Self::Named(name) => Some(name), Self::Generic(_) => None }
    }
    /// Bytes owned by this entry, excluding the shared containing slice.
    pub fn retained_bytes(&self) -> usize {
        self.named().map_or(0, |name| name.len() + 2 * core::mem::size_of::<usize>())
    }
}
impl From<&str> for FontFamily {
    fn from(value: &str) -> Self { Self::Named(Arc::from(value)) }
}
impl From<Arc<str>> for FontFamily {
    fn from(value: Arc<str>) -> Self { Self::Named(value) }
}
impl AsRef<str> for FontFamily {
    fn as_ref(&self) -> &str {
        match self { Self::Named(name) => name, Self::Generic(family) => family.as_str() }
    }
}
impl core::ops::Deref for FontFamily {
    type Target = str;
    fn deref(&self) -> &str { self.as_ref() }
}

/// One mutually exclusive group of optional OpenType text features.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FontLigatureGroup { Common, Discretionary, Historical, Contextual }
impl FontLigatureGroup {
    pub const ALL: [Self;4] = [Self::Common, Self::Discretionary, Self::Historical, Self::Contextual];
    pub fn parse(token: &str) -> Option<(Self,bool)> {
        Self::ALL.into_iter().find_map(|group| {
            if token.eq_ignore_ascii_case(group.as_str(true)) { Some((group,true)) }
            else if token.eq_ignore_ascii_case(group.as_str(false)) { Some((group,false)) } else { None }
        })
    }
    pub const fn as_str(self, enabled: bool) -> &'static str {
        match (self,enabled) {
            (Self::Common,true)=>"common-ligatures", (Self::Common,false)=>"no-common-ligatures",
            (Self::Discretionary,true)=>"discretionary-ligatures", (Self::Discretionary,false)=>"no-discretionary-ligatures",
            (Self::Historical,true)=>"historical-ligatures", (Self::Historical,false)=>"no-historical-ligatures",
            (Self::Contextual,true)=>"contextual", (Self::Contextual,false)=>"no-contextual",
        }
    }
    const fn bit(self) -> u8 { 1 << self as u8 }
}
/// Compact specified identity plus optional overrides. Required script features
/// remain owned by the shaping engine; `none` never disables `rlig`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FontLigatures { specified: u8, enabled: u8 }
impl FontLigatures {
    pub const NORMAL: Self = Self {specified:0, enabled:0};
    pub const NONE: Self = Self {specified:0x8f, enabled:0};
    pub const fn is_normal(self) -> bool { self.specified == 0 }
    pub const fn is_none(self) -> bool { self.specified & 0x80 != 0 }
    pub fn set(&mut self, group:FontLigatureGroup, enabled:bool) -> bool {
        let bit = group.bit();
        if self.specified & bit != 0 { return false; }
        self.specified |= bit;
        if enabled { self.enabled |= bit; }
        true
    }
    pub fn setting(self, group:FontLigatureGroup) -> Option<bool> {
        (self.specified & group.bit() != 0).then_some(self.enabled & group.bit() != 0)
    }
    pub const fn cache_key(self) -> [u8;2] { [self.specified,self.enabled] }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FontFeature { pub tag: [u8; 4], pub value: u32 }

#[derive(Clone, Debug, PartialEq)]
pub struct FontFeatureInput {
    pub tag: [u8; 4],
    pub value: Arc<crate::css::typed_numeric::NumericExpression>,
}

/// Ordered specified inputs and compact resolved maps share one font metadata
/// handle. Ordinary fonts retain no list; query math stays shared until resolved.
#[derive(Clone, Debug, PartialEq)]
pub enum FontFeatureSettings {
    Values(Box<[FontFeature]>),
    Expressions(Box<[FontFeatureInput]>),
}
impl FontFeatureSettings {
    pub fn resolved(&self) -> Option<&[FontFeature]> {
        match self { Self::Values(values) => Some(values), Self::Expressions(_) => None }
    }
    pub fn retained_bytes(&self) -> usize {
        match self {
            Self::Values(values) => core::mem::size_of_val(values.as_ref()),
            Self::Expressions(values) => core::mem::size_of_val(values.as_ref()) + values.iter()
                .map(|value| 2 * core::mem::size_of::<usize>() + core::mem::size_of_val(value.value.as_ref()) + value.value.retained_bytes()).sum::<usize>(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct FontSpec {
    pub families: Option<Arc<[FontFamily]>>,
    /// Shared declaration-origin shadow roots, nearest first; None is document scope.
    pub family_scope: Option<Arc<[crate::NodeId]>>,
    pub weight: u16,
    pub style: FontStyle,
    /// Valid angle math awaiting an owner's real query-container context.
    /// The bounded tree is shared; unresolved values must not select a face.
    pub unresolved_style: Option<Arc<crate::css::typed_numeric::NumericExpression>>,
    /// Percentage math retained until the owner supplies actual query bases.
    pub unresolved_stretch: Option<Arc<crate::css::typed_numeric::NumericExpression>>,
    /// Requested face width as a percentage; 100 is normal.
    pub stretch: f32,
    pub size_adjust: Option<FontSizeAdjust>,
    pub caps: FontVariantCaps,
    pub ligatures: FontLigatures,
    pub feature_settings: Option<Arc<FontFeatureSettings>>,
    pub alternates: Option<Arc<crate::css::font_feature_values::Alternates>>,
    /// Rendering policy derived from letter-spacing; separate from the
    /// specified ligature property so CSSOM still reports its computed value.
    pub disable_optional_ligatures: bool,
    pub synthesize_small_caps: bool,
}
impl Default for FontSpec {
    fn default() -> Self {
        Self {
            families: None,
            family_scope: None,
            weight: 400,
            style: FontStyle::Normal,
            unresolved_style: None,
            unresolved_stretch: None,
            stretch: 100.0,
            size_adjust: None,
            caps: FontVariantCaps::Normal,
            ligatures: FontLigatures::NORMAL,
            feature_settings: None,
            alternates: None,
            disable_optional_ligatures: false,
            synthesize_small_caps: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ShapedRun {
    pub glyphs: Arc<[Glyph]>,
    pub width: f32,
}
impl ShapedRun {
    /// Additional spacing graphemes emitted by real synthetic caps shaping.
    /// Clusters stay contiguous within visual runs; feature-backed caps emit
    /// no expansions. This iterator adds no scratch allocation or case mapping.
    pub fn caps_expansions(&self) -> CapsExpansions<'_> {
        CapsExpansions {
            glyphs: &self.glyphs,
            index: 0,
        }
    }
}

/// One indivisible source interval emitted by a shaped run and its actual
/// horizontal advance. Intervals are UTF-8 byte ranges into the input text,
/// sorted in logical order and aligned to grapheme boundaries. A shaping
/// cluster that spans several graphemes remains one interval; its advance is
/// never guessed or divided between those graphemes.
#[derive(Clone, Debug, PartialEq)]
pub struct ShapedClusterAdvance {
    pub source: Range<usize>,
    pub advance: f32,
}

/// Optional emergency-layout detail. Ordinary shaped runs and their cache
/// entries stay compact; backends produce this sidecar only when requested.
#[derive(Clone, Debug, PartialEq)]
pub struct ShapedRunWithClusterAdvances {
    pub run: ShapedRun,
    pub clusters: Arc<[ShapedClusterAdvance]>,
}
pub struct CapsExpansions<'a> {
    glyphs: &'a [Glyph],
    index: usize,
}
impl Iterator for CapsExpansions<'_> {
    type Item = (u32, u8);
    fn next(&mut self) -> Option<Self::Item> {
        while let Some(first) = self.glyphs.get(self.index) {
            let cluster = first.cluster;
            let mut expansion = 0;
            while let Some(glyph) = self
                .glyphs
                .get(self.index)
                .filter(|glyph| glyph.cluster == cluster)
            {
                expansion = expansion.max(glyph.caps_expansion);
                self.index += 1;
            }
            if expansion != 0 {
                return Some((cluster, expansion));
            }
        }
        None
    }
}

pub use lumen_common::raster::Rgba8Image as ImageData;

pub trait TextShaper {
    /// None means the provider has no metric authority. Some(None) means
    /// the selected first available font actually lacks the requested metric.
    fn first_available_font_metric(&self, _font: &FontSpec, _metric: FontMetric) -> Option<Option<f32>> {
        None
    }
    /// A provider without primary-font metadata reports an unavailable metric;
    /// layout must not substitute a guessed character width.
    fn primary_character_widths_styled(&self, _size: f32, _font: &FontSpec) -> Option<PrimaryCharacterWidths> {
        None
    }
    /// Actual raster ink in run coordinates, relative to the run origin and
    /// baseline. Used only when a shared cluster needs separate paint strips.
    fn glyph_ink_bounds(&self, _glyph: &Glyph, _size: f32) -> Option<Rect> { None }
    /// Identity/generation of the immutable font configuration used for layout.
    fn generation(&self) -> u64 {
        0
    }
    fn shape(&self, text: &str, size: f32) -> Result<ShapedRun, ()>;
    fn shape_directional(&self, text: &str, size: f32, _rtl: bool) -> Result<ShapedRun, ()> {
        self.shape(text, size)
    }
    fn measure(&self, text: &str, size: f32) -> Result<f32, ()> {
        self.shape(text, size).map(|run| run.width)
    }
    fn shape_styled(
        &self,
        text: &str,
        size: f32,
        rtl: bool,
        _font: &FontSpec,
    ) -> Result<ShapedRun, ()> {
        self.shape_directional(text, size, rtl)
    }
    /// Shape with source-cluster advances for emergency line breaking. The
    /// default keeps existing shapers allocation-free and lets callers use
    /// their ordinary run when a backend has no cluster metrics.
    fn shape_styled_with_cluster_advances(
        &self,
        _text: &str,
        _size: f32,
        _rtl: bool,
        _font: &FontSpec,
    ) -> Result<Option<ShapedRunWithClusterAdvances>, ()> {
        Ok(None)
    }
    /// Shape an item whose bidi direction has already been resolved by layout.
    fn shape_resolved(
        &self,
        text: &str,
        size: f32,
        rtl: bool,
        font: &FontSpec,
    ) -> Result<ShapedRun, ()> {
        self.shape_styled(text, size, rtl, font)
    }
    /// Resolved-direction counterpart to
    /// [`TextShaper::shape_styled_with_cluster_advances`].
    fn shape_resolved_with_cluster_advances(
        &self,
        _text: &str,
        _size: f32,
        _rtl: bool,
        _font: &FontSpec,
    ) -> Result<Option<ShapedRunWithClusterAdvances>, ()> {
        Ok(None)
    }
    /// Transient content language for an already resolved item. Font metadata
    /// and DOM nodes retain no language copy; native backends apply OpenType
    /// language systems while simple host shapers can reuse their metrics.
    fn shape_resolved_context_with_cluster_advances(
        &self, text: &str, size: f32, rtl: bool, font: &FontSpec, _language: Option<&str>,
    ) -> Result<Option<ShapedRunWithClusterAdvances>, ()> {
        self.shape_resolved_with_cluster_advances(text,size,rtl,font)
    }
/// Shape a source item with surrounding CSS-permitted joining context.
/// Clusters and advances remain relative to `source`, never its neighbors.
fn shape_resolved_segment_with_cluster_advances(
    &self, text: &str, source: core::ops::Range<usize>, size: f32,
    rtl: bool, font: &FontSpec, language: Option<&str>,
) -> Result<Option<ShapedRunWithClusterAdvances>, ()> {
    let item = text.get(source).ok_or(())?;
    self.shape_resolved_context_with_cluster_advances(item,size,rtl,font,language)
}
    fn shape_resolved_context(&self,text:&str,size:f32,rtl:bool,font:&FontSpec,language:Option<&str>) -> Result<ShapedRun,()> {
        if let Some(detail)=self.shape_resolved_context_with_cluster_advances(text,size,rtl,font,language)? {Ok(detail.run)}
        else {self.shape_resolved(text,size,rtl,font)}
    }
    fn shape_styled_context(&self,text:&str,size:f32,rtl:bool,font:&FontSpec,_language:Option<&str>) -> Result<ShapedRun,()> {
        self.shape_styled(text,size,rtl,font)
    }
    fn measure_styled(&self, text: &str, size: f32, font: &FontSpec) -> Result<f32, ()> {
        self.shape_styled(text, size, false, font)
            .map(|run| run.width)
    }
    fn ascent_styled(&self, size: f32, _font: &FontSpec) -> f32 {
        self.ascent(size)
    }
    fn line_height_styled(&self, size: f32, _font: &FontSpec) -> f32 {
        self.line_height(size)
    }
    fn underline_metrics_styled(&self, size: f32, _font: &FontSpec) -> (f32, f32) {
        self.underline_metrics(size)
    }
    fn strike_metrics_styled(&self, size: f32, _font: &FontSpec) -> (f32, f32) {
        self.strike_metrics(size)
    }
    /// Unshaped CSS unit metrics. `vertical` selects upright inline advances.
    fn font_unit_metrics_styled(&self, size: f32, font: &FontSpec, _vertical: bool, upright_zero: bool) -> FontUnitMetrics {
        FontUnitMetrics { cap: None, ch: (!upright_zero).then(|| self.font_relative_metrics_styled(size, font).ch), ic: None }
    }
    fn font_relative_metrics_styled(&self, size: f32, _font: &FontSpec) -> FontRelativeMetrics {
        FontRelativeMetrics {
            ex: size * 0.5,
            ch: size * 0.5,
        }
    }
    fn ascent(&self, size: f32) -> f32;
    fn line_height(&self, size: f32) -> f32;
    fn underline_metrics(&self, size: f32) -> (f32, f32) {
        (size * 0.1, size / 16.0)
    }
    fn strike_metrics(&self, size: f32) -> (f32, f32) {
        (-size * 0.3, size / 16.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub fn is_valid(self) -> bool {
        [self.x, self.y, self.width, self.height]
            .iter()
            .all(|n| n.is_finite())
            && self.width >= 0.0
            && self.height >= 0.0
    }

    /// Physical TL, TR, BR, BL elliptical corners, already overlap-normalized.
    pub fn contains_corners(self, x: f32, y: f32, corners: &[[f32; 2]; 4]) -> bool {
        if !self.contains_rounded(x, y, 0.0) {
            return false;
        }
        for (i, r) in corners.iter().enumerate() {
            if r[0] <= 0.0 || r[1] <= 0.0 {
                continue;
            }
            let left = i == 0 || i == 3;
            let top = i < 2;
            let cx = if left {
                self.x + r[0]
            } else {
                self.x + self.width - r[0]
            };
            let cy = if top {
                self.y + r[1]
            } else {
                self.y + self.height - r[1]
            };
            if (if left { x < cx } else { x > cx }) && (if top { y < cy } else { y > cy }) {
                let dx = (x - cx) / r[0];
                let dy = (y - cy) / r[1];
                if dx * dx + dy * dy > 1.0 {
                    return false;
                }
            }
        }
        true
    }
    pub fn contains_rounded(self, x: f32, y: f32, radius: f32) -> bool {
        if x < self.x || y < self.y || x >= self.x + self.width || y >= self.y + self.height {
            return false;
        }
        let radius = radius.min(self.width * 0.5).min(self.height * 0.5);
        if radius <= 0.0 {
            return true;
        }
        let dx = x - x.clamp(self.x + radius, self.x + self.width - radius);
        let dy = y - y.clamp(self.y + radius, self.y + self.height - radius);
        dx * dx + dy * dy <= radius * radius
    }
    pub fn intersection(self, other: Self) -> Option<Self> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = (self.x + self.width).min(other.x + other.width);
        let bottom = (self.y + self.height).min(other.y + other.height);
        (right > x && bottom > y).then_some(Self {
            x,
            y,
            width: right - x,
            height: bottom - y,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Gradient {
    pub kind: GradientKind,
    pub repeating: bool,
    pub stops: Arc<[GradientStop]>,
    /// Conic source units and transition hints, shared only when needed.
    pub angular: Option<Arc<crate::css::ConicStopMetadata>>,
    /// Explicit interpolation, unquantized endpoints and transition hints are
    /// sparse. The ordinary named/hex gradient keeps the compatibility sRGB default without it.
    pub color: Option<Arc<GradientColorMetadata>>,
}


#[derive(Clone, Debug, PartialEq)]
pub struct GradientColorMetadata {
    pub method: lumen_common::color::InterpolationMethod,
    /// Empty means exact RGBA8 endpoints stored in GradientStop.
    pub colors: Box<[lumen_common::color::Color]>,
    pub hints: Box<[(usize, GradientPosition)]>,
    /// color(srgb) and relative/color-mix sRGB endpoints retain modern syntax.
    pub color_functions: u32,
    /// A single authored stop retains two identical rendering anchors.
    pub single_stop: bool,
}
impl GradientColorMetadata {
    pub fn valid_for(&self, count:usize)->bool {
        (!self.single_stop || count==2) && count<=32 && (count==32 || self.color_functions >> count == 0)
            && (self.colors.is_empty() || self.colors.len()==count) && self.colors.iter().all(|color|color.is_finite())
            && self.hints.len()<count && self.hints.iter().all(|(after,position)|*after<count.saturating_sub(1) && match position {
                GradientPosition::Fraction(value)|GradientPosition::Pixels(value)=>value.is_finite(),
                GradientPosition::Mixed(value)=>value.pixels.is_finite() && value.fraction.is_finite(),
            })
    }
    pub fn checked_retained_bytes(&self)->Option<usize> {
        (2*core::mem::size_of::<usize>()).checked_add(core::mem::size_of::<Self>())?
            .checked_add(self.colors.len().checked_mul(core::mem::size_of::<lumen_common::color::Color>())?)?
            .checked_add(self.hints.len().checked_mul(core::mem::size_of::<(usize,GradientPosition)>())?)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GradientKind {
    /// CSS degrees: zero points up; corner directions use box-dependent angles.
    Linear {
        angle: f32,
        corner: Option<(i8, i8)>,
    },
    Radial {
        shape: RadialShape,
        size: RadialSize,
        center: [LengthPercentage; 2],
    },
    /// CSS Images §3.4: `from <angle>` in CSS degrees (zero points up,
    /// clockwise) around a center point; angles sweep clockwise.
    Conic {
        from: f32,
        center: [LengthPercentage; 2],
    },
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RadialShape {
    Circle,
    Ellipse,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RadialSize {
    ClosestSide,
    FarthestSide,
    ClosestCorner,
    FarthestCorner,
    Radii([LengthPercentage; 2]),
}
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LengthPercentage {
    pub pixels: f32,
    pub fraction: f32,
}
impl LengthPercentage {
    pub fn resolve(self, basis: f32) -> f32 {
        self.pixels + self.fraction * basis
    }
    fn is_finite(self) -> bool {
        self.pixels.is_finite() && self.fraction.is_finite()
    }
}
impl Gradient {
    pub fn is_valid(&self) -> bool {
        let valid_kind = match self.kind {
            GradientKind::Linear { angle, corner } => {
                angle.is_finite()
                    && !corner.is_some_and(|(x, y)| !matches!(x, -1 | 1) || !matches!(y, -1 | 1))
            }
            GradientKind::Radial {
                shape,
                size,
                center,
            } => {
                center.iter().all(|v| v.is_finite())
                    && match size {
                        RadialSize::Radii(radii) => {
                            radii
                                .iter()
                                .all(|v| v.is_finite() && (v.fraction != 0.0 || v.pixels >= 0.0))
                                && (shape != RadialShape::Circle
                                    || radii[0] == radii[1] && radii[0].fraction == 0.0)
                        }
                        _ => true,
                    }
            }
            GradientKind::Conic { from, center } => {
                from.is_finite() && center.iter().all(|v| v.is_finite())
            }
        };
        valid_kind
            && self.color.as_ref().is_none_or(|metadata|metadata.valid_for(self.stops.len()))
            && self.angular.as_ref().is_none_or(|metadata| matches!(self.kind, GradientKind::Conic {..}) && metadata.valid_for(self.stops.len()))
            && (2..=32).contains(&self.stops.len())
            && self.stops.iter().all(|stop| match stop.position {
                Some(GradientPosition::Fraction(v) | GradientPosition::Pixels(v)) => v.is_finite(),
                Some(GradientPosition::Mixed(v)) => v.is_finite(),
                None => true,
            })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackgroundRepeat {
    Repeat,
    NoRepeat,
    Space,
    Round,
}
#[derive(Clone, Debug, PartialEq)]
pub enum BackgroundPaint {
    Solid(Rgba),
    Border(Arc<BorderImagePaint>),
    CrossFade(Arc<[(BackgroundPaint, f32)]>),
    Gradient(Arc<Gradient>),
    Image(Arc<ImageData>),
    Worklet(Arc<PaintWorkletImage>),
}

/// One immutable nine-slice carrier. Repeat counts are evaluated by the
/// raster sampler, so large border regions never create per-tile commands.
#[derive(Clone,Debug,PartialEq)]
pub struct BorderImagePaint {
    pub image:BackgroundPaint,
    /// Canonical normal border while the image source is not yet available.
    /// Coordinates are relative to the border-image area, so command moves
    /// and affine replay preserve its geometry.
    pub fallback:BoxBorder,
    pub source_size:[f32;2],
    pub slices:[f32;4],
    pub widths:[f32;4],
    pub repeat:[crate::css::BorderImageRepeat;2],
    pub fill:bool,
}
impl BorderImagePaint {
    pub fn source_coordinate(&self,x:f32,y:f32,width:f32,height:f32)->Option<([f32;2],[f32;4])> {
        use crate::css::BorderImageRepeat as Repeat;
        fn axis(value:f32,length:f32,tile:f32,repeat:Repeat)->Option<f32> {
            if length<=0.0||tile<=0.0||!tile.is_finite(){return None;}
            match repeat {
                Repeat::Stretch=>Some(value/length),
                Repeat::Round=>{let count=libm::roundf(length/tile).max(1.0);let tile=length/count;Some({let n=value/tile;n-libm::floorf(n)})},
                Repeat::Repeat=>Some({let n=(value-(length-tile)*0.5)/tile;n-libm::floorf(n)}),
                Repeat::Space=>{let count=libm::floorf(length/tile);if count<1.0{return None;}let gap=(length-count*tile)/(count+1.0);let step=tile+gap;let index=libm::floorf((value-gap)/step);if index<0.0||index>=count{return None;}let offset=value-gap-index*step;(offset>=0.0&&offset<tile).then_some(offset/tile)},
            }
        }
        let [sw,sh]=self.source_size;let [top,right,bottom,left]=self.slices;let [wt,wr,wb,wl]=self.widths;
        let sx=[0.0,left,sw-right,sw];let sy=[0.0,top,sh-bottom,sh];
        let dx=[0.0,wl,width-wr,width];let dy=[0.0,wt,height-wb,height];
        let column=if x<dx[1]{0}else if x>=dx[2]{2}else{1};let row=if y<dy[1]{0}else if y>=dy[2]{2}else{1};
        if column==1&&row==1&&!self.fill{return None;}
        let source_width=sx[column+1]-sx[column];let source_height=sy[row+1]-sy[row];
        let region_width=dx[column+1]-dx[column];let region_height=dy[row+1]-dy[row];
        if source_width<=0.0||source_height<=0.0||region_width<=0.0||region_height<=0.0{return None;}
        let valid_scale=|a:f32,b:f32|if b>0.0&&(a/b).is_finite()&&a>0.0{Some(a/b)}else{None};
        let center_x=valid_scale(wt,top).or_else(||valid_scale(wb,bottom)).unwrap_or(1.0);
        let center_y=valid_scale(wl,left).or_else(||valid_scale(wr,right)).unwrap_or(1.0);
        let (xtile,xrepeat)=if column!=1{(region_width,Repeat::Stretch)}else if row!=1{(source_width*region_height/source_height,self.repeat[0])}else{(source_width*center_x,self.repeat[0])};
        let (ytile,yrepeat)=if row!=1{(region_height,Repeat::Stretch)}else if column!=1{(source_height*region_width/source_width,self.repeat[1])}else{(source_height*center_y,self.repeat[1])};
        Some(([sx[column]+axis(x-dx[column],region_width,xtile,xrepeat)?*source_width,
            sy[row]+axis(y-dy[row],region_height,ytile,yrepeat)?*source_height],
            [sx[column],sy[row],sx[column+1],sy[row+1]]))
    }
}

/// Immutable procedural image inputs from the actual background tile and
/// computed cascade. The host evaluates JavaScript outside layout/raster.
#[derive(Clone, Debug, PartialEq)]
pub struct PaintWorkletRequest {
    pub name: Arc<str>,
    pub arguments: Arc<[Arc<str>]>,
    /// Registration and cascade context can change typed values without
    /// changing their serialized property text.
    pub registration_revision: u64,
    pub width: f32,
    pub height: f32,
    pub properties: Arc<[(Arc<str>, Option<Arc<str>>)]>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PaintWorkletImage {
    pub request: PaintWorkletRequest,
    pub input_properties: Arc<[Arc<str>]>,
    /// Reservations follow pixels through retained frames and captures.
    pub pixels: Option<Arc<crate::render_capture::ReservedImageData>>,
}

impl BackgroundPaint {
    pub fn is_available(&self)->bool {
        fn available(value:&BackgroundPaint,depth:usize)->bool {
            if depth>16{return false;}
            match value {
                BackgroundPaint::Solid(_)|BackgroundPaint::Gradient(_)=>true,
                BackgroundPaint::Image(image)=>image.is_valid(),
                BackgroundPaint::Worklet(image)=>image.pixels.is_some(),
                BackgroundPaint::CrossFade(items)=>items.iter().all(|(image,_)|available(image,depth+1)),
                BackgroundPaint::Border(border)=>available(&border.image,depth+1),
            }
        }
        available(self,0)
    }
    pub fn natural_size(&self) -> Option<(f32, f32)> {
        match self {
            Self::Image(image) if image.is_valid() => {
                Some((image.width as f32, image.height as f32))
            }
            Self::CrossFade(items) => {
                let mut width = 0.0;
                let mut height = 0.0;
                let mut weight = 0.0;
                for (image, fraction) in items.iter() {
                    if let Some((w, h)) = image.natural_size() {
                        width += w * fraction;
                        height += h * fraction;
                        weight += fraction;
                    }
                }
                (weight > 0.0).then(|| (width / weight, height / weight))
            }
            _ => None,
        }
    }
    fn valid_at(&self, depth: usize) -> bool {
        if depth > 16 {
            return false;
        }
        match self {
            Self::Solid(_) => true,
            Self::Border(value)=>value.source_size.iter().all(|v|v.is_finite()&&*v>0.0)
                && value.slices.iter().chain(value.widths.iter()).all(|v|v.is_finite()&&*v>=0.0)
                && value.fallback.rect.is_valid()
                && value.fallback.radius.is_finite()&&value.fallback.radius>=0.0
                && value.fallback.widths.iter().all(|v|v.is_finite()&&*v>=0.0)
                && value.fallback.corners.is_none_or(|corners|corners.iter().flatten().all(|v|v.is_finite()&&*v>=0.0))
                && value.image.valid_at(depth+1),
            Self::Gradient(g) => g.is_valid(),
            Self::Image(i) => {
                i.is_valid() || (i.width == 0 && i.height == 0 && i.pixels.is_empty())
            }
            Self::Worklet(image) => image.request.width.is_finite()
                && image.request.height.is_finite()
                && image.request.width >= 0.0 && image.request.height >= 0.0
                && image.pixels.as_ref().is_none_or(|pixels| pixels.image.is_valid()),
            Self::CrossFade(items) => {
                !items.is_empty()
                    && items.len() <= 16
                    && items.iter().all(|(i, w)| {
                        w.is_finite() && *w >= 0.0 && *w <= 1.0 && i.valid_at(depth + 1)
                    })
            }
        }
    }
    pub fn is_valid(&self) -> bool {
        self.valid_at(0)
    }
    fn payload_bytes(&self) -> usize {
        let header = 2 * core::mem::size_of::<usize>();
        match self {
            Self::Solid(_) => 0,
            Self::Border(value)=>core::mem::size_of::<BorderImagePaint>()+value.image.payload_bytes()+header,
            Self::Image(i) => core::mem::size_of::<ImageData>() + i.pixels.capacity() + header,
            Self::Worklet(image) => core::mem::size_of::<PaintWorkletImage>()
                + image.request.name.len()
                + image.request.arguments.iter().map(|value| value.len()).sum::<usize>()
                + image.request.properties.iter().map(|(name,value)| name.len()+value.as_ref().map_or(0,|value|value.len())).sum::<usize>()
                + image.pixels.as_ref().map_or(0,|pixels|pixels.reserved_bytes()),
            Self::Gradient(g) => {
                core::mem::size_of::<Gradient>()
                    + g.stops.len() * core::mem::size_of::<GradientStop>()
                    + 2 * header
                    + g.angular.as_ref().map_or(0, |metadata| metadata.checked_retained_bytes().unwrap_or(usize::MAX / 2))
                    + g.color.as_ref().map_or(0, |metadata|metadata.checked_retained_bytes().unwrap_or(usize::MAX/2))
            }
            Self::CrossFade(items) => {
                items.len() * core::mem::size_of::<(BackgroundPaint, f32)>()
                    + header
                    + items.iter().map(|(i, _)| i.payload_bytes()).sum::<usize>()
            }
        }
    }
}

/// Box a background layer clips to (`background-clip`) or is positioned within
/// (`background-origin`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackgroundBox {
    Text,
    BorderArea,
    BorderAreaText,
    Border,
    Padding,
    Content,
}

/// One `background-image` layer value. `None` layers paint nothing but still
/// take part in layer cycling.
#[derive(Clone, Debug, PartialEq)]
pub enum BackgroundImage {
    Solid(Rgba),
    CrossFade(Arc<[(BackgroundImage, f32)]>),
    None,
    Url(Arc<str>),
    /// The selected image-set candidate; natural CSS dimensions divide the
    /// decoded pixel dimensions by this positive resolution in dppx.
    UrlResolution {
        url: Arc<str>,
        density: f32,
    },
    Gradient(Arc<Gradient>),
    Paint { name: Arc<str>, arguments: Arc<[Arc<str>]> },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackgroundSizeKind {
    Explicit,
    Contain,
    Cover,
}

/// `background-size`: `cover`/`contain` or per-axis `auto`/length-percentage.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BackgroundSize {
    pub kind: BackgroundSizeKind,
    pub width: Option<LengthPercentage>,
    pub height: Option<LengthPercentage>,
}

impl BackgroundSize {
    pub const AUTO: Self = Self {
        kind: BackgroundSizeKind::Explicit,
        width: None,
        height: None,
    };
}

/// One resolved background layer with the geometry properties CSS Backgrounds
/// defines per layer. At most [`MAX_BACKGROUND_LAYERS`] exist per element.
pub const MAX_BACKGROUND_LAYERS: usize = 8;

#[derive(Clone, Debug, PartialEq)]
pub struct BackgroundLayer {
    pub image: BackgroundImage,
    pub position: [LengthPercentage; 2],
    pub size: BackgroundSize,
    pub repeat: [BackgroundRepeat; 2],
    pub clip: BackgroundBox,
    pub origin: BackgroundBox,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GradientPosition {
    Fraction(f32),
    Pixels(f32),
    Mixed(LengthPercentage),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GradientStop {
    pub color: Rgba,
    pub position: Option<GradientPosition>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BoxShadow {
    pub offset_x: f32,
    pub offset_y: f32,
    pub blur: f32,
    pub spread: f32,
    pub color: Rgba,
    pub inset: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BorderPattern {
    Groove,
    Ridge,
    Inset,
    Outset,
    Double,
    Dashed,
    Dotted,
}

impl BoxShadow {
    pub fn bounds(self, rect: Rect) -> Rect {
        if self.inset {
            return rect;
        }
        let extent = self.spread + self.blur * 1.5 + 1.0;
        Rect {
            x: rect.x + self.offset_x - extent,
            y: rect.y + self.offset_y - extent,
            width: (rect.width + extent * 2.0).max(0.0),
            height: (rect.height + extent * 2.0).max(0.0),
        }
    }
}

/// CSS two-dimensional affine matrix, in column-vector order.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Affine {
    pub a: f32,
    pub b: f32,
    pub c: f32,
    pub d: f32,
    pub e: f32,
    pub f: f32,
}
impl Default for Affine {
    fn default() -> Self {
        Self::IDENTITY
    }
}
impl Affine {
    pub const IDENTITY: Self = Self {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };
    pub fn apply(self, x: f32, y: f32) -> (f32, f32) {
        (
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        )
    }
    pub fn then(self, rhs: Self) -> Self {
        Self {
            a: self.a * rhs.a + self.c * rhs.b,
            b: self.b * rhs.a + self.d * rhs.b,
            c: self.a * rhs.c + self.c * rhs.d,
            d: self.b * rhs.c + self.d * rhs.d,
            e: self.a * rhs.e + self.c * rhs.f + self.e,
            f: self.b * rhs.e + self.d * rhs.f + self.f,
        }
    }
    pub fn inverse(self) -> Option<Self> {
        let det = self.a * self.d - self.b * self.c;
        if det == 0.0 || !det.is_finite() {
            return None;
        }
        let result = Self {
            a: self.d / det,
            b: -self.b / det,
            c: -self.c / det,
            d: self.a / det,
            e: (self.c * self.f - self.d * self.e) / det,
            f: (self.b * self.e - self.a * self.f) / det,
        };
        result.is_finite().then_some(result)
    }
    pub fn is_finite(self) -> bool {
        [self.a, self.b, self.c, self.d, self.e, self.f]
            .iter()
            .all(|v| v.is_finite())
    }
    pub fn translated_space(self, x: f32, y: f32) -> Self {
        Self {
            e: self.e + x - self.a * x - self.c * y,
            f: self.f + y - self.b * x - self.d * y,
            ..self
        }
    }
    pub fn bounds(self, rect: Rect) -> Rect {
        let points = [
            self.apply(rect.x, rect.y),
            self.apply(rect.x + rect.width, rect.y),
            self.apply(rect.x, rect.y + rect.height),
            self.apply(rect.x + rect.width, rect.y + rect.height),
        ];
        let (mut x, mut y, mut right, mut bottom) =
            (points[0].0, points[0].1, points[0].0, points[0].1);
        for (px, py) in points {
            x = x.min(px);
            y = y.min(py);
            right = right.max(px);
            bottom = bottom.max(py);
        }
        Rect {
            x,
            y,
            width: right - x,
            height: bottom - y,
        }
    }
}

#[cfg(test)]
mod affine_tests {
    use super::*;
    #[test]
    fn inverse_and_coordinate_translation_preserve_geometry() {
        let matrix = Affine {
            a: 0.0,
            b: 2.0,
            c: -3.0,
            d: 0.0,
            e: 12.0,
            f: 4.0,
        };
        let point = matrix.apply(5.0, 7.0);
        assert_eq!(
            matrix.inverse().unwrap().apply(point.0, point.1),
            (5.0, 7.0)
        );
        let shifted = matrix.translated_space(20.0, -6.0).apply(25.0, 1.0);
        assert_eq!(shifted, (point.0 + 20.0, point.1 - 6.0));
        assert_eq!(matrix.then(matrix.inverse().unwrap()), Affine::IDENTITY);
        assert!(Affine {
            a: 0.0,
            b: 0.0,
            ..Affine::IDENTITY
        }
        .inverse()
        .is_none());
    }
    #[test]
    fn transform_scope_requires_matching_pop() {
        assert_eq!(
            DisplayList(alloc::vec![
                Command::PushTransform(Affine::IDENTITY),
                Command::PopClip
            ])
            .validate(),
            Err(ReplayError::UnbalancedClip)
        );
        assert!(DisplayList(alloc::vec![
            Command::PushTransform(Affine::IDENTITY),
            Command::PushClip(Rect {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 1.0
            }),
            Command::PopClip,
            Command::PopTransform
        ])
        .validate()
        .is_ok());
        assert_eq!(
            DisplayList(alloc::vec![
                Command::PushTransform(Affine {
                    e: f32::NAN,
                    ..Affine::IDENTITY
                }),
                Command::PopTransform
            ])
            .validate(),
            Err(ReplayError::InvalidGeometry)
        );
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct BackgroundFill {
    pub corners: Option<Arc<[[f32; 2]; 4]>>,
    pub rect: Rect,
    pub radius: f32,
    pub positioning_rect: Rect,
    pub image_rect: Rect,
    pub repeat: [BackgroundRepeat; 2],
    pub image: BackgroundPaint,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    PushTransform(Affine),
    PopTransform,
    /// Fractional ink/SVG clip; CSS box snapping must preserve its edges.
    PushClip(Rect),
    /// Clip derived from a CSS box edge, sharing its device snapping policy.
    PushBoxClip(Rect),
    PopClip,
    PushLayer {
        svg_clip: Option<Arc<SvgLayerClip>>,
        filters: Option<Arc<[lumen_common::filter::FilterOperation]>>,
        corners: Option<Arc<[[f32; 2]; 4]>>,
        rect: Rect,
        radius: f32,
        opacity: f32,
        clip: bool,
    },
    PopLayer,
    FillRect {
        rect: Rect,
        color: Rgba,
    },
    FillRoundedRect {
        corners: Option<Arc<[[f32; 2]; 4]>>,
        rect: Rect,
        radius: f32,
        color: Rgba,
    },
    FillGradient {
        rect: Rect,
        radius: f32,
        gradient: Arc<Gradient>,
    },
    FillBackground(Box<BackgroundFill>),
    MaskedBackground(Box<MaskedBackground>),
    BoxShadow {
        corners: Option<Arc<[[f32; 2]; 4]>>,
        rect: Rect,
        radius: f32,
        shadow: BoxShadow,
    },
    StrokeBorder {
        rect: Rect,
        radius: f32,
        width: f32,
        color: Rgba,
    },
    StrokeBoxBorder(Box<BoxBorder>),
    StrokePatternBorder {
        rect: Rect,
        radius: f32,
        width: f32,
        color: Rgba,
        pattern: BorderPattern,
    },
    GlyphRun {
        origin_x: f32,
        baseline_y: f32,
        size: f32,
        color: Rgba,
        glyphs: Arc<[Glyph]>,
    },
    Image {
        rect: Rect,
        image: Arc<ImageData>,
    },
    ReservedImage {
        rect: Rect,
        image: Arc<crate::render_capture::ReservedImageData>,
    },
    /// A renderer-neutral SVG path command. `data` uses the SVG path-data
    /// grammar and is parsed by the shared vector raster backend; `transform`
    /// maps user-space coordinates into CSS pixels.
    SvgPath {
        bounds: Rect,
        data: Arc<str>,
        transform: Affine,
        fill: Option<SvgPaint>,
        stroke: Option<SvgPaint>,
        stroke_width: f32,
        fill_rule: SvgFillRule,
        clips: Arc<[SvgClip]>,
    },
}

/// An SVG owner's clip consumes its composited, filtered result. The
/// transform belongs to that owner; object bounds are derived from unfiltered
/// source geometry by the vector renderer, before filter ink expansion.
#[derive(Clone, Debug, PartialEq)]
pub struct SvgLayerClip {
    pub transform: Affine,
    pub clip: Arc<SvgClip>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SvgFillRule {
    #[default]
    NonZero,
    EvenOdd,
}

/// Immutable ink/border coverage shared by background layers. Offset tracks
/// retained scroll movement without cloning the mask's commands.
#[derive(Clone, Debug, PartialEq)]
pub struct MaskedBackground {
    pub text_clipped: bool,
    /// Text ink is blurred without subtracting the original glyph coverage.
    pub shadow_blur: Option<f32>,
    pub rect: Rect,
    pub paint: Box<Command>,
    pub mask: Arc<DisplayList>,
    pub offset: [f32; 2],
}

#[derive(Clone, Debug, PartialEq)]
pub struct BoxBorder {
    pub rect: Rect,
    pub radius: f32,
    /// Physical top, right, bottom, left edges.
    pub widths: [f32; 4],
    pub colors: [Rgba; 4],
    pub pattern: Option<BorderPattern>,
    pub side_patterns: Option<[Option<BorderPattern>; 4]>,
    /// Optional physical top-left, top-right, bottom-right, bottom-left radii.
    pub corners: Option<[[f32; 2]; 4]>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DisplayList(pub Vec<Command>);

/// Conservatively counted bytes referenced by a command. Shared Arc payloads
/// are charged once per reference, so this is an upper bound, not heap usage.
impl Command {
    pub fn referenced_bytes(&self) -> usize {
        let arc_header = 2 * core::mem::size_of::<usize>();
        let gradient_bytes = |gradient: &Gradient| {
            core::mem::size_of::<Gradient>()
                + gradient.stops.len() * core::mem::size_of::<GradientStop>()
                + 2 * arc_header
                + gradient.angular.as_ref().map_or(0, |metadata| metadata.checked_retained_bytes().unwrap_or(usize::MAX / 2))
                + gradient.color.as_ref().map_or(0, |metadata|metadata.checked_retained_bytes().unwrap_or(usize::MAX/2))
        };
        let image_bytes = |image: &ImageData| {
            core::mem::size_of::<ImageData>()
                .saturating_add(image.pixels.capacity())
                .saturating_add(arc_header)
        };
        let payload = match self {
            Self::GlyphRun { glyphs, .. } => {
                glyphs.len() * core::mem::size_of::<Glyph>() + arc_header
            }
            Self::Image { image, .. } => image_bytes(image),
            Self::ReservedImage { image, .. } => image_bytes(&image.image),
            Self::FillGradient { gradient, .. } => gradient_bytes(gradient),
            Self::FillBackground(fill) => {
                core::mem::size_of::<BackgroundFill>() + fill.image.payload_bytes()
            }
            Self::StrokeBoxBorder(_) => core::mem::size_of::<BoxBorder>(),
            Self::SvgPath {
                data,
                fill,
                stroke,
                clips,
                ..
            } => {
                let paint_bytes = |paint: &Option<SvgPaint>| match paint {
                    Some(SvgPaint::Gradient(gradient)) => {
                        gradient.stops.len() * core::mem::size_of::<SvgGradientStop>() + arc_header
                    }
                    _ => 0,
                };
                data.len()
                    + arc_header
                    + paint_bytes(fill)
                    + paint_bytes(stroke)
                    + clips
                        .iter()
                        .map(|clip| {
                            clip.shapes
                                .iter()
                                .map(|shape| shape.data.len() + arc_header)
                                .sum::<usize>()
                                + core::mem::size_of::<SvgClip>()
                        })
                        .sum::<usize>()
            }
            Self::MaskedBackground(mask) => {
                core::mem::size_of::<MaskedBackground>()
                    + mask.paint.referenced_bytes()
                    + mask.mask.referenced_bytes()
                    + arc_header
            }
            _ => 0,
        };
        let payload = payload.saturating_add(match self {
            Self::PushLayer { filters: Some(filters), .. } => filters.iter().fold(
                core::mem::size_of_val(filters.as_ref())+arc_header,|bytes,filter|bytes.saturating_add(filter.payload_bytes())),
            _ => 0,
        });
        let payload=payload.saturating_add(match self {
            Self::PushLayer{svg_clip:Some(owner),..}=>core::mem::size_of::<SvgLayerClip>()+2*arc_header
                +core::mem::size_of::<SvgClip>()+owner.clip.shapes.iter().fold(0usize,|bytes,shape|
                    bytes.saturating_add(core::mem::size_of::<SvgClipShape>()+shape.data.len()+arc_header)),
            _=>0,
        });
        let corners = match self {
            Self::PushLayer { corners, .. }
            | Self::BoxShadow { corners, .. }
            | Self::FillRoundedRect { corners, .. } => corners.as_ref(),
            Self::FillBackground(fill) => fill.corners.as_ref(),
            _ => None,
        };
        core::mem::size_of::<Command>()
            .saturating_add(payload)
            .saturating_add(if corners.is_some() {
                core::mem::size_of::<[[f32; 2]; 4]>() + arc_header
            } else {
                0
            })
    }
}
impl DisplayList {
    pub fn referenced_bytes(&self) -> usize {
        self.0
            .iter()
            .fold(0usize, |bytes, command| {
                bytes.saturating_add(command.referenced_bytes())
            })
            .saturating_add((self.0.capacity() - self.0.len()) * core::mem::size_of::<Command>())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplayError {
    UnbalancedClip,
    InvalidGeometry,
    ClipLimit,
    InvalidImage,
    UnsupportedLayer,
}

pub trait ReplaySink {
    fn push_clip(&mut self, rect: Rect);
    fn pop_clip(&mut self);
    fn fill_rect(&mut self, rect: Rect, color: Rgba);
    fn fill_rounded_rect(
        &mut self,
        rect: Rect,
        radius: f32,
        corners: Option<&[[f32; 2]; 4]>,
        color: Rgba,
    );
    fn fill_gradient(&mut self, rect: Rect, radius: f32, gradient: &Gradient);
    fn fill_background(
        &mut self,
        corners: Option<&[[f32; 2]; 4]>,
        rect: Rect,
        radius: f32,
        positioning_rect: Rect,
        image_rect: Rect,
        repeat: [BackgroundRepeat; 2],
        image: &BackgroundPaint,
    );
    fn draw_shadow(
        &mut self,
        rect: Rect,
        radius: f32,
        corners: Option<&[[f32; 2]; 4]>,
        shadow: BoxShadow,
    );
    fn stroke_border(&mut self, rect: Rect, radius: f32, width: f32, color: Rgba);
    fn stroke_box_border(&mut self, border: &BoxBorder);
    fn stroke_pattern_border(
        &mut self,
        rect: Rect,
        radius: f32,
        width: f32,
        color: Rgba,
        pattern: BorderPattern,
    );
    fn draw_glyphs(
        &mut self,
        origin_x: f32,
        baseline_y: f32,
        size: f32,
        color: Rgba,
        glyphs: &[Glyph],
    );
    fn draw_image(&mut self, rect: Rect, image: &ImageData);
    fn draw_svg_path(
        &mut self,
        bounds: Rect,
        data: &str,
        transform: Affine,
        fill: Option<&SvgPaint>,
        stroke: Option<&SvgPaint>,
        stroke_width: f32,
        fill_rule: SvgFillRule,
        clips: &[SvgClip],
    );
}

impl DisplayList {
    /// Validate the complete list before forwarding paint to a backend.
    pub fn validate(&self) -> Result<(), ReplayError> {
        self.check().map(|_| ())
    }

    /// Validates the list and reports whether it holds layers or transforms.
    fn check(&self) -> Result<bool, ReplayError> {
        let mut depth = 0usize;
        let mut scoped = false;
        let mut scopes = [0u8; 256];
        for command in &self.0 {
            match command {
                Command::PushTransform(matrix) => {
                    if !matrix.is_finite() {
                        return Err(ReplayError::InvalidGeometry);
                    }
                    if depth == scopes.len() {
                        return Err(ReplayError::ClipLimit);
                    }
                    scoped = true;
                    scopes[depth] = 2;
                    depth += 1;
                }
                Command::PushClip(rect) | Command::PushBoxClip(rect)
                | Command::PushLayer { rect, .. }
                | Command::FillRect { rect, .. }
                | Command::FillRoundedRect { rect, .. }
                | Command::FillGradient { rect, .. }
                | Command::BoxShadow { rect, .. }
                | Command::StrokeBorder { rect, .. }
                | Command::StrokePatternBorder { rect, .. }
                | Command::Image { rect, .. }
                | Command::ReservedImage { rect, .. } => {
                    if !rect.is_valid() {
                        return Err(ReplayError::InvalidGeometry);
                    }
                    if matches!(command, Command::PushClip(_) | Command::PushBoxClip(_) | Command::PushLayer { .. }) {
                        if depth == scopes.len() {
                            return Err(ReplayError::ClipLimit);
                        }
                        scoped |= matches!(command, Command::PushLayer { .. });
                        scopes[depth] = u8::from(matches!(command, Command::PushLayer { .. }));
                        depth += 1;
                        if depth > 256 {
                            return Err(ReplayError::ClipLimit);
                        }
                    }
                    if let Command::PushLayer {
                        radius, opacity, filters, svg_clip, ..
                    } = command
                    {
                        if !radius.is_finite()
                            || *radius < 0.0
                            || !opacity.is_finite()
                            || !(0.0..=1.0).contains(opacity)
                            || svg_clip.as_ref().is_some_and(|owner|!owner.transform.is_finite()
                                || !owner.clip.transform.is_finite() || owner.clip.shapes.len()>MAX_SVG_CLIP_PATHS
                                || owner.clip.shapes.iter().any(|shape|shape.data.len()>MAX_SVG_PATH_BYTES || !shape.transform.is_finite()))
                            || filters.as_ref().is_some_and(|values| values.is_empty() || values.len() > lumen_common::filter::MAX_COLOR_FILTERS || values.iter().any(|value| !value.is_valid()))
                        {
                            return Err(ReplayError::InvalidGeometry);
                        }
                    }
                    if let Command::Image { image, .. } = command {
                        if !image.is_valid() {
                            return Err(ReplayError::InvalidImage);
                        }
                    }
                    if let Command::ReservedImage { image, .. } = command {
                        if !image.image.is_valid() { return Err(ReplayError::InvalidImage); }
                    }
                    if let Command::PushLayer { corners, .. }
                    | Command::BoxShadow { corners, .. }
                    | Command::FillRoundedRect { corners, .. } = command
                    {
                        if corners
                            .as_ref()
                            .is_some_and(|r| r.iter().flatten().any(|v| !v.is_finite() || *v < 0.0))
                        {
                            return Err(ReplayError::InvalidGeometry);
                        }
                    }
                    if let Command::FillRoundedRect { radius, .. } = command {
                        if !radius.is_finite() || *radius < 0.0 {
                            return Err(ReplayError::InvalidGeometry);
                        }
                    }
                    if let Command::FillGradient {
                        radius, gradient, ..
                    } = command
                    {
                        if !radius.is_finite() || *radius < 0.0 || !gradient.is_valid() {
                            return Err(ReplayError::InvalidGeometry);
                        }
                    }
                    if let Command::StrokeBorder { radius, width, .. }
                    | Command::StrokePatternBorder { radius, width, .. } = command
                    {
                        if !radius.is_finite()
                            || *radius < 0.0
                            || !width.is_finite()
                            || *width < 0.0
                        {
                            return Err(ReplayError::InvalidGeometry);
                        }
                    }
                    if let Command::BoxShadow { radius, shadow, .. } = command {
                        if !radius.is_finite()
                            || *radius < 0.0
                            || !shadow.blur.is_finite()
                            || shadow.blur < 0.0
                            || ![shadow.offset_x, shadow.offset_y, shadow.spread]
                                .iter()
                                .all(|value| value.is_finite())
                        {
                            return Err(ReplayError::InvalidGeometry);
                        }
                    }
                }
                Command::MaskedBackground(mask) => {
                    scoped = true;
                    if !mask.rect.is_valid()
                        || mask.offset.iter().any(|v| !v.is_finite())
                        || mask.shadow_blur.is_some_and(|blur| !blur.is_finite() || blur < 0.0)
                        || !matches!(
                            *mask.paint,
                            Command::FillRect { .. }
                                | Command::FillRoundedRect { .. }
                                | Command::FillGradient { .. }
                                | Command::FillBackground(_)
                        )
                    {
                        return Err(ReplayError::InvalidGeometry);
                    }
                    DisplayList(alloc::vec![(*mask.paint).clone()]).validate()?;
                    if mask
                        .mask
                        .0
                        .iter()
                        .any(|c| matches!(c, Command::MaskedBackground(_)))
                    {
                        return Err(ReplayError::InvalidGeometry);
                    }
                    mask.mask.validate()?;
                }
                Command::StrokeBoxBorder(border) => {
                    if !border.rect.is_valid()
                        || !border.radius.is_finite()
                        || border.radius < 0.0
                        || border
                            .widths
                            .iter()
                            .any(|width| !width.is_finite() || *width < 0.0)
                        || border.corners.is_some_and(|corners| {
                            corners.iter().flatten().any(|v| !v.is_finite() || *v < 0.0)
                        })
                    {
                        return Err(ReplayError::InvalidGeometry);
                    }
                }
                Command::FillBackground(fill) => {
                    if fill
                        .corners
                        .as_ref()
                        .is_some_and(|r| r.iter().flatten().any(|v| !v.is_finite() || *v < 0.0))
                    {
                        return Err(ReplayError::InvalidGeometry);
                    }
                    if !fill.rect.is_valid()
                        || !fill.radius.is_finite()
                        || fill.radius < 0.0
                        || !fill.positioning_rect.is_valid()
                        || !fill.image_rect.is_valid()
                    {
                        return Err(ReplayError::InvalidGeometry);
                    }
                    if !fill.image.is_valid() {
                        return Err(ReplayError::InvalidImage);
                    }
                }
                Command::PopClip | Command::PopLayer | Command::PopTransform => {
                    depth = depth.checked_sub(1).ok_or(ReplayError::UnbalancedClip)?;
                    let kind = match command {
                        Command::PopTransform => 2,
                        Command::PopLayer => 1,
                        _ => 0,
                    };
                    if scopes[depth] != kind {
                        return Err(ReplayError::UnbalancedClip);
                    }
                }
                Command::GlyphRun {
                    origin_x,
                    baseline_y,
                    size,
                    glyphs,
                    ..
                } => {
                    if !origin_x.is_finite()
                        || !baseline_y.is_finite()
                        || !size.is_finite()
                        || *size <= 0.0
                        || glyphs
                            .iter()
                            .any(|glyph| !glyph.x.is_finite() || !glyph.y.is_finite())
                    {
                        return Err(ReplayError::InvalidGeometry);
                    }
                }
                Command::SvgPath {
                    bounds,
                    data,
                    transform,
                    stroke_width,
                    ref fill,
                    ref stroke,
                    ref clips,
                    ..
                } => {
                    if !bounds.is_valid()
                        || data.len() > MAX_SVG_PATH_BYTES
                        || !transform.is_finite()
                        || !stroke_width.is_finite()
                        || *stroke_width < 0.0
                        || !valid_svg_paint(fill.as_ref())
                        || !valid_svg_paint(stroke.as_ref())
                        || clips.len() > 32
                        || clips.iter().any(|clip| {
                            !clip.transform.is_finite()
                                || clip.shapes.len() > MAX_SVG_CLIP_PATHS
                                || clip.shapes.iter().any(|shape| {
                                    shape.data.len() > MAX_SVG_PATH_BYTES
                                        || !shape.transform.is_finite()
                                })
                        })
                    {
                        return Err(ReplayError::InvalidGeometry);
                    }
                }
            }
        }
        if depth != 0 {
            return Err(ReplayError::UnbalancedClip);
        }
        Ok(scoped)
    }

    pub fn replay(&self, sink: &mut impl ReplaySink) -> Result<(), ReplayError> {
        if self.check()? {
            return Err(ReplayError::UnsupportedLayer);
        }
        for command in &self.0 {
            match *command {
                Command::PushClip(rect) | Command::PushBoxClip(rect) => sink.push_clip(rect),
                Command::PopClip => sink.pop_clip(),
                Command::MaskedBackground(_)
                | Command::PushLayer { .. }
                | Command::PopLayer
                | Command::PushTransform(_)
                | Command::PopTransform => {
                    return Err(ReplayError::UnsupportedLayer);
                }
                Command::FillRect { rect, color }
                    if rect.width > 0.0 && rect.height > 0.0 && color.a > 0 =>
                {
                    sink.fill_rect(rect, color)
                }
                Command::FillRect { .. } => {}
                Command::FillRoundedRect {
                    ref corners,
                    rect,
                    radius,
                    color,
                } if rect.width > 0.0 && rect.height > 0.0 && color.a > 0 => {
                    sink.fill_rounded_rect(rect, radius, corners.as_deref(), color)
                }
                Command::FillRoundedRect { .. } => {}
                Command::FillGradient {
                    rect,
                    radius,
                    ref gradient,
                } => sink.fill_gradient(rect, radius, gradient),
                Command::FillBackground(ref fill) => sink.fill_background(
                    fill.corners.as_deref(),
                    fill.rect,
                    fill.radius,
                    fill.positioning_rect,
                    fill.image_rect,
                    fill.repeat,
                    &fill.image,
                ),
                Command::BoxShadow {
                    ref corners,
                    rect,
                    radius,
                    shadow,
                } => sink.draw_shadow(rect, radius, corners.as_deref(), shadow),
                Command::StrokeBorder {
                    rect,
                    radius,
                    width,
                    color,
                } if rect.width > 0.0 && rect.height > 0.0 && width > 0.0 && color.a > 0 => {
                    sink.stroke_border(rect, radius, width, color)
                }
                Command::StrokeBorder { .. } => {}
                Command::StrokeBoxBorder(ref border) => sink.stroke_box_border(border),
                Command::StrokePatternBorder {
                    rect,
                    radius,
                    width,
                    color,
                    pattern,
                } => sink.stroke_pattern_border(rect, radius, width, color, pattern),
                Command::GlyphRun {
                    origin_x,
                    baseline_y,
                    size,
                    color,
                    ref glyphs,
                } => sink.draw_glyphs(origin_x, baseline_y, size, color, glyphs),
                Command::Image { rect, ref image } => sink.draw_image(rect, image),
                Command::ReservedImage { rect, ref image } => sink.draw_image(rect, &image.image),
                Command::SvgPath {
                    bounds,
                    ref data,
                    transform,
                    ref fill,
                    ref stroke,
                    stroke_width,
                    fill_rule,
                    ref clips,
                } => sink.draw_svg_path(
                    bounds,
                    data,
                    transform,
                    fill.as_ref(),
                    stroke.as_ref(),
                    stroke_width,
                    fill_rule,
                    clips,
                ),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod border_image_tests {
    use super::*;
    use crate::css::BorderImageRepeat as Repeat;
    fn carrier()->BorderImagePaint {
        BorderImagePaint {image:BackgroundPaint::Solid(Rgba{r:0,g:128,b:0,a:255}),
            fallback:BoxBorder{rect:Rect{x:0.0,y:0.0,width:14.0,height:14.0},radius:0.0,widths:[2.0;4],colors:[Rgba{r:255,g:0,b:0,a:255};4],pattern:None,side_patterns:None,corners:None},
            source_size:[9.0;2],slices:[3.0;4],widths:[2.0;4],repeat:[Repeat::Repeat;2],fill:false}
    }
    #[test]
    fn specification_border_image_sampler_centers_repeats_spaces_tiles_and_rejects_empty_pieces() {
        let mut image=carrier();
        assert_eq!(image.source_coordinate(1.0,1.0,14.0,14.0),Some(([1.5,1.5],[0.0,0.0,3.0,3.0])));
        assert_eq!(image.source_coordinate(3.0,1.0,14.0,14.0),Some(([4.5,1.5],[3.0,0.0,6.0,3.0])));
        assert!(image.source_coordinate(7.0,7.0,14.0,14.0).is_none(),"middle is transparent without fill");
        image.fill=true;assert!(image.source_coordinate(7.0,7.0,14.0,14.0).is_some());
        image.repeat=[Repeat::Space;2];
        assert!(image.source_coordinate(2.05,1.0,15.0,15.0).is_none(),"space is distributed before the first complete tile");
        assert!(image.source_coordinate(2.3,1.0,15.0,15.0).is_some());
        assert!(image.source_coordinate(2.5,1.0,5.0,5.0).is_none(),"space discards a tile that cannot fit completely");
        image.repeat=[Repeat::Round;2];
        assert!(image.source_coordinate(2.5,1.0,5.0,5.0).is_some(),"round scales one tile into a smaller region");
        image.slices=[5.0;4];
        assert!(image.source_coordinate(7.0,1.0,14.0,14.0).is_none(),"opposite overlapping slices make the edge empty");
        assert!(image.source_coordinate(1.0,1.0,14.0,14.0).is_some(),"overlap does not discard corner images");
    }
}
