//! Parse one large import-free stylesheet on several threads. The text is cut
//! at top-level rule boundaries, each chunk is parsed independently and the
//! results are merged so the output equals `parse_graph`. A sheet dominated by
//! one `@layer`/`@media`/`@supports` block is cut inside that block instead,
//! with the block's layer and conditions as the chunks' context. Anything that could
//! make the merge differ (imports, anonymous layers, resource limits, a cut
//! that is not a real rule boundary) makes `parse_graph_parallel` return the
//! sequential result instead.
use super::*;

mod incremental;
pub use incremental::IncrementalParser;

/// Fork-join provider for stylesheet chunks; lumen-html owns no threads.
pub trait ChunkRunner: Sync {
    /// Number of jobs that can usefully run at the same time, the caller included.
    fn threads(&self) -> usize;
    /// Run every job to completion, on any thread and in any order, then return.
    fn run<'a>(&self, jobs: Vec<Box<dyn FnOnce() + Send + 'a>>);
}

const MIN_PARALLEL_BYTES: usize = 64 * 1024;
const MIN_CHUNK_BYTES: usize = 16 * 1024;
const MAX_CHUNKS: usize = 16;

pub fn parse_graph_parallel(
    root: &StylesheetSource,
    environment: MediaEnvironment,
    runner: &dyn ChunkRunner,
) -> Result<ParsedStylesheets, CssError> {
    match try_parse(root, runner, MIN_PARALLEL_BYTES, MIN_CHUNK_BYTES) {
        Some(parsed) => Ok(parsed),
        None => {
            #[cfg(test)]
            fallbacks::record();
            parse_graph(root, environment)
        }
    }
}

#[cfg(test)]
mod fallbacks {
    extern crate std;
    use core::cell::Cell;

    std::thread_local! {
        static COUNT: Cell<usize> = const { Cell::new(0) };
    }

    pub(super) fn record() {
        COUNT.with(|count| count.set(count.get() + 1));
    }

    pub(super) fn take() -> usize {
        COUNT.with(|count| count.replace(0))
    }
}

type Scan = Vec<nesting::SourceRuleRange>;

#[derive(Default)]
struct ChunkOutput {
    rules: Vec<Rule>,
    font_faces: Vec<FontFaceRule>,
    family_display: Vec<FontFamilyDisplayRule>,
    keyframes: Vec<KeyframesRuleText>,
    layers: Vec<String>,
    nodes: usize,
}

const MAX_DESCENT: usize = 3;
const MAX_EMIT_DEPTH: usize = 24;

/// Conditions a chunk inherits from the blocks it was cut out of. `layers`
/// seeds the chunk's layer table so the enclosing layers are registered at the
/// same position as in the sequential parse.
#[derive(Clone)]
struct Context {
    media: Arc<[Arc<str>]>,
    supports: Arc<[Arc<str>]>,
    layers: Vec<String>,
    layer: Option<usize>,
    depth: usize,
}

impl Context {
    fn root() -> Self {
        Context { media: Arc::from([]), supports: Arc::from([]), layers: Vec::new(), layer: None, depth: 0 }
    }
}

/// A byte range of the sheet parsed on its own. `top` holds the rule ranges
/// relative to `start`; segments without it are scanned by `scan_segments`.
struct Segment {
    start: usize,
    end: usize,
    context: usize,
    first: bool,
    last: bool,
    top: Option<Scan>,
}

fn try_parse(
    root: &StylesheetSource,
    runner: &dyn ChunkRunner,
    min_bytes: usize,
    min_chunk: usize,
) -> Option<ParsedStylesheets> {
    let text: &str = &root.text;
    if root.disabled || !root.imports.is_empty() || text.len() < min_bytes || text.len() > MAX_CSS_BYTES {
        return None;
    }
    let parts = runner.threads().min(text.len() / min_chunk.max(1)).min(MAX_CHUNKS);
    if parts < 2 {
        return None;
    }
    let cuts = candidate_cuts(text, parts);
    let count = cuts.len() - 1;
    if cuts.windows(2).any(|pair| (pair[1] - pair[0]) * 10 > text.len() * 6) {
        if let Some(parsed) = try_descend(root, runner, parts) {
            return Some(parsed);
        }
    }
    if count < 2 {
        return None;
    }

    let mut segments: Vec<Segment> = (0..count)
        .map(|index| Segment {
            start: cuts[index],
            end: cuts[index + 1],
            context: 0,
            first: index == 0,
            last: index == count - 1,
            top: None,
        })
        .collect();
    scan_segments(text, &mut segments, runner)?;
    let items: usize = segments.iter().filter_map(|segment| segment.top.as_ref()).map(Vec::len).sum();
    if items > MAX_RULES / 4 {
        return None;
    }
    let mut absolute = Vec::with_capacity(items);
    for segment in &segments {
        let shift = segment.start;
        absolute.extend(segment.top.iter().flatten().map(|&(start, end, open, close)| {
            (start + shift, end + shift, open.map(|v| v + shift), close.map(|v| v + shift))
        }));
    }
    let namespaces = NamespaceMap::from_ranges(text, &absolute);
    drop(absolute);
    finish_parse(root, segments, &[Context::root()], &namespaces, runner)
}

fn scan_segments(text: &str, segments: &mut [Segment], runner: &dyn ChunkRunner) -> Option<()> {
    let jobs: Vec<Box<dyn FnOnce() + Send + '_>> = segments
        .iter_mut()
        .filter(|segment| segment.top.is_none())
        .map(|segment| {
            Box::new(move || {
                segment.top = scan_chunk(&text[segment.start..segment.end], segment.first, segment.last)
            }) as Box<dyn FnOnce() + Send + '_>
        })
        .collect();
    runner.run(jobs);
    segments.iter().all(|segment| segment.top.is_some()).then_some(())
}

fn finish_parse(
    root: &StylesheetSource,
    segments: Vec<Segment>,
    contexts: &[Context],
    namespaces: &NamespaceMap,
    runner: &dyn ChunkRunner,
) -> Option<ParsedStylesheets> {
    let outputs = emit_segments(&root.text, segments, contexts, namespaces, runner)?;
    assemble(outputs, root)
}

fn emit_segments(
    text: &str,
    segments: Vec<Segment>,
    contexts: &[Context],
    namespaces: &NamespaceMap,
    runner: &dyn ChunkRunner,
) -> Option<Vec<ChunkOutput>> {
    let segments = split_large_segments(segments, runner.threads());
    let mut outputs: Vec<Option<ChunkOutput>> = (0..segments.len()).map(|_| None).collect();
    let jobs: Vec<Box<dyn FnOnce() + Send + '_>> = outputs
        .iter_mut()
        .zip(segments)
        .map(|(slot, segment)| {
            let context = &contexts[segment.context];
            Box::new(move || {
                *slot = segment.top.and_then(|top| {
                    emit_chunk(&text[segment.start..segment.end], segment.start, top, context, namespaces)
                })
            }) as Box<dyn FnOnce() + Send + '_>
        })
        .collect();
    runner.run(jobs);
    outputs.into_iter().collect()
}

/// Cuts segments much larger than their fair share at block boundaries of
/// their known rule ranges, so one big segment (e.g. a whole streamed
/// backlog) does not serialise the parse. The pieces keep source order.
fn split_large_segments(segments: Vec<Segment>, threads: usize) -> Vec<Segment> {
    if threads < 2 {
        return segments;
    }
    let total: usize = segments.iter().map(|segment| segment.end - segment.start).sum();
    let target = (total / (threads * 2)).max(MIN_CHUNK_BYTES);
    let mut pieces = Vec::with_capacity(segments.len());
    for segment in segments {
        if segment.end - segment.start <= target * 3 / 2 {
            pieces.push(segment);
            continue;
        }
        split_segment(segment, target, &mut pieces);
    }
    pieces
}

fn split_segment(segment: Segment, target: usize, pieces: &mut Vec<Segment>) {
    let Some(top) = &segment.top else {
        pieces.push(segment);
        return;
    };
    let mut from_range = 0;
    let mut from_byte = 0;
    for (index, &(_, end, open, close)) in top.iter().enumerate() {
        let is_block = open.is_some() && close.is_some_and(|close| close < end);
        let remaining = segment.end - segment.start - end;
        if !is_block || index + 1 == top.len() || end - from_byte < target || remaining < target / 2 {
            continue;
        }
        pieces.push(Segment {
            start: segment.start + from_byte,
            end: segment.start + end,
            context: segment.context,
            first: segment.first && from_byte == 0,
            last: false,
            top: Some(rebase_ranges(&top[from_range..=index], from_byte)),
        });
        from_range = index + 1;
        from_byte = end;
    }
    pieces.push(Segment {
        start: segment.start + from_byte,
        end: segment.end,
        context: segment.context,
        first: segment.first && from_byte == 0,
        last: segment.last,
        top: Some(rebase_ranges(&top[from_range..], from_byte)),
    });
}

fn assemble(outputs: Vec<ChunkOutput>, root: &StylesheetSource) -> Option<ParsedStylesheets> {
    let parser = merge(outputs, root)?;
    parser.finish(root).ok()
}

fn shift_ranges(ranges: Scan, by: usize) -> Scan {
    ranges
        .into_iter()
        .map(|(start, end, open, close)| (start + by, end + by, open.map(|v| v + by), close.map(|v| v + by)))
        .collect()
}

fn rebase_ranges(ranges: &[nesting::SourceRuleRange], by: usize) -> Scan {
    ranges
        .iter()
        .map(|&(start, end, open, close)| (start - by, end - by, open.map(|v| v - by), close.map(|v| v - by)))
        .collect()
}

/// A sheet dominated by one block (`@layer x { ... }`) has no top-level cut
/// points. Descend into that block, parse its siblings as their own segments
/// and cut its body at the child rule boundaries, with the block's layer and
/// conditions seeded as the context of every body chunk.
fn try_descend(root: &StylesheetSource, runner: &dyn ChunkRunner, parts: usize) -> Option<ParsedStylesheets> {
    let text: &str = &root.text;
    let all = nesting::source_rule_ranges(text, false).ok()?;
    if all.len() > MAX_RULES / 4 {
        return None;
    }
    let header_end = all.iter().find(|range| range.2.is_some())?.1;
    if !imports(&text[..header_end]).ok()?.is_empty() {
        return None;
    }
    let namespaces = NamespaceMap::from_ranges(text, &all);

    let mut contexts = vec![Context::root()];
    let mut before: Vec<Segment> = Vec::new();
    let mut after: Vec<Segment> = Vec::new();
    let (mut region, mut tops, mut current) = (0..text.len(), all, 0usize);
    let mut descended = 0;
    while descended < MAX_DESCENT {
        let Some(index) = tops.iter().position(|&(start, end, open, close)| {
            open.is_some() && close.is_some() && (end - start) * 2 >= region.len()
        }) else {
            break;
        };
        let (start, end, open, close) = tops[index];
        let (Some(open), Some(close)) = (open, close) else { break };
        let Some(context) = child_context(text, &contexts[current], start, open, &namespaces) else {
            break;
        };
        if index > 0 {
            before.push(Segment {
                start: region.start,
                end: start,
                context: current,
                first: false,
                last: false,
                top: Some(rebase_ranges(&tops[..index], region.start)),
            });
        }
        if index + 1 < tops.len() {
            after.push(Segment {
                start: end,
                end: region.end,
                context: current,
                first: false,
                last: false,
                top: Some(rebase_ranges(&tops[index + 1..], end)),
            });
        }
        contexts.push(context);
        current = contexts.len() - 1;
        region = open + 1..close;
        tops = shift_ranges(nesting::source_rule_ranges(&text[region.clone()], false).ok()?, region.start);
        if tops.len() > MAX_RULES / 4 {
            return None;
        }
        descended += 1;
    }
    if descended == 0 {
        return None;
    }

    let cuts = candidate_cuts(&text[region.clone()], parts);
    let count = cuts.len() - 1;
    let mut segments = before;
    segments.extend((0..count).map(|index| Segment {
        start: region.start + cuts[index],
        end: region.start + cuts[index + 1],
        context: current,
        first: false,
        last: index == count - 1,
        top: None,
    }));
    segments.extend(after.into_iter().rev());
    if segments.len() < 2 {
        return None;
    }
    scan_segments(text, &mut segments, runner)?;
    finish_parse(root, segments, &contexts, &namespaces, runner)
}

/// Context of the children of the block whose prelude is `text[start..open]`,
/// as `emit_source_rules` derives it. Anything but a valid named `@layer`, an
/// `@media` or a supported `@supports` block is not descended into.
fn child_context(
    text: &str,
    parent: &Context,
    start: usize,
    open: usize,
    namespaces: &NamespaceMap,
) -> Option<Context> {
    let prelude = text[start..open].trim();
    let probe = alloc::format!("{prelude}{{}}");
    let probed = nesting::parse_source_rules_with_boundaries_in_namespace_context(&probe, &[], false, &[], namespaces).ok()?;
    match probed.as_slice() {
        [node] if node.kind == nesting::SourceRuleKind::Group && node.children.is_empty() => {}
        _ => return None,
    }
    let mut context = parent.clone();
    context.depth += 1;
    if nesting::at_rule(prelude, "@media") {
        context.media = parent
            .media
            .iter()
            .cloned()
            .chain([Arc::from(prelude[6..].trim())])
            .collect();
    } else if nesting::at_rule(prelude, "@supports") {
        if parse_supports_condition(&prelude[9..], 0, namespaces) != Some(true) {
            return None;
        }
    } else if nesting::at_rule(prelude, "@layer") {
        let raw = prelude[6..].trim();
        if raw.is_empty() {
            return None;
        }
        let name = match parent.layer {
            Some(layer) => alloc::format!("{}.{raw}", parent.layers[layer]),
            None => raw.into(),
        };
        if context.layers.len() >= 128 || name.split('.').count() >= 8 {
            return None;
        }
        let index = match context.layers.iter().position(|value| value == &name) {
            Some(index) => index,
            None => {
                context.layers.push(name);
                context.layers.len() - 1
            }
        };
        context.layer = Some(index);
    } else {
        return None;
    }
    Some(context)
}


/// Positions just after a top-level `}` near equal byte shares. These are
/// only candidates: `scan_chunk` proves each one is a real rule boundary.
fn candidate_cuts(text: &str, parts: usize) -> Vec<usize> {
    let bytes = text.as_bytes();
    let target = bytes.len() / parts;
    let mut cuts = Vec::with_capacity(parts + 1);
    cuts.push(0);
    let (mut depth, mut next, mut at) = (0usize, target, 0usize);
    while at < bytes.len() {
        match bytes[at] {
            b'/' if bytes.get(at + 1) == Some(&b'*') => {
                match bytes[at + 2..].windows(2).position(|pair| pair == b"*/") {
                    Some(end) => at += end + 3,
                    None => break,
                }
            }
            quote @ (b'"' | b'\'') => {
                at += 1;
                while at < bytes.len() {
                    match bytes[at] {
                        b'\\' => at += 2,
                        byte if byte == quote => break,
                        b'\n' | b'\r' | b'\x0c' => break,
                        _ => at += 1,
                    }
                }
            }
            b'{' => depth += 1,
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 && at + 1 >= next && cuts.len() < parts && bytes.len() - (at + 1) >= target / 2 {
                    cuts.push(at + 1);
                    next = at + 1 + target;
                }
            }
            _ => {}
        }
        at += 1;
    }
    cuts.push(bytes.len());
    cuts
}

/// Top-level ranges of one chunk. A chunk that is not last must end exactly
/// where its final closed block ends, which proves the cut is a boundary the
/// whole-sheet scan would also find. The first chunk also proves the sheet has
/// no leading imports: it ends in a block, which closes the import header.
fn scan_chunk(chunk: &str, first: bool, last: bool) -> Option<Scan> {
    if first && !imports(chunk).ok()?.is_empty() {
        return None;
    }
    let ranges = nesting::source_rule_ranges(chunk, false).ok()?;
    if !last {
        let &(_, end, open, close) = ranges.last()?;
        if end != chunk.len() || open.is_none() || !close.is_some_and(|close| close < end) {
            return None;
        }
    }
    Some(ranges)
}

fn emit_chunk(
    chunk: &str,
    base: usize,
    top: Scan,
    context: &Context,
    namespaces: &NamespaceMap,
) -> Option<ChunkOutput> {
    let source = nesting::parse_stylesheet_source_rules_with_boundaries(chunk, &[], namespaces, top).ok()?;
    if context.depth + tree_depth(&source) > MAX_EMIT_DEPTH {
        return None;
    }
    let mut output = ChunkOutput { nodes: node_count(&source), layers: context.layers.clone(), ..ChunkOutput::default() };
    emit_source_rules(
        chunk,
        &source,
        &mut output.rules,
        &context.media,
        &context.supports,
        context.layer,
        &mut output.layers,
        &mut output.font_faces,
        &mut output.family_display,
        &mut output.keyframes,
        base,
        &Arc::from([]),
        context.depth,
        false,
        namespaces,
    )
    .ok()?;
    Some(output)
}

fn tree_depth(tree: &[nesting::SourceRule]) -> usize {
    tree.iter().map(|node| 1 + tree_depth(&node.children)).max().unwrap_or(0)
}

fn node_count(tree: &[nesting::SourceRule]) -> usize {
    tree.iter().map(|node| 1 + node_count(&node.children)).sum()
}

fn anonymous_layer(name: &str) -> bool {
    name.starts_with('#') || name.contains(".#")
}

fn merge<'a>(outputs: Vec<ChunkOutput>, root: &StylesheetSource) -> Option<StylesheetGraphParser<'a>> {
    if outputs.iter().map(|output| output.nodes).sum::<usize>() > MAX_RULES / 2
        || outputs.iter().map(|output| output.rules.len()).sum::<usize>() > MAX_RULES
        || outputs.iter().map(|output| output.font_faces.len()).sum::<usize>() > 64
        || outputs.iter().map(|output| output.family_display.len()).sum::<usize>() > 64
        || outputs.iter().any(|output| output.layers.iter().any(|name| anonymous_layer(name)))
    {
        return None;
    }
    let mut parser = StylesheetGraphParser {
        topology_owner: None,
        topologies: &[],
        rules: Vec::new(),
        font_faces: Vec::new(),
        family_display: Vec::new(),
        keyframes: Vec::new(),
        position_try: super::anchor::collect_position_try_rules(&root.text),
        counter_styles: super::counter_styles::collect_rules(&root.text),
        functions: super::function_eval::collect_rules(&root.text),
        layers: Vec::new(),
        active_urls: Vec::new(),
        bytes: root.text.len(),
        sheets: 1,
        import_count: 0,
    };
    for output in outputs {
        let mut map = Vec::with_capacity(output.layers.len());
        for name in output.layers {
            map.push(match parser.layers.iter().position(|existing| existing == &name) {
                Some(index) => index,
                None => {
                    parser.layers.push(name);
                    parser.layers.len() - 1
                }
            });
        }
        let remap = |layer: Option<usize>| layer.map(|index| map[index]);
        let (face_base, keyframe_base) = (parser.font_faces.len(), parser.keyframes.len());
        parser.rules.extend(output.rules.into_iter().map(|mut rule| {
            rule.layer = remap(rule.layer);
            rule
        }));
        parser.font_faces.extend(output.font_faces.into_iter().map(|mut face| {
            face.layer = remap(face.layer);
            face.source_order += face_base;
            face
        }));
        parser.family_display.extend(output.family_display.into_iter().map(|mut rule| {
            rule.layer = remap(rule.layer);
            rule
        }));
        parser.keyframes.extend(output.keyframes.into_iter().map(|mut keyframes| {
            keyframes.source_order += keyframe_base;
            keyframes
        }));
    }
    // A full table makes layer blocks fail in the sequential parse only.
    if parser.layers.len() >= 128 {
        return None;
    }
    let revision = css_source_revision(&root.text);
    parser.stamp_segment(0, 0, 0, &root.url, &root.text, &[], revision);
    Some(parser)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;

    struct Inline(usize);

    impl ChunkRunner for Inline {
        fn threads(&self) -> usize {
            self.0
        }
        fn run<'a>(&self, jobs: Vec<Box<dyn FnOnce() + Send + 'a>>) {
            for job in jobs.into_iter().rev() {
                job();
            }
        }
    }

    fn source(text: &str) -> StylesheetSource {
        StylesheetSource {
            disabled: false,
            url: Arc::from("https://chunk.test/a.css"),
            text: Arc::from(text),
            imports: Vec::new(),
        }
    }

    fn dump(parsed: &ParsedStylesheets) -> alloc::string::String {
        format!(
            "{:?}\n{:?}\n{:?}\n{:?}\n{:?}",
            parsed.rules, parsed.font_faces, parsed.family_display, parsed.keyframes, parsed.layers
        )
    }

    fn assert_same(text: &str, threads: usize, expect_parallel: bool) {
        let root = source(text);
        let sequential = parse_graph(&root, MediaEnvironment::default());
        let parallel = try_parse(&root, &Inline(threads), 0, 1);
        assert_eq!(parallel.is_some(), expect_parallel, "parallel path taken for {text:.80}");
        if let Some(parallel) = parallel {
            assert_eq!(dump(&parallel), dump(&sequential.expect("sequential parse")), "{text:.80}");
        }
    }

    fn filler(prefix: &str, count: usize) -> alloc::string::String {
        (0..count).map(|n| format!(".{prefix}{n}{{color:red;margin:{n}px}}")).collect()
    }

    #[test]
    fn plain_rules_merge_in_source_order() {
        let text = filler("a", 60);
        for threads in [2, 3, 8] {
            assert_same(&text, threads, true);
        }
        assert_same(&format!("{}{}", filler("a", 30), "a,b>c{width:1px}d:hover::before{height:2px}"), 4, true);
    }

    #[test]
    fn layer_statements_and_nested_blocks_get_sequential_ids() {
        let mut text = alloc::string::String::from("@layer base, theme;");
        text += &filler("a", 10);
        text += "@layer theme{.t{color:red}@layer inner{.ti{color:blue}}}";
        text += &filler("b", 10);
        text += "@layer late, base;@layer base.deep{.d{color:green}}";
        text += &filler("c", 10);
        text += "@layer inner.more{.x{color:red}}@layer theme{@layer inner{.y{width:1px}}}";
        text += &filler("d", 10);
        for threads in [2, 3, 5] {
            assert_same(&text, threads, true);
        }
    }

    #[test]
    fn layer_declared_first_in_a_later_chunk_keeps_global_order() {
        let mut text = filler("a", 20);
        text += "@layer z{.z{color:red}}@layer a{.a{color:red}}";
        text += &filler("b", 20);
        text += "@layer a{.a2{color:blue}}@layer z{.z2{color:blue}}@layer m{.m{width:2px}}";
        text += &filler("c", 20);
        assert_same(&text, 3, true);
    }

    #[test]
    fn anonymous_layers_fall_back_to_sequential() {
        let mut text = filler("a", 20);
        text += "@layer{.anon{color:red}}";
        text += &filler("b", 20);
        assert_same(&text, 3, false);
    }

    #[test]
    fn media_and_supports_blocks_carry_conditions() {
        let mut text = filler("a", 15);
        text += "@media (min-width:600px){.m{color:red}@media print{.n{color:blue}}}";
        text += &filler("b", 15);
        text += "@supports (display:grid){.s{display:grid}}@supports (foo:bar){.no{color:red}}";
        text += &filler("c", 15);
        text += "@media screen and (max-width:10px){@supports (display:flex){.q{display:flex}}}";
        text += &filler("d", 15);
        assert_same(&text, 4, true);
    }

    #[test]
    fn font_faces_keyframes_and_properties_keep_source_order() {
        let mut text = filler("a", 10);
        text += "@font-face{font-family:A;src:url(a.woff2)}";
        text += "@keyframes one{from{opacity:0}to{opacity:1}}";
        text += &filler("b", 10);
        text += "@font-face{font-family:B;src:url(b.woff2);font-weight:700}";
        text += "@property --x{syntax:'<length>';inherits:false;initial-value:0px}";
        text += &filler("c", 10);
        text += "@keyframes two{0%{width:1px}100%{width:2px}}@-webkit-keyframes three{to{width:3px}}";
        text += "@layer fonts{@font-face{font-family:C;src:url(c.woff2)}}";
        text += "@font-feature-values Foo{font-display:swap}";
        text += &filler("d", 10);
        assert_same(&text, 4, true);
    }

    #[test]
    fn namespaces_apply_to_every_chunk() {
        let mut text = alloc::string::String::from("@namespace svg url(http://www.w3.org/2000/svg);@namespace 'urn:default';");
        text += &filler("a", 10);
        text += "svg|rect{fill:red}";
        text += &filler("b", 10);
        text += "@namespace late 'urn:late';late|x{width:1px}svg|circle{fill:blue}";
        text += &filler("c", 10);
        text += "@supports selector(svg|rect){.n{color:red}}";
        text += &filler("d", 10);
        assert_same(&text, 4, true);
    }

    #[test]
    fn imports_fall_back_to_sequential() {
        let mut text = alloc::string::String::from("@import 'x.css';");
        text += &filler("a", 30);
        let root = source(&text);
        assert!(try_parse(&root, &Inline(4), 0, 1).is_none());
        let mut root = source(&filler("a", 30));
        root.disabled = true;
        assert!(try_parse(&root, &Inline(4), 0, 1).is_none());
        assert_same("@charset 'utf-8';.a{color:red}.b{color:red}.c{color:red}.d{color:red}", 2, true);
    }

    #[test]
    fn braces_inside_strings_comments_and_urls_do_not_cut_rules() {
        let mut text = alloc::string::String::new();
        for n in 0..12 {
            text += &format!(".s{n}::before{{content:\"}} {{ /* \";color:red}}");
            text += &format!("/* }} {{ */.c{n}{{background:url(\"a}}b.png\")}}");
            text += &format!(".u{n}{{background:url(x{{y.png)}}.e{n}[data-x='}}']{{width:{n}px}}");
            text += &format!(".b{n}{{--v:{{a:b}};content:'\\'}}'}}");
        }
        for threads in [2, 3, 6] {
            let root = source(&text);
            let sequential = parse_graph(&root, MediaEnvironment::default()).unwrap();
            if let Some(parallel) = try_parse(&root, &Inline(threads), 0, 1) {
                assert_eq!(dump(&parallel), dump(&sequential));
            }
        }
    }

    #[test]
    fn malformed_rules_near_chunk_edges_match_or_fall_back() {
        let cases = [
            format!("{}}}{}", filler("a", 10), filler("b", 10)),
            format!("{}.broken{{color:red;{}", filler("a", 10), filler("b", 10)),
            format!("{}@media (x{{.m{{color:red}}}}{}", filler("a", 10), filler("b", 10)),
            format!("{}@foo bar;;{{}}{}", filler("a", 10), filler("b", 10)),
            format!("{}.x{{color:red}} ;;; .y{{color:blue}}{}", filler("a", 10), filler("b", 10)),
            format!("{}<!-- .p{{color:red}} -->{}", filler("a", 10), filler("b", 10)),
            format!("{}.u{{width:1px}}\"unterminated\n.z{{height:1px}}{}", filler("a", 10), filler("b", 10)),
            format!("{}.last{{color:red}} /* unterminated", filler("a", 10)),
            format!("{}.tail{{color:red", filler("a", 10)),
            format!("{}@layer a,;{}", filler("a", 10), filler("b", 10)),
        ];
        for text in &cases {
            for threads in [2, 3, 7] {
                let root = source(text);
                let sequential = parse_graph(&root, MediaEnvironment::default());
                match try_parse(&root, &Inline(threads), 0, 1) {
                    Some(parallel) => {
                        assert_eq!(dump(&parallel), dump(&sequential.expect("sequential parse")), "{text:.80}")
                    }
                    None => {}
                }
            }
        }
        assert_same(&cases[4], 3, true);
    }

    #[test]
    fn errors_are_reported_by_the_sequential_parse() {
        let mut text = filler("a", 20);
        text += &format!("@layer {}{{.x{{color:red}}}}", ["l"; 9].join("."));
        text += &filler("b", 20);
        let root = source(&text);
        let expected = parse_graph(&root, MediaEnvironment::default());
        let actual = parse_graph_parallel(&root, MediaEnvironment::default(), &Inline(4));
        assert_eq!(format!("{expected:?}"), format!("{actual:?}"));
        assert!(try_parse(&root, &Inline(4), 0, 1).is_none());
    }

    #[test]
    fn small_sheets_and_single_thread_stay_sequential() {
        let root = source(&filler("a", 30));
        assert!(try_parse(&root, &Inline(1), 0, 1).is_none());
        let parsed = parse_graph_parallel(&root, MediaEnvironment::default(), &Inline(4)).unwrap();
        assert_eq!(parsed.rules.len(), 30);
        assert_eq!(dump(&parsed), dump(&parse_graph(&root, MediaEnvironment::default()).unwrap()));
        assert_eq!(candidate_cuts("a{}b{}c{}d{}", 2).len(), 3);
    }

    #[test]
    fn single_layer_wrapper_is_cut_inside() {
        let mut text = alloc::string::String::from("@layer x{");
        text += &filler("a", 40);
        text += "@font-face{font-family:W;src:url(w.woff2)}@keyframes k{from{opacity:0}to{opacity:1}}";
        text += &filler("b", 40);
        text += "}";
        for threads in [2, 3, 8] {
            assert_same(&text, threads, true);
        }
    }

    #[test]
    fn nested_layers_inside_a_wrapper_keep_global_order() {
        let mut text = alloc::string::String::from("@layer first;@layer x{@layer a, b;");
        text += &filler("a", 20);
        text += "@layer a.b{.n{color:red}}";
        text += &filler("b", 20);
        text += "@layer c{@layer a.b{.m{color:blue}}.c{width:1px}}@layer b;";
        text += &filler("c", 20);
        text += "@layer a{.z{height:1px}}}";
        for threads in [2, 3, 5] {
            assert_same(&text, threads, true);
        }
    }

    #[test]
    fn media_and_supports_wrappers_carry_conditions() {
        let media = format!("@media (min-width:1px){{{}}}", filler("a", 60));
        let supports = format!("@supports (display:grid){{{}}}", filler("a", 60));
        let unsupported = format!("@supports (foo:bar){{{}}}", filler("a", 60));
        let nested = format!("@layer o{{@media screen{{{}@supports (display:grid){{.s{{color:red}}}}}}}}", filler("a", 60));
        for text in [media, supports, nested] {
            for threads in [2, 4] {
                assert_same(&text, threads, true);
            }
        }
        assert_same(&unsupported, 3, false);
    }

    #[test]
    fn content_around_a_wrapper_is_parsed_in_order() {
        let mut text = alloc::string::String::from("@namespace svg url(http://www.w3.org/2000/svg);@layer z, x;");
        text += &filler("p", 2);
        text += "svg|rect{fill:red}@layer y{.y{color:red}}";
        text += &format!("@layer x{{{}}}", filler("a", 60));
        text += "@layer x{.again{color:blue}}@layer w{.w{width:1px}}";
        text += &filler("q", 2);
        text += "@media print{.pm{color:red}}";
        for threads in [2, 3, 6] {
            assert_same(&text, threads, true);
        }
    }

    #[test]
    fn anonymous_layers_around_a_wrapper_fall_back() {
        let inside = format!("@layer x{{{}@layer{{.anon{{color:red}}}}{}}}", filler("a", 30), filler("b", 30));
        assert_same(&inside, 3, false);
        let wrapper = format!("@layer{{{}}}", filler("a", 60));
        assert_same(&wrapper, 3, false);
    }

    #[test]
    fn malformed_children_near_wrapper_cuts_match_or_fall_back() {
        let cases = [
            format!("@layer x{{{}.broken{{color:red;{}}}", filler("a", 20), filler("b", 20)),
            format!("@layer x{{{}@media (x{{.m{{color:red}}}}{}}}", filler("a", 20), filler("b", 20)),
            format!("@layer x{{{}.x{{color:red}} ;;; @foo bar;{{}}.y{{color:blue}}{}}}", filler("a", 20), filler("b", 20)),
            format!("@layer x{{{}@layer a,;{}}}", filler("a", 20), filler("b", 20)),
            format!("@layer x{{{}@import 'a.css';{}}}", filler("a", 20), filler("b", 20)),
            format!("@layer x{{{}}} }} {}", filler("a", 20), filler("b", 2)),
            format!("{}@layer x{{{}.tail{{color:red", filler("p", 2), filler("a", 20)),
        ];
        for text in &cases {
            for threads in [2, 3, 7] {
                let root = source(text);
                let sequential = parse_graph(&root, MediaEnvironment::default());
                if let Some(parallel) = try_parse(&root, &Inline(threads), 0, 1) {
                    assert_eq!(dump(&parallel), dump(&sequential.expect("sequential parse")), "{text:.80}");
                }
            }
        }
    }

    #[test]
    fn real_size_sheet_takes_the_parallel_path() {
        let mut text = alloc::string::String::from("@layer reset, base, theme;");
        for block in 0..12 {
            text += &format!("@layer {}{{", ["reset", "base", "theme"][block % 3]);
            text += &filler(&format!("k{block}x"), 220);
            text += "}";
            text += &format!("@media (min-width:{}px){{{}}}", 100 + block, filler(&format!("m{block}x"), 60));
        }
        assert!(text.len() > MIN_PARALLEL_BYTES, "{}", text.len());
        let root = source(&text);
        let parallel = try_parse(&root, &Inline(4), MIN_PARALLEL_BYTES, MIN_CHUNK_BYTES).expect("parallel");
        assert_eq!(dump(&parallel), dump(&parse_graph(&root, MediaEnvironment::default()).unwrap()));
    }

    fn primer_like(custom_properties: usize, media_blocks: usize, rules_per_block: usize) -> alloc::string::String {
        let mut text = alloc::string::String::from(":root{");
        for n in 0..custom_properties {
            text += &format!("--token-{n}:var(--base-{},#{:06x});", n % 17, (n * 2_654_435) % 0xff_ffff);
        }
        text += "}:root,[data-color-mode=light]{--lightningcss-light:initial;--lightningcss-dark: ;color-scheme:light}";
        text += "@layer primer-css-base{html{-webkit-text-size-adjust:100%;font-size:16px}body{margin:0}}";
        for block in 0..media_blocks {
            match block % 8 {
                0 => text += &format!("@keyframes spin-{block}{{0%{{transform:rotate(0)}}100%{{transform:rotate(360deg)}}}}"),
                1 => text += "ol[type=a\\ s]{list-style-type:lower-alpha}",
                2 => text += "@supports not selector(:popover-open){[popover]:not(.\\:popover-open){display:none}}",
                3 => text += &format!(".icon-{block}{{background-image:url(data:image/svg+xml;base64,PHN2ZyB3aWR0aD0iMTYi{block}PC9zdmc+)}}"),
                4 => text += "@property --dialog-scrollgutter{syntax:\"<length>\";inherits:false;initial-value:0}",
                5 => text += "@supports (clip-path:polygon(50% 0, 100% 50%, 50% 100%)){.clip{clip-path:polygon(50% 0, 100% 50%, 50% 100%)}}",
                _ => {}
            }
            text += &format!("@media (width>={}px){{", 400 + block);
            for rule in 0..rules_per_block {
                text += &format!(
                    ".m{block}-{rule}{{margin:{rule}px!important;padding-top:var(--base-size-{rule},{rule}px)!important}}"
                );
            }
            text += "}";
        }
        text += "\n/*# sourceMappingURL=primer.css.map*/";
        text
    }

    fn assert_parallel_without_fallback(text: &str) {
        assert!(text.len() > MIN_PARALLEL_BYTES, "{}", text.len());
        let root = source(text);
        let expected = dump(&parse_graph(&root, MediaEnvironment::default()).expect("sequential parse"));
        for threads in [2, 4, 8] {
            fallbacks::take();
            let parsed = parse_graph_parallel(&root, MediaEnvironment::default(), &Inline(threads)).expect("parallel parse");
            assert_eq!(fallbacks::take(), 0, "sequential fallback with {threads} threads");
            assert_eq!(dump(&parsed), expected, "{threads} threads");
        }
    }

    #[test]
    fn utility_sheet_with_custom_properties_and_mixed_at_rules_stays_parallel() {
        assert_parallel_without_fallback(&primer_like(600, 120, 20));
    }

    #[test]
    fn large_custom_property_block_beside_other_rules_stays_parallel() {
        assert_parallel_without_fallback(&primer_like(3000, 150, 20));
    }

    #[test]
    fn fallback_counter_sees_sequential_parses() {
        fallbacks::take();
        let root = source(&filler("a", 30));
        parse_graph_parallel(&root, MediaEnvironment::default(), &Inline(4)).unwrap();
        assert_eq!(fallbacks::take(), 1);
    }
}
