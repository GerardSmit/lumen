//! `parse_graph_parallel` for text that is still arriving. Complete top-level
//! rule segments are cut and parsed while more text is fed in, so the work that
//! is left when the last byte arrives is the unfinished tail. A block that stays
//! open across several segments (`@layer x { ... }`) is descended into the same
//! way `try_descend` does. `finish` yields the result of the whole-text parse,
//! or `None` wherever that parse would not have been chunked (imports,
//! anonymous layers, a changed namespace table, resource limits, a cut that is
//! not a rule boundary); the caller then parses the complete text itself.
use super::*;
use alloc::collections::VecDeque;

/// Unprocessed text that makes a scan for a cut worthwhile.
const SEGMENT_TRIGGER: usize = MIN_CHUNK_BYTES;
/// Open-block size from which the block is descended into.
const WRAPPER_MIN_BYTES: usize = 3 * MIN_CHUNK_BYTES;

struct Wrapper {
    start: usize,
    open: usize,
    context: usize,
}

pub struct IncrementalParser {
    text: String,
    cut: usize,
    trigger: usize,
    segment_bytes: usize,
    wrapper_bytes: usize,
    started: bool,
    failed: bool,
    items: usize,
    contexts: Vec<Context>,
    wrappers: Vec<Wrapper>,
    tops: Scan,
    namespaces: Option<NamespaceMap>,
    queue: VecDeque<Segment>,
    outputs: Vec<ChunkOutput>,
}

impl Default for IncrementalParser {
    fn default() -> Self {
        Self::new()
    }
}

impl IncrementalParser {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            cut: 0,
            trigger: SEGMENT_TRIGGER,
            segment_bytes: SEGMENT_TRIGGER,
            wrapper_bytes: WRAPPER_MIN_BYTES,
            started: false,
            failed: false,
            items: 0,
            contexts: vec![Context::root()],
            wrappers: Vec::new(),
            tops: Vec::new(),
            namespaces: None,
            queue: VecDeque::new(),
            outputs: Vec::new(),
        }
    }

    /// The text fed so far; stale once `failed`.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// True once the streamed parse can no longer match the whole-text parse.
    pub fn failed(&self) -> bool {
        self.failed
    }

    /// Gives up on the streamed parse, e.g. when the text is not valid UTF-8.
    pub fn abandon(&mut self) {
        self.fail();
    }

    /// Appends text and queues every segment it completes; parses nothing.
    pub fn feed(&mut self, increment: &str) {
        if self.failed {
            return;
        }
        if self.text.len() + increment.len() > MAX_CSS_BYTES {
            self.fail();
            return;
        }
        self.text.push_str(increment);
        while !self.failed && self.text.len() - self.cut >= self.trigger {
            if !self.cut_next() {
                break;
            }
        }
    }

    /// Parses the oldest queued segment; false when none could be parsed.
    pub fn parse_ready(&mut self) -> bool {
        if self.failed {
            return false;
        }
        let Some(namespaces) = &self.namespaces else {
            return false;
        };
        let Some(segment) = self.queue.pop_front() else {
            return false;
        };
        let context = &self.contexts[segment.context];
        let output = segment.top.and_then(|top| {
            emit_chunk(&self.text[segment.start..segment.end], segment.start, top, context, namespaces)
        });
        match output {
            Some(output) => self.outputs.push(output),
            None => self.fail(),
        }
        true
    }

    /// Parses every queued segment on `runner` when at least `min_bytes` are
    /// queued; false when the backlog is smaller or nothing could be parsed.
    pub fn parse_backlog(&mut self, runner: &dyn ChunkRunner, min_bytes: usize) -> bool {
        if self.failed || self.namespaces.is_none() {
            return false;
        }
        let queued: usize = self.queue.iter().map(|segment| segment.end - segment.start).sum();
        if queued < min_bytes {
            return false;
        }
        let segments: Vec<Segment> = self.queue.drain(..).collect();
        let Some(namespaces) = &self.namespaces else {
            return false;
        };
        match emit_segments(&self.text, segments, &self.contexts, namespaces, runner) {
            Some(outputs) => self.outputs.extend(outputs),
            None => self.fail(),
        }
        true
    }

    /// Parses the tail and everything still queued (on `runner`) and combines
    /// all segments. `root.text` must be the text that was fed.
    pub fn finish(mut self, root: &StylesheetSource, runner: &dyn ChunkRunner) -> Option<ParsedStylesheets> {
        if self.failed
            || root.disabled
            || !root.imports.is_empty()
            || self.text.as_bytes() != root.text.as_bytes()
        {
            return None;
        }
        let tail = loop {
            let (ranges, closed) = self.scan_tail().ok()?;
            match closed {
                Some(close) => self.close_wrapper(ranges, close),
                None => break ranges,
            }
            if self.failed {
                return None;
            }
        };
        if !self.wrappers.is_empty() || !self.started && self.cut == 0 {
            return None;
        }
        let end = self.text.len();
        self.queue_segment(self.cut, end, tail);
        if self.failed {
            return None;
        }
        let namespaces = NamespaceMap::from_ranges(&self.text, &self.tops);
        if self.namespaces.as_ref().is_some_and(|used| used != &namespaces) {
            return None;
        }
        let segments: Vec<Segment> = self.queue.drain(..).collect();
        let mut outputs = core::mem::take(&mut self.outputs);
        outputs.extend(emit_segments(&root.text, segments, &self.contexts, &namespaces, runner)?);
        assemble(outputs, root)
    }

    fn fail(&mut self) {
        self.failed = true;
        self.queue.clear();
        self.outputs.clear();
    }

    fn scan_tail(&self) -> Result<(Scan, Option<usize>), CssError> {
        let tail = &self.text[self.cut..];
        if self.wrappers.is_empty() {
            Ok((nesting::source_rule_ranges(tail, false)?, None))
        } else {
            nesting::source_rule_ranges_until_close(tail)
        }
    }

    fn current(&self) -> usize {
        self.wrappers.last().map_or(0, |wrapper| wrapper.context)
    }

    /// Whether the text before `end` is free of `@import`; checked once, for
    /// the first segment, as `scan_chunk` does.
    fn check_first(&mut self, end: usize) -> bool {
        if self.started {
            return true;
        }
        self.started = true;
        match imports(&self.text[..end]) {
            Ok(rules) if rules.is_empty() => true,
            _ => {
                self.fail();
                false
            }
        }
    }

    /// Queues `text[start..end]` with ranges relative to `start`.
    fn queue_segment(&mut self, start: usize, end: usize, top: Scan) {
        self.items += top.len();
        if self.items > MAX_RULES / 4 {
            self.fail();
            return;
        }
        if self.wrappers.is_empty() {
            self.tops.extend(top.iter().map(|&(first, last, open, close)| {
                (first + start, last + start, open.map(|v| v + start), close.map(|v| v + start))
            }));
        }
        let context = self.current();
        self.queue.push_back(Segment { start, end, context, first: false, last: false, top: Some(top) });
    }

    /// The enclosing block closed at `close` (relative to `cut`); `ranges` are
    /// the rules of its last segment.
    fn close_wrapper(&mut self, ranges: Scan, close: usize) {
        let close = self.cut + close;
        self.queue_segment(self.cut, close, ranges);
        let Some(wrapper) = self.wrappers.pop() else {
            self.fail();
            return;
        };
        if self.wrappers.is_empty() {
            self.tops.push((wrapper.start, close + 1, Some(wrapper.open), Some(close)));
        }
        self.cut = close + 1;
        self.trigger = self.segment_bytes;
    }

    /// Cuts the unprocessed text at the end of its last complete block, or
    /// descends into an open block; false when nothing could be cut yet.
    fn cut_next(&mut self) -> bool {
        let tail_len = self.text.len() - self.cut;
        let Ok((mut ranges, closed)) = self.scan_tail() else {
            self.fail();
            return false;
        };
        if let Some(close) = closed {
            self.close_wrapper(ranges, close);
            return !self.failed;
        }
        let unfinished = ranges
            .last()
            .filter(|range| range.2.is_some() && !range.3.is_some_and(|close| close < range.1))
            .copied();
        if unfinished.is_some() {
            ranges.pop();
        }
        if let Some(last_block) = ranges.iter().rposition(|range| range.2.is_some()) {
            let end = ranges[last_block].1;
            ranges.truncate(last_block + 1);
            if !self.check_first(self.cut + end) {
                return false;
            }
            self.queue_segment(self.cut, self.cut + end, ranges);
            if self.failed {
                return false;
            }
            self.cut += end;
            self.trigger = self.segment_bytes;
            if self.namespaces.is_none() {
                self.namespaces = Some(NamespaceMap::from_ranges(&self.text, &self.tops));
            }
            return true;
        }
        if let Some(block) = unfinished {
            if tail_len - block.0 >= self.wrapper_bytes && self.descend(ranges, block) {
                return true;
            }
        }
        self.trigger = (tail_len * 2).max(self.segment_bytes);
        false
    }

    /// Enters the open block `block` (relative to `cut`) that follows `before`.
    fn descend(&mut self, before: Scan, block: nesting::SourceRuleRange) -> bool {
        let (Some(open), true) = (block.2, self.wrappers.len() < MAX_DESCENT) else {
            return false;
        };
        let (start, open) = (self.cut + block.0, self.cut + open);
        if self.namespaces.is_none() {
            let mut probe = self.tops.clone();
            probe.extend(before.iter().map(|&(first, last, open, close)| {
                (first + self.cut, last + self.cut, open.map(|v| v + self.cut), close.map(|v| v + self.cut))
            }));
            probe.push((start, start, Some(open), None));
            self.namespaces = Some(NamespaceMap::from_ranges(&self.text, &probe));
        }
        let Some(namespaces) = &self.namespaces else {
            return false;
        };
        let Some(context) = child_context(&self.text, &self.contexts[self.current()], start, open, namespaces) else {
            return false;
        };
        if !self.check_first(start) {
            return false;
        }
        if !before.is_empty() {
            self.queue_segment(self.cut, start, before);
        }
        self.contexts.push(context);
        self.wrappers.push(Wrapper { start, open, context: self.contexts.len() - 1 });
        self.cut = open + 1;
        self.trigger = self.segment_bytes;
        !self.failed
    }
}

#[cfg(test)]
impl IncrementalParser {
    fn with_thresholds(segment_bytes: usize, wrapper_bytes: usize) -> Self {
        Self { trigger: segment_bytes, segment_bytes, wrapper_bytes, ..Self::new() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;

    struct Inline;

    impl ChunkRunner for Inline {
        fn threads(&self) -> usize {
            2
        }
        fn run<'a>(&self, jobs: Vec<Box<dyn FnOnce() + Send + 'a>>) {
            for job in jobs {
                job();
            }
        }
    }

    fn source(text: &str) -> StylesheetSource {
        StylesheetSource {
            disabled: false,
            url: Arc::from("https://stream.test/a.css"),
            text: Arc::from(text),
            imports: Vec::new(),
        }
    }

    fn dump(parsed: &ParsedStylesheets) -> String {
        format!(
            "{:?}\n{:?}\n{:?}\n{:?}\n{:?}",
            parsed.rules, parsed.font_faces, parsed.family_display, parsed.keyframes, parsed.layers
        )
    }

    fn filler(prefix: &str, count: usize) -> String {
        (0..count).map(|n| format!(".{prefix}{n}{{color:red;margin:{n}px}}")).collect()
    }

    fn next(seed: &mut u64) -> usize {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (*seed >> 33) as usize
    }

    fn stream(text: &str, seed: u64, max_step: usize, segment: usize, wrapper: usize) -> Option<ParsedStylesheets> {
        let mut seed = seed;
        let mut parser = IncrementalParser::with_thresholds(segment, wrapper);
        let mut at = 0;
        while at < text.len() {
            let mut end = (at + 1 + next(&mut seed) % max_step).min(text.len());
            while !text.is_char_boundary(end) {
                end += 1;
            }
            parser.feed(&text[at..end]);
            if next(&mut seed) % 2 == 0 {
                parser.parse_ready();
            }
            at = end;
        }
        parser.finish(&source(text), &Inline)
    }

    fn assert_streamed(text: &str, expect_streamed: bool) {
        // A sheet with imports has no sequential parse without its import graph; it must fall back.
        let expected = parse_graph(&source(text), MediaEnvironment::default()).ok().map(|graph| dump(&graph));
        assert!(expected.is_some() || !expect_streamed, "sequential parse failed: {text:.80}");
        for seed in 0..24 {
            for (max_step, segment, wrapper) in [(7, 64, 128), (40, 200, 400), (3000, 1000, 3000)] {
                // One increment that covers a whole wrapped sheet leaves nothing to descend into.
                let may_fall_back = !expect_streamed || max_step >= text.len() / 2;
                match (stream(text, seed, max_step, segment, wrapper), &expected) {
                    (Some(parsed), Some(expected)) => assert_eq!(&dump(&parsed), expected, "seed {seed} step {max_step}: {text:.80}"),
                    (Some(_), None) => panic!("seed {seed} step {max_step} streamed a sheet that needs its imports: {text:.80}"),
                    (None, _) => assert!(may_fall_back, "seed {seed} step {max_step} fell back: {text:.80}"),
                }
            }
        }
    }

    #[test]
    fn plain_rules_stream_in_any_increments() {
        assert_streamed(&filler("a", 120), true);
    }

    #[test]
    fn splits_inside_strings_comments_urls_escapes_and_multibyte_text() {
        let mut text = String::new();
        for n in 0..30 {
            text += &format!(
                ".s{n}::before{{content:\"}}{{ \\\" \u{e9}\u{4e16}\";background:url(data:image/png;base64,AA}}A{n})}}\n"
            );
            text += &format!("/* {{ }} \u{1f600} comment {n} */.c{n}{{color:red}}");
            text += &format!(".e{n}\\{{x{{margin:{n}px}}.u{n}{{background:url( 'a}}{n}.png' )}}");
            text += &format!(".\u{4e16}{n}{{font-family:\"\u{e9}\u{4e16}\"}}");
        }
        assert_streamed(&text, true);
    }

    #[test]
    fn layer_statements_and_ids_keep_source_order() {
        let mut text = String::from("@layer base, theme;");
        text += &filler("a", 30);
        text += "@layer theme{.t{color:red}@layer inner{.ti{color:blue}}}";
        text += &filler("b", 30);
        text += "@layer late, base;@layer base.deep{.d{color:green}}";
        text += &filler("c", 30);
        text += "@media (min-width:1px){.m{color:red}}@font-face{font-family:x;src:url(a.woff2)}@keyframes k{from{opacity:0}to{opacity:1}}";
        text += &filler("d", 30);
        assert_streamed(&text, true);
    }

    #[test]
    fn a_dominant_block_is_streamed_from_inside() {
        let layer = format!("@layer x{{@layer a, b;{}@layer a.b{{.n{{color:red}}}}{}}}", filler("a", 80), filler("b", 80));
        let media = format!("@media (min-width:1px){{{}}}", filler("a", 120));
        let supports = format!("@supports (display:grid){{{}}}", filler("a", 120));
        let nested = format!("@layer o{{@media screen{{{}@supports (display:grid){{.s{{color:red}}}}}}}}", filler("a", 120));
        let around = format!(
            "@namespace svg url(http://www.w3.org/2000/svg);@layer z, x;{}svg|rect{{fill:red}}@layer x{{{}}}@layer x{{.again{{color:blue}}}}{}@media print{{.pm{{color:red}}}}",
            filler("p", 4),
            filler("a", 120),
            filler("q", 4)
        );
        for text in [layer, media, supports, nested, around] {
            assert_streamed(&text, true);
        }
    }

    #[test]
    fn unsupported_or_anonymous_blocks_match_or_fall_back() {
        let unsupported = format!("@supports (foo:bar){{{}}}", filler("a", 120));
        let anonymous = format!("@layer{{{}}}", filler("a", 120));
        let inside = format!("@layer x{{{}@layer{{.anon{{color:red}}}}{}}}", filler("a", 40), filler("b", 40));
        let first = format!("{}@layer{{.anon{{color:red}}}}{}", filler("a", 40), filler("b", 40));
        for text in [unsupported, anonymous, inside, first] {
            assert_streamed(&text, false);
        }
    }

    #[test]
    fn imports_and_late_namespaces_fall_back() {
        let imports = format!("@import 'a.css';{}", filler("a", 60));
        let after_layer = format!("@layer a;@import url(b.css) screen;{}", filler("a", 60));
        let namespace = format!("@foo{{}}{}@namespace svg url(http://www.w3.org/2000/svg);{}", filler("a", 40), filler("b", 40));
        for text in [imports, after_layer, namespace] {
            assert_streamed(&text, false);
        }
    }

    #[test]
    fn malformed_sheets_match_or_fall_back() {
        let cases = [
            format!("@layer x{{{}.broken{{color:red;{}}}", filler("a", 40), filler("b", 40)),
            format!("@layer x{{{}@media (x{{.m{{color:red}}}}{}}}", filler("a", 40), filler("b", 40)),
            format!("@layer x{{{}.x{{color:red}} ;;; @foo bar;{{}}.y{{color:blue}}{}}}", filler("a", 40), filler("b", 40)),
            format!("@layer x{{{}@import 'a.css';{}}}", filler("a", 40), filler("b", 40)),
            format!("@layer x{{{}junk }} }} {}", filler("a", 40), filler("b", 4)),
            format!("{}@layer x{{{}.tail{{color:red", filler("p", 4), filler("a", 40)),
            format!("{}.cut{{background:url(a}}b)}}{}", filler("a", 40), filler("b", 40)),
            format!("{}\\}} .z{{color:red}}{}", filler("a", 40), filler("b", 40)),
            format!("{} .open{{color:red;{}", filler("a", 40), filler("b", 40)),
        ];
        for text in &cases {
            assert_streamed(text, false);
        }
    }

    struct Wide;

    impl ChunkRunner for Wide {
        fn threads(&self) -> usize {
            8
        }
        fn run<'a>(&self, jobs: Vec<Box<dyn FnOnce() + Send + 'a>>) {
            for job in jobs.into_iter().rev() {
                job();
            }
        }
    }

    fn stream_backlog(text: &str, step: usize, segment: usize, wrapper: usize, backlog: usize) -> Option<ParsedStylesheets> {
        let mut parser = IncrementalParser::with_thresholds(segment, wrapper);
        let mut at = 0;
        while at < text.len() {
            let mut end = (at + step).min(text.len());
            while !text.is_char_boundary(end) {
                end += 1;
            }
            parser.feed(&text[at..end]);
            parser.parse_backlog(&Wide, backlog);
            at = end;
        }
        parser.finish(&source(text), &Wide)
    }

    #[test]
    fn backlog_and_tail_parse_in_parallel_matches_the_whole_parse() {
        let layered = format!("@layer a, b;{}@layer a{{{}}}{}", filler("p", 200), filler("q", 600), filler("r", 800));
        let wrapped = format!("@media (min-width:1px){{{}}}", filler("m", 3000));
        for text in [filler("a", 6000), layered, wrapped] {
            let expected = dump(&parse_graph(&source(&text), MediaEnvironment::default()).unwrap());
            for step in [97, 4096, 50_000, text.len()] {
                for backlog in [0, 4096, 64 * 1024, usize::MAX] {
                    let parsed = stream_backlog(&text, step, 1000, 3000, backlog);
                    match parsed {
                        Some(parsed) => assert_eq!(dump(&parsed), expected, "step {step} backlog {backlog}"),
                        None => assert!(step >= text.len() / 2, "fell back at step {step} backlog {backlog}"),
                    }
                }
            }
        }
    }

    #[test]
    fn one_large_feed_is_split_across_threads() {
        let text = filler("a", 8000);
        let expected = dump(&parse_graph(&source(&text), MediaEnvironment::default()).unwrap());
        let mut parser = IncrementalParser::new();
        parser.feed(&text);
        let parsed = parser.finish(&source(&text), &Wide).expect("chunked");
        assert_eq!(dump(&parsed), expected);
    }

    #[test]
    fn backlog_below_the_threshold_is_left_queued() {
        let text = filler("a", 400);
        let mut parser = IncrementalParser::with_thresholds(1000, 3000);
        parser.feed(&text);
        assert!(!parser.parse_backlog(&Wide, usize::MAX));
        assert!(parser.parse_backlog(&Wide, 1));
        assert!(!parser.parse_backlog(&Wide, 1));
        let expected = dump(&parse_graph(&source(&text), MediaEnvironment::default()).unwrap());
        assert_eq!(dump(&parser.finish(&source(&text), &Wide).unwrap()), expected);
    }

    #[test]
    fn small_and_empty_sheets_leave_the_parse_to_the_caller() {
        assert!(stream("", 1, 5, 64, 128).is_none());
        assert!(stream(".a{color:red}", 1, 5, 64, 128).is_none());
    }

    #[test]
    fn a_text_that_differs_from_the_fed_text_is_rejected() {
        let text = filler("a", 60);
        let mut parser = IncrementalParser::with_thresholds(64, 128);
        parser.feed(&text);
        let other = filler("b", 60);
        assert!(parser.finish(&source(&other), &Inline).is_none());
    }
}
