//! Explicit-font shaping and glyph coverage shared by render backends.
#![no_std]

extern crate alloc;

mod manual_font_registry;
pub use manual_font_registry::{
    FontFaceStatus, FontLoadRequest, FontRegistryContext, FontRegistrySnapshot, ManualFontFace,
    ManualFontFaceState, ManualFontRegistry, ManualFontSource, MAX_MANUAL_FONT_BYTES_PER_FACE,
};

pub use lumen_common::ucd::{
    graphemes, line_breaks, next_grapheme_boundary, previous_grapheme_boundary, BreakOpportunity,
};

use alloc::{boxed::Box, string::String, sync::Arc, vec::Vec};
use core::cell::UnsafeCell;
use core::cmp::Ordering as CmpOrdering;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use lumen_common::bidi::{self, Script, ScriptExtension, UnicodeScript};
use lumen_html::paint::{
    PrimaryCharacterWidths,
    font_match_style_range_rank, FontFamily, FontMetric, FontRelativeMetrics, FontSizeAdjust, FontSizeAdjustValue,
    FontSpec, FontLigatures, FontLigatureGroup, FontFeatureSettings, FontStyle, Glyph, ShapedClusterAdvance, ShapedRun, ShapedRunWithClusterAdvances,
    TextShaper,
};

static NEXT_FACE_ID: AtomicU64 = AtomicU64::new(1);
const MAX_FONT_BYTES: usize = 2 * 1024 * 1024;
const MAX_SHAPE_TEXT_BYTES: usize = 64 * 1024;
const RUN_CACHE_SLOTS: usize = 512;
const MAX_RUN_CACHE_BYTES: usize = 512 * 1024;
const MAX_CACHED_TEXT_BYTES: usize = 256;
const MAX_REGISTERED_FONTS: usize = 64;
const MAX_REQUESTED_FAMILIES: usize = 64;
const MAX_FAMILY_NAME_BYTES: usize = 256;
const RASTERIZER_BLOCK_GLYPHS: u16 = 256;
const RASTERIZER_CACHE_BLOCKS: usize = 2;

/// Builds a temporary fontdue font whose cmap exposes only the requested
/// glyph. Fontdue eagerly retains every mapped outline when it loads a face;
/// a raster pass needs just one already-shaped glyph ID. Keep all other font
/// tables and glyph indices intact so GSUB output from rustybuzz rasterizes
/// identically, while bounding retained outline memory to this call.
fn fontdue_bytes_for_glyph_range(
    bytes: &[u8],
    first_glyph: u16,
    glyph_count: u16,
) -> Result<Vec<u8>, &'static str> {
    fn read_u16(bytes: &[u8], at: usize) -> Option<u16> {
        Some(u16::from_be_bytes(
            bytes.get(at..at.checked_add(2)?)?.try_into().ok()?,
        ))
    }
    fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
        Some(u32::from_be_bytes(
            bytes.get(at..at.checked_add(4)?)?.try_into().ok()?,
        ))
    }
    fn write_u16(bytes: &mut [u8], at: usize, value: u16) -> Result<(), &'static str> {
        bytes
            .get_mut(at..at.checked_add(2).ok_or("invalid font table")?)
            .ok_or("invalid font table")?
            .copy_from_slice(&value.to_be_bytes());
        Ok(())
    }
    fn write_u32(bytes: &mut [u8], at: usize, value: u32) -> Result<(), &'static str> {
        bytes
            .get_mut(at..at.checked_add(4).ok_or("invalid font table")?)
            .ok_or("invalid font table")?
            .copy_from_slice(&value.to_be_bytes());
        Ok(())
    }
    fn checksum(bytes: &[u8]) -> u32 {
        bytes.chunks(4).fold(0u32, |sum, word| {
            let mut padded = [0; 4];
            padded[..word.len()].copy_from_slice(word);
            sum.wrapping_add(u32::from_be_bytes(padded))
        })
    }
    fn small_cmap(first_glyph: u16, glyph_count: u16) -> Result<[u8; 44], &'static str> {
        if glyph_count == 0 || glyph_count > RASTERIZER_BLOCK_GLYPHS {
            return Err("invalid glyph block");
        }
        let first_codepoint = 0xe000u16;
        let last_codepoint = first_codepoint
            .checked_add(glyph_count - 1)
            .ok_or("invalid glyph block")?;
        let mut cmap = [0; 44];
        // cmap header and one Windows Unicode BMP encoding record.
        cmap[2..4].copy_from_slice(&1u16.to_be_bytes());
        cmap[4..6].copy_from_slice(&3u16.to_be_bytes());
        cmap[6..8].copy_from_slice(&1u16.to_be_bytes());
        cmap[8..12].copy_from_slice(&12u32.to_be_bytes());
        // Format 4: one delta segment maps this private-use range to a
        // contiguous block of already-shaped glyph IDs. The sentinel remains
        // required by the SFNT format.
        cmap[12..14].copy_from_slice(&4u16.to_be_bytes());
        cmap[14..16].copy_from_slice(&32u16.to_be_bytes());
        cmap[18..20].copy_from_slice(&4u16.to_be_bytes()); // segCountX2
        cmap[20..22].copy_from_slice(&4u16.to_be_bytes()); // searchRange
        cmap[22..24].copy_from_slice(&1u16.to_be_bytes()); // entrySelector
        cmap[26..28].copy_from_slice(&last_codepoint.to_be_bytes());
        cmap[28..30].copy_from_slice(&0xffffu16.to_be_bytes());
        cmap[32..34].copy_from_slice(&first_codepoint.to_be_bytes());
        cmap[34..36].copy_from_slice(&0xffffu16.to_be_bytes());
        cmap[36..38].copy_from_slice(&first_glyph.wrapping_sub(first_codepoint).to_be_bytes());
        cmap[38..40].copy_from_slice(&1u16.to_be_bytes());
        Ok(cmap)
    }

    #[derive(Clone, Copy)]
    struct Record {
        tag: [u8; 4],
        offset: usize,
        length: usize,
    }

    if bytes.len() > MAX_FONT_BYTES {
        return Err("font too large");
    }
    let sfnt_offset = if bytes.get(..4) == Some(b"ttcf") {
        usize::try_from(read_u32(bytes, 12).ok_or("invalid font collection")?)
            .map_err(|_| "invalid font collection")?
    } else {
        0
    };
    let scaler = bytes
        .get(sfnt_offset..sfnt_offset.checked_add(4).ok_or("invalid font table")?)
        .ok_or("invalid font table")?;
    let table_count = read_u16(
        bytes,
        sfnt_offset.checked_add(4).ok_or("invalid font table")?,
    )
    .ok_or("invalid font table")? as usize;
    if glyph_count == 0
        || glyph_count > RASTERIZER_BLOCK_GLYPHS
        || first_glyph.checked_add(glyph_count).is_none()
    {
        return Err("invalid glyph block");
    }
    let directory = sfnt_offset.checked_add(12).ok_or("invalid font table")?;
    let directory_bytes = table_count.checked_mul(16).ok_or("font table limit")?;
    if directory
        .checked_add(directory_bytes)
        .is_none_or(|end| end > bytes.len())
    {
        return Err("invalid font table");
    }

    let mut records = Vec::new();
    records
        .try_reserve(table_count)
        .map_err(|_| "font allocation failed")?;
    let mut cmap_index = None;
    for index in 0..table_count {
        let entry = directory + index * 16;
        let tag: [u8; 4] = bytes
            .get(entry..entry + 4)
            .ok_or("invalid font table")?
            .try_into()
            .map_err(|_| "invalid font table")?;
        if &tag == b"DSIG" {
            continue;
        }
        let offset = usize::try_from(read_u32(bytes, entry + 8).ok_or("invalid font table")?)
            .map_err(|_| "invalid font table")?;
        let length = usize::try_from(read_u32(bytes, entry + 12).ok_or("invalid font table")?)
            .map_err(|_| "invalid font table")?;
        let end = offset.checked_add(length).ok_or("invalid font table")?;
        if end > bytes.len() {
            return Err("invalid font table");
        }
        if &tag == b"cmap" {
            cmap_index = Some(records.len());
        }
        records.push(Record {
            tag,
            offset,
            length,
        });
    }
    let cmap_index = cmap_index.ok_or("font has no character map")?;
    let table_count = u16::try_from(records.len()).map_err(|_| "font table limit")?;
    let mut output = Vec::new();
    let directory_end = 12usize
        .checked_add(records.len().checked_mul(16).ok_or("font table limit")?)
        .ok_or("font table limit")?;
    output
        .try_reserve(directory_end)
        .map_err(|_| "font allocation failed")?;
    output.resize(directory_end, 0);
    output[..4].copy_from_slice(scaler);
    write_u16(&mut output, 4, table_count)?;
    let mut search_power = 1u16;
    let mut selector = 0u16;
    while search_power.saturating_mul(2) <= table_count {
        search_power *= 2;
        selector += 1;
    }
    let search_range = search_power.saturating_mul(16);
    write_u16(&mut output, 6, search_range)?;
    write_u16(&mut output, 8, selector)?;
    write_u16(
        &mut output,
        10,
        table_count.saturating_mul(16) - search_range,
    )?;

    let cmap = small_cmap(first_glyph, glyph_count)?;
    let mut head_adjustment = None;
    for (index, record) in records.iter().enumerate() {
        let aligned = output.len().checked_add(3).ok_or("font table limit")? & !3;
        let table = if index == cmap_index {
            cmap.as_slice()
        } else {
            bytes
                .get(record.offset..record.offset + record.length)
                .ok_or("invalid font table")?
        };
        let end = aligned.checked_add(table.len()).ok_or("font table limit")?;
        if end > MAX_FONT_BYTES {
            return Err("font too large");
        }
        output
            .try_reserve(end.saturating_sub(output.len()))
            .map_err(|_| "font allocation failed")?;
        output.resize(aligned, 0);
        output.extend_from_slice(table);
        if &record.tag == b"head" {
            if table.len() < 12 {
                return Err("invalid font header");
            }
            write_u32(&mut output, aligned + 8, 0)?;
            head_adjustment = Some(aligned + 8);
        }
        let entry = 12 + index * 16;
        output[entry..entry + 4].copy_from_slice(&record.tag);
        let table_checksum = checksum(&output[aligned..end]);
        write_u32(&mut output, entry + 4, table_checksum)?;
        write_u32(
            &mut output,
            entry + 8,
            u32::try_from(aligned).map_err(|_| "font table limit")?,
        )?;
        write_u32(
            &mut output,
            entry + 12,
            u32::try_from(table.len()).map_err(|_| "font table limit")?,
        )?;
    }
    let adjustment = head_adjustment.ok_or("font has no header")?;
    let check_sum_adjustment = 0xB1B0_AFBAu32.wrapping_sub(checksum(&output));
    write_u32(&mut output, adjustment, check_sum_adjustment)?;
    Ok(output)
}

/// Rasterization access for glyphs produced by a shaper.
///
/// A face value of zero is the compatibility path for a provider with one default face. Shapers
/// that select among multiple faces put each selected face's stable identity on the glyph.
pub trait FontProvider: TextShaper {
    /// Immutable registration metadata for composing web fonts with this
    /// provider's real platform fallback faces. Opaque providers may decline.
    fn registrations(&self) -> Option<Vec<FontRegistration>> {
        None
    }
    fn first_available_metric(&self,_font:&FontSpec,_metric:FontMetric)->Option<f32> {None}

    fn rasterize_glyph(&self, face: u64, id: u16, size: f32)
        -> Result<GlyphCoverage, &'static str>;

    fn shape_canvas_text(
        &self,
        text: &str,
        size: f32,
        rtl: bool,
        font: &FontSpec,
        options: &CanvasTextOptions,
    ) -> Result<ShapedRun, &'static str>;

    /// SVG2 object bounds use full glyph cells: native horizontal advance
    /// and the face's ascent/descent, before raster coverage or filter ink.
    /// Opaque providers may decline instead of guessing from visible pixels.
    fn glyph_cell_bounds(&self,_face:u64,_id:u16,_size:f32)->Option<lumen_html::paint::Rect>{None}

    /// Returns the native vector outline used for stroked Canvas text.
    fn outline_glyph(&self, face: u64, id: u16, size: f32) -> Result<GlyphOutline, &'static str>;

    /// Resolve a glyph face value to the stable identity used by raster caches.
    fn face_key(&self, face: u64) -> Result<u64, &'static str>;
    fn shape_cache_stats(&self) -> ShapeCacheStats {
        ShapeCacheStats::default()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CanvasFontKerning {
    #[default]
    Auto,
    Normal,
    None,
}

pub use lumen_html::paint::FontVariantCaps as CanvasFontVariantCaps;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CanvasTextRendering {
    #[default]
    Auto,
    OptimizeSpeed,
    OptimizeLegibility,
    GeometricPrecision,
}

/// Canvas text controls shared by shaping, measurement, and raster backends.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CanvasTextOptions {
    pub letter_spacing: f32,
    pub word_spacing: f32,
    pub font_kerning: CanvasFontKerning,
    pub font_variant_caps: CanvasFontVariantCaps,
    pub text_rendering: CanvasTextRendering,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GlyphOutlineCommand {
    MoveTo(f32, f32),
    LineTo(f32, f32),
    QuadTo(f32, f32, f32, f32),
    CurveTo(f32, f32, f32, f32, f32, f32),
    Close,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GlyphOutline {
    pub commands: Vec<GlyphOutlineCommand>,
}

const MAX_GLYPH_OUTLINE_COMMANDS: usize = 16_384;

struct GlyphOutlineBuilder {
    commands: Vec<GlyphOutlineCommand>,
    overflowed: bool,
}

impl GlyphOutlineBuilder {
    fn push(&mut self, command: GlyphOutlineCommand) {
        if self.commands.len() == MAX_GLYPH_OUTLINE_COMMANDS {
            self.overflowed = true;
        } else if !self.overflowed {
            self.commands.push(command);
        }
    }
}

impl ttf_parser::OutlineBuilder for GlyphOutlineBuilder {
    fn move_to(&mut self, x: f32, y: f32) {
        self.push(GlyphOutlineCommand::MoveTo(x, y));
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.push(GlyphOutlineCommand::LineTo(x, y));
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.push(GlyphOutlineCommand::QuadTo(x1, y1, x, y));
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.push(GlyphOutlineCommand::CurveTo(x1, y1, x2, y2, x, y));
    }
    fn close(&mut self) {
        self.push(GlyphOutlineCommand::Close);
    }
}

/// A registered face and its CSS family, weight, and style metadata.
#[derive(Clone)]
pub struct RegisteredFont {
    pub family: Arc<str>,
    pub weight: u16,
    pub style: FontStyle,
    pub stretch: f32,
    pub face: Arc<FontFace>,
}

/// Descriptor ranges that belong to one registration of reusable font bytes.
/// Variable axes are matched by range here; this does not imply that the
/// shaping backend applies variable-axis coordinates.
#[derive(Clone, Debug, PartialEq)]
pub struct RegisteredFontDescriptors {
    pub oblique_range: Option<[f32; 2]>,
    pub weight_range: [u16; 2],
    pub stretch_range: [f32; 2],
    pub size_adjust: f32,
    pub feature_settings: Option<Arc<FontFeatureSettings>>,
    pub family_scope: Option<Arc<[lumen_html::NodeId]>>,
    pub feature_values: Option<Arc<[lumen_html::css::FontFamilyDisplayRule]>>,
}

#[derive(Clone)]
pub struct FontRegistration {
    pub font: RegisteredFont,
    pub unicode_range: Option<Arc<[(u32, u32)]>>,
    pub descriptors: RegisteredFontDescriptors,
}

impl FontRegistration {
    /// Register one decoded CSS face with the same unicode-range and descriptor
    /// conversion used by document and WPT font snapshots.
    pub fn from_css_rule(
        rule: &lumen_html::css::FontFaceRule,
        face: Arc<FontFace>,
    ) -> Result<Self, &'static str> {
        let stretch_range = lumen_html::css::font_face_width_without_context(&rule.descriptors)
            .ok_or("font width descriptor requires an available font or query context")?;
        let unicode_range = Some(match rule.unicode_range.as_deref() {
            Some(raw) => lumen_html::css::parse_unicode_ranges(raw)
                .ok_or("invalid parsed font unicode-range")?,
            None => Arc::from([(0, 0x10ffff)]),
        });
        Ok(Self {
            font: RegisteredFont {
                family: rule.family.clone(),
                weight: rule.weight,
                style: rule.style,
                stretch: stretch_range[0],
                face,
            },
            unicode_range,
            descriptors: RegisteredFontDescriptors {stretch_range, ..RegisteredFontDescriptors::from_css(rule)},
        })
    }
}

impl RegisteredFontDescriptors {
    pub fn from_css(rule: &lumen_html::css::FontFaceRule) -> Self {
        Self {
            weight_range: rule.weight_range,
            oblique_range: rule.descriptors.oblique_range,
            stretch_range: rule.stretch_range,
            size_adjust: rule.size_adjust / 100.0,
            feature_settings: rule.feature_settings.clone(),
            family_scope: rule.family_scope.clone(),
            feature_values: (!rule.family_display.is_empty()).then(||rule.family_display.clone()),
        }
    }
    fn scalar(font: &RegisteredFont, size_adjust: f32) -> Self {
        Self {
            weight_range: [font.weight, font.weight],
            oblique_range: None,
            stretch_range: [font.stretch, font.stretch],
            size_adjust,
            feature_settings: None,
            family_scope: None,
            feature_values: None,
        }
    }
}

/// A bounded collection of faces with CSS family, style, weight, and cluster fallback.
pub struct FontSet {
    faces: Arc<[RegisteredFont]>,
    unicode_ranges: Vec<Option<Arc<[(u32, u32)]>>>,
    descriptors: Vec<RegisteredFontDescriptors>,
    default_ignorable: &'static [(u32, u32)],
    generation: u64,
    shapes: RunCache<ShapedRun>,
    feature_environment: lumen_html::css::MediaEnvironment,
    display: Option<Arc<[lumen_html::font_display::DisplayPhase]>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ScriptSelection {
    candidates: ScriptExtension,
    preferred: Script,
    explicit: bool,
}
impl From<Script> for ScriptSelection {
    fn from(script:Script)->Self { Self {candidates:script.into(),preferred:script,explicit:is_strong_script(script)} }
}

#[derive(Clone, Copy)]
struct ScriptRange {
    start: usize,
    end: usize,
    script: ScriptSelection,
}

#[derive(Clone, Copy)]
struct FontCluster {
    start: usize,
    end: usize,
    face: usize,
    invisible: bool,
}

#[derive(Clone, Copy)]
struct RawClusterAdvance {
    source_start: usize,
    advance: f32,
}

fn push_cluster_advance(
    advances: &mut Vec<RawClusterAdvance>,
    source_start: usize,
    advance: f32,
) -> Result<(), &'static str> {
    if advances.len() >= MAX_SHAPE_TEXT_BYTES {
        return Err("cluster advance limit exceeded");
    }
    advances
        .try_reserve(1)
        .map_err(|_| "cluster advance allocation failed")?;
    advances.push(RawClusterAdvance {
        source_start,
        advance,
    });
    Ok(())
}

fn finish_cluster_advances(
    text: &str,
    mut raw: Vec<RawClusterAdvance>,
) -> Result<Arc<[ShapedClusterAdvance]>, &'static str> {
    if text.is_empty() {
        return Ok(Arc::from([]));
    }
    if raw.len() > MAX_SHAPE_TEXT_BYTES {
        return Err("cluster advance limit exceeded");
    }

    // Rustybuzz returns visual glyph order. Sort its real cluster advances
    // back into logical source order, combining glyphs emitted for one
    // shaping cluster without changing their measured advances.
    raw.sort_unstable_by_key(|item| item.source_start);
    let mut unique = 0;
    for read in 0..raw.len() {
        let item = raw[read];
        if unique != 0 && raw[unique - 1].source_start == item.source_start {
            raw[unique - 1].advance += item.advance;
        } else {
            raw[unique] = item;
            unique += 1;
        }
    }
    raw.truncate(unique);

    // Stream source graphemes rather than keeping a second boundary table.
    // Clusters that begin or end inside one grapheme are merged; ligatures
    // spanning graphemes remain indivisible and retain their summed advance.
    let mut clusters: Vec<ShapedClusterAdvance> = Vec::new();
    clusters
        .try_reserve(text.len().min(MAX_SHAPE_TEXT_BYTES))
        .map_err(|_| "cluster advance allocation failed")?;
    let mut raw_index = 0;
    let mut pending: Option<ShapedClusterAdvance> = None;
    for (start, grapheme) in graphemes(text) {
        let end = start + grapheme.len();
        if let Some(item) = raw.get(raw_index) {
            if item.source_start < start || item.source_start > text.len() {
                return Err("invalid shaping cluster boundary");
            }
        }

        if pending.is_none()
            && raw
                .get(raw_index)
                .is_none_or(|item| item.source_start >= end)
        {
            clusters.push(ShapedClusterAdvance {
                source: start..end,
                advance: 0.0,
            });
            continue;
        }

        if pending.is_none() {
            pending = Some(ShapedClusterAdvance {
                source: start..end,
                advance: 0.0,
            });
        }
        let mut covered_until = pending
            .as_ref()
            .map_or(end, |item| item.source.end.max(end));
        while raw
            .get(raw_index)
            .is_some_and(|item| item.source_start < end)
        {
            let item = raw[raw_index];
            if !text.is_char_boundary(item.source_start) {
                return Err("invalid shaping cluster boundary");
            }
            let item_end = raw
                .get(raw_index + 1)
                .map_or(text.len(), |next| next.source_start);
            if item_end < item.source_start
                || item_end > text.len()
                || !text.is_char_boundary(item_end)
            {
                return Err("invalid shaping cluster interval");
            }
            let current = pending.as_mut().ok_or("missing cluster interval")?;
            current.advance += item.advance;
            covered_until = covered_until.max(item_end);
            raw_index += 1;
        }
        let current = pending.as_mut().ok_or("missing cluster interval")?;
        current.source.end = current.source.end.max(end).max(covered_until);
        if covered_until <= end {
            clusters.push(pending.take().ok_or("missing cluster interval")?);
        }
    }
    if raw_index != raw.len() {
        return Err("shaping cluster lies outside source text");
    }
    if let Some(mut pending) = pending {
        pending.source.end = text.len();
        clusters.push(pending);
    }
    Ok(clusters.into())
}

fn validate_shape_input(text: &str, size: f32) -> Result<(), &'static str> {
    if !size.is_finite() || size <= 0.0 || size > 512.0 {
        return Err("invalid font size");
    }
    if text.len() > MAX_SHAPE_TEXT_BYTES {
        return Err("text run too large");
    }
    Ok(())
}

fn is_strong_script(script: Script) -> bool {
    !matches!(script, Script::Common | Script::Inherited | Script::Unknown)
}

/// Itemize script boundaries only between complete grapheme clusters.
fn script_ranges(text: &str, rtl: bool) -> Result<Vec<ScriptRange>, &'static str> {
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let mut current=ScriptSelection::from(Script::Common);
    let mut start = 0;
    let mut ranges = Vec::new();
    for (cluster_start, cluster) in graphemes(text) {
        // A base and its combining marks are indivisible. Inherited marks
        // follow a real base even when their extension set omits its script.
        let base=cluster.chars().map(|ch|ch.script()).find(|&script|is_strong_script(script));
        let mut candidates=base.map_or_else(||ScriptExtension::for_str(cluster),ScriptExtension::from);
        if candidates.is_empty() {candidates=Script::Common.into();}
        let intersection=current.candidates.intersection(candidates);
        if intersection.is_empty() {
                ranges
                    .try_reserve(1)
                    .map_err(|_| "script allocation failed")?;
                ranges.push(ScriptRange {
                    start,
                    end: cluster_start,
                    script: current,
                });
                start = cluster_start;
                current=ScriptSelection {candidates,preferred:base.unwrap_or(Script::Common),explicit:base.is_some()};
        } else {
            current.candidates=intersection;
            if let Some(base)=base {current.preferred=base;current.explicit=true;}
        }
    }
    ranges
        .try_reserve(1)
        .map_err(|_| "script allocation failed")?;
    ranges.push(ScriptRange {
        start,
        end: text.len(),
        script: current,
    });
    if rtl {
        ranges.reverse();
    }
    Ok(ranges)
}

fn resolve_script(face:&rustybuzz::Face<'_>,selection:ScriptSelection,context:ShapeContext<'_>)->Script {
    let candidates=selection.candidates;
    if selection.explicit {return selection.preferred;}
    if candidates.is_common()||candidates.is_inherited() {return Script::Common;}
    if let Some(full)=context.surrounding {
        let preceding=full[..context.start].chars().rev().map(|ch|ch.script()).find(|&script|is_strong_script(script));
        let following=full[context.end..].chars().map(|ch|ch.script()).find(|&script|is_strong_script(script));
        if let Some(script)=preceding.into_iter().chain(following).find(|&script|candidates.contains_script(script)) {return script;}
    }
    let supported=|script:Script| {
        let mut tag=script.as_iso15924_tag().to_be_bytes();tag.make_ascii_lowercase();
        let tag=ttf_parser::Tag::from_bytes(&tag);
        face.tables().gsub.is_some_and(|table|table.scripts.find(tag).is_some())
            ||face.tables().gpos.is_some_and(|table|table.scripts.find(tag).is_some())
    };
    candidates.iter().find(|&script|supported(script)).or_else(||candidates.iter().next()).unwrap_or(Script::Common)
}

fn shape_buffer(
    bytes: &[u8],
    text: &str,
    rtl: bool,
    script: ScriptSelection,
    features: &[rustybuzz::Feature],
) -> Result<rustybuzz::GlyphBuffer, &'static str> {
    shape_buffer_context(bytes,text,rtl,script,features,ShapeContext::language(None))
}

#[derive(Clone, Copy)]
struct ShapeContext<'a> {
    language: Option<&'a str>,
    surrounding: Option<&'a str>,
    start: usize,
    end: usize,
}
impl<'a> ShapeContext<'a> {
    fn language(language: Option<&'a str>) -> Self {
        Self { language, surrounding: None, start: 0, end: 0 }
    }
    fn subrange(self, start: usize, end: usize) -> Self {
        Self { start: self.start + start, end: self.start + end, ..self }
    }
}

fn shape_buffer_context(bytes: &[u8], text: &str, rtl: bool, script: ScriptSelection,
    features: &[rustybuzz::Feature], context: ShapeContext<'_>) -> Result<rustybuzz::GlyphBuffer, &'static str> {
        let language = context.language;
    let face = rustybuzz::Face::from_slice(bytes, 0).ok_or("font cannot be shaped")?;
    let script=resolve_script(&face,script,context);
    let mut buffer = rustybuzz::UnicodeBuffer::new();
    // UAX #9 has already consumed directional controls before a resolved
    // script run reaches this function. They contribute no glyph or advance;
    // retain original byte clusters while passing the remaining text to GSUB.
    let controls = lumen_common::unicode_props::lookup("Bidi_Control", None)
        .ok_or("missing bidi control properties")?;
    let is_control = |c: char| {
        let cp=c as u32;
        let index=controls.partition_point(|&(_,end)| end<cp);
        controls.get(index).is_some_and(|&(start,_)| start<=cp)
    };
    if text.chars().any(is_control) {
        for (offset,c) in text.char_indices() {
            if !is_control(c) {buffer.add(c,offset as u32);}
        }
    } else {buffer.push_str(text);}
    if let Some(full)=context.surrounding {
        buffer.set_pre_context(&full[..context.start]);
        buffer.set_post_context(&full[context.end..]);
    }
    buffer.set_direction(if rtl {
        rustybuzz::Direction::RightToLeft
    } else {
        rustybuzz::Direction::LeftToRight
    });
    if let Some(script) =
        rustybuzz::Script::from_iso15924_tag(ttf_parser::Tag(script.as_iso15924_tag()))
    {
        buffer.set_script(script);
    }
    buffer.guess_segment_properties();
    if let Some(language)=language.filter(|language|!language.is_empty()) {
        if let Ok(language)=language.parse::<rustybuzz::Language>() {buffer.set_language(language);}
    }
    Ok(rustybuzz::shape(&face, features, buffer))
}

fn shape_bidi_context(shaper:&dyn TextShaper,text:&str,size:f32,rtl:bool,font:&FontSpec,language:Option<&str>) -> Result<ShapedRun,()> {
    validate_shape_input(text,size).map_err(|_|())?;
    let info=bidi::resolve(text,Some(rtl)).map_err(|_|())?;
    let mut glyphs=Vec::new();let mut width=0.0;
    for paragraph in &info.paragraphs {
        let (levels,runs)=bidi::shaping_runs(&info,paragraph,paragraph.range.clone()).map_err(|_|())?;
        for range in runs {
            let run=shaper.shape_resolved_context(&text[range.clone()],size,levels[range.start].is_rtl(),font,language)?;
            if glyphs.len()+run.glyphs.len()>MAX_SHAPE_TEXT_BYTES{return Err(());}
            glyphs.try_reserve(run.glyphs.len()).map_err(|_|())?;
            for glyph in run.glyphs.iter(){let mut glyph=*glyph;glyph.x+=width;glyph.cluster+=range.start as u32;glyphs.push(glyph);}
            width+=run.width;
        }
    }
    Ok(ShapedRun{glyphs:glyphs.into(),width})
}

fn append_glyphs(
    shaped: rustybuzz::GlyphBuffer,
    scale: f32,
    face: u64,
    size_scale: f32,
    cluster_offset: usize,
    glyphs: &mut Vec<Glyph>,
    x: &mut f32,
    mut advances: Option<&mut Vec<RawClusterAdvance>>,
) -> Result<(), &'static str> {
    glyphs
        .try_reserve(shaped.len())
        .map_err(|_| "glyph allocation failed")?;
    if let Some(advances) = advances.as_deref_mut() {
        if advances.len().saturating_add(shaped.len()) > MAX_SHAPE_TEXT_BYTES {
            return Err("cluster advance limit exceeded");
        }
        advances
            .try_reserve(shaped.len())
            .map_err(|_| "cluster advance allocation failed")?;
    }
    for (info, position) in shaped.glyph_infos().iter().zip(shaped.glyph_positions()) {
        let source_start = cluster_offset + info.cluster as usize;
        let advance = position.x_advance as f32 * scale;
        glyphs.push(Glyph {
            id: u16::try_from(info.glyph_id).map_err(|_| "glyph id out of range")?,
            face,
            cluster: u32::try_from(source_start)
                .map_err(|_| "glyph cluster offset out of range")?,
            caps_expansion: 0,
            x: *x + position.x_offset as f32 * scale,
            y: position.y_offset as f32 * scale,
            size_scale,
        });
        *x += advance;
        if let Some(advances) = advances.as_deref_mut() {
            advances.push(RawClusterAdvance {
                source_start,
                advance,
            });
        }
    }
    Ok(())
}

fn canvas_features(options: &CanvasTextOptions) -> Vec<rustybuzz::Feature> {
    let mut features = Vec::with_capacity(7);
    let mut add = |tag: &[u8; 4], enabled: bool| {
        features.push(rustybuzz::Feature::new(
            ttf_parser::Tag::from_bytes(tag),
            u32::from(enabled),
            ..,
        ));
    };
    let speed = options.text_rendering == CanvasTextRendering::OptimizeSpeed;
    let kerning = match options.font_kerning {
        CanvasFontKerning::Auto => !speed,
        CanvasFontKerning::Normal => true,
        CanvasFontKerning::None => false,
    };
    add(b"kern", kerning);
    let ligatures = !speed && options.letter_spacing == 0.0;
    add(b"liga", ligatures);
    add(b"clig", ligatures);
    append_caps_features(&mut features, options.font_variant_caps);
    features
}

fn font_features(spec: &FontSpec) -> Vec<rustybuzz::Feature> {
    let mut features = Vec::new();
    append_caps_features(&mut features, spec.caps);
    append_ligature_features(&mut features, spec.ligatures);
    if spec.alternates.as_ref().is_some_and(|value|value.historical) {features.push(rustybuzz::Feature::new(ttf_parser::Tag::from_bytes(b"hist"),1,..));}
    if spec.disable_optional_ligatures { disable_optional_ligatures(&mut features); }
    append_feature_settings(&mut features, spec.feature_settings.as_deref());
    features
}

fn disable_optional_ligatures(features: &mut Vec<rustybuzz::Feature>) {
    for tag in [b"liga", b"clig", b"dlig", b"hlig"] {
        features.push(rustybuzz::Feature::new(ttf_parser::Tag::from_bytes(tag), 0, ..));
    }
}
fn append_feature_settings(features: &mut Vec<rustybuzz::Feature>, settings: Option<&FontFeatureSettings>) {
    if let Some(values) = settings.and_then(FontFeatureSettings::resolved) {
        for feature in values {
            features.push(rustybuzz::Feature::new(ttf_parser::Tag::from_bytes(&feature.tag), feature.value, ..));
        }
    }
}
fn features_resolved(spec: &FontSpec) -> bool {
    spec.unresolved_style.is_none() && spec.unresolved_stretch.is_none() && spec.feature_settings.as_ref().is_none_or(|settings| settings.resolved().is_some())
}
fn explicit_feature(spec: &FontSpec, tag: &[u8;4]) -> Option<u32> {
    spec.feature_settings.as_ref().and_then(|settings| settings.resolved())?
        .iter().find(|feature| &feature.tag == tag).map(|feature| feature.value)
}

fn append_ligature_features(features:&mut Vec<rustybuzz::Feature>, ligatures:FontLigatures) {
    for group in FontLigatureGroup::ALL {
        let Some(enabled) = ligatures.setting(group) else { continue; };
        let tags: &[&[u8;4]] = match group {
            FontLigatureGroup::Common => &[b"liga",b"clig"],
            FontLigatureGroup::Discretionary => &[b"dlig"],
            FontLigatureGroup::Historical => &[b"hlig"],
            FontLigatureGroup::Contextual => &[b"calt"],
        };
        for tag in tags {
            features.push(rustybuzz::Feature::new(ttf_parser::Tag::from_bytes(tag), u32::from(enabled), ..));
        }
    }
}
fn append_caps_features(features: &mut Vec<rustybuzz::Feature>, caps: CanvasFontVariantCaps) {
    let mut add = |tag: &[u8; 4], enabled: bool| {
        features.push(rustybuzz::Feature::new(
            ttf_parser::Tag::from_bytes(tag),
            u32::from(enabled),
            ..,
        ))
    };
    match caps {
        CanvasFontVariantCaps::Normal => {}
        CanvasFontVariantCaps::SmallCaps => add(b"smcp", true),
        CanvasFontVariantCaps::AllSmallCaps => {
            add(b"smcp", true);
            add(b"c2sc", true);
        }
        CanvasFontVariantCaps::PetiteCaps => add(b"pcap", true),
        CanvasFontVariantCaps::AllPetiteCaps => {
            add(b"pcap", true);
            add(b"c2pc", true);
        }
        CanvasFontVariantCaps::Unicase => add(b"unic", true),
        CanvasFontVariantCaps::TitlingCaps => add(b"titl", true),
    }
}

fn apply_canvas_spacing(text: &str, mut run: ShapedRun, options: &CanvasTextOptions) -> ShapedRun {
    if run.glyphs.is_empty() || (options.letter_spacing == 0.0 && options.word_spacing == 0.0) {
        return run;
    }
    let mut glyphs = run.glyphs.to_vec();
    let mut prior_cluster = None;
    let mut prior_was_space = false;
    let mut offset = 0.0f32;
    for glyph in &mut glyphs {
        if prior_cluster != Some((glyph.cluster, glyph.caps_expansion)) {
            if prior_cluster.is_some() {
                offset += options.letter_spacing;
                if prior_was_space {
                    offset += options.word_spacing;
                }
            }
            prior_cluster = Some((glyph.cluster, glyph.caps_expansion));
            prior_was_space = text
                .get(glyph.cluster as usize..)
                .and_then(|tail| graphemes(tail).next())
                .is_some_and(|(_, cluster)| cluster.chars().all(char::is_whitespace));
        }
        glyph.x += offset;
    }
    if prior_was_space {
        offset += options.word_spacing;
    }
    run.width += offset;
    run.glyphs = glyphs.into();
    run
}

impl FontSet {
    /// Build an immutable renderer set from platform fallback registrations
    /// and one generation-tagged document font snapshot. CSS faces must be
    /// resolved by the host without initiating loads; manual faces are
    /// included only after successful decoding. Decoded `FontFace` bytes stay
    /// shared through `Arc` across sets and registrations.
    pub fn from_font_registry_snapshot(
        fallback: &[FontRegistration],
        css_rules: &[lumen_html::css::FontFaceRule],
        resolved_css_faces: &[Option<Arc<FontFace>>],
        manual_faces: &[ManualFontFace],
    ) -> Result<Self, &'static str> {
        Self::from_font_registry_snapshot_with_query(fallback, css_rules, resolved_css_faces, manual_faces, lumen_html::css::ContainerUnitContext::default())
    }

    pub fn from_font_registry_snapshot_with_query(
        fallback: &[FontRegistration],
        css_rules: &[lumen_html::css::FontFaceRule],
        resolved_css_faces: &[Option<Arc<FontFace>>],
        manual_faces: &[ManualFontFace],
        query: lumen_html::css::ContainerUnitContext,
    ) -> Result<Self, &'static str> {
        Self::from_font_registry_snapshot_with_display(fallback,css_rules,resolved_css_faces,
            manual_faces,query,None,None)
    }

    /// Presentation is independent of resource status. Unavailable block/swap
    /// faces participate in descriptor matching, then shape with a loaded fallback.
    pub fn from_font_registry_snapshot_with_display(
        fallback:&[FontRegistration], css_rules:&[lumen_html::css::FontFaceRule],
        resolved_css_faces:&[Option<Arc<FontFace>>], manual_faces:&[ManualFontFace],
        query:lumen_html::css::ContainerUnitContext,
        css_display:Option<&[lumen_html::font_display::DisplayPhase]>,
        manual_display:Option<&[lumen_html::font_display::DisplayPhase]>,
    ) -> Result<Self,&'static str> {
        use lumen_html::font_display::DisplayPhase;
        if css_display.is_some_and(|values|values.len()!=css_rules.len())
            || manual_display.is_some_and(|values|values.len()!=manual_faces.len()) {
            return Err("font display states need matching registrations");
        }
        let initial = FontSpec::default();
        let initial_face = fallback.first().ok_or("font snapshot needs platform fallback")?;
        let metrics = initial_face.font.face.font_relative_metrics_styled(16.0,&initial);
        let viewport = (query.width != lumen_html::css::ContainerUnitBasis::Unknown || query.height != lumen_html::css::ContainerUnitBasis::Unknown)
            .then_some(query.small_viewport);
        let descriptor_context = lumen_html::css::FontShorthandContext {font_size:16.0,root_font_size:16.0,ex:metrics.ex,ch:metrics.ch,
            units:Some([lumen_html::css::font_unit_bases(Some(initial_face.font.face.as_ref()),16.0,&initial,lumen_html::css::LineHeight::Normal,false,false);2]),weight:400,viewport,query:Some(query)};
        if css_rules.len() != resolved_css_faces.len() {
            return Err("CSS font rules need matching resolved faces");
        }
        let css_count=css_rules.iter().zip(resolved_css_faces).enumerate().filter(|(index,(_,face))| {
            match css_display.map_or(DisplayPhase::Loaded,|values|values[*index]) {
                DisplayPhase::Failure=>false,
                DisplayPhase::Block|DisplayPhase::Swap=>true,
                DisplayPhase::Loaded=>face.is_some(),
            }
        }).count();
        let manual_count=manual_faces.iter().enumerate().filter(|(index,face)| {
            match manual_display.map_or(DisplayPhase::Loaded,|values|values[*index]) {
                DisplayPhase::Failure=>false,
                DisplayPhase::Block|DisplayPhase::Swap=>true,
                DisplayPhase::Loaded=>face.status==FontFaceStatus::Loaded && face.decoded.is_some(),
            }
        }).count();
        let capacity = fallback.len().checked_add(css_count)
            .and_then(|capacity|capacity.checked_add(manual_count))
            .ok_or("too many registered fonts")?;
        if capacity > MAX_REGISTERED_FONTS {
            return Err("too many registered fonts");
        }

        let mut registrations = Vec::new();
        registrations
            .try_reserve_exact(capacity)
            .map_err(|_| "font registration allocation failed")?;
        registrations.extend_from_slice(fallback);
        let mut modes=Vec::new();
        modes.try_reserve_exact(capacity).map_err(|_|"font display allocation failed")?;
        modes.resize(fallback.len(),DisplayPhase::Loaded);
        for (index,(rule, face)) in css_rules.iter().zip(resolved_css_faces).enumerate() {
            let mode=css_display.map_or(DisplayPhase::Loaded,|values|values[index]);
            let face=match (mode,face) {
                (DisplayPhase::Failure,_)=>None,
                (DisplayPhase::Block|DisplayPhase::Swap,None)=>Some(initial_face.font.face.clone()),
                (_,face)=>face.clone(),
            };
            if let Some(face) = face {
                let mut resolved;
                let rule = if rule.stretch_expressions.is_some() {
                    resolved = rule.clone();
                    if !lumen_html::css::resolve_font_face_width_context(&mut resolved.descriptors,descriptor_context) {
                        return Err("font width descriptor requires query context");
                    }
                    &resolved
                } else { rule };
                let mut registration = FontRegistration::from_css_rule(rule, face.clone())?;
                registration.descriptors.feature_settings = lumen_html::css::resolve_font_feature_settings_with_context(registration.descriptors.feature_settings.as_ref(), descriptor_context)
                    .ok_or("font descriptor feature settings require query context")?;
                registrations.push(registration);
                modes.push(mode);
            }
        }
        for (index,manual) in manual_faces.iter().enumerate() {
            if manual.rule.identity.as_ref() != Some(&manual.identity) {
                return Err("manual font registration identity does not match its descriptor");
            }
            let mode=manual_display.map_or(DisplayPhase::Loaded,|values|values[index]);
            let face=match mode {
                DisplayPhase::Failure=>None,
                DisplayPhase::Block|DisplayPhase::Swap if manual.decoded.is_none()=>Some(initial_face.font.face.clone()),
                _ if manual.status==FontFaceStatus::Loaded=>manual.decoded.clone(),
                _=>None,
            };
            if let Some(face) = face {
                    let mut resolved;
                    let rule = if manual.rule.stretch_expressions.is_some() {
                        resolved = manual.rule.clone();
                        if !lumen_html::css::resolve_font_face_width_context(&mut resolved.descriptors,descriptor_context) {
                            return Err("font width descriptor requires query context");
                        }
                        &resolved
                    } else { &manual.rule };
                    let mut registration = FontRegistration::from_css_rule(rule, face)?;
                    registration.descriptors.feature_settings = lumen_html::css::resolve_font_feature_settings_with_context(registration.descriptors.feature_settings.as_ref(), descriptor_context)
                        .ok_or("font descriptor feature settings require query context")?;
                    registrations.push(registration);
                    modes.push(mode);
            }
        }

        let mut fonts = Vec::new();
        let mut unicode_ranges = Vec::new();
        let mut descriptors = Vec::new();
        fonts
            .try_reserve_exact(registrations.len())
            .map_err(|_| "font registration allocation failed")?;
        unicode_ranges
            .try_reserve_exact(registrations.len())
            .map_err(|_| "font registration allocation failed")?;
        descriptors
            .try_reserve_exact(registrations.len())
            .map_err(|_| "font registration allocation failed")?;
        for registration in registrations {
            fonts.push(registration.font);
            unicode_ranges.push(registration.unicode_range);
            descriptors.push(registration.descriptors);
        }
        let mut set=Self::new_with_unicode_ranges_and_descriptors(fonts, unicode_ranges, descriptors)?;
        set.feature_environment=query.small_viewport;
        if modes.iter().any(|mode|*mode!=DisplayPhase::Loaded) {
            set.display=Some(modes.into());
        }
        Ok(set)
    }

    pub fn new(faces: Vec<RegisteredFont>) -> Result<Self, &'static str> {
        let ranges = alloc::vec![None; faces.len()];
        Self::new_with_unicode_ranges(faces, ranges)
    }

    /// Web faces carry descriptor coverage; platform fallback faces use None.
    /// Equal-matching web faces are checked in reverse declaration order.
    pub fn new_with_unicode_ranges(
        faces: Vec<RegisteredFont>,
        unicode_ranges: Vec<Option<Arc<[(u32, u32)]>>>,
    ) -> Result<Self, &'static str> {
        let descriptors = faces
            .iter()
            .map(|font| RegisteredFontDescriptors::scalar(font, 1.0))
            .collect();
        Self::new_with_unicode_ranges_and_descriptors(faces, unicode_ranges, descriptors)
    }

    /// Build a face set with the CSS `size-adjust` descriptor for each face.
    /// Ratios are percentages converted to multipliers by the caller; decoded
    /// `FontFace` values stay reusable across registrations.
    pub fn new_with_unicode_ranges_and_size_adjust(
        faces: Vec<RegisteredFont>,
        unicode_ranges: Vec<Option<Arc<[(u32, u32)]>>>,
        size_adjust: Vec<f32>,
    ) -> Result<Self, &'static str> {
        if size_adjust.len() != faces.len() {
            return Err("font descriptors need matching faces");
        }
        let descriptors = faces
            .iter()
            .zip(size_adjust)
            .map(|(font, size_adjust)| RegisteredFontDescriptors::scalar(font, size_adjust))
            .collect();
        Self::new_with_unicode_ranges_and_descriptors(faces, unicode_ranges, descriptors)
    }

    /// Build a face set from the full descriptor metadata shared with CSS
    /// parsing and font loading, while retaining the existing scalar API for
    /// platform-font callers.
    pub fn new_with_unicode_ranges_and_descriptors(
        faces: Vec<RegisteredFont>,
        unicode_ranges: Vec<Option<Arc<[(u32, u32)]>>>,
        mut descriptors: Vec<RegisteredFontDescriptors>,
    ) -> Result<Self, &'static str> {
        if faces.is_empty() {
            return Err("font set is empty");
        }
        if faces.len() > MAX_REGISTERED_FONTS {
            return Err("too many registered fonts");
        }
        if unicode_ranges.len() != faces.len()
            || descriptors.len() != faces.len()
            || !unicode_ranges.iter().any(Option::is_none)
        {
            return Err("font coverage needs matching faces and platform fallback");
        }
        for range in unicode_ranges.iter().flatten() {
            if range.is_empty()
                || range.len() > 256
                || range
                    .iter()
                    .any(|&(first, last)| first > last || last > 0x10ffff)
            {
                return Err("invalid font Unicode coverage");
            }
        }
        for font in &faces {
            // Family names are CSS strings. Whitespace inside a quoted name is
            // significant, including at either end, so only reject the empty
            // string here rather than normalizing the value with `trim`.
            if font.family.is_empty() || font.family.len() > MAX_FAMILY_NAME_BYTES {
                return Err("invalid font family name");
            }
            if !(1..=1000).contains(&font.weight) {
                return Err("invalid font weight");
            }
            if !font.stretch.is_finite() || font.stretch < 0.0 {
                return Err("invalid font stretch");
            }
        }
        let initial_face = faces.iter().zip(&unicode_ranges).find(|(_,range)| range.is_none()).map(|(face,_)|face)
            .ok_or("font snapshot needs platform fallback")?;
        let metrics = initial_face.face.font_relative_metrics_styled(16.0,&FontSpec::default());
        let context = lumen_html::css::FontShorthandContext {font_size:16.0,root_font_size:16.0,ex:metrics.ex,ch:metrics.ch,
            units:Some([lumen_html::css::font_unit_bases(Some(initial_face.face.as_ref()),16.0,&FontSpec::default(),lumen_html::css::LineHeight::Normal,false,false);2]),weight:400,viewport:None,query:None};
        for descriptor in &mut descriptors {
            descriptor.feature_settings = lumen_html::css::resolve_font_feature_settings_with_context(descriptor.feature_settings.as_ref(), context)
                .ok_or("font descriptor feature settings require query context")?;
        }
        if descriptors.iter().any(|descriptor| {
            descriptor.weight_range[0] == 0
                || descriptor.weight_range[0] > descriptor.weight_range[1]
                || descriptor.weight_range[1] > 1000
                || descriptor
                    .stretch_range
                    .iter()
                    .any(|value| !value.is_finite() || *value < 0.0)
                || descriptor.stretch_range[0] > descriptor.stretch_range[1]
                || !descriptor.size_adjust.is_finite()
                || descriptor.size_adjust < 0.0
        }) {
            return Err("invalid font registration descriptors");
        }
        let default_ignorable: &'static [(u32, u32)] =
            lumen_common::unicode_props::lookup("DI", None)
                .ok_or("default-ignorable Unicode property unavailable")?;
        let generation = NEXT_FACE_ID
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                next.checked_add(1)
            })
            .map_err(|_| "font identity exhausted")?;
        Ok(Self {
            faces: faces.into(),
            unicode_ranges,
            descriptors,
            default_ignorable,
            generation,
            shapes: RunCache::new(),
            feature_environment: lumen_html::css::MediaEnvironment::default(),
            display: None,
        })
    }

    fn face_order(&self, spec: &FontSpec) -> Result<Vec<usize>, &'static str> {
        if spec.unresolved_style.is_some() || spec.unresolved_stretch.is_some() { return Err("font style or width requires query-container context"); }
        if !(1..=1000).contains(&spec.weight) {
            return Err("invalid requested font weight");
        }
        if !spec.stretch.is_finite() || spec.stretch < 0.0 {
            return Err("invalid requested font stretch");
        }
        if let Some(families) = &spec.families {
            if families.len() > MAX_REQUESTED_FAMILIES {
                return Err("too many requested font families");
            }
            for family in families.iter() {
                if family.is_empty() || family.len() > MAX_FAMILY_NAME_BYTES {
                    return Err("invalid requested font family");
                }
            }
        }

        let mut order = Vec::new();
        order
            .try_reserve_exact(self.faces.len())
            .map_err(|_| "font order allocation failed")?;

        if let Some(families) = &spec.families {
            for requested in families.iter() {
                let Some(best) = self.best_candidate(spec, Some(requested), &order) else {
                    continue;
                };
                let matched_instance = self.font_match_instance(spec, best);
                while let Some(index) = self.best_candidate(spec, Some(requested), &order) {
                    order.push(index);
                    let face_instance = self.font_match_instance(spec, index);
                    // A family selects one width/style/weight combination.
                    // Keep equal-matching unicode-range slices together;
                    // missing glyphs then fall through to the next family.
                    if face_instance != matched_instance {
                        order.pop();
                        break;
                    }
                }
            }
        }
        while let Some(index) = self.best_candidate(spec, None, &order) {
            order.push(index);
        }
        Ok(order)
    }

    fn best_candidate(
        &self,
        spec: &FontSpec,
        family: Option<&FontFamily>,
        excluded: &[usize],
    ) -> Option<usize> {
        let mut best = None;
        let nearest=family.filter(|family|!family.is_generic()).and_then(|family|self.faces.iter().enumerate()
            .filter(|(index,font)| self.unicode_ranges[*index].is_some() && font.family.eq_ignore_ascii_case(family))
            .filter_map(|(index,_)|lumen_html::css::font_feature_values::scope_rank(self.descriptors[index].family_scope.as_deref().and_then(|chain|chain.first().copied()),spec.family_scope.as_deref())).min());
        let mut best_rank: Option<(lumen_html::paint::FontMatchRank, u8, usize)> = None;
        for (index, font) in self.faces.iter().enumerate() {
            if excluded.contains(&index)
                || (family.is_some_and(|family|!family.is_generic()) && self.unicode_ranges[index].is_some()
                    && lumen_html::css::font_feature_values::scope_rank(self.descriptors[index].family_scope.as_deref().and_then(|chain|chain.first().copied()),spec.family_scope.as_deref())!=nearest)
                // Downloaded faces are document-family resources, never
                // members of the installed-font fallback pool.
                || (family.is_none() && self.unicode_ranges[index].is_some())
                || family.is_some_and(|requested| {
                    (requested.is_generic() && self.unicode_ranges[index].is_some())
                        || !font.family.eq_ignore_ascii_case(requested)
                })
            {
                continue;
            }
            let Some((descriptor_rank, _, _, _)) = font_match_style_range_rank(
                spec,
                font.style,
                self.descriptors[index].oblique_range,
                self.descriptors[index].weight_range,
                self.descriptors[index].stretch_range,
            ) else {
                continue;
            };
            let rank = (
                descriptor_rank,
                u8::from(self.unicode_ranges[index].is_none()),
                if self.unicode_ranges[index].is_some() {
                    usize::MAX - index
                } else {
                    index
                },
            );
            if best_rank.is_none_or(|best| rank < best) {
                best = Some(index);
                best_rank = Some(rank);
            }
        }
        best
    }

    fn font_match_instance(&self, spec: &FontSpec, index: usize) -> Option<(u16, f32, FontStyle)> {
        let descriptor = &self.descriptors[index];
        font_match_style_range_rank(
            spec,
            self.faces[index].style,
            descriptor.oblique_range,
            descriptor.weight_range,
            descriptor.stretch_range,
        )
        .map(|(_, weight, stretch, style)| (weight, stretch, style))
    }

    fn face_covers(&self, index: usize, cluster: &str) -> bool {
        let face = &self.faces[index].face;
        face.covers_cluster_with_properties(
            cluster,
            self.unicode_ranges[index].as_deref(),
            self.default_ignorable,
        )
    }

    fn display_phase(&self,index:usize)->lumen_html::font_display::DisplayPhase {
        self.display.as_ref().map_or(lumen_html::font_display::DisplayPhase::Loaded,|modes|modes[index])
    }

    fn loaded_fallback(&self,cluster:&str,order:&[usize])->usize {
        use lumen_html::font_display::DisplayPhase;
        order.iter().copied().find(|&index|self.display_phase(index)==DisplayPhase::Loaded
            && self.face_covers(index,cluster)).unwrap_or_else(||order.iter().rev().copied()
            .find(|&index|self.unicode_ranges[index].is_none()).expect("platform fallback validated"))
    }

    fn cluster_face(&self, cluster: &str, order: &[usize]) -> (usize,bool) {
        use lumen_html::font_display::DisplayPhase;
        for &index in order {
            match self.display_phase(index) {
                DisplayPhase::Loaded if self.face_covers(index,cluster)=>return (index,false),
                phase @ (DisplayPhase::Block|DisplayPhase::Swap) if cluster.chars().all(|character|
                    character.is_control() || self.default_ignorable.binary_search_by(|&(first,last)| {
                        if (character as u32)<first {CmpOrdering::Greater} else if (character as u32)>last {CmpOrdering::Less} else {CmpOrdering::Equal}
                    }).is_ok() || self.unicode_ranges[index].as_ref().is_none_or(|ranges|ranges.iter()
                        .any(|&(first,last)|first<=character as u32 && character as u32<=last)))=>
                    return (self.loaded_fallback(cluster,order),phase==DisplayPhase::Block),
                _=>{},
            }
        }
        (self.loaded_fallback(cluster,order),false)
    }

    fn face_by_identity(&self,identity:u64)->Option<&Arc<FontFace>> {
        if identity==0 {return self.faces.first().map(|font|&font.face);}
        self.faces.iter().find(|font|font.face.id()==identity || font.face.is_invisible_key(identity)).map(|font|&font.face)
    }

    fn append_resolved_range(
        &self,
        text: &str,
        text_offset: usize,
        size: f32,
        rtl: bool,
        order: &[usize],
        spec: &FontSpec,
        features: &[rustybuzz::Feature],
        glyphs: &mut Vec<Glyph>,
        x: &mut f32,
        advances: Option<&mut Vec<RawClusterAdvance>>,
    ) -> Result<(), &'static str> {
        self.append_resolved_range_context(text,text_offset,size,rtl,order,spec,features,glyphs,x,advances,ShapeContext {language:None,surrounding:Some(text),start:0,end:text.len()})
    }

    fn append_resolved_range_context(&self,text:&str,text_offset:usize,size:f32,rtl:bool,
        order:&[usize],spec:&FontSpec,features:&[rustybuzz::Feature],glyphs:&mut Vec<Glyph>,x:&mut f32,
        mut advances:Option<&mut Vec<RawClusterAdvance>>,context:ShapeContext<'_>) -> Result<(), &'static str> {
        for script_range in script_ranges(text, rtl)? {
            let value = &text[script_range.start..script_range.end];
            let mut clusters = Vec::new();
            for (start, cluster) in graphemes(value) {
                clusters.try_reserve(1).map_err(|_|"font fallback allocation failed")?;
                let (face,invisible)=self.cluster_face(cluster,order);
                clusters.push(FontCluster {start,end:start+cluster.len(),face,invisible});
            }
            if rtl {
                clusters.reverse();
            }

            let mut current: Option<FontCluster> = None;
            for cluster in clusters {
                if let Some(mut run) = current {
                    if run.face == cluster.face && run.invisible==cluster.invisible {
                        run.start = run.start.min(cluster.start);
                        run.end = run.end.max(cluster.end);
                        current = Some(run);
                        continue;
                    }
                    self.append_cluster_run_context(
                        value,
                        text_offset + script_range.start,
                        run,
                        script_range.script,
                        size,
                        rtl,
                        spec,
                        features,
                        glyphs,
                        x,
                        advances.as_deref_mut(),
                        context.subrange(script_range.start,script_range.end),
                    )?;
                }
                current = Some(cluster);
            }
            if let Some(run) = current {
                self.append_cluster_run_context(
                    value,
                    text_offset + script_range.start,
                    run,
                    script_range.script,
                    size,
                    rtl,
                    spec,
                    features,
                    glyphs,
                    x,
                    advances.as_deref_mut(),
                    context.subrange(script_range.start,script_range.end),
                )?;
            }
        }
        Ok(())
    }

    fn append_cluster_run_context(
        &self,
        text: &str,
        text_offset: usize,
        run: FontCluster,
        script:ScriptSelection,
        size: f32,
        rtl: bool,
        spec: &FontSpec,
        features: &[rustybuzz::Feature],
        glyphs: &mut Vec<Glyph>,
        x: &mut f32,
        advances: Option<&mut Vec<RawClusterAdvance>>,
        context: ShapeContext<'_>,
    ) -> Result<(), &'static str> {
        let registered = &self.faces[run.face];
        let mut merged = Vec::new();
        let descriptor=&self.descriptors[run.face];
        let features = if descriptor.feature_settings.is_some() || spec.alternates.is_some() {
            append_feature_settings(&mut merged, descriptor.feature_settings.as_deref());
            let aliases=lumen_html::css::font_feature_values::resolve_features(spec.alternates.as_deref(),
                &registered.family,descriptor.feature_values.as_deref().unwrap_or(&[]),self.feature_environment,spec.family_scope.as_deref());
            for feature in aliases {merged.push(rustybuzz::Feature::new(ttf_parser::Tag::from_bytes(&feature.tag),feature.value,..));}
            // Explicit property features win over descriptor and variant features.
            merged.extend_from_slice(features);
            merged.as_slice()
        } else { features };
        let size_scale = self.size_adjust_scale(spec, run.face);
        if size_scale == 0.0 {
            if let Some(advances) = advances {
                push_cluster_advance(advances, text_offset + run.start, 0.0)?;
            }
            return Ok(());
        }
        let face=&registered.face;
        let glyph_face=if run.invisible {face.invisible_key()?}else{face.id()};
        face.shape_caps_into_context(
            &text[run.start..run.end],
            size * size_scale,
            rtl,
            script,
            glyph_face,
            size_scale,
            text_offset + run.start,
            spec,
            features,
            glyphs,
            x,
            advances,
            context.subrange(run.start,run.end),
        )
    }

    fn metric_face_index(&self, spec: &FontSpec) -> usize {
        self.face_order(spec)
            .ok()
            // CSS Fonts 4: the first available font excludes faces whose
            // unicode-range does not include U+0020, independent of its cmap.
            .and_then(|order| {
                order.iter().copied().find(|&index| {
                    self.unicode_ranges[index].as_ref().is_none_or(|ranges| {
                        ranges
                            .iter()
                            .any(|&(first, last)| first <= 0x20 && 0x20 <= last)
                    })
                }).and_then(|index| {
                    if self.display_phase(index)==lumen_html::font_display::DisplayPhase::Loaded {Some(index)}
                    else {order.into_iter().find(|&candidate|self.display_phase(candidate)==lumen_html::font_display::DisplayPhase::Loaded
                        && self.unicode_ranges[candidate].as_ref().is_none_or(|ranges|ranges.iter().any(|&(first,last)|first<=0x20 && 0x20<=last)))}
                })
            })
            .unwrap_or(0)
    }

    #[cfg(test)]
    fn metric_face(&self, spec: &FontSpec) -> &FontFace {
        &self.faces[self.metric_face_index(spec)].face
    }

    /// Return a metric from the same first-available face used by CSS font
    /// selection, including family/descriptor matching and unicode-range.
    pub fn first_available_metric(&self, spec: &FontSpec, metric: FontMetric) -> Option<f32> {
        let index = self.metric_face_index(spec);
        self.faces[index]
            .face
            .metric_ratio(metric)
            .map(|value| value * self.descriptors[index].size_adjust)
    }

    fn size_adjust_scale(&self, spec: &FontSpec, face_index: usize) -> f32 {
        let face = &self.faces[face_index].face;
        let face_scale = self.descriptors[face_index].size_adjust;
        let Some(adjust) = spec.size_adjust else {
            return face_scale;
        };
        let reference_index = self.metric_face_index(spec);
        let reference = &self.faces[reference_index].face;
        let target = match adjust.value {
            FontSizeAdjustValue::Number(number) => number,
            FontSizeAdjustValue::FromFont => {
                let Some(metric) = reference.metric(adjust.metric) else {
                    return face_scale;
                };
                metric * self.descriptors[reference_index].size_adjust
            }
        };
        if target == 0.0 {
            return 0.0;
        }
        let Some(actual) = face.metric(adjust.metric) else {
            return face_scale;
        };
        let actual = actual * face_scale;
        if actual == 0.0 {
            return 0.0;
        }
        let scale = target / actual;
        if scale.is_finite() && scale >= 0.0 {
            face_scale * scale
        } else {
            face_scale
        }
    }

    fn shape_with_direction(
        &self,
        text: &str,
        size: f32,
        rtl: bool,
        spec: &FontSpec,
        resolve_bidi: bool,
    ) -> Result<ShapedRun, &'static str> {
        self.shape_with_direction_and_advances(text, size, rtl, spec, resolve_bidi, None)
    }

    fn shape_with_direction_and_advances(
        &self,
        text: &str,
        size: f32,
        rtl: bool,
        spec: &FontSpec,
        resolve_bidi: bool,
        mut advances: Option<&mut Vec<RawClusterAdvance>>,
    ) -> Result<ShapedRun, &'static str> {
        validate_shape_input(text, size)?;
        let mode = if resolve_bidi { 1 } else { 2 };
        if !features_resolved(spec) { return Err("font feature settings require query-container context"); }
        if advances.is_none() {
            if let Some(run) = self
                .shapes
                .get_styled(text, size, Some(rtl), mode, Some(spec))
            {
                return Ok(run);
            }
        }
        let order = self.face_order(spec)?;
        let features = font_features(spec);
        let mut glyphs = Vec::new();
        let mut x = 0.0;
        if resolve_bidi {
            let info = bidi::resolve(text, Some(rtl))?;
            for paragraph in &info.paragraphs {
                let (levels, runs) = bidi::shaping_runs(&info, paragraph, paragraph.range.clone())?;
                for range in runs {
                    self.append_resolved_range(
                        &text[range.clone()],
                        range.start,
                        size,
                        levels[range.start].is_rtl(),
                        &order,
                        spec,
                        &features,
                        &mut glyphs,
                        &mut x,
                        advances.as_deref_mut(),
                    )?;
                }
            }
        } else {
            self.append_resolved_range(
                text,
                0,
                size,
                rtl,
                &order,
                spec,
                &features,
                &mut glyphs,
                &mut x,
                advances.as_deref_mut(),
            )?;
        }
        let run = ShapedRun {
            glyphs: glyphs.into(),
            width: x,
        };
        self.shapes
            .insert_styled(text, size, Some(rtl), mode, Some(spec), run.clone());
        Ok(run)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ShapeCacheStats {
    pub bytes: usize,
    pub entries: usize,
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
}
impl ShapeCacheStats {
    fn add(self, other: Self) -> Self {
        Self {
            bytes: self.bytes + other.bytes,
            entries: self.entries + other.entries,
            hits: self.hits + other.hits,
            misses: self.misses + other.misses,
            evictions: self.evictions + other.evictions,
        }
    }
}
trait CachedValue: Clone {
    fn cache_bytes(&self) -> usize;
}
impl CachedValue for f32 {
    fn cache_bytes(&self) -> usize {
        0
    }
}
impl CachedValue for ShapedRun {
    fn cache_bytes(&self) -> usize {
        self.glyphs.len() * core::mem::size_of::<Glyph>() + 2 * core::mem::size_of::<usize>()
    }
}
struct RunSlot<V> {
    hash: u64,
    size: u32,
    dir: u8,
    mode: u8,
    language: Option<Arc<str>>,
    source: Option<(usize,usize)>,
    clusters: Option<Arc<[ShapedClusterAdvance]>>,
    spec: Option<FontSpec>,
    text: Box<str>,
    bytes: usize,
    value: V,
}

/// Direct-mapped cache, bounded by slots, text length and retained bytes.
/// Contention is a miss; shaping never waits for another thread's cache lock.
struct RunCache<V> {
    busy: AtomicBool,
    slots: UnsafeCell<Vec<Option<RunSlot<V>>>>,
    hits: AtomicU64,
    misses: AtomicU64,
    evictions: AtomicU64,
}
// SAFETY: slots is touched only while the busy flag is held.
unsafe impl<V: Send> Sync for RunCache<V> {}
impl<V: CachedValue> RunCache<V> {
    const fn new() -> Self {
        Self {
            busy: AtomicBool::new(false),
            slots: UnsafeCell::new(Vec::new()),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
        }
    }
    fn key(
        text: &str,
        size: f32,
        rtl: Option<bool>,
        mode: u8,
        spec: Option<&FontSpec>,
    ) -> (u64, u32, u8) {
        let size = size.to_bits();
        let dir = match rtl {
            None => 0,
            Some(false) => 1,
            Some(true) => 2,
        };
        let mut hash = lumen_common::fasthash::FNV1A64_OFFSET;
        let mut feed = |bytes: &[u8]| hash = lumen_common::fasthash::fnv1a64(hash, bytes);
        feed(text.as_bytes());
        feed(&size.to_le_bytes());
        feed(&[dir, mode]);
        if let Some(spec) = spec {
            feed(&spec.ligatures.cache_key());
            if let Some(chain)=&spec.family_scope {for root in chain.iter(){feed(&root.key().to_le_bytes());}}
            if let Some(value)=&spec.alternates {feed(&[u8::from(value.historical)]);for request in value.requests.iter(){feed(&[request.kind as u8]);for name in request.names.iter(){feed(&name.len().to_le_bytes());feed(name.as_bytes());}}}
            feed(&[u8::from(spec.disable_optional_ligatures)]);
            if let Some(values) = spec.feature_settings.as_ref().and_then(|settings| settings.resolved()) {
                for feature in values { feed(&feature.tag); feed(&feature.value.to_le_bytes()); }
            }
            feed(&spec.weight.to_le_bytes());
            feed(&spec.stretch.to_bits().to_le_bytes());
            let (kind, angle) = spec.style.cache_key();
            feed(&[kind]);
            feed(&angle.to_le_bytes());
            if let Some(families) = &spec.families {
                for family in families.iter() {
                    feed(&[u8::from(family.is_generic())]);
                    feed(&family.len().to_le_bytes());
                    feed(family.as_bytes());
                }
            }
        }
        (hash, size, dir)
    }
    fn locked<R>(&self, f: impl FnOnce(&mut Vec<Option<RunSlot<V>>>) -> R) -> Option<R> {
        struct Release<'a>(&'a AtomicBool);
        impl Drop for Release<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        if self.busy.swap(true, Ordering::Acquire) {
            return None;
        }
        let _release = Release(&self.busy);
        // SAFETY: busy grants exclusive access until Release drops.
        Some(f(unsafe { &mut *self.slots.get() }))
    }
    fn bytes(slots: &Vec<Option<RunSlot<V>>>) -> usize {
        slots.capacity() * core::mem::size_of::<Option<RunSlot<V>>>()
            + slots
                .iter()
                .filter_map(Option::as_ref)
                .map(|slot| slot.bytes)
                .sum::<usize>()
    }
    fn stats(&self) -> ShapeCacheStats {
        let (bytes, entries) = self
            .locked(|slots| {
                (
                    Self::bytes(slots),
                    slots.iter().filter(|slot| slot.is_some()).count(),
                )
            })
            .unwrap_or_default();
        ShapeCacheStats {
            bytes,
            entries,
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
        }
    }
    fn get(&self, text: &str, size: f32, rtl: Option<bool>) -> Option<V> {
        self.get_styled(text, size, rtl, 0, None)
    }
    fn get_styled(
        &self,
        text: &str,
        size: f32,
        rtl: Option<bool>,
        mode: u8,
        spec: Option<&FontSpec>,
    ) -> Option<V> {
        self.get_styled_context(text,size,rtl,mode,spec,None)
    }
    fn get_styled_context(&self,text:&str,size:f32,rtl:Option<bool>,mode:u8,spec:Option<&FontSpec>,language:Option<&str>) -> Option<V> {
        self.get_context(text,size,rtl,mode,spec,language,None).map(|(value,_)|value)
    }
    fn get_context(&self,text:&str,size:f32,rtl:Option<bool>,mode:u8,spec:Option<&FontSpec>,language:Option<&str>,source:Option<(usize,usize)>) -> Option<(V,Option<Arc<[ShapedClusterAdvance]>>)> {
        let result = if spec.is_some_and(|spec| !features_resolved(spec)) || text.len() > MAX_CACHED_TEXT_BYTES {
            None
        } else {
            let (mut hash, size, dir) = Self::key(text, size, rtl, mode, spec);
            if let Some(language)=language {hash=lumen_common::fasthash::fnv1a64(hash,language.as_bytes());}
            if let Some((start,end))=source {hash=lumen_common::fasthash::fnv1a64(hash,&start.to_le_bytes());hash=lumen_common::fasthash::fnv1a64(hash,&end.to_le_bytes());}
            self.locked(|slots| {
                let slot = slots.get(hash as usize % RUN_CACHE_SLOTS)?.as_ref()?;
                (slot.hash == hash
                    && slot.size == size
                    && slot.dir == dir
                    && slot.mode == mode
                    && slot.spec.as_ref() == spec
                    && slot.language.as_deref()==language && slot.source==source
                    && &*slot.text == text)
                    .then(|| (slot.value.clone(),slot.clusters.clone()))
            })
            .flatten()
        };
        if result.is_some() {
            self.hits.fetch_add(1, Ordering::Relaxed);
        } else {
            self.misses.fetch_add(1, Ordering::Relaxed);
        }
        result
    }
    fn insert(&self, text: &str, size: f32, rtl: Option<bool>, value: V) {
        self.insert_styled(text, size, rtl, 0, None, value);
    }
    fn insert_styled(
        &self,
        text: &str,
        size: f32,
        rtl: Option<bool>,
        mode: u8,
        spec: Option<&FontSpec>,
        value: V,
    ) {
        self.insert_styled_context(text,size,rtl,mode,spec,value,None)
    }
    fn insert_styled_context(&self,text:&str,size:f32,rtl:Option<bool>,mode:u8,spec:Option<&FontSpec>,value:V,language:Option<&str>) {
        self.insert_context(text,size,rtl,mode,spec,value,language,None,None)
    }
    fn insert_context(&self,text:&str,size:f32,rtl:Option<bool>,mode:u8,spec:Option<&FontSpec>,value:V,language:Option<&str>,source:Option<(usize,usize)>,clusters:Option<Arc<[ShapedClusterAdvance]>>) {
        if text.len() > MAX_CACHED_TEXT_BYTES {
            return;
        }
        // Charge retained font family metadata conservatively, even when Arcs are shared.
        let family_bytes = spec
            .and_then(|spec| spec.families.as_ref())
            .map_or(0, |families| {
                families.len() * core::mem::size_of::<FontFamily>()
                    + families
                        .iter()
                        .map(FontFamily::retained_bytes)
                        .sum::<usize>()
            });
        let feature_bytes = spec.and_then(|spec| spec.feature_settings.as_ref()).map_or(0, |settings|
            2 * core::mem::size_of::<usize>() + core::mem::size_of_val(settings.as_ref()) + settings.retained_bytes());
        let alternate_bytes=spec.and_then(|spec|spec.alternates.as_ref()).map_or(0,|value|value.retained_bytes());
        let scope_bytes=spec.and_then(|spec|spec.family_scope.as_ref()).map_or(0,|chain|core::mem::size_of_val(chain.as_ref()));
        let bytes = text.len() + value.cache_bytes() + family_bytes + feature_bytes + alternate_bytes + scope_bytes + language.map_or(0,str::len) + clusters.as_ref().map_or(0,|clusters|core::mem::size_of_val(clusters.as_ref()));
        let (mut hash, size, dir) = Self::key(text, size, rtl, mode, spec);
        if let Some(language)=language {hash=lumen_common::fasthash::fnv1a64(hash,language.as_bytes());}
        if let Some((start,end))=source {hash=lumen_common::fasthash::fnv1a64(hash,&start.to_le_bytes());hash=lumen_common::fasthash::fnv1a64(hash,&end.to_le_bytes());}
        self.locked(|slots| {
            if slots.is_empty() {
                if slots.try_reserve_exact(RUN_CACHE_SLOTS).is_err() {
                    return;
                }
                slots.resize_with(RUN_CACHE_SLOTS, || None);
            }
            if slots.capacity() * core::mem::size_of::<Option<RunSlot<V>>>() + bytes
                > MAX_RUN_CACHE_BYTES
            {
                return;
            }
            let index = hash as usize % RUN_CACHE_SLOTS;
            if slots[index].take().is_some() {
                self.evictions.fetch_add(1, Ordering::Relaxed);
            }
            let mut retained = Self::bytes(slots);
            if retained + bytes > MAX_RUN_CACHE_BYTES {
                for slot in slots.iter_mut() {
                    if let Some(previous) = slot.take() {
                        retained -= previous.bytes;
                        self.evictions.fetch_add(1, Ordering::Relaxed);
                    }
                    if retained + bytes <= MAX_RUN_CACHE_BYTES {
                        break;
                    }
                }
            }
            slots[index] = Some(RunSlot {
                hash,
                size,
                dir,
                mode,
                language:language.map(Arc::from),
                source, clusters,
                spec: spec.cloned(),
                text: text.into(),
                bytes,
                value,
            });
        });
    }
}

struct RasterizerEntry {
    first_glyph: u16,
    last_used: u64,
    font: fontdue::Font,
}

/// Retains a bounded number of fontdue glyph blocks. A full fontdue parse
/// reserves a glyph slot for every glyph even when only a few outlines are
/// needed, so reuse the parsed face across nearby glyph IDs and evict the
/// least recently used block when the cache is full.
struct RasterizerCache {
    busy: AtomicBool,
    clock: AtomicU64,
    entries: UnsafeCell<Vec<RasterizerEntry>>,
}

// SAFETY: entries are only read or changed while `busy` is held.
unsafe impl Sync for RasterizerCache {}

impl RasterizerCache {
    const fn new() -> Self {
        Self {
            busy: AtomicBool::new(false),
            clock: AtomicU64::new(1),
            entries: UnsafeCell::new(Vec::new()),
        }
    }

    fn with_font<R>(
        &self,
        bytes: &[u8],
        face_glyph_count: u16,
        glyph_id: u16,
        visit: impl Fn(&fontdue::Font)->R,
    ) -> Result<Option<R>, &'static str> {
        struct Release<'a>(&'a AtomicBool);
        impl Drop for Release<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }

        if self.busy.swap(true, Ordering::Acquire) {
            return Ok(None);
        }
        let _release = Release(&self.busy);
        // SAFETY: `busy` grants exclusive access until Release drops.
        let entries = unsafe { &mut *self.entries.get() };
        let first_glyph = (glyph_id / RASTERIZER_BLOCK_GLYPHS) * RASTERIZER_BLOCK_GLYPHS;
        let used = self.clock.fetch_add(1, Ordering::Relaxed);
        if let Some(index) = entries
            .iter()
            .position(|entry| entry.first_glyph == first_glyph)
        {
            let entry = &mut entries[index];
            entry.last_used = used;
            return Ok(Some(visit(&entry.font)));
        }

        let glyphs_in_block = face_glyph_count
            .saturating_sub(first_glyph)
            .min(RASTERIZER_BLOCK_GLYPHS);
        if glyphs_in_block == 0 {
            return Err("glyph id out of range");
        }
        let subset = fontdue_bytes_for_glyph_range(bytes, first_glyph, glyphs_in_block)?;
        let font = fontdue::Font::from_bytes(
            subset,
            fontdue::FontSettings {
                load_substitutions: false,
                ..fontdue::FontSettings::default()
            },
        )
        .map_err(|_| "unsupported font")?;
        if entries.len() == RASTERIZER_CACHE_BLOCKS {
            if let Some(oldest) = entries
                .iter()
                .enumerate()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(index, _)| index)
            {
                entries.remove(oldest);
            }
        }
        entries
            .try_reserve(1)
            .map_err(|_| "font allocation failed")?;
        entries.push(RasterizerEntry {
            first_glyph,
            last_used: used,
            font,
        });
        let entry = entries.last().ok_or("font allocation failed")?;
        Ok(Some(visit(&entry.font)))
    }

    #[cfg(test)]
    fn cached_blocks(&self) -> usize {
        if self.busy.swap(true, Ordering::Acquire) {
            return 0;
        }
        let count = unsafe { (&*self.entries.get()).len() };
        self.busy.store(false, Ordering::Release);
        count
    }
}

fn rasterize_coverage(font: &fontdue::Font, glyph_id: u16, size: f32) -> GlyphCoverage {
    let (metrics, alpha) = font.rasterize_indexed(glyph_id, size);
    GlyphCoverage {
        x_min: metrics.xmin,
        y_min: metrics.ymin,
        width: metrics.width,
        height: metrics.height,
        alpha,
    }
}

pub struct FontFace {
    id: u64,
    bytes: Arc<[u8]>,
    glyph_count: u16,
    invisible_id: AtomicU64,
    units_per_em: f32,
    ascent: f32,
    descent: f32,
    underline: (f32, f32),
    strike: (f32, f32),
    metrics: [Option<f32>; 6],
    primary_character_widths: Option<PrimaryCharacterWidths>,
    caps_features: u8,
    shapes: RunCache<ShapedRun>,
    widths: RunCache<f32>,
    rasterizers: RasterizerCache,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GlyphCoverage {
    pub x_min: i32,
    pub y_min: i32,
    pub width: usize,
    pub height: usize,
    pub alpha: Vec<u8>,
}

impl FontFace {
    /// Retained normalized font bytes, including WOFF expansion.
    pub fn byte_length(&self) -> usize {
        self.bytes.len()
    }

    pub fn new(bytes: Arc<[u8]>) -> Result<Self, &'static str> {
        if bytes.len() > MAX_FONT_BYTES {
            return Err("font too large");
        }
        let bytes = if bytes.starts_with(b"wOFF") {
            Arc::from(lumen_common::font::decode_woff(&bytes, MAX_FONT_BYTES)?)
        } else if bytes.starts_with(b"wOF2") {
            Arc::from(lumen_common::font::decode_woff2(&bytes, MAX_FONT_BYTES)?)
        } else {
            bytes
        };
        let face = ttf_parser::Face::parse(&bytes, 0).map_err(|_| "invalid font")?;
        let glyph_count = face.number_of_glyphs();
        let units_per_em = face.units_per_em() as f32;
        // Use the maintained table reader rather than a second font codec.
        // The normalized immutable pair is shared by every control using this face.
        let primary_character_widths = {
            use read_fonts::TableProvider;
            read_fonts::FontRef::new(&bytes).ok().and_then(|font| {
                let average = font.os2().ok()?.x_avg_char_width() as f32 / units_per_em;
                let maximum = font.hhea().ok()?.advance_width_max().to_u16() as f32 / units_per_em;
                (average >= 0.0 && maximum >= 0.0).then_some(PrimaryCharacterWidths { average, maximum })
            })
        };
        let ascent = face.ascender() as f32 / units_per_em;
        let descent = face.descender() as f32 / units_per_em;
        let underline = face.underline_metrics().map_or((0.1, 1.0 / 16.0), |m| {
            (
                -m.position as f32 / units_per_em,
                m.thickness as f32 / units_per_em,
            )
        });
        let strike = face.strikeout_metrics().map_or((-0.3, 1.0 / 16.0), |m| {
            (
                -m.position as f32 / units_per_em,
                m.thickness as f32 / units_per_em,
            )
        });
        let horizontal_advance = |character| {
            face.glyph_index(character)
                .and_then(|glyph| face.glyph_hor_advance(glyph))
                .map(|advance| advance as f32 / units_per_em)
        };
        let metrics = [
            face.x_height().map(|height| height as f32 / units_per_em),
            face.capital_height()
                .map(|height| height as f32 / units_per_em),
            horizontal_advance('0'),
            horizontal_advance('水'),
            face.glyph_index('水')
                .and_then(|glyph| face.glyph_ver_advance(glyph))
                .map(|advance| advance as f32 / units_per_em),
            face.glyph_index('0').and_then(|glyph| face.glyph_ver_advance(glyph)).map(|advance| advance as f32 / units_per_em),
        ];
        let mut caps_features = 0u8;
        if let Some(table) = face.tables().gsub {
            for (index, tag) in [b"smcp", b"c2sc", b"pcap", b"c2pc", b"unic", b"titl"]
                .into_iter()
                .enumerate()
            {
                if table
                    .features
                    .find(ttf_parser::Tag::from_bytes(tag))
                    .is_some()
                {
                    caps_features |= 1 << index;
                }
            }
        }
        if rustybuzz::Face::from_slice(&bytes, 0).is_none() {
            return Err("font cannot be shaped");
        }
        let id = NEXT_FACE_ID
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                next.checked_add(1)
            })
            .map_err(|_| "font face identity exhausted")?;
        Ok(Self {
            id,
            bytes,
            glyph_count,
            invisible_id: AtomicU64::new(0),
            units_per_em,
            ascent,
            descent,
            underline,
            strike,
            metrics,
            primary_character_widths,
            caps_features,
            shapes: RunCache::new(),
            widths: RunCache::new(),
            rasterizers: RasterizerCache::new(),
        })
    }

    /// A separate stable glyph identity prevents invisible fallback from sharing
    /// visible raster-cache entries. Immutable font data and shaping remain shared.
    pub fn invisible_key(&self)->Result<u64,&'static str> {
        let existing=self.invisible_id.load(Ordering::Relaxed);
        if existing!=0 {return Ok(existing);}
        let id=NEXT_FACE_ID.try_update(Ordering::Relaxed,Ordering::Relaxed,
            |next|next.checked_add(1)).map_err(|_|"font face identity exhausted")?;
        match self.invisible_id.compare_exchange(0,id,Ordering::Relaxed,Ordering::Relaxed) {
            Ok(_)=>Ok(id),Err(existing)=>Ok(existing),
        }
    }
    fn is_invisible_key(&self,identity:u64)->bool {
        identity!=0 && identity==self.invisible_id.load(Ordering::Relaxed)
    }

    /// Override CSS font metrics in em units without modifying glyph outlines.
    pub fn with_metric_overrides(
        mut self,
        ascent: Option<f32>,
        descent: Option<f32>,
    ) -> Result<Self, &'static str> {
        if [ascent, descent]
            .into_iter()
            .flatten()
            .any(|value| !value.is_finite() || value < 0.0)
        {
            return Err("invalid font metric override");
        }
        if let Some(value) = ascent {
            self.ascent = value;
        }
        if let Some(value) = descent {
            self.descent = -value;
        }
        Ok(self)
    }

    /// Create an independently registered variant of a shared platform face.
    /// The source bytes remain shared; its metrics and caches belong to the
    /// variant so CSS descriptors never mutate the platform fallback.
    pub fn copy_with_metric_overrides(
        &self,
        ascent: Option<f32>,
        descent: Option<f32>,
    ) -> Result<Self, &'static str> {
        Self::new(self.bytes.clone())?.with_metric_overrides(ascent, descent)
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    /// Actual cmap coverage, shared by shaping and the web-font demand policy.
    pub fn covers_cluster(&self, cluster: &str) -> bool {
        let Some(default_ignorable) = lumen_common::unicode_props::lookup("DI", None) else {
            return false;
        };
        self.covers_cluster_with_properties(cluster, None, default_ignorable)
    }

    fn covers_cluster_with_properties(
        &self,
        cluster: &str,
        ranges: Option<&[(u32, u32)]>,
        default_ignorable: &[(u32, u32)],
    ) -> bool {
        let Ok(cmap) = ttf_parser::Face::parse(&self.bytes, 0) else {
            return false;
        };
        cluster.chars().all(|ch| {
            let codepoint = ch as u32;
            ch.is_control()
                || default_ignorable
                    .binary_search_by(|&(start, end)| {
                        if codepoint < start {
                            CmpOrdering::Greater
                        } else if codepoint > end {
                            CmpOrdering::Less
                        } else {
                            CmpOrdering::Equal
                        }
                    })
                    .is_ok()
                || (ranges.is_none_or(|ranges| {
                    ranges
                        .iter()
                        .any(|&(first, last)| first <= codepoint && codepoint <= last)
                }) && cmap.glyph_index(ch).is_some_and(|glyph| glyph.0 != 0))
        })
    }

    fn metric(&self, metric: FontMetric) -> Option<f32> {
        let index = match metric {
            FontMetric::ExHeight => 0,
            FontMetric::CapHeight => 1,
            FontMetric::ChWidth => 2,
            FontMetric::IcWidth => 3,
            FontMetric::IcHeight => 4,
        };
        self.metrics[index].filter(|value| value.is_finite() && *value > 0.0)
    }

    /// Metric divided by units-per-em for use by CSS computed-value queries.
    pub fn metric_ratio(&self, metric: FontMetric) -> Option<f32> {
        self.metric(metric)
    }

    fn size_adjust_scale(&self, adjust: Option<FontSizeAdjust>, reference: &FontFace) -> f32 {
        let Some(adjust) = adjust else {
            return 1.0;
        };
        let target = match adjust.value {
            FontSizeAdjustValue::Number(number) => number,
            FontSizeAdjustValue::FromFont => {
                let Some(value) = reference.metric(adjust.metric) else {
                    return 1.0;
                };
                value
            }
        };
        if target == 0.0 {
            return 0.0;
        }
        let Some(actual) = self.metric(adjust.metric) else {
            return 1.0;
        };
        let scale = target / actual;
        if scale.is_finite() && scale >= 0.0 {
            scale
        } else {
            1.0
        }
    }

    fn relative_metrics(&self, size: f32, scale: f32) -> FontRelativeMetrics {
        FontRelativeMetrics {
            ex: size * scale * self.metric(FontMetric::ExHeight).unwrap_or(0.5),
            ch: size * scale * self.metric(FontMetric::ChWidth).unwrap_or(0.5),
        }
    }

    fn primary_character_widths(&self, size: f32, scale: f32) -> Option<PrimaryCharacterWidths> {
        let value = self.primary_character_widths?;
        let size = size * scale;
        let average = value.average * size;
        let maximum = value.maximum * size;
        (average.is_finite() && maximum.is_finite() && average >= 0.0 && maximum >= 0.0)
            .then_some(PrimaryCharacterWidths { average, maximum })
    }

    pub fn shape(&self, text: &str, size: f32) -> Result<ShapedRun, &'static str> {
        self.shape_with_direction(text, size, None)
    }

    pub fn shape_with_direction(
        &self,
        text: &str,
        size: f32,
        rtl: Option<bool>,
    ) -> Result<ShapedRun, &'static str> {
        self.shape_with_direction_and_advances(text, size, rtl, None)
    }

    fn shape_with_direction_and_advances(
        &self,
        text: &str,
        size: f32,
        rtl: Option<bool>,
        advances: Option<&mut Vec<RawClusterAdvance>>,
    ) -> Result<ShapedRun, &'static str> {
        if advances.is_none() {
            if let Some(run) = self.shapes.get(text, size, rtl) {
                return Ok(run);
            }
        }
        let run = self.shape_uncached_with_advances(text, size, rtl, advances)?;
        self.shapes.insert(text, size, rtl, run.clone());
        Ok(run)
    }

    fn shape_uncached_with_advances(
        &self,
        text: &str,
        size: f32,
        rtl: Option<bool>,
        mut advances: Option<&mut Vec<RawClusterAdvance>>,
    ) -> Result<ShapedRun, &'static str> {
        let scale = size / self.units_per_em;
        let mut glyphs = Vec::new();
        let mut x = 0.0;
        self.visit_shaped(text, size, rtl, |shaped, cluster_offset| {
            append_glyphs(
                shaped,
                scale,
                0,
                1.0,
                cluster_offset,
                &mut glyphs,
                &mut x,
                advances.as_deref_mut(),
            )
        })?;
        Ok(ShapedRun {
            glyphs: glyphs.into(),
            width: x,
        })
    }

    pub fn measure(&self, text: &str, size: f32) -> Result<f32, &'static str> {
        if let Some(width) = self.widths.get(text, size, None) {
            return Ok(width);
        }
        let width = self.measure_uncached(text, size)?;
        self.widths.insert(text, size, None, width);
        Ok(width)
    }

    fn measure_uncached(&self, text: &str, size: f32) -> Result<f32, &'static str> {
        let scale = size / self.units_per_em;
        let mut width = 0.0;
        self.visit_shaped(text, size, None, |shaped, _cluster_offset| {
            width += shaped
                .glyph_positions()
                .iter()
                .map(|position| position.x_advance as f32 * scale)
                .sum::<f32>();
            Ok(())
        })?;
        Ok(width)
    }

    fn shape_script_into(
        &self,
        text: &str,
        size: f32,
        rtl: bool,
        script: ScriptSelection,
        glyph_face: u64,
        size_scale: f32,
        cluster_offset: usize,
        features: &[rustybuzz::Feature],
        glyphs: &mut Vec<Glyph>,
        x: &mut f32,
        advances: Option<&mut Vec<RawClusterAdvance>>,
    ) -> Result<(), &'static str> {
        self.shape_script_into_context(text,size,rtl,script,glyph_face,size_scale,cluster_offset,features,glyphs,x,advances,ShapeContext::language(None))
    }

    fn shape_script_into_context(&self,text:&str,size:f32,rtl:bool,script:ScriptSelection,glyph_face:u64,
        size_scale:f32,cluster_offset:usize,features:&[rustybuzz::Feature],glyphs:&mut Vec<Glyph>,x:&mut f32,
        advances:Option<&mut Vec<RawClusterAdvance>>,context:ShapeContext<'_>) -> Result<(), &'static str> {
        let shaped = shape_buffer_context(&self.bytes, text, rtl, script, features,context)?;
        append_glyphs(
            shaped,
            size / self.units_per_em,
            glyph_face,
            size_scale,
            cluster_offset,
            glyphs,
            x,
            advances,
        )
    }

    fn shape_caps_into(
        &self,
        text: &str,
        size: f32,
        rtl: bool,
        script: ScriptSelection,
        glyph_face: u64,
        size_scale: f32,
        cluster_offset: usize,
        spec: &FontSpec,
        features: &[rustybuzz::Feature],
        glyphs: &mut Vec<Glyph>,
        x: &mut f32,
        advances: Option<&mut Vec<RawClusterAdvance>>,
    ) -> Result<(), &'static str> {
        self.shape_caps_into_context(text,size,rtl,script,glyph_face,size_scale,cluster_offset,spec,features,glyphs,x,advances,ShapeContext::language(None))
    }

    fn shape_caps_into_context(&self,text:&str,size:f32,rtl:bool,script:ScriptSelection,glyph_face:u64,
        size_scale:f32,cluster_offset:usize,spec:&FontSpec,features:&[rustybuzz::Feature],glyphs:&mut Vec<Glyph>,
        x:&mut f32,mut advances:Option<&mut Vec<RawClusterAdvance>>,context:ShapeContext<'_>) -> Result<(), &'static str> {
        let language = context.language;
        use CanvasFontVariantCaps as Caps;
        let required = match spec.caps {
            Caps::SmallCaps => 1,
            Caps::AllSmallCaps => 3,
            Caps::PetiteCaps => 4,
            Caps::AllPetiteCaps => 12,
            Caps::Unicase => 16,
            Caps::TitlingCaps => 32,
            Caps::Normal => 0,
        };
        if required == 0 || matches!(spec.caps, Caps::Unicase | Caps::TitlingCaps) {
            return self.shape_script_into_context(
                text,
                size,
                rtl,
                script,
                glyph_face,
                size_scale,
                cluster_offset,
                features,
                glyphs,
                x,
                advances,
                context,
            );
        }
        let all = matches!(spec.caps, Caps::AllSmallCaps | Caps::AllPetiteCaps);
        let petite = matches!(spec.caps, Caps::PetiteCaps | Caps::AllPetiteCaps);
        let lower_native = self.caps_features & if petite { 4 | 1 } else { 1 } != 0;
        let upper_native = self.caps_features & if petite { 8 | 2 } else { 2 } != 0;
        let mut fallback_features = Vec::new();
        let features = if petite
            && (self.caps_features & 4 == 0 && self.caps_features & 1 != 0
                || all && self.caps_features & 8 == 0 && self.caps_features & 2 != 0)
        {
            fallback_features.extend_from_slice(features);
            if self.caps_features & 4 == 0 && self.caps_features & 1 != 0 && explicit_feature(spec, b"pcap") != Some(0) {
                fallback_features.push(rustybuzz::Feature::new(
                    ttf_parser::Tag::from_bytes(b"smcp"),
                    1,
                    ..,
                ));
            }
            if all && self.caps_features & 8 == 0 && self.caps_features & 2 != 0 && explicit_feature(spec, b"c2pc") != Some(0) {
                fallback_features.push(rustybuzz::Feature::new(
                    ttf_parser::Tag::from_bytes(b"c2sc"),
                    1,
                    ..,
                ));
            }
            append_feature_settings(&mut fallback_features, spec.feature_settings.as_deref());
            fallback_features.as_slice()
        } else {
            features
        };
        let synth_lower = !lower_native && spec.synthesize_small_caps && explicit_feature(spec, if petite { b"pcap" } else { b"smcp" }) != Some(0);
        let synth_upper = all && !upper_native && spec.synthesize_small_caps && explicit_feature(spec, if petite { b"c2pc" } else { b"c2sc" }) != Some(0);
        if !text
            .chars()
            .any(|ch| synth_lower && ch.is_lowercase() || synth_upper && ch.is_uppercase())
        {
            return self.shape_script_into_context(
                text,
                size,
                rtl,
                script,
                glyph_face,
                size_scale,
                cluster_offset,
                features,
                glyphs,
                x,
                advances,
                context,
            );
        }
        let mut transformed = String::new();
        transformed
            .try_reserve(text.len())
            .map_err(|_| "caps allocation failed")?;
        let mut clusters = Vec::new();
        for (original, cluster) in graphemes(text) {
            let reduced = cluster
                .chars()
                .any(|ch| synth_lower && ch.is_lowercase() || synth_upper && ch.is_uppercase());
            let start = transformed.len();
            if reduced {
                lumen_common::case_transform::visit_case_range(text,original..original+cluster.len(),lumen_common::case_transform::CaseTransform::Uppercase,language,|upper,_| {
                        if transformed.len().saturating_add(upper.len_utf8()) > MAX_SHAPE_TEXT_BYTES
                        {
                            return Err("caps text run too large");
                        }
                        transformed
                            .try_reserve(upper.len_utf8())
                            .map_err(|_| "caps allocation failed")?;
                        transformed.push(upper);
                        Ok(())
                })?;
            } else {
                    if transformed.len().saturating_add(cluster.len()) > MAX_SHAPE_TEXT_BYTES {
                        return Err("caps text run too large");
                    }
                    transformed
                        .try_reserve(cluster.len())
                        .map_err(|_| "caps allocation failed")?;
                    transformed.push_str(cluster);
            }
            for (expansion, (offset, _)) in graphemes(&transformed[start..]).enumerate() {
                clusters
                    .try_reserve(1)
                    .map_err(|_| "caps allocation failed")?;
                clusters.push((
                    u32::try_from(start + offset).map_err(|_| "caps cluster overflow")?,
                    u32::try_from(cluster_offset + original)
                        .map_err(|_| "caps cluster overflow")?,
                    u8::try_from(expansion).map_err(|_| "caps expansion too large")?,
                    if reduced { 0.8f32 } else { 1.0 },
                ));
            }
        }
        // Native caps must not be applied a second time to letters which were
        // already synthesized as reduced uppercase glyphs.
        let mut synthetic_features = Vec::new();
        let features = if self.caps_features & 15 != 0 {
            synthetic_features.extend_from_slice(features);
            let mut index = 0;
            while index < clusters.len() {
                if clusters[index].3 == 1.0 {
                    index += 1;
                    continue;
                }
                let start = clusters[index].0;
                while index < clusters.len() && clusters[index].3 != 1.0 {
                    index += 1;
                }
                let end = match clusters.get(index) {
                    Some(cluster) => cluster.0,
                    None => {
                        u32::try_from(transformed.len()).map_err(|_| "caps cluster overflow")?
                    }
                };
                for tag in [b"smcp", b"c2sc", b"pcap", b"c2pc"] {
                    synthetic_features
                        .try_reserve(1)
                        .map_err(|_| "caps allocation failed")?;
                    // Rustybuzz 0.20's range constructor subtracts one from an
                    // excluded end, but its mask application uses an exclusive
                    // cluster end. Supply the public byte bounds directly so
                    // a one-byte synthetic interval is not silently empty.
                    synthetic_features.push(rustybuzz::Feature {
                        tag: ttf_parser::Tag::from_bytes(tag),
                        value: 0,
                        start,
                        end,
                    });
                }
            }
            synthetic_features.as_slice()
        } else {
            features
        };
        let shaped = shape_buffer_context(&self.bytes, &transformed, rtl, script, features,context)?;
        if glyphs.len().saturating_add(shaped.len()) > MAX_SHAPE_TEXT_BYTES {
            return Err("caps glyph run too large");
        }
        glyphs
            .try_reserve(shaped.len())
            .map_err(|_| "glyph allocation failed")?;
        if let Some(advances) = advances.as_deref_mut() {
            if advances.len().saturating_add(shaped.len()) > MAX_SHAPE_TEXT_BYTES {
                return Err("cluster advance limit exceeded");
            }
            advances
                .try_reserve(shaped.len())
                .map_err(|_| "cluster advance allocation failed")?;
        }
        for (info, position) in shaped.glyph_infos().iter().zip(shaped.glyph_positions()) {
            let index = clusters
                .partition_point(|cluster| cluster.0 <= info.cluster)
                .checked_sub(1)
                .ok_or("caps cluster missing")?;
            let (_, cluster, caps_expansion, reduced) = clusters[index];
            let scale = size / self.units_per_em * reduced;
            let advance = position.x_advance as f32 * scale;
            glyphs.push(Glyph {
                id: u16::try_from(info.glyph_id).map_err(|_| "glyph id out of range")?,
                face: glyph_face,
                cluster,
                caps_expansion,
                x: *x + position.x_offset as f32 * scale,
                y: position.y_offset as f32 * scale,
                size_scale: size_scale * reduced,
            });
            *x += advance;
            if let Some(advances) = advances.as_deref_mut() {
                advances.push(RawClusterAdvance {
                    source_start: cluster as usize,
                    advance,
                });
            }
        }
        Ok(())
    }

    fn visit_shaped(
        &self,
        text: &str,
        size: f32,
        rtl: Option<bool>,
        mut visit: impl FnMut(rustybuzz::GlyphBuffer, usize) -> Result<(), &'static str>,
    ) -> Result<(), &'static str> {
        self.visit_shaped_with_features(text, size, rtl, &[], &mut visit)
    }

    fn visit_shaped_with_features(
        &self,
        text: &str,
        size: f32,
        rtl: Option<bool>,
        features: &[rustybuzz::Feature],
        mut visit: impl FnMut(rustybuzz::GlyphBuffer, usize) -> Result<(), &'static str>,
    ) -> Result<(), &'static str> {
        validate_shape_input(text, size)?;
        if text.is_ascii() && rtl != Some(true) {
            return visit(
                shape_buffer(&self.bytes, text, false, Script::Latin.into(), features)?,
                0,
            );
        }
        let info = bidi::resolve(text, rtl)?;
        for paragraph in &info.paragraphs {
            let (levels, runs) = bidi::shaping_runs(&info, paragraph, paragraph.range.clone())?;
            for range in runs {
                let rtl = levels[range.start].is_rtl();
                let value = &text[range.clone()];
                for item in script_ranges(value, rtl)? {
                    visit(
                        shape_buffer(
                            &self.bytes,
                            &value[item.start..item.end],
                            rtl,
                            item.script,
                            features,
                        )?,
                        range.start + item.start,
                    )?;
                }
            }
        }
        Ok(())
    }

    fn shape_resolved_run_with_advances(
        &self,
        text: &str,
        size: f32,
        rtl: bool,
        mut advances: Option<&mut Vec<RawClusterAdvance>>,
    ) -> Result<ShapedRun, &'static str> {
        validate_shape_input(text, size)?;
        if advances.is_none() {
            if let Some(run) = self.shapes.get_styled(text, size, Some(rtl), 2, None) {
                return Ok(run);
            }
        }
        let mut glyphs = Vec::new();
        let mut x = 0.0;
        for item in script_ranges(text, rtl)? {
            self.shape_script_into(
                &text[item.start..item.end],
                size,
                rtl,
                item.script,
                0,
                1.0,
                item.start,
                &[],
                &mut glyphs,
                &mut x,
                advances.as_deref_mut(),
            )?;
        }
        let run = ShapedRun {
            glyphs: glyphs.into(),
            width: x,
        };
        self.shapes
            .insert_styled(text, size, Some(rtl), 2, None, run.clone());
        Ok(run)
    }

    fn shape_styled_adjusted(
        &self,
        text: &str,
        size: f32,
        rtl: bool,
        spec: &FontSpec,
        resolved: bool,
    ) -> Result<ShapedRun, &'static str> {
        self.shape_styled_adjusted_with_advances(text, size, rtl, spec, resolved, None)
    }

    fn shape_styled_adjusted_with_advances(
        &self,
        text: &str,
        size: f32,
        rtl: bool,
        spec: &FontSpec,
        resolved: bool,
        mut advances: Option<&mut Vec<RawClusterAdvance>>,
    ) -> Result<ShapedRun, &'static str> {
        if !features_resolved(spec) { return Err("font style requires query-container context"); }
        if spec.size_adjust.is_none() && spec.caps == CanvasFontVariantCaps::Normal && spec.ligatures.is_normal() && spec.feature_settings.is_none() && spec.alternates.is_none() && !spec.disable_optional_ligatures {
            return if resolved {
                self.shape_resolved_run_with_advances(text, size, rtl, advances)
            } else if advances.is_some() {
                self.shape_with_direction_and_advances(text, size, Some(rtl), advances)
            } else {
                self.shape_with_direction(text, size, Some(rtl))
            };
        }
        validate_shape_input(text, size)?;
        let mode = if resolved { 4 } else { 3 };
        if advances.is_none() {
            if let Some(run) = self
                .shapes
                .get_styled(text, size, Some(rtl), mode, Some(spec))
            {
                return Ok(run);
            }
        }
        let size_scale = self.size_adjust_scale(spec.size_adjust, self);
        let run = if size_scale == 0.0 {
            ShapedRun {
                glyphs: Arc::from([]),
                width: 0.0,
            }
        } else {
            let mut run = if spec.caps != CanvasFontVariantCaps::Normal || !spec.ligatures.is_normal() || spec.feature_settings.is_some() || spec.alternates.is_some() || spec.disable_optional_ligatures {
                self.shape_caps_run_with_advances(
                    text,
                    size,
                    rtl,
                    spec,
                    resolved,
                    &font_features(spec),
                    advances.as_deref_mut(),
                )?
            } else if resolved {
                self.shape_resolved_run_with_advances(
                    text,
                    size * size_scale,
                    rtl,
                    advances.as_deref_mut(),
                )?
            } else if advances.is_some() {
                self.shape_with_direction_and_advances(
                    text,
                    size * size_scale,
                    Some(rtl),
                    advances.as_deref_mut(),
                )?
            } else {
                self.shape_with_direction(text, size * size_scale, Some(rtl))?
            };
            if size_scale != 1.0 && spec.caps == CanvasFontVariantCaps::Normal {
                let mut glyphs = run.glyphs.to_vec();
                for glyph in &mut glyphs {
                    glyph.size_scale *= size_scale;
                }
                run.glyphs = glyphs.into();
            }
            run
        };
        self.shapes
            .insert_styled(text, size, Some(rtl), mode, Some(spec), run.clone());
        Ok(run)
    }

    fn shape_caps_run(
        &self,
        text: &str,
        size: f32,
        rtl: bool,
        spec: &FontSpec,
        resolved: bool,
        features: &[rustybuzz::Feature],
    ) -> Result<ShapedRun, &'static str> {
        self.shape_caps_run_with_advances(text, size, rtl, spec, resolved, features, None)
    }

    fn shape_caps_run_with_advances(
        &self,
        text: &str,
        size: f32,
        rtl: bool,
        spec: &FontSpec,
        resolved: bool,
        features: &[rustybuzz::Feature],
        mut advances: Option<&mut Vec<RawClusterAdvance>>,
    ) -> Result<ShapedRun, &'static str> {
        validate_shape_input(text, size)?;
        let size_scale = self.size_adjust_scale(spec.size_adjust, self);
        let mut glyphs = Vec::new();
        let mut x = 0.0;
        if size_scale == 0.0 {
            return Ok(ShapedRun {
                glyphs: glyphs.into(),
                width: x,
            });
        }
        let mut append = |text: &str, offset: usize, rtl| {
            for range in script_ranges(text, rtl)? {
                self.shape_caps_into(
                    &text[range.start..range.end],
                    size * size_scale,
                    rtl,
                    range.script,
                    self.id,
                    size_scale,
                    offset + range.start,
                    spec,
                    features,
                    &mut glyphs,
                    &mut x,
                    advances.as_deref_mut(),
                )?;
            }
            Ok::<_, &'static str>(())
        };
        if !resolved {
            let info = bidi::resolve(text, Some(rtl))?;
            for paragraph in &info.paragraphs {
                let (levels, runs) = bidi::shaping_runs(&info, paragraph, paragraph.range.clone())?;
                for range in runs {
                    append(
                        &text[range.clone()],
                        range.start,
                        levels[range.start].is_rtl(),
                    )?;
                }
            }
        } else {
            append(text, 0, rtl)?;
        }
        Ok(ShapedRun {
            glyphs: glyphs.into(),
            width: x,
        })
    }

    pub fn line_height(&self, size: f32) -> f32 {
        (self.ascent - self.descent) * size
    }
    pub fn ascent(&self, size: f32) -> f32 {
        self.ascent * size
    }

    pub fn rasterize(&self, id: u16, size: f32) -> Result<GlyphCoverage, &'static str> {
        self.with_rasterizer(id,size,|font|rasterize_coverage(font,id,size))
    }

    fn with_rasterizer<R>(&self,id:u16,size:f32,visit:impl Fn(&fontdue::Font)->R) -> Result<R,&'static str> {
        if !size.is_finite() || size <= 0.0 || size > 512.0 {
            return Err("invalid font size");
        }
        if id >= self.glyph_count {
            return Err("glyph id out of range");
        }
        if let Some(coverage) =
            self.rasterizers
                .with_font(&self.bytes, self.glyph_count, id, &visit)?
        {
            return Ok(coverage);
        }
        // Another thread is building or using the bounded block cache. Treat
        // contention as a cache miss so rasterization remains nonblocking.
        let bytes = fontdue_bytes_for_glyph_range(&self.bytes, id, 1)?;
        let rasterizer = fontdue::Font::from_bytes(
            bytes,
            fontdue::FontSettings {
                load_substitutions: false,
                ..fontdue::FontSettings::default()
            },
        )
        .map_err(|_| "unsupported font")?;
        Ok(visit(&rasterizer))
    }

    pub fn outline(&self, id: u16, size: f32) -> Result<GlyphOutline, &'static str> {
        if !size.is_finite() || size <= 0.0 || size > 512.0 {
            return Err("invalid font size");
        }
        let mut builder = GlyphOutlineBuilder {
            commands: Vec::new(),
            overflowed: false,
        };
        let face = ttf_parser::Face::parse(&self.bytes, 0).map_err(|_| "invalid font")?;
        let _bounds = face.outline_glyph(ttf_parser::GlyphId(id), &mut builder);
        if builder.overflowed {
            return Err("glyph outline exceeds the command limit");
        }
        let scale = size / self.units_per_em;
        for command in &mut builder.commands {
            let point = |x: &mut f32, y: &mut f32| {
                *x *= scale;
                *y *= scale;
            };
            match command {
                GlyphOutlineCommand::MoveTo(x, y) | GlyphOutlineCommand::LineTo(x, y) => {
                    point(x, y);
                }
                GlyphOutlineCommand::QuadTo(x1, y1, x, y) => {
                    point(x1, y1);
                    point(x, y);
                }
                GlyphOutlineCommand::CurveTo(x1, y1, x2, y2, x, y) => {
                    point(x1, y1);
                    point(x2, y2);
                    point(x, y);
                }
                GlyphOutlineCommand::Close => {}
            }
        }
        Ok(GlyphOutline {
            commands: builder.commands,
        })
    }
}

impl TextShaper for FontFace {
    fn first_available_font_metric(&self, _font: &FontSpec, metric: FontMetric) -> Option<Option<f32>> {
        Some(self.metric_ratio(metric))
    }
    fn primary_character_widths_styled(&self, size: f32, font: &FontSpec) -> Option<PrimaryCharacterWidths> {
        self.primary_character_widths(size, self.size_adjust_scale(font.size_adjust, self))
    }
    fn glyph_ink_bounds(&self,glyph:&Glyph,size:f32) -> Option<lumen_html::paint::Rect> {
        if glyph.face!=0 && glyph.face!=self.id {return None;}
        self.with_rasterizer(glyph.id,size*glyph.size_scale,|font| {
            let metrics=font.metrics_indexed(glyph.id,size*glyph.size_scale);
            lumen_html::paint::Rect{x:glyph.x+metrics.xmin as f32,y:glyph.y-metrics.ymin as f32-metrics.height as f32,width:metrics.width as f32,height:metrics.height as f32}
        }).ok()
    }
    fn shape_styled_context(&self,text:&str,size:f32,rtl:bool,font:&FontSpec,language:Option<&str>) -> Result<ShapedRun,()> {
        let language=language.filter(|language|!language.is_empty());
        if language.is_none(){return self.shape_styled(text,size,rtl,font);}
        if let Some(run)=self.shapes.get_styled_context(text,size,Some(rtl),6,Some(font),language){return Ok(run);}
        let run=shape_bidi_context(self,text,size,rtl,font,language)?;
        self.shapes.insert_styled_context(text,size,Some(rtl),6,Some(font),run.clone(),language);Ok(run)
    }
    fn shape_resolved_context(&self,text:&str,size:f32,rtl:bool,font:&FontSpec,language:Option<&str>) -> Result<ShapedRun,()> {
        let language=language.filter(|language|!language.is_empty());
        if language.is_none(){return self.shape_resolved(text,size,rtl,font);}
        if let Some(run)=self.shapes.get_styled_context(text,size,Some(rtl),5,Some(font),language){return Ok(run);}
        let run=self.shape_resolved_context_with_cluster_advances(text,size,rtl,font,language)?.ok_or(())?.run;
        self.shapes.insert_styled_context(text,size,Some(rtl),5,Some(font),run.clone(),language);Ok(run)
    }
    fn shape_resolved_context_with_cluster_advances(&self,text:&str,size:f32,rtl:bool,font:&FontSpec,language:Option<&str>) -> Result<Option<ShapedRunWithClusterAdvances>,()> {
    if language.is_none_or(str::is_empty) {return self.shape_resolved_with_cluster_advances(text,size,rtl,font);}
    self.shape_resolved_segment_with_cluster_advances(text,0..text.len(),size,rtl,font,language)
}
fn shape_resolved_segment_with_cluster_advances(&self,full:&str,source:core::ops::Range<usize>,size:f32,rtl:bool,font:&FontSpec,language:Option<&str>) -> Result<Option<ShapedRunWithClusterAdvances>,()> {
let _=full.get(source.clone()).ok_or(())?;
// Rustybuzz retains at most five scalar values on either side. Keep
// exactly that surrounding identity in the existing bounded cache.
let before=full[..source.start].char_indices().rev().take(5).last().map_or(source.start,|(at,_)|at);
let after=full[source.end..].char_indices().nth(5).map_or(full.len(),|(at,_)|source.end+at);
let full=&full[before..after];
let source=source.start-before..source.end-before;
let text=&full[source.clone()];

    let context=ShapeContext {language,surrounding:Some(full),start:source.start,end:source.end};
let language=language.filter(|value|!value.is_empty());
let key=Some((source.start,source.end));
if let Some((run,Some(clusters)))=self.shapes.get_context(full,size,Some(rtl),7,Some(font),language,key) {
    return Ok(Some(ShapedRunWithClusterAdvances{run,clusters}));
}

        validate_shape_input(text,size).map_err(|_|())?;
        if !features_resolved(font) {return Err(());}
        let features=font_features(font);
        let scale=self.size_adjust_scale(font.size_adjust,self);
        let mut glyphs=Vec::new();let mut width=0.0;let mut raw=Vec::new();
        if scale!=0.0 {
            for range in script_ranges(text,rtl).map_err(|_|())? {
                self.shape_caps_into_context(&text[range.start..range.end],size*scale,rtl,range.script,self.id,scale,range.start,font,&features,&mut glyphs,&mut width,Some(&mut raw),context.subrange(range.start,range.end)).map_err(|_|())?;
            }
        }
        let clusters=finish_cluster_advances(text,raw).map_err(|_|())?;
        // Context, language and local source range share the existing bounded
        // cache while ordinary language-neutral runs keep their old keys.
        let run=ShapedRun{glyphs:glyphs.into(),width};
        self.shapes.insert_context(full,size,Some(rtl),7,Some(font),run.clone(),language,key,Some(clusters.clone()));
        Ok(Some(ShapedRunWithClusterAdvances {run,clusters}))
    }
    fn generation(&self) -> u64 {
        self.id
    }

    fn shape_directional(&self, text: &str, size: f32, rtl: bool) -> Result<ShapedRun, ()> {
        self.shape_with_direction(text, size, Some(rtl))
            .map_err(|_| ())
    }
    fn underline_metrics(&self, size: f32) -> (f32, f32) {
        (self.underline.0 * size, (self.underline.1 * size).max(1.0))
    }
    fn strike_metrics(&self, size: f32) -> (f32, f32) {
        (self.strike.0 * size, (self.strike.1 * size).max(1.0))
    }
    fn shape(&self, text: &str, size: f32) -> Result<ShapedRun, ()> {
        FontFace::shape(self, text, size).map_err(|_| ())
    }
    fn shape_styled(
        &self,
        text: &str,
        size: f32,
        rtl: bool,
        font: &FontSpec,
    ) -> Result<ShapedRun, ()> {
        self.shape_styled_adjusted(text, size, rtl, font, false)
            .map_err(|_| ())
    }
    fn shape_styled_with_cluster_advances(
        &self,
        text: &str,
        size: f32,
        rtl: bool,
        font: &FontSpec,
    ) -> Result<Option<ShapedRunWithClusterAdvances>, ()> {
        let mut raw = Vec::new();
        let run = self
            .shape_styled_adjusted_with_advances(text, size, rtl, font, false, Some(&mut raw))
            .map_err(|_| ())?;
        let clusters = finish_cluster_advances(text, raw).map_err(|_| ())?;
        Ok(Some(ShapedRunWithClusterAdvances { run, clusters }))
    }
    fn shape_resolved(
        &self,
        text: &str,
        size: f32,
        rtl: bool,
        font: &FontSpec,
    ) -> Result<ShapedRun, ()> {
        self.shape_styled_adjusted(text, size, rtl, font, true)
            .map_err(|_| ())
    }
    fn shape_resolved_with_cluster_advances(
        &self,
        text: &str,
        size: f32,
        rtl: bool,
        font: &FontSpec,
    ) -> Result<Option<ShapedRunWithClusterAdvances>, ()> {
        let mut raw = Vec::new();
        let run = self
            .shape_styled_adjusted_with_advances(text, size, rtl, font, true, Some(&mut raw))
            .map_err(|_| ())?;
        let clusters = finish_cluster_advances(text, raw).map_err(|_| ())?;
        Ok(Some(ShapedRunWithClusterAdvances { run, clusters }))
    }
    fn measure(&self, text: &str, size: f32) -> Result<f32, ()> {
        FontFace::measure(self, text, size).map_err(|_| ())
    }
    fn ascent(&self, size: f32) -> f32 {
        FontFace::ascent(self, size)
    }
    fn line_height(&self, size: f32) -> f32 {
        FontFace::line_height(self, size)
    }

    fn ascent_styled(&self, size: f32, font: &FontSpec) -> f32 {
        FontFace::ascent(self, size * self.size_adjust_scale(font.size_adjust, self))
    }

    fn line_height_styled(&self, size: f32, font: &FontSpec) -> f32 {
        FontFace::line_height(self, size * self.size_adjust_scale(font.size_adjust, self))
    }

    fn underline_metrics_styled(&self, size: f32, font: &FontSpec) -> (f32, f32) {
        let size = size * self.size_adjust_scale(font.size_adjust, self);
        self.underline_metrics(size)
    }

    fn strike_metrics_styled(&self, size: f32, font: &FontSpec) -> (f32, f32) {
        let size = size * self.size_adjust_scale(font.size_adjust, self);
        self.strike_metrics(size)
    }

    fn font_unit_metrics_styled(&self, size: f32, font: &FontSpec, vertical: bool, upright_zero: bool) -> lumen_html::paint::FontUnitMetrics {
        let scale = size * self.size_adjust_scale(font.size_adjust, self);
        let scaled = |ratio: Option<f32>| ratio.map(|value| value * scale).filter(|value| value.is_finite());
        lumen_html::paint::FontUnitMetrics {
            cap: scaled(self.metric(FontMetric::CapHeight)),
            ch: scaled(self.metrics[if upright_zero { 5 } else { 2 }]),
            ic: scaled(self.metrics[if vertical { 4 } else { 3 }]),
        }
    }

    fn font_relative_metrics_styled(&self, size: f32, font: &FontSpec) -> FontRelativeMetrics {
        self.relative_metrics(size, self.size_adjust_scale(font.size_adjust, self))
    }
}

impl FontProvider for FontFace {
    fn first_available_metric(&self,_font:&FontSpec,metric:FontMetric)->Option<f32> {self.metric_ratio(metric)}
    fn shape_canvas_text(
        &self,
        text: &str,
        size: f32,
        rtl: bool,
        font: &FontSpec,
        options: &CanvasTextOptions,
    ) -> Result<ShapedRun, &'static str> {
        let mut features = canvas_features(options);
        append_ligature_features(&mut features, font.ligatures);
        if options.letter_spacing != 0.0 || options.text_rendering == CanvasTextRendering::OptimizeSpeed { disable_optional_ligatures(&mut features); }
        append_feature_settings(&mut features, font.feature_settings.as_deref());
        if !features_resolved(font) { return Err("font feature settings require query-container context"); }
        let mut font = font.clone();
        font.caps = options.font_variant_caps;
        if font.caps != CanvasFontVariantCaps::Normal {
            return self
                .shape_caps_run(text, size, rtl, &font, false, &features)
                .map(|run| apply_canvas_spacing(text, run, options));
        }
        let mut glyphs = Vec::new();
        let mut width = 0.0;
        self.visit_shaped_with_features(
            text,
            size,
            Some(rtl),
            &features,
            |shaped, cluster_offset| {
                append_glyphs(
                    shaped,
                    size / self.units_per_em,
                    self.id,
                    1.0,
                    cluster_offset,
                    &mut glyphs,
                    &mut width,
                    None,
                )
            },
        )?;
        Ok(apply_canvas_spacing(
            text,
            ShapedRun {
                glyphs: glyphs.into(),
                width,
            },
            options,
        ))
    }

    fn shape_cache_stats(&self) -> ShapeCacheStats {
        self.shapes.stats().add(self.widths.stats())
    }
    fn rasterize_glyph(
        &self,
        face: u64,
        id: u16,
        size: f32,
    ) -> Result<GlyphCoverage, &'static str> {
        if self.is_invisible_key(face) {
            if !size.is_finite() || size<=0.0 || size>512.0 {return Err("invalid font size");}
            if id>=self.glyph_count {return Err("glyph id out of range");}
            return Ok(GlyphCoverage {x_min:0,y_min:0,width:0,height:0,alpha:Vec::new()});
        }
        if face != 0 && face != self.id {
            return Err("font face identity mismatch");
        }
        self.rasterize(id, size)
    }

    fn glyph_cell_bounds(&self,face:u64,id:u16,size:f32)->Option<lumen_html::paint::Rect> {
        if face!=0 && face!=self.id && !self.is_invisible_key(face){return None;}
        self.with_rasterizer(id,size,|font| {
            let metrics=font.metrics_indexed(id,size);
            let line=font.horizontal_line_metrics(size)?;
            Some(lumen_html::paint::Rect{x:0.0,y:-line.ascent,width:metrics.advance_width,height:line.ascent-line.descent})
        }).ok().flatten()
    }

    fn outline_glyph(&self, face: u64, id: u16, size: f32) -> Result<GlyphOutline, &'static str> {
        if self.is_invisible_key(face) {
            if !size.is_finite() || size<=0.0 || size>512.0 {return Err("invalid font size");}
            if id>=self.glyph_count {return Err("glyph id out of range");}
            return Ok(GlyphOutline {commands:Vec::new()});
        }
        if face != 0 && face != self.id {
            return Err("font face identity mismatch");
        }
        self.outline(id, size)
    }

    fn face_key(&self, face: u64) -> Result<u64, &'static str> {
        if self.is_invisible_key(face) {Ok(face)} else if face == 0 || face == self.id {
            Ok(self.id)
        } else {
            Err("font face identity mismatch")
        }
    }
}

impl TextShaper for FontSet {
    fn first_available_font_metric(&self, font: &FontSpec, metric: FontMetric) -> Option<Option<f32>> {
        Some(self.first_available_metric(font, metric))
    }
    fn primary_character_widths_styled(&self, size: f32, font: &FontSpec) -> Option<PrimaryCharacterWidths> {
        let index = self.metric_face_index(font);
        self.faces[index].face.primary_character_widths(size, self.size_adjust_scale(font, index))
    }
    fn glyph_ink_bounds(&self,glyph:&Glyph,size:f32) -> Option<lumen_html::paint::Rect> {
        self.face_by_identity(glyph.face)?.glyph_ink_bounds(glyph,size)
    }
    fn shape_styled_context(&self,text:&str,size:f32,rtl:bool,font:&FontSpec,language:Option<&str>) -> Result<ShapedRun,()> {
        let language=language.filter(|language|!language.is_empty());
        if language.is_none(){return self.shape_styled(text,size,rtl,font);}
        if let Some(run)=self.shapes.get_styled_context(text,size,Some(rtl),6,Some(font),language){return Ok(run);}
        let run=shape_bidi_context(self,text,size,rtl,font,language)?;
        self.shapes.insert_styled_context(text,size,Some(rtl),6,Some(font),run.clone(),language);Ok(run)
    }
    fn shape_resolved_context(&self,text:&str,size:f32,rtl:bool,font:&FontSpec,language:Option<&str>) -> Result<ShapedRun,()> {
        let language=language.filter(|language|!language.is_empty());
        if language.is_none(){return self.shape_resolved(text,size,rtl,font);}
        if let Some(run)=self.shapes.get_styled_context(text,size,Some(rtl),5,Some(font),language){return Ok(run);}
        let run=self.shape_resolved_context_with_cluster_advances(text,size,rtl,font,language)?.ok_or(())?.run;
        self.shapes.insert_styled_context(text,size,Some(rtl),5,Some(font),run.clone(),language);Ok(run)
    }
    fn shape_resolved_context_with_cluster_advances(&self,text:&str,size:f32,rtl:bool,font:&FontSpec,language:Option<&str>) -> Result<Option<ShapedRunWithClusterAdvances>,()> {
    if language.is_none_or(str::is_empty) {return self.shape_resolved_with_cluster_advances(text,size,rtl,font);}
    self.shape_resolved_segment_with_cluster_advances(text,0..text.len(),size,rtl,font,language)
}
fn shape_resolved_segment_with_cluster_advances(&self,full:&str,source:core::ops::Range<usize>,size:f32,rtl:bool,font:&FontSpec,language:Option<&str>) -> Result<Option<ShapedRunWithClusterAdvances>,()> {
let _=full.get(source.clone()).ok_or(())?;
// Rustybuzz retains at most five scalar values on either side. Keep
// exactly that surrounding identity in the existing bounded cache.
let before=full[..source.start].char_indices().rev().take(5).last().map_or(source.start,|(at,_)|at);
let after=full[source.end..].char_indices().nth(5).map_or(full.len(),|(at,_)|source.end+at);
let full=&full[before..after];
let source=source.start-before..source.end-before;
let text=&full[source.clone()];

    let context=ShapeContext {language,surrounding:Some(full),start:source.start,end:source.end};
let language=language.filter(|value|!value.is_empty());
let key=Some((source.start,source.end));
if let Some((run,Some(clusters)))=self.shapes.get_context(full,size,Some(rtl),7,Some(font),language,key) {
    return Ok(Some(ShapedRunWithClusterAdvances{run,clusters}));
}

        validate_shape_input(text,size).map_err(|_|())?;
        if !features_resolved(font) {return Err(());}
        let order=self.face_order(font).map_err(|_|())?;let features=font_features(font);
        let mut glyphs=Vec::new();let mut width=0.0;let mut raw=Vec::new();
        self.append_resolved_range_context(text,0,size,rtl,&order,font,&features,&mut glyphs,&mut width,Some(&mut raw),context).map_err(|_|())?;
        let clusters=finish_cluster_advances(text,raw).map_err(|_|())?;
        let run=ShapedRun{glyphs:glyphs.into(),width};
        self.shapes.insert_context(full,size,Some(rtl),7,Some(font),run.clone(),language,key,Some(clusters.clone()));
        Ok(Some(ShapedRunWithClusterAdvances {run,clusters}))
    }
    fn generation(&self) -> u64 {
        self.generation
    }

    fn shape(&self, text: &str, size: f32) -> Result<ShapedRun, ()> {
        self.shape_with_direction(text, size, false, &FontSpec::default(), true)
            .map_err(|_| ())
    }

    fn shape_directional(&self, text: &str, size: f32, rtl: bool) -> Result<ShapedRun, ()> {
        self.shape_with_direction(text, size, rtl, &FontSpec::default(), true)
            .map_err(|_| ())
    }

    fn shape_styled(
        &self,
        text: &str,
        size: f32,
        rtl: bool,
        font: &FontSpec,
    ) -> Result<ShapedRun, ()> {
        self.shape_with_direction(text, size, rtl, font, true)
            .map_err(|_| ())
    }

    fn shape_styled_with_cluster_advances(
        &self,
        text: &str,
        size: f32,
        rtl: bool,
        font: &FontSpec,
    ) -> Result<Option<ShapedRunWithClusterAdvances>, ()> {
        let mut raw = Vec::new();
        let run = self
            .shape_with_direction_and_advances(text, size, rtl, font, true, Some(&mut raw))
            .map_err(|_| ())?;
        let clusters = finish_cluster_advances(text, raw).map_err(|_| ())?;
        Ok(Some(ShapedRunWithClusterAdvances { run, clusters }))
    }

    fn shape_resolved(
        &self,
        text: &str,
        size: f32,
        rtl: bool,
        font: &FontSpec,
    ) -> Result<ShapedRun, ()> {
        self.shape_with_direction(text, size, rtl, font, false)
            .map_err(|_| ())
    }

    fn shape_resolved_with_cluster_advances(
        &self,
        text: &str,
        size: f32,
        rtl: bool,
        font: &FontSpec,
    ) -> Result<Option<ShapedRunWithClusterAdvances>, ()> {
        let mut raw = Vec::new();
        let run = self
            .shape_with_direction_and_advances(text, size, rtl, font, false, Some(&mut raw))
            .map_err(|_| ())?;
        let clusters = finish_cluster_advances(text, raw).map_err(|_| ())?;
        Ok(Some(ShapedRunWithClusterAdvances { run, clusters }))
    }

    fn measure(&self, text: &str, size: f32) -> Result<f32, ()> {
        self.shape(text, size).map(|run| run.width)
    }

    fn measure_styled(&self, text: &str, size: f32, font: &FontSpec) -> Result<f32, ()> {
        self.shape_styled(text, size, false, font)
            .map(|run| run.width)
    }

    fn ascent(&self, size: f32) -> f32 {
        self.faces[0].face.ascent(size)
    }

    fn line_height(&self, size: f32) -> f32 {
        self.faces[0].face.line_height(size)
    }

    fn underline_metrics(&self, size: f32) -> (f32, f32) {
        self.faces[0].face.underline_metrics(size)
    }

    fn strike_metrics(&self, size: f32) -> (f32, f32) {
        self.faces[0].face.strike_metrics(size)
    }

    fn ascent_styled(&self, size: f32, font: &FontSpec) -> f32 {
        let index = self.metric_face_index(font);
        self.faces[index]
            .face
            .ascent(size * self.size_adjust_scale(font, index))
    }

    fn line_height_styled(&self, size: f32, font: &FontSpec) -> f32 {
        let index = self.metric_face_index(font);
        self.faces[index]
            .face
            .line_height(size * self.size_adjust_scale(font, index))
    }

    fn underline_metrics_styled(&self, size: f32, font: &FontSpec) -> (f32, f32) {
        let index = self.metric_face_index(font);
        self.faces[index]
            .face
            .underline_metrics(size * self.size_adjust_scale(font, index))
    }

    fn strike_metrics_styled(&self, size: f32, font: &FontSpec) -> (f32, f32) {
        let index = self.metric_face_index(font);
        self.faces[index]
            .face
            .strike_metrics(size * self.size_adjust_scale(font, index))
    }

    fn font_unit_metrics_styled(&self, size: f32, font: &FontSpec, vertical: bool, upright_zero: bool) -> lumen_html::paint::FontUnitMetrics {
        let order = self.face_order(font).ok();
        let first = order.as_ref().and_then(|order| order.iter().copied().find(|&index| {
            self.display_phase(index) == lumen_html::font_display::DisplayPhase::Loaded
                && self.unicode_ranges[index].as_ref().is_none_or(|ranges| {
                    ranges.iter().any(|&(first, last)| first <= 0x20 && 0x20 <= last)
                })
        })).unwrap_or(0);
        let metric = |name: FontMetric, glyph: Option<&str>| {
            let index = glyph.and_then(|glyph| order.as_ref().map(|order| self.cluster_face(glyph, order).0)).unwrap_or(first);
            let face = &self.faces[index].face;
            let ratio = match name {
                FontMetric::ChWidth => face.metrics[if upright_zero { 5 } else { 2 }],
                FontMetric::IcWidth => face.metrics[if vertical { 4 } else { 3 }],
                _ => face.metric(name),
            }?;
            let value = ratio * size * self.size_adjust_scale(font, index);
            value.is_finite().then_some(value)
        };
        lumen_html::paint::FontUnitMetrics {
            cap: metric(FontMetric::CapHeight, None),
            ch: metric(FontMetric::ChWidth, Some("0")),
            ic: metric(FontMetric::IcWidth, Some("水")),
        }
    }

    fn font_relative_metrics_styled(&self, size: f32, font: &FontSpec) -> FontRelativeMetrics {
        let index = self.metric_face_index(font);
        self.faces[index]
            .face
            .relative_metrics(size, self.size_adjust_scale(font, index))
    }
}

impl FontProvider for FontSet {
    fn first_available_metric(&self,font:&FontSpec,metric:FontMetric)->Option<f32> {FontSet::first_available_metric(self,font,metric)}
    fn registrations(&self) -> Option<Vec<FontRegistration>> {
        Some(
            self.faces
                .iter()
                .cloned()
                .zip(&self.unicode_ranges)
                .zip(&self.descriptors).enumerate()
                .filter(|(index,_)|self.display_phase(*index)==lumen_html::font_display::DisplayPhase::Loaded)
                .map(|(_,((font, coverage), descriptors))| FontRegistration {
                    font,
                    unicode_range: coverage.clone(),
                    descriptors: descriptors.clone(),
                })
                .collect(),
        )
    }
    fn shape_canvas_text(
        &self,
        text: &str,
        size: f32,
        rtl: bool,
        font: &FontSpec,
        options: &CanvasTextOptions,
    ) -> Result<ShapedRun, &'static str> {
        validate_shape_input(text, size)?;
        if !features_resolved(font) { return Err("font feature settings require query-container context"); }
        let mut font = font.clone();
        font.caps = options.font_variant_caps;
        let order = self.face_order(&font)?;
        let mut features = canvas_features(options);
        append_ligature_features(&mut features, font.ligatures);
        if options.letter_spacing != 0.0 || options.text_rendering == CanvasTextRendering::OptimizeSpeed { disable_optional_ligatures(&mut features); }
        append_feature_settings(&mut features, font.feature_settings.as_deref());
        let mut glyphs = Vec::new();
        let mut width = 0.0;
        let info = bidi::resolve(text, Some(rtl))?;
        for paragraph in &info.paragraphs {
            let (levels, runs) = bidi::shaping_runs(&info, paragraph, paragraph.range.clone())?;
            for range in runs {
                self.append_resolved_range(
                    &text[range.clone()],
                    range.start,
                    size,
                    levels[range.start].is_rtl(),
                    &order,
                    &font,
                    &features,
                    &mut glyphs,
                    &mut width,
                    None,
                )?;
            }
        }
        Ok(apply_canvas_spacing(
            text,
            ShapedRun {
                glyphs: glyphs.into(),
                width,
            },
            options,
        ))
    }

    fn shape_cache_stats(&self) -> ShapeCacheStats {
        let mut stats = self.shapes.stats();
        for (index, font) in self.faces.iter().enumerate() {
            if !self.faces[..index]
                .iter()
                .any(|previous| previous.face.id() == font.face.id())
            {
                stats = stats.add(font.face.shape_cache_stats());
            }
        }
        stats
    }
    fn rasterize_glyph(
        &self,
        face: u64,
        id: u16,
        size: f32,
    ) -> Result<GlyphCoverage, &'static str> {
        let font=self.face_by_identity(face).ok_or("unknown font face")?;
        font.rasterize_glyph(face,id,size)
    }

    fn glyph_cell_bounds(&self,face:u64,id:u16,size:f32)->Option<lumen_html::paint::Rect> {
        self.face_by_identity(face)?.glyph_cell_bounds(face,id,size)
    }

    fn outline_glyph(&self, face: u64, id: u16, size: f32) -> Result<GlyphOutline, &'static str> {
        let font=self.face_by_identity(face).ok_or("unknown font face")?;
        font.outline_glyph(face,id,size)
    }

    fn face_key(&self,face:u64)->Result<u64,&'static str> {
        self.face_by_identity(face).ok_or("unknown font face")?.face_key(face)
    }
}

pub const DEFAULT_FONT_BYTES: &[u8] = include_bytes!("../fonts/Inconsolata-Regular.ttf");
/// Proportional conformance face, redistributed unchanged under SIL OFL 1.1.
pub const TEST_FONT_BYTES: &[u8] = include_bytes!("../fonts/LiberationSans-Regular.ttf");
/// Bold proportional conformance face from the same Liberation Fonts release.
pub const TEST_FONT_BOLD_BYTES: &[u8] = include_bytes!("../fonts/LiberationSans-Bold.ttf");

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;
    use alloc::vec;

    #[test]
    fn specification_primary_font_control_metadata_tracks_selection_and_size_adjust() {
        use read_fonts::TableProvider;
        let face=Arc::new(FontFace::new(Arc::from(DEFAULT_FONT_BYTES)).unwrap());
        let tables=read_fonts::FontRef::new(DEFAULT_FONT_BYTES).unwrap();
        let size=20.0;
        let units=face.units_per_em;
        let expected=PrimaryCharacterWidths {
            average:tables.os2().unwrap().x_avg_char_width() as f32/units*size,
            maximum:tables.hhea().unwrap().advance_width_max().to_u16() as f32/units*size,
        };
        let plain=FontSpec::default();
        assert_eq!(face.primary_character_widths_styled(size,&plain),Some(expected));
        let font=RegisteredFont{family:Arc::from("Primary"),weight:400,style:FontStyle::Normal,stretch:100.,face:face.clone()};
        let fonts=FontSet::new(vec![font]).unwrap();
        let spec=FontSpec{families:Some(Arc::from([FontFamily::from("Primary")])),..plain};
        assert_eq!(fonts.primary_character_widths_styled(size,&spec),Some(expected));
        let doubled=PrimaryCharacterWidths{average:expected.average*2.,maximum:expected.maximum*2.};
        assert_eq!(fonts.primary_character_widths_styled(size*2.,&spec),Some(doubled));
        let mut adjusted=spec.clone();
        adjusted.size_adjust=Some(FontSizeAdjust{metric:FontMetric::ExHeight,value:FontSizeAdjustValue::Number(face.metric_ratio(FontMetric::ExHeight).unwrap()*2.)});
        assert_eq!(fonts.primary_character_widths_styled(size,&adjusted),Some(doubled));
        let shaped_before=fonts.shape_cache_stats();
        for _ in 0..16 {assert_eq!(fonts.primary_character_widths_styled(size,&spec),Some(expected));}
        assert_eq!(fonts.shape_cache_stats(),shaped_before);
    }

    #[test]
    fn specification_font_relative_units_use_cached_unshaped_selected_metrics() {
        let face=Arc::new(FontFace::new(Arc::from(TEST_FONT_BYTES)).unwrap());
        let plain=FontSpec::default();
        let size=20.0;
        let selected=face.font_unit_metrics_styled(size,&plain,false,false);
        assert_eq!(selected.cap,face.metric_ratio(FontMetric::CapHeight).map(|value|value*size));
        assert_eq!(selected.ch,face.metric_ratio(FontMetric::ChWidth).map(|value|value*size));
        let font=RegisteredFont{family:Arc::from("Primary"),weight:400,style:FontStyle::Normal,stretch:100.0,face:face.clone()};
        let fonts=FontSet::new(vec![font]).unwrap();
        let spec=FontSpec{families:Some(Arc::from([FontFamily::from("Primary")])),..plain};
        assert_eq!(fonts.font_unit_metrics_styled(size,&spec,false,false),selected);
        let vertical=fonts.font_unit_metrics_styled(size,&spec,true,true);
        assert_eq!(vertical.ch,face.metrics[5].map(|value|value*size));
        assert_eq!(vertical.ic,face.metric_ratio(FontMetric::IcHeight).map(|value|value*size));
        let before=fonts.shape_cache_stats();
        for _ in 0..16 {assert_eq!(fonts.font_unit_metrics_styled(size,&spec,false,false),selected);}
        assert_eq!(fonts.shape_cache_stats(),before,"font-relative units must not shape glyphs");
    }

    #[test]
    fn specification_font_display_shares_fallback_geometry_and_separates_ink_identity() {
        use lumen_html::font_display::DisplayPhase;
        let platform=FontSet::new(vec![registered("platform",400,FontStyle::Normal,DEFAULT_FONT_BYTES)]).unwrap();
        let fallback=platform.registrations().unwrap();
        let rules=lumen_html::css::parse_font_faces("@font-face{font-family:Pending;src:url(pending.ttf);unicode-range:U+0000-FFFF;size-adjust:180%}@font-face{font-family:Loaded;src:url(loaded.ttf)}").unwrap();
        let loaded=Arc::new(FontFace::new(Arc::from(TEST_FONT_BYTES)).unwrap());
        let spec=FontSpec{families:Some(Arc::from([FontFamily::from("Pending"),FontFamily::from("Loaded")])),..FontSpec::default()};
        let expected=loaded.shape_resolved("A ",20.,false,&spec).unwrap();
        let unused=alloc::vec![rules[0].clone();MAX_REGISTERED_FONTS+1];
        let unresolved=alloc::vec![None;unused.len()];
        assert!(FontSet::from_font_registry_snapshot_with_display(&fallback,&unused,&unresolved,&[],
            lumen_html::css::ContainerUnitContext::default(),None,None).is_ok(),
            "unused unloaded rules do not consume presentation registrations");
        let build=|phase|FontSet::from_font_registry_snapshot_with_display(&fallback,&rules,
            &[None,Some(loaded.clone())],&[],lumen_html::css::ContainerUnitContext::default(),
            Some(&[phase,DisplayPhase::Loaded]),None).unwrap();
        let block=build(DisplayPhase::Block);
        let hidden=block.shape_resolved("A ",20.,false,&spec).unwrap();
        assert_eq!(hidden.width,expected.width,"pending face adjustments cannot change loaded fallback metrics");
        assert_eq!(hidden.glyphs.iter().map(|glyph|(glyph.id,glyph.cluster,glyph.x,glyph.y,glyph.size_scale)).collect::<Vec<_>>(),
            expected.glyphs.iter().map(|glyph|(glyph.id,glyph.cluster,glyph.x,glyph.y,glyph.size_scale)).collect::<Vec<_>>());
        assert!(hidden.glyphs.iter().all(|glyph|glyph.face!=loaded.id()));
        for glyph in hidden.glyphs.iter() {
            assert!(block.rasterize_glyph(glyph.face,glyph.id,20.).unwrap().alpha.is_empty());
            assert!(block.outline_glyph(glyph.face,glyph.id,20.).unwrap().commands.is_empty());
            assert!(block.glyph_ink_bounds(glyph,20.).is_none());
            assert_eq!(block.face_key(glyph.face).unwrap(),glyph.face);
        }
        let geometry=|run:&lumen_html::paint::ShapedRun| (run.width,run.glyphs.iter()
            .map(|glyph|(glyph.id,glyph.cluster,glyph.x,glyph.y,glyph.size_scale)).collect::<Vec<_>>());
        let swap=build(DisplayPhase::Swap);
        let visible=swap.shape_resolved("A ",20.,false,&spec).unwrap();
        assert_eq!(geometry(&visible),geometry(&expected));
        assert!(visible.glyphs.iter().all(|glyph|glyph.face==loaded.id()),
            "swap uses the real loaded fallback identity, not the standalone face-zero convention");
        assert!(visible.glyphs.iter().any(|glyph| !swap.rasterize_glyph(glyph.face,glyph.id,20.).unwrap().alpha.is_empty()),
            "swap restores actual fallback ink");
        assert_eq!(block.ascent_styled(20.,&spec),swap.ascent_styled(20.,&spec));
        assert_eq!(block.font_relative_metrics_styled(20.,&spec),swap.font_relative_metrics_styled(20.,&spec));
        // A delayed downloaded face is still a loaded resource; its presentation
        // failure excludes it without changing its resource state or promise.
        let failed=FontSet::from_font_registry_snapshot_with_display(&fallback,&rules,
            &[Some(Arc::new(FontFace::new(Arc::from(DEFAULT_FONT_BYTES)).unwrap())),Some(loaded.clone())],&[],
            lumen_html::css::ContainerUnitContext::default(),Some(&[DisplayPhase::Failure,DisplayPhase::Loaded]),None).unwrap();
        let failed_run=failed.shape_resolved("A ",20.,false,&spec).unwrap();
        assert_eq!(geometry(&failed_run),geometry(&expected));
        assert!(failed_run.glyphs.iter().all(|glyph|glyph.face==loaded.id()),
            "a late failed face cannot replace the real loaded fallback");
        let same_hidden=build(DisplayPhase::Block).shape_resolved("A ",20.,false,&spec).unwrap();
        assert_eq!(hidden.glyphs[0].face,same_hidden.glyphs[0].face,"rebuilding presentation snapshots preserves invisible fallback identity");
        assert!(swap.rasterize_glyph(hidden.glyphs[0].face,hidden.glyphs[0].id,20.).unwrap().alpha.is_empty(),
            "retained invisible glyphs remain transparent after a phase transition");
    }

    #[test]
    fn specification_script_extensions_preserve_graphemes_and_actual_arabic_mark_shaping() {
        let bytes=include_bytes!("../tests/fixtures/shaping/NotoNaskhArabic-regular.woff2");
        let face=FontFace::new(Arc::from(bytes.as_slice())).unwrap();
        for text in ["\u{a0}\u{654}\u{670}","\u{a0}\u{670}\u{654}"] {
            let ranges=script_ranges(text,true).unwrap();
            assert_eq!(ranges.len(),1,"combining marks stay with their original NBSP grapheme");
            let selection=ranges[0].script;
            assert!(!selection.explicit&&selection.candidates.contains_script(Script::Arabic)
                &&selection.candidates.contains_script(Script::Syriac)
                &&!selection.candidates.contains_script(Script::Latin),"inherited marks constrain the actual script extensions");
            let decoded=rustybuzz::Face::from_slice(&face.bytes,0).unwrap();
            assert!(resolve_script(&decoded,selection,ShapeContext::language(None))==Script::Arabic,
                "actual selected font layout tables resolve the ambiguous marks");
            let actual=shape_buffer(&face.bytes,text,true,selection,&[]).unwrap();
            let reference=shape_buffer(&face.bytes,text,true,Script::Arabic.into(),&[]).unwrap();
            let ink=|buffer:&rustybuzz::GlyphBuffer|buffer.glyph_infos().iter().zip(buffer.glyph_positions())
                .map(|(glyph,position)|(glyph.glyph_id,position.x_advance,position.y_advance,position.x_offset,position.y_offset)).collect::<Vec<_>>();
            assert_eq!(ink(&actual),ink(&reference),"AMTRA uses the existing Arabic backend with original source order {text:?}");
            assert!(actual.glyph_infos().iter().all(|glyph|text.is_char_boundary(glyph.cluster as usize)),"backend clusters remain original UTF8 offsets");
        }
        for text in ["a\u{654} ბ", "a \u{a0}\u{654}\u{670} ბ", "\u{301}abc Ελληνικά"] {
            let ranges=script_ranges(text,false).unwrap();
            let mut end=0;
            for range in ranges {
                assert_eq!(range.start,end,"script ranges cover source once without holes");
                assert!(range.end>range.start&&text.is_char_boundary(range.end));
                assert!(range.start==0||graphemes(text).any(|(at,_)|at==range.start),"never split a grapheme");
                end=range.end;
            }
            assert_eq!(end,text.len());
        }
        let attached=script_ranges("a\u{654}",false).unwrap();
        assert_eq!(attached.len(),1);
        assert!(attached[0].script.preferred==Script::Latin&&attached[0].script.explicit,"a real base owns its combining mark script");
        let text="\u{a0}\u{654}\u{670}";
        let selection=script_ranges(text,false).unwrap()[0].script;
        let decoded=rustybuzz::Face::from_slice(&face.bytes,0).unwrap();
        for (base,expected) in [("ع",Script::Arabic),("ܐ",Script::Syriac)] {
            let full=format!("{base}{text}");
            let context=ShapeContext{language:None,surrounding:Some(&full),start:base.len(),end:full.len()};
            assert!(resolve_script(&decoded,selection,context)==expected,"only eligible surrounding script resolves the ambiguous set");
        }
    }

    #[test]
    fn specification_css_bidi_isolation_and_neutral_marks_use_actual_font_shapes() {
        use lumen_html::paint::Command;
        let fonts=FontSet::new(vec![registered("sans-serif",400,FontStyle::Normal,include_bytes!("../tests/fixtures/shaping/NotoNaskhArabic-regular.woff2"))]).unwrap();
        let render=|content:&str| {
            let document=lumen_html::html::parse(&format!("<style>body{{margin:0}}div{{font:30px sans-serif;width:300px}}</style>{content}"),128).unwrap();
            lumen_html::layout::display_list(&document,400,200,&fonts).unwrap()
        };
        let ink=|list:lumen_html::paint::DisplayList| {
            let mut values=Vec::new();for command in list.0 {if let Command::GlyphRun{origin_x,baseline_y,size,glyphs,..}=command {
                for glyph in glyphs.iter() {
                    // Rustybuzz preserves ZWNJ as a zero-advance invisible glyph.
                    // Compare painted ink, including real zero-advance marks.
                    let coverage=fonts.rasterize_glyph(glyph.face,glyph.id,size*glyph.size_scale).unwrap();
                    if coverage.alpha.iter().any(|&alpha|alpha!=0) {
                        values.push((glyph.id,origin_x+glyph.x,baseline_y+glyph.y));
                    }
                }}}
            values.sort_by(|a,b|a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));values
        };
        let isolated=ink(render("<div dir=rtl>ع<span dir=auto>ع</span>ع</div>"));
        let explicit=ink(render("<div dir=rtl>ع\u{200c}ع\u{200c}ع</div>"));
        assert_eq!(isolated.len(),explicit.len());for (a,b) in isolated.iter().zip(&explicit) {
            assert_eq!(a.0,b.0,"same-direction isolate must retain isolated Arabic forms");assert!((a.1-b.1).abs()<0.001&&(a.2-b.2).abs()<0.001,"source isolation preserves actual glyph positions {a:?} versus {b:?}");
        }
        let empty=ink(render("<div dir=rtl>ع<span dir=auto></span>ع</div>"));
        let stopped=ink(render("<div dir=rtl>ع\u{200c}ع</div>"));
        assert_eq!(empty.len(),stopped.len(),"empty isolate adds no ink or source character");
        for (a,b) in empty.iter().zip(&stopped) {
            assert_eq!(a.0,b.0,"wholly empty isolate stops joining across its boundary");
            assert!((a.1-b.1).abs()<0.001&&(a.2-b.2).abs()<0.001,"empty isolation retains real glyph placement {a:?} versus {b:?}");
        }
        for text in ["\u{a0}\u{654}\u{670}","\u{a0}\u{670}\u{654}"] {
            let list=render(&format!("<div>a<span dir=rtl style='color:red'>{text}</span>z</div>"));
            let mut actual=Vec::new();for command in list.0 {if let Command::GlyphRun{origin_x,baseline_y,color,glyphs,..}=command {if color.r==255&&color.g==0&&color.b==0 {
                actual.extend(glyphs.iter().map(|glyph|(glyph.id,origin_x+glyph.x,baseline_y+glyph.y)));}}}
            let expected=fonts.shape_resolved(text,30.0,true,&FontSpec::default()).unwrap();assert_eq!(actual.len(),expected.glyphs.len());
            let anchor=actual[0];let origin=expected.glyphs[0];
            for (a,b) in actual.iter().zip(expected.glyphs.iter()){assert_eq!(a.0,b.id);assert!(((a.1-anchor.1)-(b.x-origin.x)).abs()<0.001&&((a.2-anchor.2)-(b.y-origin.y)).abs()<0.001,"neutral base marks use RTL shaping positions {a:?}, {b:?}");}
        }
    }

    #[test]
    fn specification_boxless_typographic_pseudos_preserve_actual_kerning_and_glyph_origins() {
        use lumen_html::paint::Command;
        let fonts=FontSet::new(vec![registered("sans-serif",400,FontStyle::Normal,TEST_FONT_BYTES)]).unwrap();
        let ink=|markup:&str| {
            let document=lumen_html::html::parse(markup,128).unwrap();
            let list=lumen_html::layout::display_list(&document,400,200,&fonts).unwrap();
            let mut glyphs=Vec::new();
            for command in &list.0 {
                if let Command::GlyphRun{origin_x,baseline_y,glyphs:run,color,..}=command {
                    assert_eq!((color.r,color.g,color.b),(0,128,0),"only the real principal box supplies the typography; boxless pseudo styles paint no red");
                    glyphs.extend(run.iter().map(|glyph|(glyph.id,*origin_x+glyph.x,*baseline_y+glyph.y)));
                }
            }
            glyphs.sort_by(|a,b|a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)).then(a.2.total_cmp(&b.2)));
            glyphs
        };
        for direction in ["ltr","rtl"] {
        let reference=ink(&alloc::format!("<style>body{{margin:0;direction:{direction}}}</style><div style='color:green'><span>P</span>ASS</div>"));
        for (css,contents) in [
            ("#container::first-line{color:green}#contents::first-line{background:red}","<span>P</span>ASS"),
            ("#container::first-letter{color:green}#contents::first-letter{background:red}span{color:green}","P<span>ASS</span>"),
            ("#container::first-line{color:red}span{color:green}","<span style='display:contents'>P</span><span>ASS</span>"),
        ] {
            for spaces in ["","\n  "] {
                let markup=alloc::format!("<style>body{{margin:0;direction:{direction}}}#contents{{display:contents}}{css}</style><div id=container>{spaces}<div id=contents>{contents}</div>{spaces}</div>");
                let actual=ink(&markup);
                assert_eq!(actual.len(),reference.len(),"source wrappers preserve the actual glyph set: {markup}");
                for (a,b) in actual.iter().zip(&reference) {
                    assert_eq!(a.0,b.0);
                    assert!((a.1-b.1).abs()<0.001&&(a.2-b.2).abs()<0.001,"compatible glyph origin {a:?} != {b:?}; {markup}; actual={actual:?}; reference={reference:?}");
                }
            }
        }
        }
    }

    #[test]
    fn compatible_inline_shaping_preserves_real_kerning_joining_and_cluster_paint_partitions() {
        use lumen_html::paint::Command;
        let render=|markup:&str,fonts:&FontSet| {
            let document=lumen_html::html::parse(markup,128).unwrap();
            lumen_html::layout::display_list(&document,400,200,fonts).unwrap()
        };
        let ink=|list:&lumen_html::paint::DisplayList| {
            let mut values=Vec::new();
            for command in &list.0 {if let Command::GlyphRun{origin_x,baseline_y,glyphs,..}=command {
                values.extend(glyphs.iter().map(|glyph|(glyph.id,*origin_x+glyph.x,*baseline_y+glyph.y)));
            }}
            values.sort_by(|a,b|a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)).then(a.2.total_cmp(&b.2)));
            // A shared cluster painted through two disjoint clips retains one
            // glyph origin, rather than two independently shaped glyphs.
            values.dedup_by(|a,b|a.0==b.0&&(a.1-b.1).abs()<0.001&&(a.2-b.2).abs()<0.001);
            values
        };
        let compare=|a:lumen_html::paint::DisplayList,b:lumen_html::paint::DisplayList| {
            let a=ink(&a);let b=ink(&b);assert_eq!(a.len(),b.len());
            for (a,b) in a.iter().zip(&b){assert_eq!(a.0,b.0);assert!((a.1-b.1).abs()<0.001&&(a.2-b.2).abs()<0.001,"shared origin {a:?} != {b:?}");}
        };
        let check_paint_partitions=|list:&lumen_html::paint::DisplayList,fonts:&FontSet| {
            let mut clips=Vec::new();let mut painted=Vec::new();
            for command in &list.0 {match command {
                Command::PushClip(rect) | Command::PushBoxClip(rect)=>{assert!(rect.width>0.0&&rect.height>0.0,"paint partitions must have positive area");clips.push(*rect);},
                Command::PopClip=>{clips.pop();},
                Command::GlyphRun{origin_x,baseline_y,size,glyphs,..}=>for glyph in glyphs.iter(){
                    let x=*origin_x+glyph.x;let y=*baseline_y+glyph.y;
                    let coverage=fonts.rasterize_glyph(glyph.face,glyph.id,*size*glyph.size_scale).unwrap();
                    if coverage.width==0 {continue;}
                    let left=x+coverage.x_min as f32;let right=left+coverage.width as f32;
                    let interval=clips.last().map_or((left,right),|clip|(left.max(clip.x),right.min(clip.x+clip.width)));
                    painted.push((glyph.face,glyph.id,x,y,left,right,interval));
                },
                _=>{}
            }}
            for (index,item) in painted.iter().enumerate(){
                if painted[..index].iter().any(|other|other.0==item.0&&other.1==item.1&&(other.2-item.2).abs()<0.001&&(other.3-item.3).abs()<0.001){continue;}
                let mut intervals=painted.iter().filter(|other|other.0==item.0&&other.1==item.1&&(other.2-item.2).abs()<0.001&&(other.3-item.3).abs()<0.001).map(|other|other.6).filter(|(left,right)|right>left).collect::<Vec<_>>();
                intervals.sort_by(|a,b|a.0.total_cmp(&b.0));
                let mut end=item.4;
                for (left,right) in intervals {assert!(right>left,"nonempty glyph paint strip {item:?}");assert!((left-end).abs()<0.001,"paint strips overlap or leave missing ink: {left} versus {end}, glyph {item:?}");end=right;}
                assert!((end-item.5).abs()<0.001,"paint strips must cover the complete raster bounds {item:?}");
            }
        };
        let fonts=FontSet::new(vec![registered("sans-serif",400,FontStyle::Normal,TEST_FONT_BYTES)]).unwrap();
        for spacing in ["", "letter-spacing:2px;word-spacing:3px"] {
            compare(render(&alloc::format!("<style>body,p{{margin:0}}</style><p style='{spacing}'>[1] <span>A</span>1</p>"),&fonts),
                render(&alloc::format!("<style>body,p{{margin:0}}</style><p style='{spacing}'>[1] A1</p>"),&fonts));
        }
        // Exact font bytes from the pinned WPT /fonts/noto fixture; OFL license
        // is preserved beside it. This face supplies required lam-alef GSUB.
        let arabic=FontSet::new(vec![registered("sans-serif",400,FontStyle::Normal,
            include_bytes!("../tests/fixtures/shaping/NotoNaskhArabic-regular.woff2"))]).unwrap();
        let whole=render("<style>body,p{margin:0}</style><p lang=ar dir=rtl>علا</p>",&arabic);
        let split=render("<style>body,p{margin:0}</style><p lang=ar dir=rtl>ع<span style='color:blue'>ل</span>ا</p>",&arabic);
        check_paint_partitions(&split,&arabic);
        compare(split,whole);
let paint_intervals=|list:&lumen_html::paint::DisplayList| {
    let mut clips=Vec::new();let mut values=Vec::new();
    for command in &list.0 {match command {
        Command::PushClip(rect) | Command::PushBoxClip(rect)=>clips.push(*rect),Command::PopClip=>{clips.pop();},
        Command::GlyphRun{origin_x,baseline_y,size,color,glyphs}=>for glyph in glyphs.iter(){
            let coverage=arabic.rasterize_glyph(glyph.face,glyph.id,*size*glyph.size_scale).unwrap();
            if coverage.width==0 || coverage.height==0 {continue;}
            let x=*origin_x+glyph.x;let y=*baseline_y+glyph.y;
            let left=x+coverage.x_min as f32;let right=left+coverage.width as f32;
            let (left,right)=clips.last().map_or((left,right),|clip|(left.max(clip.x),right.min(clip.x+clip.width)));
            if right>left {values.push((glyph.id,x,y,*color,left,right));}
        },_=>{}
    }}
    values.sort_by(|a,b|a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)).then(a.4.total_cmp(&b.4)));
    values
};
let bare=render("<style>body,p{margin:0}</style><p lang=ar dir=rtl>ع<span style='color:blue'>ع</span>ع</p>",&arabic);
let joiners=render("<style>body,p{margin:0}</style><p lang=ar dir=rtl>ع&zwj;<span style='color:blue'>&zwj;ع&zwj;</span>&zwj;ع</p>",&arabic);
check_paint_partitions(&bare,&arabic);check_paint_partitions(&joiners,&arabic);
let bare=paint_intervals(&bare);let joiners=paint_intervals(&joiners);
assert_eq!(bare.len(),joiners.len(),"joiners cannot acquire or steal a paint interval");
for (a,b) in bare.iter().zip(&joiners) {
    assert_eq!((a.0,a.3),(b.0,b.3),"joining controls preserve glyph and span paint identity");
    assert!((a.1-b.1).abs()<0.001&&(a.2-b.2).abs()<0.001&&(a.4-b.4).abs()<0.001&&(a.5-b.5).abs()<0.001,"default-ignorable paint coverage {a:?} versus {b:?}");
}
        let mark=render("<style>body,p{margin:0}</style><p lang=ar dir=rtl>ع<span style='color:blue'>َ</span></p>",&arabic);
        check_paint_partitions(&mark,&arabic);
        compare(mark,render("<style>body,p{margin:0}</style><p lang=ar dir=rtl>عَ</p>",&arabic));
        let same_paint=render("<style>body,p{margin:0}</style><p lang=ar dir=rtl>ع<span>َ</span></p>",&arabic);
        check_paint_partitions(&same_paint,&arabic);
        compare(same_paint,render("<style>body,p{margin:0}</style><p lang=ar dir=rtl>عَ</p>",&arabic));
        // Bounds remain in the run's local coordinate system, including
        // deferred vertical transforms and text translated into the viewport.
        for context in ["transform:translateX(-300px)","writing-mode:vertical-rl"] {
            let partitioned=render(&alloc::format!("<style>body,p{{margin:0}}</style><p style='{context}' lang=ar dir=rtl>ع<span style='color:blue'>َ</span></p>"),&arabic);
            check_paint_partitions(&partitioned,&arabic);
            assert!(partitioned.0.iter().any(|command|matches!(command,Command::PushTransform(_))),"fixture must exercise a transform");
        }
        let wrapped=render("<style>body,p{margin:0}p{width:1px;overflow-wrap:anywhere}</style><p lang=ar dir=rtl>ل<span>ا</span></p>",&arabic);
        let baseline=ink(&wrapped);assert!(!baseline.is_empty());
        let normal=arabic.shape_resolved_with_cluster_advances("لا",16.0,true,&FontSpec::default()).unwrap().unwrap();
        let mut expected=normal.run.glyphs.iter().map(|glyph|glyph.id).collect::<Vec<_>>();expected.sort_unstable();expected.dedup();
        let mut actual=baseline.iter().map(|glyph|glyph.0).collect::<Vec<_>>();actual.sort_unstable();actual.dedup();
        assert_eq!(actual,expected,"wrapping retains the whole word's required contextual glyph forms");
    }

    #[test]
    fn content_language_changes_synthetic_caps_and_retains_exact_cache_identity() {
        let face=FontFace::new(Arc::from(TEST_FONT_BYTES)).unwrap();
        let font=FontSpec{caps:CanvasFontVariantCaps::SmallCaps,..FontSpec::default()};
        let english=face.shape_resolved_context("i",20.0,false,&font,Some("en")).unwrap();
        let turkish=face.shape_resolved_context("i",20.0,false,&font,Some("tr")).unwrap();
        assert_ne!(english.glyphs[0].id,turkish.glyphs[0].id,"Turkish synthesis uses dotted uppercase I");
        assert_eq!(english.glyphs[0].cluster,0);assert_eq!(turkish.glyphs[0].cluster,0);
        let before=face.shapes.stats().hits;
        assert_eq!(face.shape_resolved_context("i",20.0,false,&font,Some("en")).unwrap(),english);
        assert_eq!(face.shape_resolved_context("i",20.0,false,&font,Some("tr")).unwrap(),turkish);
        assert_eq!(face.shapes.stats().hits,before+2);
    }

#[test]
fn joining_context_preserves_font_item_clusters_and_exact_cache_identity() {
    let face=FontFace::new(Arc::from(include_bytes!("../tests/fixtures/shaping/NotoNaskhArabic-regular.woff2").as_slice())).unwrap();
    let font=FontSpec::default();
    let full="ععع";
    let whole=face.shape_resolved_with_cluster_advances(full,24.0,true,&font).unwrap().unwrap();
    let item=face.shape_resolved_segment_with_cluster_advances(full,2..4,24.0,true,&font,Some("ar")).unwrap().unwrap();
    let expected=whole.run.glyphs.iter().filter(|glyph|glyph.cluster==2).map(|glyph|glyph.id).collect::<Vec<_>>();
    assert!(!expected.is_empty());
    assert_eq!(item.run.glyphs.iter().map(|glyph|glyph.id).collect::<Vec<_>>(),expected,"font item retains both neighboring joining contexts");
    assert!(item.run.glyphs.iter().all(|glyph|glyph.cluster<2),"item clusters stay local");
    assert_eq!(item.clusters.last().unwrap().source.end,2);
    let isolated=face.shape_resolved("ع",24.0,true,&font).unwrap();
    assert_ne!(isolated.glyphs[0].id,item.run.glyphs[0].id,"fixture distinguishes isolated and medial shaping");
    let repeated=face.shape_resolved_segment_with_cluster_advances(full,2..4,24.0,true,&font,Some("ar")).unwrap().unwrap();
    assert_eq!(item.run,repeated.run);
    assert!(Arc::ptr_eq(&item.clusters,&repeated.clusters),"existing cache retains shared cluster metadata");
    let boundary=face.shape_resolved_segment_with_cluster_advances(" ع ",1..3,24.0,true,&font,Some("ar")).unwrap().unwrap();
    assert_eq!(boundary.run.glyphs[0].id,isolated.glyphs[0].id,"changed surrounding identity cannot reuse medial glyphs");
    let fonts=FontSet::new(vec![registered("sans-serif",400,FontStyle::Normal,include_bytes!("../tests/fixtures/shaping/NotoNaskhArabic-regular.woff2"))]).unwrap();
    let fallback=fonts.shape_resolved_segment_with_cluster_advances(full,2..4,24.0,true,&font,Some("ar")).unwrap().unwrap();
    assert_eq!(fallback.run.glyphs.iter().map(|glyph|glyph.id).collect::<Vec<_>>(),expected,"registered fallback forwards the same real context");
}

    #[test]
    fn intrinsic_inline_and_anonymous_cells_share_real_spacing_and_contextual_shapes() {
        let width=|markup:&str,fonts:&FontSet| {
            let document=lumen_html::html::parse(markup,128).unwrap();
            let node=lumen_html::selector::query_selector(&document,document.root(),"#box").unwrap().unwrap();
            let mut session=lumen_html::session::RenderSession::new(document);
            session.display_list(400,200,fonts).unwrap();
            session.layout_rect(node).unwrap().width
        };
        let latin=FontSet::new(vec![registered("sans-serif",400,FontStyle::Normal,TEST_FONT_BYTES)]).unwrap();
        let arabic=FontSet::new(vec![registered("sans-serif",400,FontStyle::Normal,include_bytes!("../tests/fixtures/shaping/NotoNaskhArabic-regular.woff2"))]).unwrap();
        for display in ["inline-block","table"] {
            let actual=width(&alloc::format!("<style>body,p{{margin:0}}#box{{display:{display};width:max-content;border-spacing:0}}</style><p><span id=box>[1] <span>A</span>1</span></p>"),&latin);
            let expected=latin.shape_styled("[1] A1",16.0,false,&FontSpec::default()).unwrap().width;
            assert!((actual-expected).abs()<0.001,"{display}: real inter-span kerning {actual} versus {expected}");
            let actual=width(&alloc::format!("<style>body,p{{margin:0}}#box{{display:{display};width:max-content;border-spacing:0}}</style><p lang=ar dir=rtl><span id=box>ع<span>ل</span>ا</span></p>"),&arabic);
            let expected=arabic.shape_resolved_context("علا",16.0,true,&FontSpec::default(),Some("ar")).unwrap().width;
            assert!((actual-expected).abs()<0.001,"{display}: real Arabic joining {actual} versus {expected}");
        }
    }

    #[test]
    fn registered_fallback_spaces_keep_advances_across_bidi_and_cache_modes() {
        for bytes in [DEFAULT_FONT_BYTES, TEST_FONT_BYTES] {
            let registration = registered("FallbackSpace", 400, FontStyle::Normal, bytes);
            let face = registration.face.clone();
            let fonts = FontSet::new(vec![registration]).unwrap();
            let spec = FontSpec::default();
            let space = face.shape_resolved(" ", 20.0, false, &spec).unwrap().width;
            assert!(space > 0.0, "fixture must provide an advancing space");
            for text in [" ", "  ", "A ", " A", "A A", "[1] ", "should be on two lines."] {
                let reference = face.shape_resolved(text, 20.0, false, &spec).unwrap();
                let paragraph = fonts.shape_styled(text, 20.0, false, &spec).unwrap();
                let resolved = fonts.shape_resolved(text, 20.0, false, &spec).unwrap();
                assert!((paragraph.width-reference.width).abs() < 0.001, "paragraph {text:?}: {} versus {}", paragraph.width, reference.width);
                assert!((resolved.width-reference.width).abs() < 0.001, "resolved {text:?}");
                let detail = fonts.shape_styled_with_cluster_advances(text, 20.0, false, &spec).unwrap().unwrap();
                assert_eq!(detail.run, paragraph, "cluster shaping {text:?}");
                assert!((detail.clusters.iter().map(|cluster| cluster.advance).sum::<f32>() - paragraph.width).abs() < 0.001, "cluster advances {text:?}");
                for (offset, character) in text.char_indices() {
                    if character == ' ' {
                        assert!(detail.clusters.iter().any(|cluster| cluster.source.contains(&offset) && cluster.advance > 0.0), "space cluster {text:?} at {offset}");
                    }
                }
                assert_eq!(fonts.shape_styled(text, 20.0, false, &spec).unwrap(), paragraph, "cached paragraph {text:?}");
            }
            for text in ["\u{2066}A \u{2069}", "\u{200e}A "] {
                let plain = fonts.shape_styled("A ", 20.0, false, &spec).unwrap();
                let controlled = fonts.shape_styled(text, 20.0, false, &spec).unwrap();
                assert!((controlled.width-plain.width).abs() < 0.001, "directional controls must retain the terminal space in {text:?}: actual {} expected {}, glyphs {:?}", controlled.width, plain.width, controlled.glyphs);
                assert_eq!(controlled.glyphs.len(),plain.glyphs.len(),"control-only runs emit no glyphs: {text:?}");
                for (actual,expected) in controlled.glyphs.iter().zip(plain.glyphs.iter()) {
                    assert_eq!(actual.id,expected.id,"isolate glyph: {text:?}");
                    assert!((actual.x-expected.x).abs()<0.001,"isolate contextual glyph origin: {text:?}");
                }
                let detail=fonts.shape_styled_with_cluster_advances(text,20.0,false,&spec).unwrap().unwrap();
                assert!((detail.clusters.iter().map(|cluster|cluster.advance).sum::<f32>()-controlled.width).abs()<0.001,"isolate source advances: {text:?}");
            }
        }
        let regular = registered("sans-serif", 400, FontStyle::Normal, TEST_FONT_BYTES);
        let reference = regular.face.clone();
        let fonts = FontSet::new(vec![regular,
            registered("sans-serif", 700, FontStyle::Normal, TEST_FONT_BOLD_BYTES),
            registered("monospace", 400, FontStyle::Normal, DEFAULT_FONT_BYTES)]).unwrap();
        let spec = FontSpec::default();
        for text in [" ", "[1] ", "should be on two lines.", "A \u{200e}"] {
            let expected = reference.shape_styled(text, 16.0, false, &spec).unwrap();
            let actual = fonts.shape_styled(text, 16.0, false, &spec).unwrap();
            assert!((actual.width-expected.width).abs() < 0.001, "platform fallback paragraph {text:?}");
            assert_eq!(actual.glyphs.len(), expected.glyphs.len());
            for (actual, expected) in actual.glyphs.iter().zip(expected.glyphs.iter()) {
                assert_eq!(actual.id, expected.id, "platform fallback glyph {text:?}");
                assert!((actual.x-expected.x).abs() < 0.001, "platform fallback glyph advance {text:?}");
            }
            assert_eq!(fonts.shape_styled(text, 16.0, false, &spec).unwrap(), actual);
        }
    }

    #[test]
    fn shared_small_caps_synthesis_preserves_capitals_source_clusters_and_spacing() {
        let face = FontFace::new(Arc::from(DEFAULT_FONT_BYTES)).unwrap();
        assert_eq!(
            face.caps_features & 1,
            0,
            "fallback fixture must lack native small caps"
        );
        let normal = FontSpec::default();
        let caps = FontSpec {
            caps: CanvasFontVariantCaps::SmallCaps,
            ..normal.clone()
        };
        let capitals = face.shape_styled("HI", 32.0, false, &caps).unwrap();
        assert_eq!(
            capitals.width,
            face.shape_styled("HI", 32.0, false, &normal).unwrap().width
        );
        assert!(capitals.glyphs.iter().all(|glyph| glyph.size_scale == 1.0));
        let lower = face.shape_styled("hi", 32.0, false, &caps).unwrap();
        assert!(lower.glyphs.iter().all(|glyph| glyph.size_scale == 0.8));
        assert_eq!(
            lower
                .glyphs
                .iter()
                .map(|glyph| glyph.id)
                .collect::<Vec<_>>(),
            capitals
                .glyphs
                .iter()
                .map(|glyph| glyph.id)
                .collect::<Vec<_>>()
        );
        assert!((lower.width - capitals.width * 0.8).abs() < 0.01);
        let all = FontSpec {
            caps: CanvasFontVariantCaps::AllSmallCaps,
            ..caps.clone()
        };
        assert!(face
            .shape_styled("HI", 32.0, false, &all)
            .unwrap()
            .glyphs
            .iter()
            .all(|glyph| glyph.size_scale == 0.8));
        let disabled = FontSpec {
            synthesize_small_caps: false,
            ..caps.clone()
        };
        assert_eq!(
            face.shape_styled("hi", 32.0, false, &disabled)
                .unwrap()
                .width,
            face.shape_styled("hi", 32.0, false, &normal).unwrap().width
        );
        let text = "ß";
        let plain = face
            .shape_canvas_text(
                text,
                32.0,
                false,
                &caps,
                &CanvasTextOptions {
                    font_variant_caps: caps.caps,
                    ..CanvasTextOptions::default()
                },
            )
            .unwrap();
        assert_eq!(plain.caps_expansions().collect::<Vec<_>>(), vec![(0, 1)]);
        assert!(plain.glyphs.iter().all(|glyph| glyph.cluster == 0));
        let spaced = face
            .shape_canvas_text(
                text,
                32.0,
                false,
                &caps,
                &CanvasTextOptions {
                    font_variant_caps: caps.caps,
                    letter_spacing: 5.0,
                    ..CanvasTextOptions::default()
                },
            )
            .unwrap();
        assert!((spaced.width - plain.width - 5.0).abs() < 0.01);
        let combining = face.shape_styled("a\u{301}", 32.0, false, &caps).unwrap();
        assert!(combining.caps_expansions().next().is_none());
        assert!(combining.glyphs.iter().all(|glyph| glyph.cluster == 0));
        let mixed = face.shape_styled("Hi אב ß", 32.0, false, &caps).unwrap();
        assert!(mixed
            .glyphs
            .iter()
            .all(|glyph| (glyph.cluster as usize) < "Hi אב ß".len()));
        assert!(mixed.glyphs.iter().any(|glyph| glyph.size_scale == 1.0));
        assert!(mixed.glyphs.iter().any(|glyph| glyph.size_scale == 0.8));
        assert!(
            face.shape_styled(
                &"\u{390}".repeat(MAX_SHAPE_TEXT_BYTES / 2),
                32.0,
                false,
                &caps
            )
            .is_err(),
            "uppercase expansion must respect the shared byte bound"
        );
    }

    #[test]
    fn caps_expansion_metadata_fits_the_existing_glyph_storage_budget() {
        #[allow(dead_code)]
        struct PreviousGlyph {
            id: u16,
            face: u64,
            cluster: u32,
            x: f32,
            y: f32,
            size_scale: f32,
        }
        assert!(core::mem::size_of::<Glyph>() <= core::mem::size_of::<PreviousGlyph>());
    }

    // WPT css/css-fonts/support/fonts/FontWithFancyFeatures.otf, revision
    // 74ca910926d76710943f2a8798817102f69d6e40; exact BSD3 license beside fixture.
    const CAPS_FEATURE_FONT: &[u8] =
        include_bytes!("../tests/fixtures/caps/FontWithFancyFeatures.otf");

    #[test]
    fn feature_settings_override_descriptor_variant_and_spacing_in_native_shaping() {
        let face = Arc::new(FontFace::new(Arc::from(CAPS_FEATURE_FONT)).unwrap());
        let parse = |raw| lumen_html::css::resolve_font_feature_settings(
            lumen_html::css::parse_font_feature_settings(raw).unwrap().as_ref(),
            lumen_html::css::ContainerUnitContext::default()).unwrap();
        let font = RegisteredFont {family:Arc::from("Fixture"),weight:400,style:FontStyle::Normal,stretch:100.0,face:face.clone()};
        let mut descriptor = RegisteredFontDescriptors::scalar(&font, 1.0);
        descriptor.feature_settings = parse("'liga' off");
        let set = FontSet::new_with_unicode_ranges_and_descriptors(vec![font],vec![None],vec![descriptor]).unwrap();
        let parsed = ttf_parser::Face::parse(CAPS_FEATURE_FONT, 0).unwrap();
        let plain = parsed.glyph_index('C').unwrap().0;
        let enabled = parsed.glyph_index('A').unwrap().0;
        let mut spec = FontSpec::default();
        assert_eq!(set.shape_styled("C",32.0,false,&spec).unwrap().glyphs[0].id,plain);
        assert!(spec.ligatures.set(FontLigatureGroup::Common,true));
        assert_eq!(set.shape_styled("C",32.0,false,&spec).unwrap().glyphs[0].id,enabled);
        spec.disable_optional_ligatures = true;
        assert_eq!(set.shape_styled("C",32.0,false,&spec).unwrap().glyphs[0].id,plain);
        spec.feature_settings = parse("'liga' on");
        let run = set.shape_styled("C",32.0,false,&spec).unwrap();
        assert_eq!(run.glyphs[0].id,enabled);
        assert_eq!(set.shape_styled("C",32.0,false,&spec).unwrap(),run);
        assert_eq!(face.shape_styled("C",32.0,false,&spec).unwrap().glyphs[0].id,enabled);
        let options = CanvasTextOptions {letter_spacing:2.0,..CanvasTextOptions::default()};
        assert_eq!(set.shape_canvas_text("C",32.0,false,&spec,&options).unwrap().glyphs[0].id,enabled);
        spec.feature_settings = parse("'liga' off");
        assert_eq!(set.shape_styled("C",32.0,false,&spec).unwrap().glyphs[0].id,plain);
        spec.feature_settings = parse("'notA' on");
        spec.disable_optional_ligatures = false;
        assert_eq!(set.shape_styled("C",32.0,false,&spec).unwrap().glyphs[0].id,enabled, "unknown tags must not trigger font fallback or synthesis");
        let mut second = RegisteredFontDescriptors::scalar(&set.faces[0],1.0);
        second.feature_settings = parse("'liga' on");
        let shared = FontSet::new_with_unicode_ranges_and_descriptors(vec![set.faces[0].clone()],vec![None],vec![second]).unwrap();
        assert_eq!(shared.shape_styled("C",32.0,false,&FontSpec::default()).unwrap().glyphs[0].id,enabled);
        assert_eq!(set.shape_styled("C",32.0,false,&FontSpec::default()).unwrap().glyphs[0].id,plain, "shared decoded bytes must not leak descriptor settings across registrations");
        let rules = lumen_html::css::parse_font_faces("@font-face{font-family:Web;src:url(fixture.otf);font-feature-settings:'liga' calc(sign(2cqw - 10px))}").unwrap();
        let fallback = FontRegistration {font:set.faces[0].clone(),unicode_range:None,
            descriptors:RegisteredFontDescriptors::scalar(&set.faces[0],1.0)};
        let web = lumen_html::css::parse_font_shorthand("16px Web").unwrap();
        for (width,expected) in [(100.0,plain),(1000.0,enabled)] {
            let query = lumen_html::css::ContainerUnitContext::no_container(lumen_html::css::MediaEnvironment {width,height:600.0,..lumen_html::css::MediaEnvironment::default()});
            let snapshot = FontSet::from_font_registry_snapshot_with_query(core::slice::from_ref(&fallback),&rules,&[Some(face.clone())],&[],query).unwrap();
            assert_eq!(snapshot.shape_styled("C",32.0,false,&web).unwrap().glyphs[0].id,expected,"descriptor no-container viewport {width}");
        }
        assert!(FontSet::from_font_registry_snapshot(core::slice::from_ref(&fallback),&rules,&[Some(face)],&[]).is_err(),"unavailable descriptor query context must not become guessed viewport features");
        let text_face = FontFace::new(Arc::from(DEFAULT_FONT_BYTES)).unwrap();
        let trailing = text_face.shape_styled("A ",32.0,false,&FontSpec::default()).unwrap();
        assert!(trailing.width > text_face.shape_styled("A",32.0,false,&FontSpec::default()).unwrap().width, "native shaping must retain terminal-space advances");
        let mut discretionary = FontSpec::default();
        assert!(discretionary.ligatures.set(FontLigatureGroup::Discretionary,true));
        assert_eq!(set.shape_styled("E",32.0,false,&discretionary).unwrap().glyphs[0].id,enabled);
        discretionary.disable_optional_ligatures = true;
        assert_eq!(set.shape_styled("E",32.0,false,&discretionary).unwrap().glyphs[0].id,parsed.glyph_index('E').unwrap().0);
        discretionary.feature_settings = parse("'dlig' on");
        assert_eq!(set.shape_styled("E",32.0,false,&discretionary).unwrap().glyphs[0].id,enabled,"author features override spacing suppression for discretionary ligatures too");
    }

    #[test]
    fn ligature_policy_changes_native_glyphs_and_keeps_styled_cache_identity() {
        let face = Arc::new(FontFace::new(Arc::from(CAPS_FEATURE_FONT)).unwrap());
        let normal = FontSpec::default();
        let disabled = FontSpec {ligatures:FontLigatures::NONE,..normal.clone()};
        let ids = |run:ShapedRun| run.glyphs.iter().map(|glyph| glyph.id).collect::<Vec<_>>();
        assert_eq!(ids(face.shape_styled("CD",32.0,false,&normal).unwrap()), ids(face.shape_styled("AA",32.0,false,&normal).unwrap()));
        let raw = ttf_parser::Face::parse(CAPS_FEATURE_FONT, 0).unwrap();
        let unfeatured: Vec<_> = "CDGFE".chars().map(|ch| raw.glyph_index(ch).unwrap().0).collect();
        assert_eq!(ids(face.shape_styled("CDGFE",32.0,false,&disabled).unwrap()), unfeatured);
        let mut discretionary = normal.clone();
        assert!(discretionary.ligatures.set(FontLigatureGroup::Discretionary,true));
        assert_eq!(ids(face.shape_styled("E",32.0,false,&discretionary).unwrap()), ids(face.shape_styled("A",32.0,false,&normal).unwrap()));
        assert_ne!(ids(face.shape_styled("E",32.0,false,&normal).unwrap()), ids(face.shape_styled("E",32.0,false,&discretionary).unwrap()));
        let features = font_features(&disabled);
        assert!(!features.iter().any(|feature| feature.tag == ttf_parser::Tag::from_bytes(b"rlig")), "required script ligatures must stay enabled");
        let set = FontSet::new(vec![RegisteredFont {family:Arc::from("Fixture"),weight:400,style:FontStyle::Normal,stretch:100.0,face}]).unwrap();
        let enabled_run = set.shape_styled("CDGFE",32.0,false,&normal).unwrap();
        let disabled_run = set.shape_styled("CDGFE",32.0,false,&disabled).unwrap();
        let reference = set.shape_styled("B",32.0,false,&normal).unwrap();
        let expected = &reference.glyphs[0];
        for glyph in disabled_run.glyphs.iter() {
            assert_eq!(set.rasterize_glyph(glyph.face,glyph.id,32.0).unwrap(),
                set.rasterize_glyph(expected.face,expected.id,32.0).unwrap());
        }
        assert_ne!(ids(enabled_run.clone()), ids(disabled_run));
        assert_eq!(set.shape_styled("CDGFE",32.0,false,&normal).unwrap(), enabled_run);
        let canvas = set.shape_canvas_text("CDGFE",32.0,false,&disabled,&CanvasTextOptions::default()).unwrap();
        assert_eq!(ids(canvas), unfeatured);
    }

    fn caps_fixture_without_feature(tag: &[u8; 4], renamed: &[u8; 4]) -> FontFace {
        let parsed = ttf_parser::Face::parse(CAPS_FEATURE_FONT, 0).unwrap();
        assert!(parsed
            .tables()
            .gsub
            .unwrap()
            .features
            .find(ttf_parser::Tag::from_bytes(tag))
            .is_some());
        let raw = parsed
            .raw_face()
            .table(ttf_parser::Tag::from_bytes(b"GSUB"))
            .unwrap();
        let matches: Vec<_> = raw
            .windows(4)
            .enumerate()
            .filter_map(|(index, bytes)| (bytes == tag).then_some(index))
            .collect();
        assert_eq!(
            matches.len(),
            1,
            "fixture must contain one exact GSUB feature tag"
        );
        let offset = raw.as_ptr() as usize - CAPS_FEATURE_FONT.as_ptr() as usize + matches[0];
        let mut bytes = CAPS_FEATURE_FONT.to_vec();
        bytes[offset..offset + 4].copy_from_slice(renamed);
        let reparsed = ttf_parser::Face::parse(&bytes, 0).unwrap();
        assert!(reparsed
            .tables()
            .gsub
            .unwrap()
            .features
            .find(ttf_parser::Tag::from_bytes(tag))
            .is_none());
        assert!(reparsed
            .tables()
            .gsub
            .unwrap()
            .features
            .find(ttf_parser::Tag::from_bytes(renamed))
            .is_some());
        FontFace::new(bytes.into()).unwrap()
    }

    #[test]
    fn shared_caps_use_real_gsub_and_synthesize_only_missing_partial_features() {
        let face = FontFace::new(Arc::from(CAPS_FEATURE_FONT)).unwrap();
        assert_eq!(face.caps_features & 15, 15);
        let normal = FontSpec::default();
        let small = FontSpec {
            caps: CanvasFontVariantCaps::SmallCaps,
            ..normal.clone()
        };
        let native = face.shape_styled("J", 32.0, false, &small).unwrap();
        let reference = face.shape_styled("A", 32.0, false, &normal).unwrap();
        assert_eq!(
            native.glyphs[0].id, reference.glyphs[0].id,
            "real smcp maps fixture J to its check glyph"
        );
        assert_ne!(
            native.glyphs[0].id,
            face.shape_styled("J", 32.0, false, &normal).unwrap().glyphs[0].id
        );
        assert_eq!(native.glyphs[0].size_scale, 1.0);
        assert!(!face
            .outline(native.glyphs[0].id, 32.0)
            .unwrap()
            .commands
            .is_empty());
        let all = FontSpec {
            caps: CanvasFontVariantCaps::AllSmallCaps,
            ..normal.clone()
        };
        let missing_lower = caps_fixture_without_feature(b"smcp", b"smcq");
        assert_eq!(missing_lower.caps_features & 3, 2);
        let partial = missing_lower.shape_styled("kK", 32.0, false, &all).unwrap();
        let base_k = missing_lower
            .shape_styled("K", 32.0, false, &normal)
            .unwrap();
        let native_k = missing_lower.shape_styled("K", 32.0, false, &all).unwrap();
        assert_eq!(
            (partial.glyphs[0].id, partial.glyphs[0].size_scale),
            (base_k.glyphs[0].id, 0.8)
        );
        assert_eq!(
            (partial.glyphs[1].id, partial.glyphs[1].size_scale),
            (native_k.glyphs[0].id, 1.0)
        );
        assert_ne!(
            partial.glyphs[0].id, partial.glyphs[1].id,
            "synthetic lowercase must bypass real c2sc instead of shrinking it twice"
        );
        let alternating = missing_lower
            .shape_styled("KkKk", 32.0, false, &all)
            .unwrap();
        assert_eq!(alternating.glyphs.len(), 4);
        for (index, glyph) in alternating.glyphs.iter().enumerate() {
            let expected = if index % 2 == 0 {
                (native_k.glyphs[0].id, 1.0)
            } else {
                (base_k.glyphs[0].id, 0.8)
            };
            assert_eq!(
                (glyph.id, glyph.size_scale),
                expected,
                "disjoint single-byte synthetic ranges preserve neighboring native caps"
            );
            assert_eq!(glyph.cluster, index as u32);
        }
        let missing_upper = caps_fixture_without_feature(b"c2sc", b"c2sd");
        assert_eq!(missing_upper.caps_features & 3, 1);
        let partial = missing_upper.shape_styled("jJ", 32.0, false, &all).unwrap();
        assert_eq!(partial.glyphs[0].size_scale, 1.0);
        assert_eq!(
            (partial.glyphs[1].id, partial.glyphs[1].size_scale),
            (
                missing_upper
                    .shape_styled("J", 32.0, false, &normal)
                    .unwrap()
                    .glyphs[0]
                    .id,
                0.8
            )
        );
        let petite = FontSpec {
            caps: CanvasFontVariantCaps::AllPetiteCaps,
            ..normal.clone()
        };
        let missing_petite = caps_fixture_without_feature(b"pcap", b"pcaq");
        let run = missing_petite
            .shape_styled("J", 32.0, false, &petite)
            .unwrap();
        assert_eq!(
            run.glyphs[0].size_scale, 1.0,
            "missing petite feature reuses available real smcp"
        );
    }

    fn woff_fixture(compress: bool) -> Vec<u8> {
        let sfnt = DEFAULT_FONT_BYTES;
        let read16 = |at| u16::from_be_bytes(sfnt[at..at + 2].try_into().unwrap());
        let read32 = |at| u32::from_be_bytes(sfnt[at..at + 4].try_into().unwrap()) as usize;
        let count = read16(4) as usize;
        let mut tables: Vec<_> = (0..count)
            .map(|index| {
                let at = 12 + index * 16;
                (
                    sfnt[at..at + 4].to_vec(),
                    sfnt[at + 4..at + 8].to_vec(),
                    read32(at + 8),
                    read32(at + 12),
                )
            })
            .collect();
        tables.sort_by(|left, right| left.0.cmp(&right.0));
        let mut woff = vec![0; 44 + count * 20];
        woff[..4].copy_from_slice(b"wOFF");
        woff[4..8].copy_from_slice(&sfnt[..4]);
        woff[12..14].copy_from_slice(&(count as u16).to_be_bytes());
        let total = 12 + count * 16 + tables.iter().map(|table| (table.3 + 3) & !3).sum::<usize>();
        woff[16..20].copy_from_slice(&(total as u32).to_be_bytes());
        for (index, (tag, sum, start, len)) in tables.into_iter().enumerate() {
            let original = &sfnt[start..start + len];
            let compressed = if compress {
                lumen_common::compress::zlib_compress(original)
            } else {
                Vec::new()
            };
            let packed = if compress && compressed.len() < len {
                compressed.as_slice()
            } else {
                original
            };
            let offset = woff.len();
            let at = 44 + index * 20;
            woff[at..at + 4].copy_from_slice(&tag);
            woff[at + 4..at + 8].copy_from_slice(&(offset as u32).to_be_bytes());
            woff[at + 8..at + 12].copy_from_slice(&(packed.len() as u32).to_be_bytes());
            woff[at + 12..at + 16].copy_from_slice(&(len as u32).to_be_bytes());
            woff[at + 16..at + 20].copy_from_slice(&sum);
            woff.extend_from_slice(packed);
            woff.resize((woff.len() + 3) & !3, 0);
        }
        let len = woff.len();
        woff[8..12].copy_from_slice(&(len as u32).to_be_bytes());
        woff
    }

    #[test]
    fn woff_fonts_restore_real_shaping_and_reject_bad_containers() {
        let original = FontFace::new(Arc::from(DEFAULT_FONT_BYTES)).unwrap();
        for compress in [false, true] {
            let mut woff = woff_fixture(compress);
            let restored = FontFace::new(Arc::from(woff.as_slice())).unwrap();
            for text in ["office", "ABC xyz", "12345"] {
                let left = original.shape(text, 17.0).unwrap();
                let right = restored.shape(text, 17.0).unwrap();
                assert_eq!(left.width, right.width);
                assert_eq!(left.glyphs.len(), right.glyphs.len());
                for (left, right) in left.glyphs.iter().zip(right.glyphs.iter()) {
                    assert_eq!((left.id, left.x, left.y), (right.id, right.x, right.y));
                }
            }
            assert!(lumen_common::font::decode_woff(&woff, 100).is_err());
            // Corrupt an advertised checksum, then the reserved header field.
            woff[60] ^= 1;
            assert!(FontFace::new(Arc::from(woff.as_slice())).is_err());
            woff[60] ^= 1;
            woff[14] = 1;
            assert!(FontFace::new(Arc::from(woff.as_slice())).is_err());
            assert!(lumen_common::font::decode_woff(&woff[..20], MAX_FONT_BYTES).is_err());
        }
    }

    #[test]
    fn woff2_faces_decode_valid_and_transformed_wpt_fonts() {
        for bytes in [
            &include_bytes!("../../../vendor/wuff/tests/fixtures/valid-001.woff2")[..],
            &include_bytes!(
                "../../../vendor/wuff/tests/fixtures/tabledata-glyf-origlength-003.woff2"
            )[..],
        ] {
            let face = FontFace::new(Arc::from(bytes)).unwrap();
            assert!(face.glyph_count > 0);
            let shaped = face.shape("A", 12.0).unwrap();
            assert!(shaped.width.is_finite());
        }
        let overlap =
            include_bytes!("../../../vendor/wuff/tests/fixtures/blocks-overlap-002.woff2");
        assert!(FontFace::new(Arc::from(overlap.as_slice())).is_err());
    }

    #[test]
    fn registry_snapshot_builds_loaded_css_and_manual_registrations() {
        let fallback_set = FontSet::new(vec![registered(
            "fallback",
            400,
            FontStyle::Normal,
            DEFAULT_FONT_BYTES,
        )])
        .unwrap();
        let fallback = fallback_set.registrations().unwrap();
        let css_rule = lumen_html::css::parse_font_faces(
            "@font-face { font-family: Loaded; src: url(loaded.woff2); font-weight: 300 700; font-stretch: 75% 125%; size-adjust: 110%; unicode-range: U+0041; }",
        )
        .unwrap()
        .remove(0);
        let decoded = Arc::new(FontFace::new(Arc::from(TEST_FONT_BYTES)).unwrap());
        let mut manual_rule = lumen_html::css::parse_font_faces(
            "@font-face { font-family: Manual; src: url(manual.woff2); unicode-range: U+0042; }",
        )
        .unwrap()
        .remove(0);
        let manual_identity = lumen_html::css::FontFaceIdentity::Manual(7);
        manual_rule.identity = Some(manual_identity.clone());
        let manual = ManualFontFace {
            identity: manual_identity,
            rule: manual_rule,
            status: FontFaceStatus::Loaded,
            decoded: Some(decoded.clone()),
            byte_length: Some(TEST_FONT_BYTES.len()),
        };
        let mut unloaded_rule = manual.rule.clone();
        let unloaded_identity = lumen_html::css::FontFaceIdentity::Manual(8);
        unloaded_rule.identity = Some(unloaded_identity.clone());
        unloaded_rule.family = Arc::from("Unloaded");
        let unloaded = ManualFontFace {
            identity: unloaded_identity,
            rule: unloaded_rule,
            status: FontFaceStatus::Unloaded,
            decoded: None,
            byte_length: None,
        };

        let set = FontSet::from_font_registry_snapshot(
            &fallback,
            &[css_rule],
            &[Some(decoded)],
            &[manual, unloaded],
        )
        .unwrap();
        let registrations = set.registrations().unwrap();
        assert_eq!(registrations.len(), 3);
        assert_eq!(registrations[0].font.family.as_ref(), "fallback");
        assert_eq!(registrations[1].font.family.as_ref(), "Loaded");
        assert_eq!(
            registrations[1].unicode_range.as_deref().unwrap(),
            &[(0x41, 0x41)]
        );
        assert_eq!(registrations[1].descriptors.weight_range, [300, 700]);
        assert_eq!(registrations[1].descriptors.stretch_range, [75.0, 125.0]);
        assert!((registrations[1].descriptors.size_adjust - 1.1).abs() < f32::EPSILON);
        assert_eq!(registrations[2].font.family.as_ref(), "Manual");
        assert_eq!(
            registrations[2].unicode_range.as_deref().unwrap(),
            &[(0x42, 0x42)]
        );
    }

    #[test]
    fn resolved_run_cache_separates_fonts_and_paragraph_direction_modes() {
        let set = FontSet::new(vec![
            registered("mono", 400, FontStyle::Normal, DEFAULT_FONT_BYTES),
            registered("sans", 400, FontStyle::Normal, TEST_FONT_BYTES),
        ])
        .unwrap();
        let mono = FontSpec {
            families: Some(Arc::from([lumen_html::paint::FontFamily::from("mono")])),
            ..FontSpec::default()
        };
        let sans = FontSpec {
            families: Some(Arc::from([lumen_html::paint::FontFamily::from("sans")])),
            ..FontSpec::default()
        };
        let text = "office אב 123";
        let first = set.shape_resolved(text, 18.0, false, &mono).unwrap();
        let again = set.shape_resolved(text, 18.0, false, &mono).unwrap();
        assert!(Arc::ptr_eq(&first.glyphs, &again.glyphs));
        assert_eq!(set.shape_cache_stats().hits, 1);
        let other = set.shape_resolved(text, 18.0, false, &sans).unwrap();
        assert!(!Arc::ptr_eq(&first.glyphs, &other.glyphs));
        assert_ne!(first.glyphs[0].face, other.glyphs[0].face);
        let paragraph = set.shape_styled(text, 18.0, false, &mono).unwrap();
        assert!(!Arc::ptr_eq(&first.glyphs, &paragraph.glyphs));
        let resized = set.shape_resolved(text, 20.0, false, &mono).unwrap();
        assert_ne!(first.width, resized.width);
        assert_eq!(set.shape_cache_stats().entries, 4);
    }

    #[test]
    fn shape_cache_eviction_enforces_retained_byte_budget() {
        let cache = RunCache::new();
        let run = ShapedRun {
            glyphs: Arc::from(vec![
                Glyph {
                    face: 0,
                    id: 1,
                    cluster: 0,
                    caps_expansion: 0,
                    x: 0.0,
                    y: 0.0,
                    size_scale: 1.0,
                };
                256
            ]),
            width: 256.0,
        };
        for index in 0..2000 {
            let text = alloc::format!("bounded-run-{index}");
            cache.insert(&text, 16.0, None, run.clone());
            assert!(cache.stats().bytes <= MAX_RUN_CACHE_BYTES);
        }
        assert!(cache.stats().evictions > 0);
        cache.insert("last", 16.0, None, run.clone());
        assert!(Arc::ptr_eq(
            &cache.get("last", 16.0, None).unwrap().glyphs,
            &run.glyphs
        ));
        assert!(cache.get("last", 17.0, None).is_none());
        assert!(cache.get("last", 16.0, Some(true)).is_none());
        let entries = cache.stats().entries;
        cache.insert(&"x".repeat(MAX_CACHED_TEXT_BYTES + 1), 16.0, None, run);
        assert_eq!(cache.stats().entries, entries);
    }

    fn registered(family: &str, weight: u16, style: FontStyle, bytes: &[u8]) -> RegisteredFont {
        RegisteredFont {
            stretch: 100.0,
            family: Arc::from(family),
            weight,
            style,
            face: Arc::new(FontFace::new(Arc::from(bytes)).unwrap()),
        }
    }

    #[test]
    fn metric_overrides_preserve_shapes_and_outlines() {
        let face = FontFace::new(Arc::from(DEFAULT_FONT_BYTES)).unwrap();
        let before = face.shape("metrics", 20.0).unwrap();
        let outline = face.outline(before.glyphs[0].id, 20.0).unwrap();
        let face = face.with_metric_overrides(Some(0.8), Some(0.2)).unwrap();
        assert_eq!(face.ascent(20.0), 16.0);
        assert_eq!(face.line_height(20.0), 20.0);
        assert_eq!(face.shape("metrics", 20.0).unwrap(), before);
        assert_eq!(face.outline(before.glyphs[0].id, 20.0).unwrap(), outline);
        assert!(FontFace::new(Arc::from(DEFAULT_FONT_BYTES))
            .unwrap()
            .with_metric_overrides(Some(f32::NAN), None)
            .is_err());
    }

    #[test]
    fn font_size_adjust_scales_glyphs_metrics_and_font_relative_units() {
        let face = FontFace::new(Arc::from(TEST_FONT_BYTES)).unwrap();
        let ex_height = face.metric(FontMetric::ExHeight).unwrap();
        let size = 24.0;
        let base = face.shape("Aa", size).unwrap();
        let spec = FontSpec {
            size_adjust: Some(FontSizeAdjust {
                metric: FontMetric::ExHeight,
                value: FontSizeAdjustValue::Number(ex_height * 1.5),
            }),
            ..FontSpec::default()
        };
        let adjusted = face.shape_styled("Aa", size, false, &spec).unwrap();
        assert!((adjusted.width - base.width * 1.5).abs() < 0.001);
        assert!(adjusted
            .glyphs
            .iter()
            .all(|glyph| (glyph.size_scale - 1.5).abs() < 0.001));
        assert!((face.ascent_styled(size, &spec) - face.ascent(size * 1.5)).abs() < 0.001);
        assert!(
            (face.line_height_styled(size, &spec) - face.line_height(size * 1.5)).abs() < 0.001
        );
        let relative = face.font_relative_metrics_styled(size, &spec);
        assert!((relative.ex - size * ex_height * 1.5).abs() < 0.001);
        assert!(
            (relative.ch - size * face.metric(FontMetric::ChWidth).unwrap() * 1.5).abs() < 0.001
        );

        let from_font = FontSpec {
            size_adjust: Some(FontSizeAdjust {
                metric: FontMetric::ExHeight,
                value: FontSizeAdjustValue::FromFont,
            }),
            ..FontSpec::default()
        };
        assert_eq!(
            face.shape_styled("Aa", size, false, &from_font).unwrap(),
            base
        );
    }

    #[test]
    fn font_size_adjust_uses_first_available_font_for_fallback_glyphs() {
        let primary = registered("Shared", 400, FontStyle::Normal, TEST_FONT_BYTES);
        let fallback = registered("Shared", 400, FontStyle::Normal, DEFAULT_FONT_BYTES);
        let installed = registered("serif", 400, FontStyle::Normal, DEFAULT_FONT_BYTES);
        let primary_metric = primary.face.metric(FontMetric::ExHeight).unwrap();
        let fallback_face = fallback.face.clone();
        let fallback_id = fallback.face.id();
        let primary_face = primary.face.clone();
        let fonts = FontSet::new_with_unicode_ranges(
            vec![primary, fallback, installed],
            vec![
                Some(Arc::from([(0x20, 0x20), (0x41, 0x41)])),
                Some(Arc::from([(0x42, 0x42)])),
                None,
            ],
        )
        .unwrap();
        let spec = FontSpec {
            families: Some(Arc::from([lumen_html::paint::FontFamily::from("Shared")])),
            size_adjust: Some(FontSizeAdjust {
                metric: FontMetric::ExHeight,
                value: FontSizeAdjustValue::FromFont,
            }),
            ..FontSpec::default()
        };
        let scale = primary_metric / fallback_face.metric(FontMetric::ExHeight).unwrap();
        let run = fonts.shape_resolved("B", 24.0, false, &spec).unwrap();
        assert!(!run.glyphs.is_empty());
        assert!(run.glyphs.iter().all(|glyph| glyph.face == fallback_id));
        assert!(run
            .glyphs
            .iter()
            .all(|glyph| (glyph.size_scale - scale).abs() < 0.001));
        assert!((run.width - fallback_face.shape("B", 24.0 * scale).unwrap().width).abs() < 0.001);
        let metrics = fonts.font_relative_metrics_styled(24.0, &spec);
        assert!((metrics.ex - 24.0 * primary_metric).abs() < 0.001);
        assert!((fonts.ascent_styled(24.0, &spec) - primary_face.ascent(24.0)).abs() < 0.001);
    }

    #[test]
    fn quoted_generic_family_selects_named_web_font_and_has_distinct_shape_cache_identity() {
        let web = registered("serif", 400, FontStyle::Normal, TEST_FONT_BYTES);
        let installed = registered("Fallback", 400, FontStyle::Normal, DEFAULT_FONT_BYTES);
        let web_id = web.face.id();
        let installed_id = installed.face.id();
        let fonts = FontSet::new_with_unicode_ranges(vec![web, installed],
            vec![Some(Arc::from([(0x41, 0x41)])), None]).unwrap();
        let generic = lumen_html::css::parse_font_shorthand("16px serif").unwrap();
        let named = lumen_html::css::parse_font_shorthand(r#"16px "serif""#).unwrap();
        for (spec, expected) in [(&generic, installed_id), (&named, web_id),
            (&generic, installed_id), (&named, web_id)] {
            let run = fonts.shape_resolved("A", 16.0, false, spec).unwrap();
            assert!(!run.glyphs.is_empty());
            assert!(run.glyphs.iter().all(|glyph| glyph.face == expected));
        }
    }

    #[test]
    fn font_face_size_adjust_scales_geometry_and_first_available_metrics() {
        let primary = registered("Shared", 400, FontStyle::Normal, TEST_FONT_BYTES);
        let fallback = registered("Shared", 400, FontStyle::Normal, DEFAULT_FONT_BYTES);
        let installed = registered("serif", 400, FontStyle::Normal, DEFAULT_FONT_BYTES);
        let primary_metric = primary.face.metric(FontMetric::ExHeight).unwrap();
        let fallback_face = fallback.face.clone();
        let fallback_id = fallback.face.id();
        let primary_id = primary.face.id();
        let descriptors = vec![
            RegisteredFontDescriptors {
                oblique_range: None,
                weight_range: [300, 700],
                stretch_range: [75.0, 125.0],
                size_adjust: 1.5,
                feature_settings: None,
                family_scope: None,
                feature_values: None,
            },
            RegisteredFontDescriptors::scalar(&fallback, 1.0),
            RegisteredFontDescriptors::scalar(&installed, 1.0),
        ];
        let fonts = FontSet::new_with_unicode_ranges_and_descriptors(
            vec![primary, fallback, installed],
            vec![
                Some(Arc::from([(0x20, 0x20), (0x41, 0x41)])),
                Some(Arc::from([(0x42, 0x42)])),
                None,
            ],
            descriptors,
        )
        .unwrap();
        let spec = FontSpec {
            families: Some(Arc::from([lumen_html::paint::FontFamily::from("Shared")])),
            size_adjust: Some(FontSizeAdjust {
                metric: FontMetric::ExHeight,
                value: FontSizeAdjustValue::FromFont,
            }),
            ..FontSpec::default()
        };
        assert!(
            (fonts
                .first_available_metric(&spec, FontMetric::ExHeight)
                .unwrap()
                - primary_metric * 1.5)
                .abs()
                < 0.001
        );

        let primary_run = fonts.shape_resolved("A", 20.0, false, &spec).unwrap();
        assert!(primary_run
            .glyphs
            .iter()
            .all(|glyph| glyph.face == primary_id && (glyph.size_scale - 1.5).abs() < 0.001));
        let fallback_run = fonts.shape_resolved("B", 20.0, false, &spec).unwrap();
        let expected = primary_metric * 1.5 / fallback_face.metric(FontMetric::ExHeight).unwrap();
        assert!(fallback_run.glyphs.iter().all(|glyph| {
            glyph.face == fallback_id && (glyph.size_scale - expected).abs() < 0.001
        }));
    }

    #[test]
    fn web_fonts_respect_matched_width_and_stay_out_of_system_fallback() {
        let platform = registered("Fallback", 400, FontStyle::Normal, TEST_FONT_BYTES);
        let mut narrow = registered("Web", 400, FontStyle::Normal, DEFAULT_FONT_BYTES);
        narrow.stretch = 75.0;
        let normal = registered("Web", 400, FontStyle::Normal, DEFAULT_FONT_BYTES);
        let platform_id = platform.face.id();
        let narrow_id = narrow.face.id();
        let fonts = FontSet::new_with_unicode_ranges(
            vec![platform, narrow, normal],
            vec![
                None,
                Some(Arc::from([(0x41, 0x41)])),
                Some(Arc::from([(0x42, 0x42)])),
            ],
        )
        .unwrap();
        let spec = FontSpec {
            families: Some(Arc::from([lumen_html::paint::FontFamily::from("Web"), lumen_html::paint::FontFamily::from("Fallback")])),
            stretch: 87.5,
            ..FontSpec::default()
        };
        assert_eq!(
            fonts
                .shape_resolved("A", 20.0, false, &spec)
                .unwrap()
                .glyphs[0]
                .face,
            narrow_id
        );
        assert_eq!(
            fonts
                .shape_resolved("B", 20.0, false, &spec)
                .unwrap()
                .glyphs[0]
                .face,
            platform_id
        );
        let unknown = FontSpec {
            families: Some(Arc::from([lumen_html::paint::FontFamily::from("Unknown")])),
            ..FontSpec::default()
        };
        assert_eq!(
            fonts
                .shape_resolved("A", 20.0, false, &unknown)
                .unwrap()
                .glyphs[0]
                .face,
            platform_id
        );
    }

    #[test]
    fn width_matching_precedes_style_weight_and_separates_cached_runs() {
        let mut narrow = registered("Width", 700, FontStyle::Italic, DEFAULT_FONT_BYTES);
        narrow.stretch = 75.0;
        let regular = registered("Width", 400, FontStyle::Normal, TEST_FONT_BYTES);
        let mut wide = registered("Width", 700, FontStyle::Italic, DEFAULT_FONT_BYTES);
        wide.stretch = 125.0;
        let ids = [narrow.face.id(), regular.face.id(), wide.face.id()];
        let fonts = FontSet::new(vec![narrow, regular, wide]).unwrap();
        for (stretch, expected) in [(50.0, 0), (87.5, 0), (100.0, 1), (112.5, 2), (200.0, 2)] {
            let spec = FontSpec {
                families: Some(Arc::from([lumen_html::paint::FontFamily::from("Width")])),
                stretch,
                ..FontSpec::default()
            };
            let run = fonts.shape_resolved("width", 20.0, false, &spec).unwrap();
            assert!(
                run.glyphs.iter().all(|glyph| glyph.face == ids[expected]),
                "{stretch}"
            );
            assert_eq!(
                fonts.shape_resolved("width", 20.0, false, &spec).unwrap(),
                run
            );
        }
    }

    #[test]
    fn line_metrics_skip_web_faces_excluding_space() {
        let platform = registered("Fallback", 400, FontStyle::Normal, TEST_FONT_BYTES);
        let web = registered("Subset", 400, FontStyle::Normal, DEFAULT_FONT_BYTES);
        let fallback_id = platform.face.id();
        let web_id = web.face.id();
        let fonts = FontSet::new_with_unicode_ranges(
            vec![platform, web],
            vec![None, Some(Arc::from([(0x41, 0x5a)]))],
        )
        .unwrap();
        let spec = FontSpec {
            families: Some(Arc::from([lumen_html::paint::FontFamily::from("Subset"), lumen_html::paint::FontFamily::from("Fallback")])),
            ..FontSpec::default()
        };
        assert_eq!(fonts.metric_face(&spec).id(), fallback_id);
        assert_eq!(
            fonts
                .shape_resolved("A", 20.0, false, &spec)
                .unwrap()
                .glyphs[0]
                .face,
            web_id
        );
        let mut with_space = fonts;
        with_space.unicode_ranges[1] = Some(Arc::from([(0x20, 0x5a)]));
        assert_eq!(with_space.metric_face(&spec).id(), web_id);
    }

    #[test]
    fn unicode_ranges_filter_web_faces_and_choose_last_matching_rule() {
        let platform = registered("sans-serif", 400, FontStyle::Normal, TEST_FONT_BYTES);
        let first = registered("Composite", 400, FontStyle::Normal, DEFAULT_FONT_BYTES);
        let last = registered("Composite", 400, FontStyle::Normal, TEST_FONT_BYTES);
        let ids = (platform.face.id(), first.face.id(), last.face.id());
        let fonts = FontSet::new_with_unicode_ranges(
            vec![platform, first, last],
            vec![
                None,
                Some(Arc::from([(0x41, 0x5a)])),
                Some(Arc::from([(0x42, 0x42)])),
            ],
        )
        .unwrap();
        let spec = FontSpec {
            families: Some(Arc::from([lumen_html::paint::FontFamily::from("Composite")])),
            ..FontSpec::default()
        };
        let run = fonts.shape_resolved("ABz", 16.0, false, &spec).unwrap();
        assert_eq!(
            run.glyphs
                .iter()
                .map(|glyph| glyph.face)
                .collect::<Vec<_>>(),
            [ids.1, ids.2, ids.0]
        );
        assert!(fonts.face_covers(1, "A"));
        assert!(!fonts.face_covers(1, "z"));
    }

    #[test]
    fn bounds_font_and_shape_input() {
        let oversized: Arc<[u8]> = alloc::vec![0; MAX_FONT_BYTES + 1].into();
        assert!(matches!(FontFace::new(oversized), Err("font too large")));
        let font = FontFace::new(Arc::from(DEFAULT_FONT_BYTES)).unwrap();
        let oversized_text = "a".repeat(MAX_SHAPE_TEXT_BYTES + 1);
        assert!(matches!(
            font.shape(&oversized_text, 16.0),
            Err("text run too large")
        ));
        let proportional = FontFace::new(Arc::from(TEST_FONT_BYTES)).unwrap();
        for text in ["Hello world", "office", "\u{05e9}\u{05dc}\u{05d5}\u{05dd}"] {
            assert_eq!(
                proportional.measure(text, 17.5).unwrap(),
                proportional.shape(text, 17.5).unwrap().width
            );
        }
    }

    #[test]
    fn glyph_scoped_rasterizer_matches_full_font_glyphs_and_ligatures() {
        let bytes: Arc<[u8]> = Arc::from(TEST_FONT_BYTES);
        let face = FontFace::new(bytes.clone()).unwrap();
        let reference = fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default()).unwrap();
        for size in [12.0, 16.0, 24.0, 32.0, 48.0] {
            for text in ["ABQO", "office", "gyp", "123"] {
                for glyph in face.shape(text, size).unwrap().glyphs.iter() {
                    let (metrics, alpha) = reference.rasterize_indexed(glyph.id, size);
                    assert_eq!(
                        face.rasterize(glyph.id, size).unwrap(),
                        GlyphCoverage {
                            x_min: metrics.xmin,
                            y_min: metrics.ymin,
                            width: metrics.width,
                            height: metrics.height,
                            alpha,
                        },
                        "glyph {} at {size}px from {text:?}",
                        glyph.id,
                    );
                }
            }
        }

        assert!(face.glyph_count > RASTERIZER_BLOCK_GLYPHS * 2);
        for id in [0, 255, 256, 512, 0] {
            let (metrics, alpha) = reference.rasterize_indexed(id, 24.0);
            assert_eq!(
                face.rasterize(id, 24.0).unwrap(),
                GlyphCoverage {
                    x_min: metrics.xmin,
                    y_min: metrics.ymin,
                    width: metrics.width,
                    height: metrics.height,
                    alpha,
                },
                "block boundary glyph {id}",
            );
        }
        assert_eq!(face.rasterizers.cached_blocks(), RASTERIZER_CACHE_BLOCKS);
    }

    #[test]
    fn mixed_direction_preserves_numbers_and_shapes_logical_hebrew() {
        let font = FontFace::new(Arc::from(TEST_FONT_BYTES)).unwrap();
        let face = ttf_parser::Face::parse(TEST_FONT_BYTES, 0).unwrap();
        let ids = |text: &str| {
            text.chars()
                .map(|ch| face.glyph_index(ch).unwrap().0)
                .collect::<Vec<_>>()
        };
        for (rtl, expected) in [(false, "a12בא"), (true, "12באa")] {
            let shaped = font.shape_with_direction("aאב12", 18.0, Some(rtl)).unwrap();
            assert_eq!(
                shaped
                    .glyphs
                    .iter()
                    .map(|glyph| glyph.id)
                    .collect::<Vec<_>>(),
                ids(expected)
            );
            assert!(shaped.glyphs.windows(2).all(|pair| pair[0].x <= pair[1].x));
        }
    }

    #[test]
    fn specification_discarded_bidi_whitespace_keeps_generated_mirrored_ink() {
        use lumen_html::paint::Command;
        let fonts=FontSet::new(vec![registered("sans-serif",400,FontStyle::Normal,TEST_FONT_BYTES)]).unwrap();
        let ink=|markup:&str| {
            let document=lumen_html::html::parse(markup,64).unwrap();
            let list=lumen_html::layout::display_list(&document,100,60,&fonts).unwrap();
            list.0.iter().filter_map(|command|match command {
                Command::GlyphRun{glyphs,origin_x,baseline_y,..}=>Some(glyphs.iter().map(|glyph|
                    (glyph.id,*origin_x+glyph.x,*baseline_y+glyph.y))),_=>None,
            }).flatten().collect::<Vec<_>>()
        };
        let generated=ink("<style>body{margin:0;direction:rtl;text-align:left}.a:before{content:'('}.a:after{content:')'}.b:after{content:''}</style><body><span class=a><span class=b></span></span></body>\n");
        let literal=ink("<style>body{margin:0}</style><body>()</body>\n");
        assert_eq!(generated.len(),2,"both actual bracket glyphs survive empty generated scopes");
        assert_eq!(generated,literal,"UAX9 reversal plus real backend mirroring preserves exact punctuation ink and trailing-space alignment");
    }

    #[test]
    fn rtl_shaping_mirrors_brackets_and_keeps_combining_clusters() {
        let font = FontFace::new(Arc::from(TEST_FONT_BYTES)).unwrap();
        let face = ttf_parser::Face::parse(TEST_FONT_BYTES, 0).unwrap();
        let shaped = font.shape_with_direction("(אב)", 18.0, Some(true)).unwrap();
        assert_eq!(shaped.glyphs[0].id, face.glyph_index('(').unwrap().0);
        assert_eq!(
            shaped.glyphs.last().unwrap().id,
            face.glyph_index(')').unwrap().0
        );
        let marked = font
            .shape_with_direction("א\u{05b7}ב", 18.0, Some(true))
            .unwrap();
        assert_eq!(marked.glyphs[0].id, face.glyph_index('ב').unwrap().0);
        assert!(marked.width > 0.0);
    }

    #[test]
    fn font_selection_obeys_family_style_and_css_weight_order() {
        let second_family = registered("Second", 400, FontStyle::Normal, TEST_FONT_BYTES);
        let first_regular = registered("First", 400, FontStyle::Normal, DEFAULT_FONT_BYTES);
        let first_medium = registered("First", 500, FontStyle::Normal, TEST_FONT_BYTES);
        let first_italic = registered("First", 700, FontStyle::Italic, DEFAULT_FONT_BYTES);
        let second_id = second_family.face.id();
        let regular_id = first_regular.face.id();
        let medium_id = first_medium.face.id();
        let italic_id = first_italic.face.id();
        let set = FontSet::new(vec![
            first_regular,
            first_medium,
            first_italic,
            second_family,
        ])
        .unwrap();

        let shape = |families: &[&str], weight, style| {
            let font = FontSpec {
                families: Some(
                    families
                        .iter()
                        .map(|name| lumen_html::paint::FontFamily::from(*name))
                        .collect::<Vec<_>>()
                        .into(),
                ),
                weight,
                style,
                stretch: 100.0,
                size_adjust: None,
                ..FontSpec::default()
            };
            set.shape_resolved("A", 18.0, false, &font).unwrap()
        };
        assert_eq!(
            shape(&["Second", "First"], 400, FontStyle::Normal).glyphs[0].face,
            second_id
        );
        assert_eq!(
            shape(&["First"], 400, FontStyle::Normal).glyphs[0].face,
            regular_id
        );
        assert_eq!(
            shape(&["First"], 450, FontStyle::Normal).glyphs[0].face,
            medium_id
        );
        assert_eq!(
            shape(&["First"], 700, FontStyle::Italic).glyphs[0].face,
            italic_id
        );
        // Style matching is applied before weight matching within one family.
        assert_eq!(
            shape(&["First"], 700, FontStyle::Normal).glyphs[0].face,
            medium_id
        );
        assert_ne!(medium_id, italic_id);
    }

    #[test]
    fn registered_static_width_faces_share_selection_metrics_glyphs_and_raster_identity() {
        let regular = Arc::new(FontFace::new(Arc::from(TEST_FONT_BYTES)).unwrap());
        let bold = Arc::new(FontFace::new(Arc::from(TEST_FONT_BOLD_BYTES)).unwrap());
        let set = FontSet::new(vec![
            RegisteredFont {family:Arc::from("WidthFixture"),stretch:75.0,weight:400,style:FontStyle::Normal,face:regular.clone()},
            RegisteredFont {family:Arc::from("WidthFixture"),stretch:125.0,weight:700,style:FontStyle::Normal,face:bold.clone()},
        ]).unwrap();
        for (stretch,weight,face) in [(100.0,700,&regular),(101.0,400,&bold)] {
            let spec = FontSpec {families:Some(vec![FontFamily::from("WidthFixture")].into()),stretch,weight,..FontSpec::default()};
            let run = set.shape_styled("ink",40.0,false,&spec).unwrap();
            assert!(run.glyphs.iter().all(|glyph| glyph.face == face.id()),"width {stretch} must select a real static face before weight matching");
            assert_eq!(run.width,face.shape_styled("ink",40.0,false,&FontSpec::default()).unwrap().width);
            assert_eq!(set.font_relative_metrics_styled(40.0,&spec),face.font_relative_metrics_styled(40.0,&FontSpec::default()));
            let glyph = run.glyphs[0];
            assert_eq!(set.rasterize_glyph(glyph.face,glyph.id,40.0).unwrap(),face.rasterize(glyph.id,40.0).unwrap());
            assert_eq!(set.shape_styled("ink",40.0,false,&spec).unwrap(),run,"styled cache must retain width-selected face identity");
        }
        let first = set.shape_styled("ink",40.0,false,&FontSpec {families:Some(vec![FontFamily::from("WidthFixture")].into()),stretch:100.0,..FontSpec::default()}).unwrap();
        let second = set.shape_styled("ink",40.0,false,&FontSpec {families:Some(vec![FontFamily::from("WidthFixture")].into()),stretch:101.0,..FontSpec::default()}).unwrap();
        assert_ne!(first.glyphs[0].face,second.glyphs[0].face);
        let rules = lumen_html::css::parse_font_faces("@font-face{font-family:WebWidth;src:url(width.ttf);font-width:calc(100% + sign(20cqw - 10px)*25%)}").unwrap();
        let fallback = FontRegistration {font:set.faces[0].clone(),unicode_range:None,descriptors:RegisteredFontDescriptors::scalar(&set.faces[0],1.0)};
        assert!(FontSet::from_font_registry_snapshot(core::slice::from_ref(&fallback),&rules,&[Some(bold.clone())],&[]).is_err());
        let query = lumen_html::css::ContainerUnitContext::no_container(lumen_html::css::MediaEnvironment {width:10.0,height:600.0,..lumen_html::css::MediaEnvironment::default()});
        let snapshot = FontSet::from_font_registry_snapshot_with_query(core::slice::from_ref(&fallback),&rules,&[Some(bold)],&[],query).unwrap();
        let registration = snapshot.registrations().unwrap().remove(1);
        assert_eq!(registration.descriptors.stretch_range,[75.0,75.0]);
        assert!(rules[0].stretch_expressions.is_some(),"snapshot computation must preserve specified descriptor math");
    }

    #[test]
    fn registered_bold_face_is_selected_and_rasterized_by_face_identity() {
        let regular = Arc::new(FontFace::new(Arc::from(TEST_FONT_BYTES)).unwrap());
        let bold = Arc::new(FontFace::new(Arc::from(TEST_FONT_BOLD_BYTES)).unwrap());
        let regular_id = regular.id();
        let bold_id = bold.id();
        let set = FontSet::new(vec![
            RegisteredFont {
                stretch: 100.0,
                family: Arc::from("LumenFixture"),
                weight: 400,
                style: FontStyle::Normal,
                face: regular.clone(),
            },
            RegisteredFont {
                stretch: 100.0,
                family: Arc::from("LumenFixture"),
                weight: 700,
                style: FontStyle::Normal,
                face: bold.clone(),
            },
        ])
        .unwrap();
        let spec = |weight| FontSpec {
            families: Some(vec![lumen_html::paint::FontFamily::from("LumenFixture")].into()),
            weight,
            ..FontSpec::default()
        };

        let normal = set.shape_resolved("ink", 40.0, false, &spec(400)).unwrap();
        let strong = set.shape_resolved("ink", 40.0, false, &spec(700)).unwrap();
        assert!(normal.glyphs.iter().all(|glyph| glyph.face == regular_id));
        assert!(strong.glyphs.iter().all(|glyph| glyph.face == bold_id));
        assert_eq!(set.face_key(strong.glyphs[0].face), Ok(bold_id));

        let normal_glyph = normal.glyphs[0];
        let bold_glyph = strong.glyphs[0];
        let regular_coverage = set
            .rasterize_glyph(regular_id, normal_glyph.id, 40.0)
            .unwrap();
        let bold_coverage = set.rasterize_glyph(bold_id, bold_glyph.id, 40.0).unwrap();
        assert_ne!(regular_coverage, bold_coverage);
    }

    #[test]
    fn quoted_family_whitespace_is_preserved_during_matching() {
        let padded = registered(" Family ", 400, FontStyle::Normal, TEST_FONT_BYTES);
        let plain = registered("Family", 400, FontStyle::Normal, DEFAULT_FONT_BYTES);
        let padded_id = padded.face.id();
        let plain_id = plain.face.id();
        let set = FontSet::new(vec![padded, plain]).unwrap();

        let shape = |family: &str| {
            let font = FontSpec {
                families: Some(vec![lumen_html::paint::FontFamily::from(family)].into()),
                ..FontSpec::default()
            };
            set.shape_resolved("A", 18.0, false, &font).unwrap()
        };

        assert_eq!(shape(" Family ").glyphs[0].face, padded_id);
        assert_eq!(shape("Family").glyphs[0].face, plain_id);
        // An unmatched family follows the normal registration fallback path.
        assert_eq!(shape("  Family  ").glyphs[0].face, padded_id);
    }

    #[test]
    fn fallback_keeps_each_grapheme_whole_and_groups_adjacent_clusters() {
        let primary = registered("Inconsolata", 400, FontStyle::Normal, DEFAULT_FONT_BYTES);
        let fallback = registered("Liberation Sans", 400, FontStyle::Normal, TEST_FONT_BYTES);
        let primary_id = primary.face.id();
        let fallback_id = fallback.face.id();
        let set = FontSet::new(vec![primary, fallback]).unwrap();
        let font = FontSpec {
            families: Some(
                vec![
                    lumen_html::paint::FontFamily::from("Inconsolata"),
                    lumen_html::paint::FontFamily::from("Liberation Sans"),
                ]
                .into(),
            ),
            ..FontSpec::default()
        };

        let hebrew = set
            .shape_resolved("ש\u{05b7}לום", 18.0, true, &font)
            .unwrap();
        assert!(!hebrew.glyphs.is_empty());
        assert!(hebrew.glyphs.iter().all(|glyph| glyph.face == fallback_id));

        let variation_selector = set.shape_resolved("a\u{fe0f}", 18.0, false, &font).unwrap();
        assert!(variation_selector
            .glyphs
            .iter()
            .all(|glyph| glyph.face == primary_id));

        let liberation = FontSpec {
            families: Some(vec![lumen_html::paint::FontFamily::from("Liberation Sans")].into()),
            ..font.clone()
        };
        let latin = set
            .shape_resolved("office", 18.0, false, &liberation)
            .unwrap();
        assert!(latin.glyphs.iter().all(|glyph| glyph.face == fallback_id));
        let expected = set.faces[1]
            .face
            .shape_resolved_run_with_advances("office", 18.0, false, None)
            .unwrap();
        assert_eq!(latin.width, expected.width);
        assert_eq!(
            latin
                .glyphs
                .iter()
                .map(|glyph| glyph.id)
                .collect::<Vec<_>>(),
            expected
                .glyphs
                .iter()
                .map(|glyph| glyph.id)
                .collect::<Vec<_>>()
        );
        assert_ne!(primary_id, fallback_id);
    }

    #[test]
    fn cluster_advance_sidecar_preserves_shaping_fallback_and_logical_graphemes() {
        fn assert_source_partition(text: &str, detail: &ShapedRunWithClusterAdvances) {
            assert!(!detail.clusters.is_empty());
            let mut source_end = 0;
            let mut total_advance = 0.0;
            for cluster in detail.clusters.iter() {
                assert_eq!(cluster.source.start, source_end);
                assert!(cluster.source.start < cluster.source.end);
                assert!(text.is_char_boundary(cluster.source.start));
                assert!(text.is_char_boundary(cluster.source.end));
                assert!(graphemes(text).any(|(start, grapheme)| {
                    start == cluster.source.start || start + grapheme.len() == cluster.source.start
                }));
                assert!(graphemes(text).any(|(start, grapheme)| {
                    start == cluster.source.end || start + grapheme.len() == cluster.source.end
                }));
                total_advance += cluster.advance;
                source_end = cluster.source.end;
            }
            assert_eq!(source_end, text.len());
            assert!((total_advance - detail.run.width).abs() < 0.02);
        }

        let face = FontFace::new(Arc::from(TEST_FONT_BYTES)).unwrap();
        let latin = "office";
        let ordinary = face
            .shape_resolved(latin, 18.0, false, &FontSpec::default())
            .unwrap();
        let detail = face
            .shape_resolved_with_cluster_advances(latin, 18.0, false, &FontSpec::default())
            .unwrap()
            .unwrap();
        assert_eq!(detail.run, ordinary);
        assert_source_partition(latin, &detail);

        let primary = registered("Primary", 400, FontStyle::Normal, DEFAULT_FONT_BYTES);
        let fallback = registered("Fallback", 400, FontStyle::Normal, TEST_FONT_BYTES);
        let primary_id = primary.face.id();
        let fallback_id = fallback.face.id();
        let fonts = FontSet::new_with_unicode_ranges(
            vec![primary, fallback],
            vec![Some(Arc::from([(0x20, 0x7e)])), None],
        )
        .unwrap();
        let spec = FontSpec {
            families: Some(Arc::from([lumen_html::paint::FontFamily::from("Primary"), lumen_html::paint::FontFamily::from("Fallback")])),
            ..FontSpec::default()
        };
        let hebrew = "ש\u{05b7}לום";
        let ordinary = fonts.shape_resolved(hebrew, 18.0, true, &spec).unwrap();
        let detail = fonts
            .shape_resolved_with_cluster_advances(hebrew, 18.0, true, &spec)
            .unwrap()
            .unwrap();
        assert_eq!(detail.run, ordinary);
        assert!(detail
            .run
            .glyphs
            .iter()
            .all(|glyph| glyph.face == fallback_id));
        assert_ne!(primary_id, fallback_id);
        assert_source_partition(hebrew, &detail);
    }

    #[test]
    fn provider_maps_stable_face_identities_and_rejects_collisions() {
        let first = Arc::new(FontFace::new(Arc::from(DEFAULT_FONT_BYTES)).unwrap());
        let second = Arc::new(FontFace::new(Arc::from(TEST_FONT_BYTES)).unwrap());
        assert_ne!(first.id(), second.id());
        let first_id = first.id();
        let second_id = second.id();
        let set = FontSet::new(vec![
            RegisteredFont {
                stretch: 100.0,
                family: Arc::from("First"),
                weight: 400,
                style: FontStyle::Normal,
                face: first.clone(),
            },
            RegisteredFont {
                stretch: 100.0,
                family: Arc::from("Second"),
                weight: 400,
                style: FontStyle::Normal,
                face: second.clone(),
            },
        ])
        .unwrap();
        assert_eq!(set.face_key(first_id), Ok(first_id));
        assert_eq!(set.face_key(second_id), Ok(second_id));
        assert_ne!(
            set.face_key(first_id).unwrap(),
            set.face_key(second_id).unwrap()
        );
        assert_ne!(first.generation(), second.generation());
        assert_ne!(set.generation(), first.generation());
        assert!(matches!(
            first.face_key(second_id),
            Err("font face identity mismatch")
        ));
        assert!(set.rasterize_glyph(first_id, 0, 18.0).is_ok());
        assert!(set.rasterize_glyph(second_id, 0, 18.0).is_ok());
        assert!(matches!(set.face_key(u64::MAX), Err("unknown font face")));
    }

    #[test]
    fn font_set_rejects_invalid_and_unbounded_inputs() {
        assert!(matches!(FontSet::new(Vec::new()), Err("font set is empty")));
        assert!(matches!(
            FontSet::new(vec![registered(
                "",
                400,
                FontStyle::Normal,
                DEFAULT_FONT_BYTES
            )]),
            Err("invalid font family name")
        ));
        assert!(matches!(
            FontSet::new(vec![registered(
                "Family",
                0,
                FontStyle::Normal,
                DEFAULT_FONT_BYTES
            )]),
            Err("invalid font weight")
        ));
        let font = Arc::new(FontFace::new(Arc::from(DEFAULT_FONT_BYTES)).unwrap());
        let too_many = (0..=MAX_REGISTERED_FONTS)
            .map(|index| RegisteredFont {
                stretch: 100.0,
                family: Arc::from(alloc::format!("Family {index}")),
                weight: 400,
                style: FontStyle::Normal,
                face: font.clone(),
            })
            .collect();
        assert!(matches!(
            FontSet::new(too_many),
            Err("too many registered fonts")
        ));

        let set = FontSet::new(vec![RegisteredFont {
            stretch: 100.0,
            family: Arc::from("Family"),
            weight: 400,
            style: FontStyle::Normal,
            face: font,
        }])
        .unwrap();
        let too_many_families: Arc<[lumen_html::paint::FontFamily]> = (0..=MAX_REQUESTED_FAMILIES)
            .map(|index| lumen_html::paint::FontFamily::from(Arc::<str>::from(alloc::format!("Family {index}"))))
            .collect::<Vec<_>>()
            .into();
        let spec = FontSpec {
            families: Some(too_many_families),
            ..FontSpec::default()
        };
        assert!(set.shape_styled("text", 18.0, false, &spec).is_err());
        assert!(set
            .shape_resolved(
                &"a".repeat(MAX_SHAPE_TEXT_BYTES + 1),
                18.0,
                false,
                &FontSpec::default()
            )
            .is_err());
    }
}
