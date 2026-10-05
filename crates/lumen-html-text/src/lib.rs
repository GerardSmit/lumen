//! Explicit-font shaping and glyph coverage shared by render backends.
#![no_std]

extern crate alloc;

pub use lumen_common::ucd::{
    graphemes, line_breaks, next_grapheme_boundary, previous_grapheme_boundary, BreakOpportunity,
};

#[cfg(test)]
use alloc::vec;
use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::cell::UnsafeCell;
use core::cmp::Ordering as CmpOrdering;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use lumen_common::bidi::{self, Script, UnicodeScript};
use lumen_html::paint::{
    font_match_range_rank, FontMetric, FontRelativeMetrics, FontSizeAdjust, FontSizeAdjustValue,
    FontSpec, FontStyle, Glyph, ShapedRun, TextShaper,
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CanvasFontVariantCaps {
    #[default]
    Normal,
    SmallCaps,
    AllSmallCaps,
    PetiteCaps,
    AllPetiteCaps,
    Unicase,
    TitlingCaps,
}

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
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RegisteredFontDescriptors {
    pub weight_range: [u16; 2],
    pub stretch_range: [f32; 2],
    pub size_adjust: f32,
}

#[derive(Clone)]
pub struct FontRegistration {
    pub font: RegisteredFont,
    pub unicode_range: Option<Arc<[(u32, u32)]>>,
    pub descriptors: RegisteredFontDescriptors,
}

impl RegisteredFontDescriptors {
    pub fn from_css(rule: &lumen_html::css::FontFaceRule) -> Self {
        Self {
            weight_range: rule.weight_range,
            stretch_range: rule.stretch_range,
            size_adjust: rule.size_adjust / 100.0,
        }
    }
    fn scalar(font: &RegisteredFont, size_adjust: f32) -> Self {
        Self {
            weight_range: [font.weight, font.weight],
            stretch_range: [font.stretch, font.stretch],
            size_adjust,
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
}

#[derive(Clone, Copy)]
struct ScriptRange {
    start: usize,
    end: usize,
    script: Script,
}

#[derive(Clone, Copy)]
struct FontCluster {
    start: usize,
    end: usize,
    face: usize,
    script: Script,
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
    let mut current = text
        .chars()
        .map(|ch| ch.script())
        .find(|&script| is_strong_script(script))
        .unwrap_or(Script::Common);
    let mut start = 0;
    let mut ranges = Vec::new();
    for (cluster_start, cluster) in graphemes(text).skip(1) {
        if let Some(next) = cluster
            .chars()
            .map(|ch| ch.script())
            .find(|&script| is_strong_script(script))
        {
            if next != current {
                ranges
                    .try_reserve(1)
                    .map_err(|_| "script allocation failed")?;
                ranges.push(ScriptRange {
                    start,
                    end: cluster_start,
                    script: current,
                });
                start = cluster_start;
                current = next;
            }
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

fn shape_buffer(
    bytes: &[u8],
    text: &str,
    rtl: bool,
    script: Script,
    features: &[rustybuzz::Feature],
) -> Result<rustybuzz::GlyphBuffer, &'static str> {
    let face = rustybuzz::Face::from_slice(bytes, 0).ok_or("font cannot be shaped")?;
    let mut buffer = rustybuzz::UnicodeBuffer::new();
    buffer.push_str(text);
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
    Ok(rustybuzz::shape(&face, features, buffer))
}

fn append_glyphs(
    shaped: rustybuzz::GlyphBuffer,
    scale: f32,
    face: u64,
    size_scale: f32,
    cluster_offset: usize,
    glyphs: &mut Vec<Glyph>,
    x: &mut f32,
) -> Result<(), &'static str> {
    glyphs
        .try_reserve(shaped.len())
        .map_err(|_| "glyph allocation failed")?;
    for (info, position) in shaped.glyph_infos().iter().zip(shaped.glyph_positions()) {
        glyphs.push(Glyph {
            id: u16::try_from(info.glyph_id).map_err(|_| "glyph id out of range")?,
            face,
            cluster: u32::try_from(cluster_offset + info.cluster as usize)
                .map_err(|_| "glyph cluster offset out of range")?,
            x: *x + position.x_offset as f32 * scale,
            y: position.y_offset as f32 * scale,
            size_scale,
        });
        *x += position.x_advance as f32 * scale;
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
    match options.font_variant_caps {
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
    features
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
        if prior_cluster != Some(glyph.cluster) {
            if prior_cluster.is_some() {
                offset += options.letter_spacing;
                if prior_was_space {
                    offset += options.word_spacing;
                }
            }
            prior_cluster = Some(glyph.cluster);
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
        descriptors: Vec<RegisteredFontDescriptors>,
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
        })
    }

    fn face_order(&self, spec: &FontSpec) -> Result<Vec<usize>, &'static str> {
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
                let matched = &self.faces[best];
                let matched_instance = self.font_match_instance(spec, best);
                while let Some(index) = self.best_candidate(spec, Some(requested), &order) {
                    order.push(index);
                    let face = &self.faces[index];
                    let face_instance = self.font_match_instance(spec, index);
                    // A family selects one width/style/weight combination.
                    // Keep equal-matching unicode-range slices together;
                    // missing glyphs then fall through to the next family.
                    if face_instance != matched_instance || face.style != matched.style {
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
        family: Option<&str>,
        excluded: &[usize],
    ) -> Option<usize> {
        let mut best = None;
        let mut best_rank: Option<(lumen_html::paint::FontMatchRank, u8, usize)> = None;
        for (index, font) in self.faces.iter().enumerate() {
            if excluded.contains(&index)
                // Downloaded faces are document-family resources, never
                // members of the installed-font fallback pool.
                || (family.is_none() && self.unicode_ranges[index].is_some())
                || family.is_some_and(|requested| !font.family.eq_ignore_ascii_case(requested))
            {
                continue;
            }
            let Some((descriptor_rank, _, _)) = font_match_range_rank(
                spec,
                font.style,
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

    fn font_match_instance(&self, spec: &FontSpec, index: usize) -> Option<(u16, f32)> {
        let descriptor = self.descriptors[index];
        font_match_range_rank(
            spec,
            self.faces[index].style,
            descriptor.weight_range,
            descriptor.stretch_range,
        )
        .map(|(_, weight, stretch)| (weight, stretch))
    }

    fn face_covers(&self, index: usize, cluster: &str) -> bool {
        let face = &self.faces[index].face;
        face.covers_cluster_with_properties(
            cluster,
            self.unicode_ranges[index].as_deref(),
            self.default_ignorable,
        )
    }

    fn cluster_face(&self, cluster: &str, order: &[usize]) -> usize {
        order
            .iter()
            .copied()
            .find(|&index| self.face_covers(index, cluster))
            // Preserve a stable final .notdef face when no registered font covers the cluster.
            .unwrap_or_else(|| {
                order
                    .iter()
                    .rev()
                    .copied()
                    .find(|&index| self.unicode_ranges[index].is_none())
                    .expect("platform fallback validated")
            })
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
    ) -> Result<(), &'static str> {
        for script_range in script_ranges(text, rtl)? {
            let value = &text[script_range.start..script_range.end];
            let mut clusters = Vec::new();
            clusters
                .try_reserve(value.len())
                .map_err(|_| "font fallback allocation failed")?;
            for (start, cluster) in graphemes(value) {
                clusters.push(FontCluster {
                    start,
                    end: start + cluster.len(),
                    face: self.cluster_face(cluster, order),
                    script: script_range.script,
                });
            }
            if rtl {
                clusters.reverse();
            }

            let mut current: Option<FontCluster> = None;
            for cluster in clusters {
                if let Some(mut run) = current {
                    if run.face == cluster.face && run.script == cluster.script {
                        run.start = run.start.min(cluster.start);
                        run.end = run.end.max(cluster.end);
                        current = Some(run);
                        continue;
                    }
                    self.append_cluster_run(
                        value,
                        text_offset + script_range.start,
                        run,
                        size,
                        rtl,
                        spec,
                        features,
                        glyphs,
                        x,
                    )?;
                }
                current = Some(cluster);
            }
            if let Some(run) = current {
                self.append_cluster_run(
                    value,
                    text_offset + script_range.start,
                    run,
                    size,
                    rtl,
                    spec,
                    features,
                    glyphs,
                    x,
                )?;
            }
        }
        Ok(())
    }

    fn append_cluster_run(
        &self,
        text: &str,
        text_offset: usize,
        run: FontCluster,
        size: f32,
        rtl: bool,
        spec: &FontSpec,
        features: &[rustybuzz::Feature],
        glyphs: &mut Vec<Glyph>,
        x: &mut f32,
    ) -> Result<(), &'static str> {
        let registered = &self.faces[run.face];
        let size_scale = self.size_adjust_scale(spec, run.face);
        if size_scale == 0.0 {
            return Ok(());
        }
        registered.face.shape_script_into(
            &text[run.start..run.end],
            size * size_scale,
            rtl,
            run.script,
            registered.face.id(),
            size_scale,
            text_offset + run.start,
            features,
            glyphs,
            x,
        )
    }

    fn metric_face_index(&self, spec: &FontSpec) -> usize {
        self.face_order(spec)
            .ok()
            // CSS Fonts 4: the first available font excludes faces whose
            // unicode-range does not include U+0020, independent of its cmap.
            .and_then(|order| {
                order.into_iter().find(|&index| {
                    self.unicode_ranges[index].as_ref().is_none_or(|ranges| {
                        ranges
                            .iter()
                            .any(|&(first, last)| first <= 0x20 && 0x20 <= last)
                    })
                })
            })
            .unwrap_or(0)
    }

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
        validate_shape_input(text, size)?;
        let mode = if resolve_bidi { 1 } else { 2 };
        if let Some(run) = self
            .shapes
            .get_styled(text, size, Some(rtl), mode, Some(spec))
        {
            return Ok(run);
        }
        let order = self.face_order(spec)?;
        let mut glyphs = Vec::new();
        let mut x = 0.0;
        if resolve_bidi {
            let info = bidi::resolve(text, Some(rtl))?;
            for paragraph in &info.paragraphs {
                let (levels, runs) = info.visual_runs(paragraph, paragraph.range.clone());
                for range in runs {
                    self.append_resolved_range(
                        &text[range.clone()],
                        range.start,
                        size,
                        levels[range.start].is_rtl(),
                        &order,
                        spec,
                        &[],
                        &mut glyphs,
                        &mut x,
                    )?;
                }
            }
        } else {
            self.append_resolved_range(text, 0, size, rtl, &order, spec, &[], &mut glyphs, &mut x)?;
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
            feed(&spec.weight.to_le_bytes());
            feed(&spec.stretch.to_bits().to_le_bytes());
            feed(&[spec.style as u8]);
            if let Some(families) = &spec.families {
                for family in families.iter() {
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
        let result = if text.len() > MAX_CACHED_TEXT_BYTES {
            None
        } else {
            let (hash, size, dir) = Self::key(text, size, rtl, mode, spec);
            self.locked(|slots| {
                let slot = slots.get(hash as usize % RUN_CACHE_SLOTS)?.as_ref()?;
                (slot.hash == hash
                    && slot.size == size
                    && slot.dir == dir
                    && slot.mode == mode
                    && slot.spec.as_ref() == spec
                    && &*slot.text == text)
                    .then(|| slot.value.clone())
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
        if text.len() > MAX_CACHED_TEXT_BYTES {
            return;
        }
        // Charge retained font family metadata conservatively, even when Arcs are shared.
        let family_bytes = spec
            .and_then(|spec| spec.families.as_ref())
            .map_or(0, |families| {
                families.len() * core::mem::size_of::<Arc<str>>()
                    + families
                        .iter()
                        .map(|family| family.len() + 2 * core::mem::size_of::<usize>())
                        .sum::<usize>()
            });
        let bytes = text.len() + value.cache_bytes() + family_bytes;
        let (hash, size, dir) = Self::key(text, size, rtl, mode, spec);
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

    fn rasterize(
        &self,
        bytes: &[u8],
        face_glyph_count: u16,
        glyph_id: u16,
        size: f32,
    ) -> Result<Option<GlyphCoverage>, &'static str> {
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
            return Ok(Some(rasterize_coverage(&entry.font, glyph_id, size)));
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
        Ok(Some(rasterize_coverage(&entry.font, glyph_id, size)))
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
    units_per_em: f32,
    ascent: f32,
    descent: f32,
    underline: (f32, f32),
    strike: (f32, f32),
    metrics: [Option<f32>; 5],
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
        ];
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
            units_per_em,
            ascent,
            descent,
            underline,
            strike,
            metrics,
            shapes: RunCache::new(),
            widths: RunCache::new(),
            rasterizers: RasterizerCache::new(),
        })
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

    pub fn shape(&self, text: &str, size: f32) -> Result<ShapedRun, &'static str> {
        self.shape_with_direction(text, size, None)
    }

    pub fn shape_with_direction(
        &self,
        text: &str,
        size: f32,
        rtl: Option<bool>,
    ) -> Result<ShapedRun, &'static str> {
        if let Some(run) = self.shapes.get(text, size, rtl) {
            return Ok(run);
        }
        let run = self.shape_uncached(text, size, rtl)?;
        self.shapes.insert(text, size, rtl, run.clone());
        Ok(run)
    }

    fn shape_uncached(
        &self,
        text: &str,
        size: f32,
        rtl: Option<bool>,
    ) -> Result<ShapedRun, &'static str> {
        let scale = size / self.units_per_em;
        let mut glyphs = Vec::new();
        let mut x = 0.0;
        self.visit_shaped(text, size, rtl, |shaped, cluster_offset| {
            append_glyphs(shaped, scale, 0, 1.0, cluster_offset, &mut glyphs, &mut x)
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
        script: Script,
        glyph_face: u64,
        size_scale: f32,
        cluster_offset: usize,
        features: &[rustybuzz::Feature],
        glyphs: &mut Vec<Glyph>,
        x: &mut f32,
    ) -> Result<(), &'static str> {
        let shaped = shape_buffer(&self.bytes, text, rtl, script, features)?;
        append_glyphs(
            shaped,
            size / self.units_per_em,
            glyph_face,
            size_scale,
            cluster_offset,
            glyphs,
            x,
        )
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
                shape_buffer(&self.bytes, text, false, Script::Latin, features)?,
                0,
            );
        }
        let info = bidi::resolve(text, rtl)?;
        for paragraph in &info.paragraphs {
            let (levels, runs) = info.visual_runs(paragraph, paragraph.range.clone());
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

    fn shape_resolved_run(
        &self,
        text: &str,
        size: f32,
        rtl: bool,
    ) -> Result<ShapedRun, &'static str> {
        validate_shape_input(text, size)?;
        if let Some(run) = self.shapes.get_styled(text, size, Some(rtl), 2, None) {
            return Ok(run);
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
        if spec.size_adjust.is_none() {
            return if resolved {
                self.shape_resolved_run(text, size, rtl)
            } else {
                self.shape_with_direction(text, size, Some(rtl))
            };
        }
        validate_shape_input(text, size)?;
        let mode = if resolved { 4 } else { 3 };
        if let Some(run) = self
            .shapes
            .get_styled(text, size, Some(rtl), mode, Some(spec))
        {
            return Ok(run);
        }
        let size_scale = self.size_adjust_scale(spec.size_adjust, self);
        let run = if size_scale == 0.0 {
            ShapedRun {
                glyphs: Arc::from([]),
                width: 0.0,
            }
        } else {
            let mut run = if resolved {
                self.shape_resolved_run(text, size * size_scale, rtl)?
            } else {
                self.shape_with_direction(text, size * size_scale, Some(rtl))?
            };
            if size_scale != 1.0 {
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

    pub fn line_height(&self, size: f32) -> f32 {
        (self.ascent - self.descent) * size
    }
    pub fn ascent(&self, size: f32) -> f32 {
        self.ascent * size
    }

    pub fn rasterize(&self, id: u16, size: f32) -> Result<GlyphCoverage, &'static str> {
        if !size.is_finite() || size <= 0.0 || size > 512.0 {
            return Err("invalid font size");
        }
        if id >= self.glyph_count {
            return Err("glyph id out of range");
        }
        if let Some(coverage) =
            self.rasterizers
                .rasterize(&self.bytes, self.glyph_count, id, size)?
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
        Ok(rasterize_coverage(&rasterizer, id, size))
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

    fn font_relative_metrics_styled(&self, size: f32, font: &FontSpec) -> FontRelativeMetrics {
        self.relative_metrics(size, self.size_adjust_scale(font.size_adjust, self))
    }
}

impl FontProvider for FontFace {
    fn shape_canvas_text(
        &self,
        text: &str,
        size: f32,
        rtl: bool,
        _font: &FontSpec,
        options: &CanvasTextOptions,
    ) -> Result<ShapedRun, &'static str> {
        let features = canvas_features(options);
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
        if face != 0 && face != self.id {
            return Err("font face identity mismatch");
        }
        self.rasterize(id, size)
    }

    fn outline_glyph(&self, face: u64, id: u16, size: f32) -> Result<GlyphOutline, &'static str> {
        if face != 0 && face != self.id {
            return Err("font face identity mismatch");
        }
        self.outline(id, size)
    }

    fn face_key(&self, face: u64) -> Result<u64, &'static str> {
        if face == 0 || face == self.id {
            Ok(self.id)
        } else {
            Err("font face identity mismatch")
        }
    }
}

impl TextShaper for FontSet {
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

    fn font_relative_metrics_styled(&self, size: f32, font: &FontSpec) -> FontRelativeMetrics {
        let index = self.metric_face_index(font);
        self.faces[index]
            .face
            .relative_metrics(size, self.size_adjust_scale(font, index))
    }
}

impl FontProvider for FontSet {
    fn registrations(&self) -> Option<Vec<FontRegistration>> {
        Some(
            self.faces
                .iter()
                .cloned()
                .zip(&self.unicode_ranges)
                .zip(&self.descriptors)
                .map(|((font, coverage), descriptors)| FontRegistration {
                    font,
                    unicode_range: coverage.clone(),
                    descriptors: *descriptors,
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
        let order = self.face_order(font)?;
        let features = canvas_features(options);
        let mut glyphs = Vec::new();
        let mut width = 0.0;
        let info = bidi::resolve(text, Some(rtl))?;
        for paragraph in &info.paragraphs {
            let (levels, runs) = info.visual_runs(paragraph, paragraph.range.clone());
            for range in runs {
                self.append_resolved_range(
                    &text[range.clone()],
                    range.start,
                    size,
                    levels[range.start].is_rtl(),
                    &order,
                    font,
                    &features,
                    &mut glyphs,
                    &mut width,
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
        let font = if face == 0 {
            self.faces.first().map(|font| &font.face)
        } else {
            self.faces
                .iter()
                .find(|font| font.face.id() == face)
                .map(|font| &font.face)
        }
        .ok_or("unknown font face")?;
        font.rasterize(id, size)
    }

    fn outline_glyph(&self, face: u64, id: u16, size: f32) -> Result<GlyphOutline, &'static str> {
        let font = if face == 0 {
            self.faces.first().map(|font| &font.face)
        } else {
            self.faces
                .iter()
                .find(|font| font.face.id() == face)
                .map(|font| &font.face)
        }
        .ok_or("unknown font face")?;
        font.outline(id, size)
    }

    fn face_key(&self, face: u64) -> Result<u64, &'static str> {
        if face == 0 {
            return Ok(self.faces[0].face.id());
        }
        self.faces
            .iter()
            .find(|font| font.face.id() == face)
            .map(|font| font.face.id())
            .ok_or("unknown font face")
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
    use alloc::vec;

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
    fn resolved_run_cache_separates_fonts_and_paragraph_direction_modes() {
        let set = FontSet::new(vec![
            registered("mono", 400, FontStyle::Normal, DEFAULT_FONT_BYTES),
            registered("sans", 400, FontStyle::Normal, TEST_FONT_BYTES),
        ])
        .unwrap();
        let mono = FontSpec {
            families: Some(Arc::from([Arc::from("mono")])),
            ..FontSpec::default()
        };
        let sans = FontSpec {
            families: Some(Arc::from([Arc::from("sans")])),
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
            families: Some(Arc::from([Arc::from("Shared")])),
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
                weight_range: [300, 700],
                stretch_range: [75.0, 125.0],
                size_adjust: 1.5,
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
            families: Some(Arc::from([Arc::from("Shared")])),
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
            families: Some(Arc::from([Arc::from("Web"), Arc::from("Fallback")])),
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
            families: Some(Arc::from([Arc::from("Unknown")])),
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
                families: Some(Arc::from([Arc::from("Width")])),
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
            families: Some(Arc::from([Arc::from("Subset"), Arc::from("Fallback")])),
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
            families: Some(Arc::from([Arc::from("Composite")])),
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
                        .map(|name| Arc::<str>::from(*name))
                        .collect::<Vec<_>>()
                        .into(),
                ),
                weight,
                style,
                stretch: 100.0,
                size_adjust: None,
            };
            set.shape_resolved("A", 18.0, false, &font).unwrap()
        };
        assert_eq!(
            shape(&["Second", "First"], 400, FontStyle::Normal).glyphs[0].face,
            second_id
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
            families: Some(vec![Arc::<str>::from("LumenFixture")].into()),
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
                families: Some(vec![Arc::<str>::from(family)].into()),
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
                    Arc::<str>::from("Inconsolata"),
                    Arc::<str>::from("Liberation Sans"),
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
            families: Some(vec![Arc::<str>::from("Liberation Sans")].into()),
            ..font.clone()
        };
        let latin = set
            .shape_resolved("office", 18.0, false, &liberation)
            .unwrap();
        assert!(latin.glyphs.iter().all(|glyph| glyph.face == fallback_id));
        let expected = set.faces[1]
            .face
            .shape_resolved_run("office", 18.0, false)
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
        let too_many_families: Arc<[Arc<str>]> = (0..=MAX_REQUESTED_FAMILIES)
            .map(|index| Arc::from(alloc::format!("Family {index}")))
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
