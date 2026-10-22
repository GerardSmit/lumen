//! TextMetrics cluster geometry: caret hit testing, selection rectangles and ink bounds.
use super::*;
use lumen_html::paint::{Rect as TextRect, ShapedRun};
use std::collections::BTreeMap;

const MERGE_EPSILON: f32 = 0.01;

#[derive(Clone, Copy)]
struct Cluster {
    start: usize,
    end: usize,
    left: f32,
    right: f32,
    rtl: bool,
}

#[derive(Clone, Copy)]
struct Ink {
    cluster: usize,
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
}

#[derive(Clone, Copy, Default)]
pub(super) struct InkBounds {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

/// Cluster and ink geometry of one measured run, relative to the text alignment point.
pub(super) struct TextLayout {
    text: String,
    clusters: Vec<Cluster>,
    ink: Vec<Ink>,
    font_ascent: f32,
    font_descent: f32,
}

pub(super) struct LayoutInput<'a> {
    pub font: &'a dyn FontProvider,
    pub run: &'a ShapedRun,
    pub text: &'a str,
    pub size: f32,
    pub anchor_shift: f32,
    pub baseline_shift: f32,
    pub font_ascent: f32,
    pub font_descent: f32,
}

impl TextLayout {
    pub(super) fn build(input: &LayoutInput) -> OpResult<Self> {
        let LayoutInput { font, run, text, size, anchor_shift, baseline_shift, .. } = *input;
        let mut starts: BTreeMap<u32, f32> = BTreeMap::new();
        for glyph in run.glyphs.iter() {
            let entry = starts.entry(glyph.cluster).or_insert(glyph.x);
            if glyph.x < *entry {
                *entry = glyph.x;
            }
        }
        let logical: Vec<usize> = starts.keys().map(|start| *start as usize).collect();
        let mut visual: Vec<(usize, f32)> =
            starts.iter().map(|(start, x)| (*start as usize, *x)).collect();
        visual.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
        let mut clusters = Vec::with_capacity(visual.len());
        for (index, (start, x)) in visual.iter().enumerate() {
            let right = visual.get(index + 1).map_or(run.width, |next| next.1).max(*x);
            let logical_index = logical.partition_point(|candidate| candidate < start);
            let end = logical.get(logical_index + 1).copied().unwrap_or(text.len());
            let rtl = match (index.checked_sub(1).map(|i| visual[i].0), visual.get(index + 1).map(|n| n.0)) {
                (_, Some(next)) => next < *start,
                (Some(previous), None) => previous > *start,
                (None, None) => false,
            };
            clusters.push(Cluster {
                start: *start,
                end,
                left: x - anchor_shift,
                right: right - anchor_shift,
                rtl,
            });
        }
        let mut ink = Vec::new();
        for glyph in run.glyphs.iter() {
            let coverage = font
                .rasterize_glyph(glyph.face, glyph.id, size * glyph.size_scale)
                .map_err(|error| {
                    OpError::new("InvalidStateError", format!("font rasterization failed: {error}"))
                })?;
            if coverage.width == 0 || coverage.height == 0 {
                continue;
            }
            let left = glyph.x + coverage.x_min as f32 - anchor_shift;
            let top = -glyph.y - coverage.y_min as f32 - coverage.height as f32 + baseline_shift;
            ink.push(Ink {
                cluster: glyph.cluster as usize,
                left,
                top,
                right: left + coverage.width as f32,
                bottom: top + coverage.height as f32,
            });
        }
        Ok(Self {
            text: text.to_owned(),
            clusters,
            ink,
            font_ascent: input.font_ascent,
            font_descent: input.font_descent,
        })
    }

    pub(super) fn ink_bounds(&self, range: Option<(usize, usize)>) -> InkBounds {
        let mut bounds: Option<InkBounds> = None;
        for ink in &self.ink {
            if range.is_some_and(|(start, end)| ink.cluster < start || ink.cluster >= end) {
                continue;
            }
            bounds = Some(match bounds {
                None => InkBounds { left: ink.left, top: ink.top, right: ink.right, bottom: ink.bottom },
                Some(b) => InkBounds {
                    left: b.left.min(ink.left),
                    top: b.top.min(ink.top),
                    right: b.right.max(ink.right),
                    bottom: b.bottom.max(ink.bottom),
                },
            });
        }
        bounds.unwrap_or_default()
    }

    fn byte_from_utf16(&self, index: usize) -> usize {
        let mut units = 0;
        for (byte, ch) in self.text.char_indices() {
            if units >= index {
                return byte;
            }
            units += ch.len_utf16();
        }
        self.text.len()
    }

    fn utf16_from_byte(&self, byte: usize) -> u32 {
        self.text[..byte.min(self.text.len())].encode_utf16().count() as u32
    }

    pub(super) fn index_from_offset(&self, offset: f64) -> u32 {
        let (Some(first), Some(last)) = (self.clusters.first(), self.clusters.last()) else {
            return 0;
        };
        let x = if offset.is_nan() { 0.0 } else { offset as f32 };
        let cluster = if x < first.left {
            first
        } else if x >= last.right {
            last
        } else {
            self.clusters
                .iter()
                .find(|cluster| x >= cluster.left && x < cluster.right)
                .unwrap_or(last)
        };
        let before_middle = x <= (cluster.left + cluster.right) / 2.0;
        let byte = if before_middle != cluster.rtl { cluster.start } else { cluster.end };
        self.utf16_from_byte(byte)
    }

    fn caret_x(&self, byte: usize) -> f32 {
        if let Some(cluster) = self.clusters.iter().find(|cluster| cluster.start == byte) {
            return if cluster.rtl { cluster.right } else { cluster.left };
        }
        let last = self.clusters.iter().max_by_key(|cluster| cluster.start);
        match last {
            Some(cluster) if cluster.rtl => cluster.left,
            Some(cluster) => cluster.right,
            None => 0.0,
        }
    }

    pub(super) fn utf16_len(&self) -> u32 {
        self.text.encode_utf16().count() as u32
    }

    pub(super) fn byte_range(&self, start: u32, end: u32) -> (usize, usize) {
        (
            self.byte_from_utf16(start as usize),
            self.byte_from_utf16(end as usize),
        )
    }

    pub(super) fn selection_rects(&self, start: u32, end: u32) -> Vec<TextRect> {
        let top = -self.font_ascent;
        let height = self.font_ascent + self.font_descent;
        let (bs, be) = self.byte_range(start, end);
        if start >= end || bs >= be {
            let caret = self.caret_x(if start > end { be } else { bs });
            return vec![TextRect { x: caret, y: top, width: 0.0, height }];
        }
        let mut spans: Vec<(f32, f32)> = self
            .clusters
            .iter()
            .filter(|cluster| cluster.start >= bs && cluster.start < be)
            .map(|cluster| (cluster.left, cluster.right))
            .collect();
        spans.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut merged: Vec<(f32, f32)> = Vec::new();
        for span in spans {
            match merged.last_mut() {
                Some(previous) if span.0 - previous.1 <= MERGE_EPSILON => previous.1 = previous.1.max(span.1),
                _ => merged.push(span),
            }
        }
        merged
            .into_iter()
            .map(|(left, right)| TextRect { x: left, y: top, width: right - left, height })
            .collect()
    }
}
