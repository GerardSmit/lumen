//! Selecting document text: hit testing a point to a DOM position, word and
//! block ranges, the rectangles a selection covers and its plain text.
//!
//! Everything reads the text runs the last painted layout recorded once
//! [`RenderSession::set_selectable_text`] is on. A run's glyphs live in the
//! display list, so a document moved by scrolling keeps the runs and the
//! glyphs in step. A document that changed since keeps its painted frame:
//! the geometry stays consistent and every node it names is checked first.
//! Positions are UTF-16 offsets in text nodes.
use crate::{
    css::UserSelect,
    layout::{ControlTextRun, SelectableParagraph, SelectablePiece},
    paint::{Affine, Command, Rect},
    ranges::{self, Boundary},
    session::RenderSession,
    Document, NodeId, NodeKind,
};
use alloc::{
    collections::{BTreeMap, BTreeSet},
    string::String,
    sync::Arc,
    vec::Vec,
};
use core::{cmp::Ordering, ops::Range};

/// A press this far (in lines) from the nearest text is not on text.
const REACH: f32 = 1.0;

/// Candidates dropped for being clipped away before a search gives up.
const CLIPPED_TRIES: usize = 8;

/// Elements a press belongs to rather than to the text under them.
const PRESS_OWNERS: [&str; 15] = [
    "button", "select", "input", "textarea", "option", "img", "video", "audio", "canvas", "iframe", "object", "embed",
    "meter", "progress", "summary",
];

/// A boundary between characters of a run: its x offset in the run and its
/// byte offset in the run's text.
type Stop = (f32, usize);

fn stops(run: &ControlTextRun, commands: &[Command]) -> Option<(f32, Vec<Stop>)> {
    let length = run.range.end.checked_sub(run.range.start)?;
    let Some(Command::GlyphRun { origin_x, glyphs, .. }) = commands.get(run.command_index?) else { return None };
    let mut ordered: Vec<(f32, usize)> = glyphs.iter().map(|glyph| (glyph.x - glyph.x_offset, glyph.cluster as usize)).collect();
    ordered.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(Ordering::Equal));
    let mut result = Vec::with_capacity(ordered.len() + 1);
    if run.rtl {
        for (index, &(_, cluster)) in ordered.iter().enumerate() {
            let right = ordered.get(index + 1).map_or(run.width, |next| next.0);
            result.push((right, cluster.min(length)));
        }
        result.push((ordered.first().map_or(0.0, |first| first.0), length));
    } else {
        for &(x, cluster) in &ordered {
            result.push((x, cluster.min(length)));
        }
        result.push((run.width, length));
    }
    Some((*origin_x - run.x, result))
}

fn byte_at(stops: &[Stop], x: f32) -> usize {
    stops
        .iter()
        .min_by(|a, b| (a.0 - x).abs().partial_cmp(&(b.0 - x).abs()).unwrap_or(Ordering::Equal))
        .map_or(0, |stop| stop.1)
}

fn x_at(stops: &[Stop], rtl: bool, byte: usize) -> f32 {
    let mut best: Option<Stop> = None;
    for &stop in stops {
        if stop.1 < byte {
            continue;
        }
        best = match best {
            Some(current) if current.1 < stop.1 => Some(current),
            Some(current) if rtl && current.0 >= stop.0 => Some(current),
            Some(current) if !rtl && current.0 <= stop.0 => Some(current),
            _ => Some(stop),
        };
    }
    best.map_or(0.0, |stop| stop.0)
}

fn char_floor(text: &str, byte: usize) -> usize {
    let mut byte = byte.min(text.len());
    while !text.is_char_boundary(byte) {
        byte -= 1;
    }
    byte
}

fn intersection(a: Rect, b: Rect) -> Option<Rect> {
    let left = a.x.max(b.x);
    let top = a.y.max(b.y);
    let right = (a.x + a.width).min(b.x + b.width);
    let bottom = (a.y + a.height).min(b.y + b.height);
    (right > left && bottom > top).then_some(Rect { x: left, y: top, width: right - left, height: bottom - top })
}

fn is_identity(matrix: Affine) -> bool {
    matrix == Affine::IDENTITY
}

/// The part of `run` that a clip leaves visible, in viewport coordinates.
fn visible_bounds(run: &ControlTextRun, clip: Option<Rect>) -> Option<Rect> {
    let local = Rect { x: run.x, y: run.y, width: run.width, height: run.height };
    let bounds = if is_identity(run.transform) { local } else { run.transform.bounds(local) };
    match clip {
        Some(clip) => intersection(bounds, clip),
        None => Some(bounds),
    }
}

/// The point `(x, y)` in the coordinates of `run`'s layout.
fn local_point(run: &ControlTextRun, x: f32, y: f32) -> Option<(f32, f32)> {
    if is_identity(run.transform) {
        return Some((x, y));
    }
    run.transform.inverse().map(|inverse| inverse.apply(x, y))
}

/// The overflow and box clips that apply to each command of `wanted` (sorted,
/// without repeats), in viewport coordinates; `None` where nothing clips.
fn clips_at(commands: &[Command], wanted: &[usize]) -> Vec<Option<Rect>> {
    let mut result = Vec::with_capacity(wanted.len());
    let Some(&last) = wanted.last() else { return result };
    let mut transforms: Vec<Affine> = Vec::new();
    let mut clips: Vec<Rect> = Vec::new();
    let mut layers: Vec<bool> = Vec::new();
    let push_clip = |clips: &mut Vec<Rect>, transforms: &[Affine], rect: Rect| {
        let rect = transforms.last().map_or(rect, |matrix| matrix.bounds(rect));
        let rect = match clips.last() {
            Some(&outer) => intersection(outer, rect).unwrap_or(Rect { x: rect.x, y: rect.y, width: 0.0, height: 0.0 }),
            None => rect,
        };
        clips.push(rect);
    };
    for (index, command) in commands.iter().enumerate().take(last + 1) {
        match command {
            Command::PushTransform(matrix) => {
                let outer = transforms.last().copied().unwrap_or(Affine::IDENTITY);
                transforms.push(matrix.then(outer));
            }
            Command::PopTransform => {
                transforms.pop();
            }
            Command::PushClip(rect) | Command::PushBoxClip(rect) => push_clip(&mut clips, &transforms, *rect),
            Command::PopClip => {
                clips.pop();
            }
            Command::PushLayer { clip, rect, .. } => {
                if *clip {
                    push_clip(&mut clips, &transforms, *rect);
                }
                layers.push(*clip);
            }
            Command::PopLayer => {
                if layers.pop() == Some(true) {
                    clips.pop();
                }
            }
            _ => {}
        }
        if wanted.binary_search(&index).is_ok() {
            result.push(clips.last().copied());
        }
    }
    result.resize(wanted.len(), None);
    result
}

impl SelectablePiece {
    /// The node offset that reads as the rendered `byte`: the last whose
    /// rendered position does not pass it, so collapsed spaces lie before it.
    fn offset_at_byte(&self, byte: usize) -> usize {
        let index = self.bytes.partition_point(|&at| at as usize <= byte);
        index.checked_sub(1).map_or(0, |index| self.unit(index))
    }

    fn byte_at_offset(&self, offset: usize) -> usize {
        let index = match &self.units {
            None => offset.saturating_add(1).min(self.bytes.len()),
            Some(units) => units.partition_point(|&unit| unit as usize <= offset),
        };
        index.checked_sub(1).map_or(self.range.start, |index| self.bytes[index] as usize)
    }

    fn end_offset(&self) -> usize {
        self.bytes.len().checked_sub(1).map_or(0, |last| self.unit(last))
    }

    fn first_offset(&self) -> usize {
        if self.bytes.is_empty() { 0 } else { self.unit(0) }
    }
}

impl SelectableParagraph {
    /// The piece holding the rendered `byte`, or the last before it.
    fn piece_at(&self, byte: usize) -> Option<&SelectablePiece> {
        let index = self.pieces.partition_point(|piece| piece.range.start <= byte);
        self.pieces.get(index.saturating_sub(1))
    }
}

fn paragraph_key(paragraph: &Arc<SelectableParagraph>) -> usize {
    Arc::as_ptr(paragraph) as usize
}

/// The selection as it meets the text nodes of painted paragraphs.
struct Coverage<'a> {
    document: &'a Document,
    start: Boundary,
    end: Boundary,
    /// The document is as laid out; nodes need no check.
    fresh: bool,
}

impl<'a> Coverage<'a> {
    fn new(document: &'a Document, (start, end): (Boundary, Boundary), fresh: bool) -> Option<Self> {
        if start.offset > ranges::length(document, start.container).ok()?
            || end.offset > ranges::length(document, end.container).ok()?
            || ranges::compare(document, start, end).ok()? != Some(Ordering::Less)
        {
            return None;
        }
        Some(Self { document, start, end, fresh })
    }

    fn compare(&self, node: NodeId, offset: usize, with: Boundary) -> Option<Ordering> {
        ranges::compare(self.document, Boundary { container: node, offset }, with).ok()?
    }

    /// The UTF-16 offsets of `piece`'s node the selection covers.
    fn node_offsets(&self, piece: &SelectablePiece) -> Option<(usize, usize)> {
        let length = ranges::length(self.document, piece.node).ok()?;
        if !self.fresh && piece.end_offset() > length {
            return None;
        }
        if self.compare(piece.node, length, self.start) != Some(Ordering::Greater)
            || self.compare(piece.node, 0, self.end) != Some(Ordering::Less)
        {
            return None;
        }
        let lo = if self.start.container == piece.node { self.start.offset } else { 0 };
        let hi = if self.end.container == piece.node { self.end.offset } else { length };
        (hi > lo).then_some((lo, hi))
    }

    /// For each piece of `paragraph`, the rendered bytes the selection covers.
    fn paragraph(&self, paragraph: &SelectableParagraph) -> Vec<Option<Range<usize>>> {
        let pieces = &paragraph.pieces;
        let mut result: Vec<Option<Range<usize>>> = (0..pieces.len()).map(|_| None).collect();
        if let (Some(first), Some(last)) = (pieces.first(), pieces.last()) {
            // Pieces follow document order, so a paragraph wholly before or
            // after the selection is known from its ends.
            let before_end = self.compare(first.node, 0, self.end);
            let last_length = ranges::length(self.document, last.node).ok();
            let after_start = last_length.and_then(|length| self.compare(last.node, length, self.start));
            if (before_end.is_some() && before_end != Some(Ordering::Less))
                || (after_start.is_some() && after_start != Some(Ordering::Greater))
            {
                return result;
            }
        }
        for (slot, piece) in result.iter_mut().zip(pieces) {
            if let Some((lo, hi)) = self.node_offsets(piece) {
                let start = piece.byte_at_offset(lo).max(piece.range.start);
                let end = piece.byte_at_offset(hi).min(piece.range.end);
                if start < end {
                    *slot = Some(start..end);
                }
            }
        }
        result
    }
}

/// Coverage of each paragraph, worked out once for the paragraphs a pass meets.
#[derive(Default)]
struct Memo {
    paragraphs: BTreeMap<usize, Vec<Option<Range<usize>>>>,
}

impl Memo {
    fn get(&mut self, coverage: &Coverage<'_>, paragraph: &Arc<SelectableParagraph>) -> &[Option<Range<usize>>] {
        self.paragraphs.entry(paragraph_key(paragraph)).or_insert_with(|| coverage.paragraph(paragraph))
    }
}

/// The rendered bytes of `run` that `covers` (per piece of its paragraph) include.
fn covered_in_run(run: &ControlTextRun, paragraph: &SelectableParagraph, covers: &[Option<Range<usize>>]) -> Vec<Range<usize>> {
    let mut result = Vec::new();
    let first = paragraph.pieces.partition_point(|piece| piece.range.end <= run.range.start);
    for (piece, cover) in paragraph.pieces.iter().zip(covers).skip(first) {
        if piece.range.start >= run.range.end {
            break;
        }
        if let Some(cover) = cover {
            let start = cover.start.max(run.range.start);
            let end = cover.end.min(run.range.end);
            if start < end {
                result.push(start..end);
            }
        }
    }
    result
}

fn append_text(output: &mut String, text: &str, range: Range<usize>) {
    let slice = &text[char_floor(text, range.start)..char_floor(text, range.end)];
    for character in slice.chars() {
        match character {
            '\u{200b}' => output.push('\n'),
            '\u{fffc}' | '\u{2029}' => {}
            other => output.push(other),
        }
    }
}

/// `base` of `text` without the `excluded` ranges (sorted, not overlapping).
fn append_without(output: &mut String, text: &str, base: Range<usize>, excluded: &[Range<usize>]) {
    let mut at = base.start;
    let from = excluded.partition_point(|range| range.end <= base.start);
    for range in &excluded[from..] {
        if range.start >= base.end {
            break;
        }
        if range.start > at {
            append_text(output, text, at..range.start);
        }
        at = at.max(range.end);
    }
    if at < base.end {
        append_text(output, text, at..base.end);
    }
}

struct Nearest<'a> {
    run: &'a ControlTextRun,
    /// The pressed x in the run's layout coordinates.
    x: f32,
}

struct Frame<'a> {
    runs: &'a [ControlTextRun],
    commands: &'a [Command],
    fresh: bool,
}

impl RenderSession {
    fn frame(&self) -> Option<Frame<'_>> {
        let (runs, commands, fresh) = self.text_run_frame()?;
        Some(Frame { runs, commands, fresh })
    }

    /// Whether the node of a painted piece can still be addressed.
    fn piece_usable(&self, piece: &SelectablePiece, fresh: bool) -> bool {
        match self.document().kind(piece.node) {
            Ok(NodeKind::Text(text) | NodeKind::CData(text)) => {
                fresh
                    || (text.encode_utf16().count() >= piece.end_offset()
                        && self.document().root_node(piece.node, true).ok() == Some(self.document().root()))
            }
            _ => false,
        }
    }

    fn contains_node(&self, ancestor: NodeId, node: NodeId) -> bool {
        let mut current = Some(node);
        while let Some(id) = current {
            if id == ancestor {
                return true;
            }
            current = self.document().parent(id).ok().flatten();
        }
        false
    }

    /// Whether a press at `node` belongs to a control or replaced element,
    /// not to text.
    fn takes_the_press(&self, node: NodeId) -> bool {
        let mut current = Some(node);
        while let Some(id) = current {
            if let Ok(NodeKind::Element { name, .. }) = self.document().kind(id) {
                if PRESS_OWNERS.contains(&name.as_str()) {
                    return true;
                }
            }
            current = self.document().parent(id).ok().flatten();
        }
        false
    }

    /// The run nearest the point `(x, y)`, by lines first and then columns;
    /// later runs paint over earlier ones, so they win a tie. Only runs of
    /// text inside `within`, closer than `reach` lines and not clipped away
    /// count.
    fn nearest_run<'a>(
        &self,
        frame: &Frame<'a>,
        (x, y): (f32, f32),
        within: Option<NodeId>,
        selectable_only: bool,
        reach: Option<f32>,
    ) -> Option<Nearest<'a>> {
        let mut rejected: Vec<usize> = Vec::new();
        for _ in 0..CLIPPED_TRIES {
            let mut best: Option<(usize, (f32, f32), f32)> = None;
            for (index, run) in frame.runs.iter().enumerate() {
                let Some(select) = run.selectable.as_ref() else { continue };
                if selectable_only && select.select == UserSelect::None {
                    continue;
                }
                let Some((local_x, local_y)) = local_point(run, x, y) else { continue };
                let dy = if local_y < run.y {
                    run.y - local_y
                } else if local_y > run.y + run.height {
                    local_y - run.y - run.height
                } else {
                    0.0
                };
                let dx = if local_x < run.x {
                    run.x - local_x
                } else if local_x > run.x + run.width {
                    local_x - run.x - run.width
                } else {
                    0.0
                };
                if reach.is_some_and(|reach| dy > run.height * reach) {
                    continue;
                }
                if !best.as_ref().is_none_or(|current| (dy, dx) <= current.1) {
                    continue;
                }
                if rejected.contains(&index) || within.is_some_and(|within| !self.contains_node(within, run.node)) {
                    continue;
                }
                best = Some((index, (dy, dx), local_x));
            }
            let (index, _, local_x) = best?;
            let run = &frame.runs[index];
            let clip = match run.command_index {
                Some(command) => clips_at(frame.commands, &[command]).into_iter().next().flatten(),
                None => None,
            };
            if visible_bounds(run, clip).is_none() {
                rejected.push(index);
                continue;
            }
            return Some(Nearest { run, x: local_x });
        }
        None
    }

    fn position_in(&self, frame: &Frame<'_>, found: &Nearest<'_>) -> Option<(NodeId, usize)> {
        let run = found.run;
        let shared = run.selectable.as_ref()?;
        let (origin, stops) = stops(run, frame.commands)?;
        let byte = byte_at(&stops, found.x - run.x - origin);
        let at = char_floor(&shared.paragraph.text, run.range.start + byte);
        let piece = shared.paragraph.piece_at(at)?;
        if !self.piece_usable(piece, frame.fresh) {
            return None;
        }
        Some((piece.node, piece.offset_at_byte(at.clamp(piece.range.start, piece.range.end))))
    }

    /// The text position under the point `(x, y)` (viewport CSS pixels): the
    /// nearest character boundary of the line of text the point is on or
    /// beside inside the element the press lands on. `None` where the press
    /// is not at text it can select: on a control or replaced element,
    /// nothing within a line of it in that element, or text under
    /// `user-select: none`.
    pub fn caret_position_from_point(&self, x: f32, y: f32) -> Option<(NodeId, usize)> {
        let frame = self.frame()?;
        let hit = self.painted_hit_test(x, y)?;
        if self.takes_the_press(hit) {
            return None;
        }
        let found = self.nearest_run(&frame, (x, y), Some(hit), false, Some(REACH))?;
        if found.run.selectable.as_ref()?.select == UserSelect::None {
            return None;
        }
        self.position_in(&frame, &found)
    }

    /// Like [`caret_position_from_point`](Self::caret_position_from_point), but
    /// text under `user-select: none` still yields a position.
    pub fn text_position_from_point(&self, x: f32, y: f32) -> Option<(NodeId, usize)> {
        let frame = self.frame()?;
        let hit = self.painted_hit_test(x, y)?;
        if self.takes_the_press(hit) {
            return None;
        }
        let found = self.nearest_run(&frame, (x, y), Some(hit), false, Some(REACH))?;
        self.position_in(&frame, &found)
    }

    /// The text position nearest the point among the selectable text, for
    /// extending a selection a drag is making, wherever the pointer is.
    pub fn nearest_caret_position(&self, x: f32, y: f32) -> Option<(NodeId, usize)> {
        let frame = self.frame()?;
        let found = self.nearest_run(&frame, (x, y), None, true, None)?;
        self.position_in(&frame, &found)
    }

    /// The word, or the run of spaces or the punctuation mark, at `offset` in
    /// the text node `node`, as UTF-16 offsets.
    pub fn word_range_at(&self, node: NodeId, offset: usize) -> Option<(usize, usize)> {
        let (NodeKind::Text(text) | NodeKind::CData(text)) = self.document().kind(node).ok()? else { return None };
        let mut units = 0;
        let mut found = None;
        for (_, segment) in lumen_common::ucd::word_boundaries(text) {
            let length = segment.encode_utf16().count();
            found = Some((units, units + length));
            if offset < units + length {
                break;
            }
            units += length;
        }
        found
    }

    /// The boundaries of the paragraph of the first run that `accept`s and
    /// whose paragraph holds the text node `node`.
    fn paragraph_bounds(&self, node: NodeId, accept: impl Fn(UserSelect) -> bool) -> Option<(Boundary, Boundary)> {
        let frame = self.frame()?;
        let mut seen = BTreeSet::new();
        for run in frame.runs {
            let Some(shared) = run.selectable.as_ref() else { continue };
            if !accept(shared.select) || !seen.insert(paragraph_key(&shared.paragraph)) {
                continue;
            }
            let pieces = &shared.paragraph.pieces;
            if !pieces.iter().any(|piece| piece.node == node) {
                continue;
            }
            let first = pieces.iter().find(|piece| self.piece_usable(piece, frame.fresh))?;
            let last = pieces.iter().rev().find(|piece| self.piece_usable(piece, frame.fresh))?;
            return Some((
                Boundary { container: first.node, offset: first.first_offset() },
                Boundary { container: last.node, offset: last.end_offset() },
            ));
        }
        None
    }

    /// The whole inline formatting context (a paragraph of text) holding the
    /// text node `node`.
    pub fn block_range_at(&self, node: NodeId) -> Option<(Boundary, Boundary)> {
        self.paragraph_bounds(node, |_| true)
    }

    /// For text under `user-select: all`, what one press selects: the
    /// paragraph (inline formatting context) of the text node `node`.
    pub fn user_select_all_range_at(&self, node: NodeId) -> Option<(Boundary, Boundary)> {
        self.paragraph_bounds(node, |select| select == UserSelect::All)
    }

    /// Select between two boundaries (either order); a collapsed or `None`
    /// selection paints nothing. True when it changed.
    pub fn set_selection(&mut self, selection: Option<(Boundary, Boundary)>) -> bool {
        let ordered = selection.and_then(|(a, b)| {
            let order = ranges::compare(self.document(), a, b).ok()??;
            match order {
                Ordering::Equal => None,
                Ordering::Less => Some((a, b)),
                Ordering::Greater => Some((b, a)),
            }
        });
        self.store_selection(ordered)
    }

    /// The selection as an ordered pair of boundaries, if it spans text.
    pub fn selection(&self) -> Option<(Boundary, Boundary)> {
        self.selection_state()
    }

    /// The rectangles (viewport CSS pixels) the selection covers: one per
    /// run, joined along a line where the gap between words is selected too.
    /// Text clipped away by an overflow is left out, and a transformed run
    /// is covered by its bounding box.
    pub fn selection_rects(&self) -> Vec<Rect> {
        let Some(selection) = self.selection_state() else { return Vec::new() };
        self.covered_rects(selection)
    }

    /// The rectangles the text between two ordered boundaries covers, as for a
    /// selection over the same text.
    pub fn range_client_rects(&self, start: Boundary, end: Boundary) -> Vec<Rect> {
        self.covered_rects((start, end))
    }

    /// The rectangles covered by all the text of a text node.
    pub fn text_node_client_rects(&self, node: NodeId) -> Vec<Rect> {
        let Ok(NodeKind::Text(text) | NodeKind::CData(text)) = self.document().kind(node) else { return Vec::new() };
        let end = Boundary { container: node, offset: text.encode_utf16().count() };
        self.covered_rects((Boundary { container: node, offset: 0 }, end))
    }

    fn covered_rects(&self, selection: (Boundary, Boundary)) -> Vec<Rect> {
        let Some(frame) = self.frame() else { return Vec::new() };
        let Some(coverage) = Coverage::new(self.document(), selection, frame.fresh) else { return Vec::new() };
        let mut memo = Memo::default();
        let mut covered: Vec<(usize, Vec<Range<usize>>)> = Vec::new();
        for (index, run) in frame.runs.iter().enumerate() {
            let Some(shared) = run.selectable.as_ref().filter(|shared| shared.select != UserSelect::None) else { continue };
            let spans = covered_in_run(run, &shared.paragraph, memo.get(&coverage, &shared.paragraph));
            if !spans.is_empty() {
                covered.push((index, spans));
            }
        }
        let mut wanted: Vec<usize> = covered.iter().filter_map(|(index, _)| frame.runs[*index].command_index).collect();
        wanted.sort_unstable();
        wanted.dedup();
        let clips = clips_at(frame.commands, &wanted);
        let mut rects: Vec<Rect> = Vec::new();
        for (index, spans) in covered {
            let run = &frame.runs[index];
            let Some((origin, stops)) = stops(run, frame.commands) else { continue };
            let clip = run
                .command_index
                .and_then(|command| wanted.binary_search(&command).ok())
                .and_then(|slot| clips[slot]);
            for span in spans {
                let a = x_at(&stops, run.rtl, span.start - run.range.start) + origin;
                let b = x_at(&stops, run.rtl, span.end - run.range.start) + origin;
                let (left, right) = (a.min(b), a.max(b));
                if right <= left {
                    continue;
                }
                let local = Rect { x: run.x + left, y: run.y, width: right - left, height: run.height };
                let bounds = if is_identity(run.transform) { local } else { run.transform.bounds(local) };
                let Some(rect) = clip.map_or(Some(bounds), |clip| intersection(bounds, clip)) else { continue };
                let joined = rects.last_mut().filter(|last| {
                    (last.y - rect.y).abs() < 0.5
                        && (last.height - rect.height).abs() < 0.5
                        && rect.x >= last.x
                        && rect.x - (last.x + last.width) <= rect.height * 0.5
                });
                if let Some(last) = joined {
                    last.width = (rect.x + rect.width - last.x).max(last.width);
                } else {
                    rects.push(rect);
                }
            }
        }
        rects
    }

    /// The selected text as the page shows it: collapsed whitespace, a line
    /// break between blocks and at forced breaks, and none of the text under
    /// `user-select: none`.
    pub fn selected_text(&self) -> Option<String> {
        let selection = self.selection_state()?;
        let frame = self.frame()?;
        let coverage = Coverage::new(self.document(), selection, frame.fresh)?;
        struct Gathered<'a> {
            paragraph: &'a Arc<SelectableParagraph>,
            selectable: bool,
            excluded: Vec<Range<usize>>,
        }
        let mut order: Vec<Gathered<'_>> = Vec::new();
        let mut slots: BTreeMap<usize, usize> = BTreeMap::new();
        for run in frame.runs {
            let Some(shared) = run.selectable.as_ref() else { continue };
            let key = paragraph_key(&shared.paragraph);
            let slot = match slots.get(&key) {
                Some(&slot) => slot,
                None => {
                    order.push(Gathered { paragraph: &shared.paragraph, selectable: false, excluded: Vec::new() });
                    slots.insert(key, order.len() - 1);
                    order.len() - 1
                }
            };
            if shared.select == UserSelect::None {
                order[slot].excluded.push(run.range.clone());
            } else {
                order[slot].selectable = true;
            }
        }
        let mut memo = Memo::default();
        let mut output = String::new();
        for gathered in order.iter_mut().filter(|gathered| gathered.selectable) {
            let covers = memo.get(&coverage, gathered.paragraph);
            let first = covers.iter().flatten().map(|range| range.start).min();
            let last = covers.iter().flatten().map(|range| range.end).max();
            let (Some(first), Some(last)) = (first, last) else { continue };
            gathered.excluded.sort_unstable_by_key(|range| range.start);
            let mut chunk = String::new();
            append_without(&mut chunk, &gathered.paragraph.text, first..last, &gathered.excluded);
            if chunk.is_empty() {
                continue;
            }
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(&chunk);
        }
        (!output.is_empty()).then_some(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{html, paint::*, selector};
    use alloc::vec;

    struct Fixed;
    impl TextShaper for Fixed {
        fn shape(&self, text: &str, _: f32) -> Result<ShapedRun, ()> {
            Ok(ShapedRun { width: text.chars().count() as f32 * 10.0,
                glyphs: text.char_indices().enumerate().map(|(position, (cluster, ch))| Glyph {
                    id: ch as u16, face: 0, cluster: cluster as u32, caps_expansion: 0, x_offset: 0.0,
                    x: position as f32 * 10.0, y: 0.0, size_scale: 1.0,
                }).collect::<Vec<_>>().into() })
        }
        fn shape_resolved_with_cluster_advances(&self, text: &str, size: f32, _: bool, _: &FontSpec) -> Result<Option<ShapedRunWithClusterAdvances>, ()> {
            Ok(Some(ShapedRunWithClusterAdvances { run: self.shape(text, size)?,
                clusters: text.char_indices().map(|(start, ch)| ShapedClusterAdvance { source: start..start + ch.len_utf8(), advance: 10.0 }).collect::<Vec<_>>().into() }))
        }
        fn ascent(&self, _: f32) -> f32 { 10.0 }
        fn line_height(&self, _: f32) -> f32 { 20.0 }
    }

    fn session(body: &str) -> RenderSession {
        let document = html::parse(&alloc::format!("<body style='margin:0'>{body}"), 4096).unwrap();
        let mut session = RenderSession::new(document);
        session.set_selectable_text(true);
        session.display_list(400, 200, &Fixed).unwrap();
        session
    }

    fn text_of(session: &RenderSession, selector: &str) -> NodeId {
        let element = selector::query_selector(session.document(), session.document().root(), selector).unwrap().unwrap();
        session.document().first_child(element).unwrap().unwrap()
    }

    #[test]
    fn a_point_maps_to_the_nearest_character_boundary_of_its_text_node() {
        let session = session("<p id=p style='margin:0'>hello world</p>");
        let node = text_of(&session, "#p");
        assert_eq!(session.caret_position_from_point(24.0, 5.0), Some((node, 2)));
        assert_eq!(session.caret_position_from_point(26.0, 5.0), Some((node, 3)));
        assert_eq!(session.caret_position_from_point(300.0, 5.0), Some((node, 11)), "beside the end of the line");
        assert_eq!(session.caret_position_from_point(0.0, 5.0), Some((node, 0)));
        assert_eq!(session.caret_position_from_point(30.0, 150.0), None, "far below any text");
    }

    #[test]
    fn collapsed_whitespace_keeps_offsets_in_the_source_text() {
        let session = session("<p id=p style='margin:0'>a   bc</p>");
        let node = text_of(&session, "#p");
        assert_eq!(session.caret_position_from_point(21.0, 5.0), Some((node, 4)), "before b, past the collapsed spaces");
        assert_eq!(session.caret_position_from_point(31.0, 5.0), Some((node, 5)));
        assert_eq!(session.caret_position_from_point(41.0, 5.0), Some((node, 6)));
    }

    #[test]
    fn text_under_user_select_none_cannot_be_pressed_or_selected() {
        let mut session = session("<p id=a style='margin:0;user-select:none'>no <b id=b>bold</b></p><p id=c style='margin:0'>yes</p>");
        assert_eq!(session.caret_position_from_point(5.0, 5.0), None);
        assert_eq!(session.caret_position_from_point(45.0, 5.0), None, "inherited by the span");
        let node = text_of(&session, "#a");
        let other = text_of(&session, "#c");
        assert_eq!(session.nearest_caret_position(5.0, 5.0), Some((other, 0)), "a drag skips it");
        assert!(session.set_selection(Some((Boundary { container: node, offset: 0 }, Boundary { container: node, offset: 3 }))));
        assert!(session.selection_rects().is_empty());
        assert_eq!(session.selected_text(), None);
    }

    #[test]
    fn words_are_found_by_text_segmentation() {
        let session = session("<p id=p style='margin:0'>hello, big world</p>");
        let node = text_of(&session, "#p");
        assert_eq!(session.word_range_at(node, 1), Some((0, 5)));
        assert_eq!(session.word_range_at(node, 5), Some((5, 6)), "the comma");
        assert_eq!(session.word_range_at(node, 8), Some((7, 10)));
        assert_eq!(session.word_range_at(node, 16), Some((11, 16)), "the end of the text belongs to the last word");
    }

    #[test]
    fn a_block_is_the_whole_inline_paragraph_across_elements() {
        let session = session("<p id=p style='margin:0'>one <b id=b>two</b> three</p><p id=q style='margin:0'>other</p>");
        let first = text_of(&session, "#p");
        let bold = text_of(&session, "#b");
        let (start, end) = session.block_range_at(bold).unwrap();
        assert_eq!((start.container, start.offset), (first, 0));
        assert_eq!(end.offset, 6, "{end:?}");
        assert_ne!(end.container, text_of(&session, "#q"));
    }

    #[test]
    fn the_selected_text_follows_the_rendered_text_and_breaks_blocks() {
        let mut session = session("<p id=p style='margin:0'>one   two</p><p id=q style='margin:0'>three four</p>");
        let first = text_of(&session, "#p");
        let second = text_of(&session, "#q");
        assert!(session.set_selection(Some((Boundary { container: first, offset: 4 }, Boundary { container: second, offset: 5 }))));
        assert_eq!(session.selected_text().as_deref(), Some("two\nthree"));
        assert!(session.set_selection(Some((Boundary { container: second, offset: 5 }, Boundary { container: first, offset: 0 }))), "either order");
        assert_eq!(session.selected_text().as_deref(), Some("one two\nthree"));
        assert!(!session.set_selection(Some((Boundary { container: first, offset: 0 }, Boundary { container: second, offset: 5 }))), "same range");
        assert!(session.set_selection(None));
        assert_eq!(session.selected_text(), None);
    }

    #[test]
    fn selection_rectangles_cover_the_selected_glyphs_line_by_line() {
        let mut session = session("<p id=p style='margin:0;width:100px'>hello wonderful world</p>");
        let node = text_of(&session, "#p");
        session.set_selection(Some((Boundary { container: node, offset: 2 }, Boundary { container: node, offset: 5 })));
        assert_eq!(session.selection_rects(), vec![Rect { x: 20.0, y: 0.0, width: 30.0, height: 20.0 }]);
        session.set_selection(Some((Boundary { container: node, offset: 0 }, Boundary { container: node, offset: 20 })));
        let rects = session.selection_rects();
        assert!(rects.len() >= 2 && rects.iter().all(|rect| rect.height == 20.0), "{rects:?}");
        assert_eq!(rects[0].y, 0.0);
        assert!(rects[1].y >= 20.0);
    }

    struct Mirrored;
    impl TextShaper for Mirrored {
        fn shape(&self, text: &str, _: f32) -> Result<ShapedRun, ()> {
            let count = text.chars().count();
            Ok(ShapedRun { width: count as f32 * 10.0,
                glyphs: text.char_indices().enumerate().map(|(position, (cluster, ch))| Glyph {
                    id: ch as u16, face: 0, cluster: cluster as u32, caps_expansion: 0, x_offset: 0.0,
                    x: (count - 1 - position) as f32 * 10.0, y: 0.0, size_scale: 1.0,
                }).collect::<Vec<_>>().into() })
        }
        fn shape_resolved_with_cluster_advances(&self, text: &str, size: f32, _: bool, _: &FontSpec) -> Result<Option<ShapedRunWithClusterAdvances>, ()> {
            Ok(Some(ShapedRunWithClusterAdvances { run: self.shape(text, size)?,
                clusters: text.char_indices().map(|(start, ch)| ShapedClusterAdvance { source: start..start + ch.len_utf8(), advance: 10.0 }).collect::<Vec<_>>().into() }))
        }
        fn ascent(&self, _: f32) -> f32 { 10.0 }
        fn line_height(&self, _: f32) -> f32 { 20.0 }
    }

    #[test]
    fn rtl_text_selects_from_its_right_edge() {
        let document = html::parse("<body style='margin:0'><p id=p dir=rtl style='margin:0;width:100px'>אבגד</p>", 128).unwrap();
        let mut session = RenderSession::new(document);
        session.set_selectable_text(true);
        session.display_list(400, 200, &Mirrored).unwrap();
        let node = text_of(&session, "#p");
        session.set_selection(Some((Boundary { container: node, offset: 0 }, Boundary { container: node, offset: 1 })));
        assert_eq!(session.selection_rects(), vec![Rect { x: 90.0, y: 0.0, width: 10.0, height: 20.0 }]);
        assert_eq!(session.caret_position_from_point(98.0, 5.0), Some((node, 0)), "the logical start is the right edge");
        assert_eq!(session.caret_position_from_point(67.0, 5.0), Some((node, 3)));
    }

    #[test]
    fn hit_testing_and_rectangles_follow_a_document_scrolled_deep() {
        let lines: String = (0..300).map(|number| alloc::format!("<p id=l{number} style='margin:0'>line {number}</p>")).collect();
        let mut session = session(&lines);
        let root = session.document().root();
        session.set_scroll_offset(root, 0.0, 3000.0).unwrap();
        session.display_list(400, 200, &Fixed).unwrap();
        let expected = text_of(&session, "#l150");
        assert_eq!(session.caret_position_from_point(5.0, 5.0), Some((expected, 0)));
        assert_eq!(session.caret_position_from_point(35.0, 25.0), Some((text_of(&session, "#l151"), 3)));
        session.set_selection(Some((Boundary { container: expected, offset: 5 }, Boundary { container: expected, offset: 8 })));
        assert_eq!(session.selection_rects(), vec![Rect { x: 50.0, y: 0.0, width: 30.0, height: 20.0 }]);
        session.set_root_scroll_window(false);
        session.display_list(400, 200, &Fixed).unwrap();
        assert_eq!(session.caret_position_from_point(5.0, 5.0), Some((expected, 0)), "laid out at the offset directly");
        assert_eq!(session.selection_rects(), vec![Rect { x: 50.0, y: 0.0, width: 30.0, height: 20.0 }]);
        session.set_scroll_offset(root, 0.0, 3020.0).unwrap();
        session.display_list(400, 200, &Fixed).unwrap();
        assert_eq!(session.selection_rects(), vec![Rect { x: 50.0, y: -20.0, width: 30.0, height: 20.0 }]);
    }

    fn rect_of(x: f32, y: f32, width: f32, height: f32) -> Rect {
        Rect { x, y, width, height }
    }

    #[test]
    fn a_document_changed_since_layout_keeps_hit_testing_rectangles_and_text_working() {
        let mut session = session("<p id=p style='margin:0'>hello world</p><p id=q style='margin:0'>other</p>");
        let node = text_of(&session, "#p");
        let other = text_of(&session, "#q");
        session.set_selection(Some((Boundary { container: node, offset: 2 }, Boundary { container: node, offset: 7 })));
        let before = session.selection_rects();
        assert_eq!(before, vec![rect_of(20.0, 0.0, 50.0, 20.0)]);
        session.document_mut().append_data(other, "!").unwrap();
        assert_ne!(session.text_run_frame().map(|frame| frame.2), Some(true), "the document moved on");
        assert_eq!(session.selection_rects(), before);
        assert_eq!(session.selected_text().as_deref(), Some("llo w"));
        assert_eq!(session.caret_position_from_point(24.0, 5.0), Some((node, 2)));
        assert_eq!(session.nearest_caret_position(24.0, 5.0), Some((node, 2)));
        assert_eq!(session.caret_position_from_point(24.0, 25.0), Some((other, 2)));
        assert!(session.block_range_at(node).is_some());
    }

    #[test]
    fn nodes_removed_or_shortened_since_layout_are_not_addressed() {
        let mut session = session("<p id=p style='margin:0'>hello world</p><p id=q style='margin:0'>other</p>");
        let node = text_of(&session, "#p");
        let other = text_of(&session, "#q");
        session.set_selection(Some((Boundary { container: node, offset: 2 }, Boundary { container: other, offset: 3 })));
        session.document_mut().delete_data(node, 4, 7).unwrap();
        assert_eq!(session.caret_position_from_point(24.0, 5.0), None, "the text is shorter than it was painted");
        assert_eq!(session.caret_position_from_point(24.0, 25.0), Some((other, 2)));
        let parent = session.document().parent(other).unwrap().unwrap();
        session.document_mut().remove(parent).unwrap();
        assert_eq!(session.caret_position_from_point(24.0, 25.0), None);
        assert!(session.selection_rects().len() <= 1);
        let _ = session.selected_text();
        assert_eq!(session.block_range_at(other), None);
    }

    #[test]
    fn user_select_none_inside_a_paragraph_is_not_copied() {
        let mut session = session("<p id=p style='margin:0'>aa <span style='user-select:none'>XX</span> bb</p>");
        let node = text_of(&session, "#p");
        let end = session.document().next_sibling(session.document().next_sibling(node).unwrap().unwrap()).unwrap().unwrap();
        session.set_selection(Some((Boundary { container: node, offset: 0 }, Boundary { container: end, offset: 3 })));
        let text = session.selected_text().unwrap();
        assert!(text.contains("aa") && text.contains("bb") && !text.contains("XX"), "{text:?}");
        let rects = session.selection_rects();
        assert!(rects.iter().all(|rect| rect.x + rect.width <= 30.0 || rect.x >= 50.0), "{rects:?}");
    }

    #[test]
    fn user_select_auto_overrides_an_inherited_none_and_all_selects_its_paragraph() {
        let session = session(
            "<div style='user-select:none'><p id=a style='margin:0;user-select:auto'>yes</p><p id=b style='margin:0'>no</p></div>\
             <p id=c style='margin:0;user-select:all'>one two</p>",
        );
        assert_eq!(session.caret_position_from_point(5.0, 5.0), Some((text_of(&session, "#a"), 0)));
        assert_eq!(session.caret_position_from_point(5.0, 25.0), None);
        let all = text_of(&session, "#c");
        assert_eq!(session.user_select_all_range_at(all).map(|(start, end)| (start.offset, end.offset)), Some((0, 7)));
        assert_eq!(session.user_select_all_range_at(text_of(&session, "#a")), None);
    }

    #[test]
    fn a_press_on_a_control_or_replaced_element_does_not_start_a_selection() {
        let session = session("<p id=p style='margin:0'>text</p><button style='margin:0;display:block;width:200px;height:20px'>b</button>\
            <img style='display:block;width:50px;height:20px'><div id=d style='height:40px'></div>");
        let node = text_of(&session, "#p");
        assert_eq!(session.caret_position_from_point(38.0, 5.0), Some((node, 4)));
        assert_eq!(session.caret_position_from_point(150.0, 30.0), None, "on the button");
        assert_eq!(session.caret_position_from_point(25.0, 50.0), None, "on the image");
        assert_eq!(session.caret_position_from_point(300.0, 62.0), None, "an empty block that holds no text");
    }

    #[test]
    fn a_press_in_a_block_selects_from_the_nearest_position_of_that_block() {
        let session = session("<div id=d style='width:300px;height:100px'><p id=p style='margin:0'>one two</p></div>");
        let node = text_of(&session, "#p");
        assert_eq!(session.caret_position_from_point(200.0, 10.0), Some((node, 7)));
        assert_eq!(session.caret_position_from_point(200.0, 30.0), Some((node, 7)), "within a line below it");
        assert_eq!(session.caret_position_from_point(200.0, 90.0), None);
    }

    #[test]
    fn text_clipped_by_overflow_is_neither_hit_nor_highlighted() {
        let mut session = session("<div id=d style='width:50px;overflow:hidden;white-space:nowrap'><span id=s>aaaaa bbbbb ccccc</span></div><p id=p style='margin:0'>tail</p>");
        let node = text_of(&session, "#s");
        session.set_selection(Some((Boundary { container: node, offset: 0 }, Boundary { container: node, offset: 17 })));
        let rects = session.selection_rects();
        assert!(!rects.is_empty() && rects.iter().all(|rect| rect.x + rect.width <= 50.5), "{rects:?}");
        let tail = text_of(&session, "#p");
        assert_eq!(session.caret_position_from_point(30.0, 25.0), Some((tail, 3)));
        assert_eq!(session.selected_text().as_deref(), Some("aaaaa bbbbb ccccc"), "clipped text is still copied");
    }

    #[test]
    fn a_transformed_paragraph_is_hit_and_highlighted_where_it_is_painted() {
        let mut session = session("<p id=p style='margin:0;transform:translate(100px,50px)'>hello</p>");
        let node = text_of(&session, "#p");
        assert_eq!(session.caret_position_from_point(125.0, 55.0), Some((node, 2)));
        assert_eq!(session.caret_position_from_point(25.0, 5.0), None, "where it was laid out, nothing is painted");
        session.set_selection(Some((Boundary { container: node, offset: 1 }, Boundary { container: node, offset: 3 })));
        assert_eq!(session.selection_rects(), vec![rect_of(110.0, 50.0, 20.0, 20.0)]);
    }

    #[test]
    fn a_scaled_paragraph_maps_points_through_its_transform() {
        let mut session = session("<p id=p style='margin:0;width:100px;transform-origin:0 0;transform:scale(2)'>hello</p>");
        let node = text_of(&session, "#p");
        assert_eq!(session.caret_position_from_point(45.0, 10.0), Some((node, 2)));
        session.set_selection(Some((Boundary { container: node, offset: 1 }, Boundary { container: node, offset: 3 })));
        assert_eq!(session.selection_rects(), vec![rect_of(20.0, 0.0, 40.0, 40.0)]);
    }

    #[test]
    fn positions_count_utf16_units_of_characters_outside_the_bmp() {
        let mut session = session("<p id=p style='margin:0'>a\u{1F600}b</p>");
        let node = text_of(&session, "#p");
        assert_eq!(session.caret_position_from_point(1.0, 5.0), Some((node, 0)));
        assert_eq!(session.caret_position_from_point(11.0, 5.0), Some((node, 1)));
        assert_eq!(session.caret_position_from_point(19.0, 5.0), Some((node, 3)));
        assert_eq!(session.caret_position_from_point(29.0, 5.0), Some((node, 4)));
        session.set_selection(Some((Boundary { container: node, offset: 1 }, Boundary { container: node, offset: 3 })));
        assert_eq!(session.selection_rects(), vec![rect_of(10.0, 0.0, 10.0, 20.0)]);
        assert_eq!(session.selected_text().as_deref(), Some("\u{1F600}"));
    }

    #[test]
    fn text_in_an_inner_scroller_follows_its_scroll_position() {
        let lines: String = (0..30).map(|number| alloc::format!("<p id=l{number} style='margin:0'>line {number}</p>")).collect();
        let mut session = session(&alloc::format!("<div id=s style='height:60px;overflow:auto'>{lines}</div>"));
        let scroller = selector::query_selector(session.document(), session.document().root(), "#s").unwrap().unwrap();
        let line = text_of(&session, "#l5");
        session.set_selection(Some((Boundary { container: line, offset: 2 }, Boundary { container: line, offset: 4 })));
        assert!(session.selection_rects().is_empty(), "below the scroller's clip");
        session.set_scroll_offset(scroller, 0.0, 100.0).unwrap();
        session.display_list(400, 200, &Fixed).unwrap();
        assert_eq!(session.selection_rects(), vec![rect_of(20.0, 0.0, 20.0, 20.0)]);
        assert_eq!(session.caret_position_from_point(5.0, 5.0), Some((line, 0)));
        assert_eq!(session.caret_position_from_point(55.0, 5.0), Some((line, 5)));
    }

    #[test]
    fn text_after_a_block_in_an_inner_scroller_is_tied_to_its_enclosing_box() {
        let mut session = session(
            "<div id=s style='height:40px;overflow:auto'><p style='margin:0'>first</p>tail <b>bold</b><p style='margin:0'>last</p><p style='margin:0'>more</p><p style='margin:0'>even more</p></div>",
        );
        let scroller = selector::query_selector(session.document(), session.document().root(), "#s").unwrap().unwrap();
        let tail = session.document().next_sibling(session.document().first_child(scroller).unwrap().unwrap()).unwrap().unwrap();
        session.set_selection(Some((Boundary { container: tail, offset: 0 }, Boundary { container: tail, offset: 4 })));
        assert_eq!(session.selection_rects(), vec![rect_of(0.0, 20.0, 40.0, 20.0)]);
        session.set_scroll_offset(scroller, 0.0, 20.0).unwrap();
        session.display_list(400, 200, &Fixed).unwrap();
        assert_eq!(session.selection_rects(), vec![rect_of(0.0, 0.0, 40.0, 20.0)]);
        assert_eq!(session.caret_position_from_point(15.0, 5.0), Some((tail, 1)));
    }
}
