//! Properties defined by a CSS value-definition grammar. Each row parses, computes and
//! inherits through `extra_props`' string slots; layout and paint do not read them yet.
use super::*;
use super::extra_props as extra;
use registered_properties::{accepts_value, compute_value_with_source};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Post {
    None,
    ScrollSnapAlign,
    ScrollSnapType,
    TextEmphasisPosition,
    OverflowClipMargin,
    ScrollMarkerGroup,
    Dasharray,
    TextEdge,
    EmphasisStyle,
    StrokeWidth,
    InsetPair,
    OffsetPairs,
}

pub(crate) struct Prop {
    pub name: &'static str,
    pub inherited: bool,
    pub initial: &'static str,
    syntax: &'static str,
    typed: Option<&'static str>,
    post: Post,
}

const fn prop(name: &'static str, inherited: bool, initial: &'static str, syntax: &'static str) -> Prop {
    Prop { name, inherited, initial, syntax, typed: None, post: Post::None }
}

const fn typed(mut prop: Prop, keywords: &'static str) -> Prop {
    prop.typed = Some(keywords);
    prop
}

const fn post(mut prop: Prop, post: Post) -> Prop {
    prop.post = post;
    prop
}

macro_rules! blend_modes {
    () => {
        "normal | multiply | screen | overlay | darken | lighten | color-dodge | color-burn | hard-light | soft-light | difference | exclusion | hue | saturation | color | luminosity"
    };
}

pub(crate) const PROPS: &[Prop] = &[
    prop("interest-delay-start", false, "normal", "normal | <time-nn>"),
    prop("interest-delay-end", false, "normal", "normal | <time-nn>"),
    prop("alignment-baseline", false, "baseline", "baseline | text-bottom | alphabetic | ideographic | middle | central | mathematical | text-top"),
    prop("backface-visibility", false, "visible", "visible | hidden"),
    prop("border-clip", false, "normal", "normal | [ <length-percentage-nn> | <flex> ]+"),
    prop("box-shadow-blur", false, "0px", "<length-nn>#"),
    prop("box-shadow-color", false, "currentcolor", "<color-resolved>#"),
    post(prop("box-shadow-offset", false, "0px", "[ <length>{1,2} ]#"), Post::OffsetPairs),
    prop("box-shadow-position", false, "outset", "[ inset | outset ]#"),
    prop("box-shadow-spread", false, "0px", "<length>#"),
    prop("background-blend-mode", false, "normal", concat!("[ ", blend_modes!(), " ]#")),
    prop("baseline-shift", false, "0px", "<length-percentage> | sub | super | top | center | bottom"),
    prop("color-interpolation", true, "srgb", "auto | srgb | linearrgb"),
    prop("column-height", false, "auto", "auto | <length-nn>"),
    prop("column-wrap", false, "auto", "auto | nowrap | wrap"),
    prop("container-name", false, "none", "none | <container-ident>+"),
    prop("view-transition-class", false, "none", "none | <container-ident>+"),
    prop("view-transition-group", false, "normal", "normal | contain | nearest | <custom-ident>"),
    prop("d", false, "none", "none | <path-function>"),
    prop("dominant-baseline", true, "auto", "auto | text-bottom | alphabetic | ideographic | middle | central | mathematical | hanging | text-top"),
    typed(prop("fill-color", true, "currentcolor", "<color>"), "currentcolor"),
    prop("font-kerning", true, "auto", "auto | normal | none"),
    prop("font-language-override", true, "normal", "normal | <string>"),
    prop("font-optical-sizing", true, "auto", "auto | none"),
    prop("font-palette", true, "normal", "normal | light | dark | <dashed-ident>"),
    prop("dynamic-range-limit", true, "no-limit", "standard | no-limit | constrained"),
    prop("font-presentation", true, "auto", "auto | text | emoji"),
    prop("font-synthesis-weight", true, "auto", "auto | none"),
    prop("font-synthesis-style", true, "auto", "auto | none | oblique-only"),
    prop("font-synthesis-position", true, "auto", "auto | none"),
    prop("font-variant-east-asian", true, "normal", "normal | [ [ jis78 | jis83 | jis90 | jis04 | simplified | traditional ] || [ full-width | proportional-width ] || ruby ]"),
    prop("font-variant-emoji", true, "normal", "normal | text | emoji | unicode"),
    prop("font-variant-position", true, "normal", "normal | sub | super"),
    prop("font-variant-numeric", true, "normal", "normal | [ [ lining-nums | oldstyle-nums ] || [ proportional-nums | tabular-nums ] || [ diagonal-fractions | stacked-fractions ] || ordinal || slashed-zero ]"),
    prop("font-variation-settings", true, "normal", "normal | [ <string> <number> ]#"),
    prop("image-rendering", true, "auto", "auto | smooth | high-quality | crisp-edges | pixelated"),
    prop("baseline-source", false, "auto", "auto | first | last"),
    prop("initial-letter", false, "normal", "normal | <number-min1> <integer-min1> | <number-min1> && [ drop | raise ]?"),
    prop("isolation", false, "auto", "auto | isolate"),
    typed(prop("lighting-color", false, "rgb(255, 255, 255)", "<color>"), "currentcolor"),
    prop("marker-start", true, "none", "none | <url>"),
    prop("marker-mid", true, "none", "none | <url>"),
    prop("marker-end", true, "none", "none | <url>"),
    prop("mask-composite", false, "add", "[ add | subtract | intersect | exclude ]#"),
    prop("mask-type", false, "luminance", "luminance | alpha"),
    prop("mix-blend-mode", false, "normal", blend_modes!()),
    prop("object-fit", false, "fill", "fill | contain | cover | none | scale-down"),
    prop("overflow-anchor", false, "auto", "auto | none"),
    prop("scroll-axis-lock", false, "auto", "auto | none"),
    post(prop("scroll-marker-group", false, "none", "none | [ before | after ] [ links | tabs ]? | [ links | tabs ]"), Post::ScrollMarkerGroup),
    prop("scroll-target-group", false, "none", "none | auto"),
    prop("block-ellipsis", true, "no-ellipsis", "no-ellipsis | ellipsis | <string>"),
    prop("max-lines", false, "auto", "auto | <integer-min1> auto?"),
    prop("continue", false, "normal", "normal | discard | collapse | -webkit-legacy"),
    prop("-webkit-line-clamp", false, "none", "none | <integer-min1>"),
    post(prop("overflow-clip-margin", false, "0px", "[ content-box | padding-box | border-box ] || <length>"), Post::OverflowClipMargin),
    prop("overscroll-behavior-x", false, "auto", "contain | none | auto"),
    prop("overscroll-behavior-y", false, "auto", "contain | none | auto"),
    prop("page", false, "auto", "auto | <custom-ident>"),
    prop("paint-order", true, "normal", "normal | <paint-order>"),
    prop("scroll-margin-top", false, "0px", "<length>"),
    prop("scroll-margin-right", false, "0px", "<length>"),
    prop("scroll-margin-bottom", false, "0px", "<length>"),
    prop("scroll-margin-left", false, "0px", "<length>"),
    prop("scroll-margin-inline-start", false, "0px", "<length>"),
    prop("scroll-margin-inline-end", false, "0px", "<length>"),
    prop("scroll-margin-block-start", false, "0px", "<length>"),
    prop("scroll-margin-block-end", false, "0px", "<length>"),
    prop("scroll-padding-top", false, "auto", "auto | <length-percentage-nn>"),
    prop("scroll-padding-right", false, "auto", "auto | <length-percentage-nn>"),
    prop("scroll-padding-bottom", false, "auto", "auto | <length-percentage-nn>"),
    prop("scroll-padding-left", false, "auto", "auto | <length-percentage-nn>"),
    prop("scroll-padding-inline-start", false, "auto", "auto | <length-percentage-nn>"),
    prop("scroll-padding-inline-end", false, "auto", "auto | <length-percentage-nn>"),
    prop("scroll-padding-block-start", false, "auto", "auto | <length-percentage-nn>"),
    prop("scroll-padding-block-end", false, "auto", "auto | <length-percentage-nn>"),
    post(prop("scroll-snap-align", false, "none", "[ none | start | end | center ]{1,2}"), Post::ScrollSnapAlign),
    prop("scroll-snap-stop", false, "normal", "normal | always | before"),
    prop("scroll-initial-target", false, "none", "none | nearest"),
    post(prop("scroll-snap-type", false, "none", "none | [ x | y | block | inline | both | pair ] [ mandatory | proximity ]?"), Post::ScrollSnapType),
    prop("scrollbar-gutter", false, "auto", "auto | stable | stable both-edges"),
    prop("scrollbar-width", false, "auto", "auto | thin | none"),
    prop("shape-rendering", true, "auto", "auto | optimizespeed | crispedges | geometricprecision"),
    prop("speak", true, "auto", "auto | never | always"),
    typed(post(prop("stroke-dasharray", true, "none", "none | [ [ <number-px-nn> | <length-percentage-nn> ] ,? ]+"), Post::Dasharray), "none"),
    prop("stroke-dashoffset", true, "0px", "<number-px> | <length-percentage>"),
    prop("stroke-linecap", true, "butt", "butt | round | square"),
    prop("stroke-linejoin", true, "miter", "[ crop | arcs | miter | miter-clip ] || [ bevel | round | fallback ]"),
    prop("stroke-miterlimit", true, "4", "<number-nn>"),
    typed(prop("stroke-color", true, "transparent", "<color>"), "currentcolor"),
    prop("text-anchor", true, "start", "start | middle | end"),
    prop("text-box-trim", false, "none", "none | trim-start | trim-end | trim-both"),
    post(prop("text-box-edge", false, "auto", "auto | alphabetic | [ text | cap | ex | ideographic | ideographic-ink ] [ text | alphabetic | ideographic | ideographic-ink ]?"), Post::TextEdge),
    prop("text-combine-upright", true, "none", "none | all | digits [ 2 | 3 | 4 ]?"),
    prop("text-decoration-skip", true, "objects", "none | auto | [ objects || [ spaces | [ leading-spaces || trailing-spaces ] ] || edges || box-decoration ]"),
    prop("text-decoration-skip-ink", true, "auto", "auto | none | all"),
    prop("text-decoration-fill", false, "match-text", "match-text | none | context-fill | context-stroke | <color> | <url> [ none | <color> ]?"),
    prop("text-decoration-stroke", false, "match-text", "match-text | none | context-fill | context-stroke | <color> | <url> [ none | <color> ]?"),
    post(prop("-webkit-text-stroke-width", true, "0px", "thin | medium | thick | <length-nn>"), Post::StrokeWidth),
    typed(prop("-webkit-text-stroke-color", true, "currentcolor", "<color>"), "currentcolor"),
    typed(prop("-webkit-text-fill-color", true, "currentcolor", "<color>"), "currentcolor"),
    post(prop("text-decoration-inset", false, "0px", "auto | [ <length-percentage> ]{1,2}"), Post::InsetPair),
    post(prop("text-emphasis-style", true, "none", "none | [ [ filled | open ] || [ dot | circle | double-circle | triangle | sesame ] ] | <string>"), Post::EmphasisStyle),
    typed(prop("text-emphasis-color", true, "currentcolor", "<color>"), "currentcolor"),
    post(prop("text-emphasis-position", true, "over", "auto | [ [ over | under ] && [ right | left ]? ]"), Post::TextEmphasisPosition),
    prop("text-rendering", true, "auto", "auto | optimizespeed | optimizelegibility | geometricprecision"),
    prop("touch-action", false, "auto", "auto | none | manipulation | [ [ pan-x | pan-left | pan-right ] || [ pan-y | pan-up | pan-down ] || pinch-zoom ]"),
    prop("transform-style", false, "flat", "auto | flat | preserve-3d"),
    prop("vector-effect", false, "none", "none | non-scaling-stroke"),
    prop("white-space-trim", false, "none", "none | [ discard-before || discard-after || discard-inner ]"),
    typed(prop("will-change", false, "auto", "auto | [ scroll-position | contents | <will-change-ident> ]#"), "auto"),
    prop("forced-color-adjust", true, "auto", "auto | none | preserve-parent-color"),
    prop("image-orientation", true, "from-image", "from-image | none"),
    prop("link-parameters", false, "none", "none | <param>#"),
];

pub(crate) const COUNT: usize = PROPS.len();

pub(crate) const fn const_index(name: &str) -> usize {
    let mut index = 0;
    while index < PROPS.len() {
        if same_name(PROPS[index].name, name) {
            return index;
        }
        index += 1;
    }
    panic!("unknown value property")
}

const fn same_name(left: &str, right: &str) -> bool {
    let (left, right) = (left.as_bytes(), right.as_bytes());
    if left.len() != right.len() {
        return false;
    }
    let mut index = 0;
    while index < left.len() {
        if left[index] != right[index] {
            return false;
        }
        index += 1;
    }
    true
}

pub(crate) fn index_of(name: &str) -> Option<usize> {
    PROPS.iter().position(|prop| prop.name == name)
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Leaf {
    Length,
    LengthNn,
    LengthPercentage,
    LengthPercentageNn,
    TimeNn,
    NumberNn,
    NumberPx,
    NumberPxNn,
    Number,
    Url,
    CustomIdent,
    ContainerIdent,
    WillChangeIdent,
    DashedIdent,
    Color,
    String,
    PaintOrder,
    PathFunction,
    NumberMin1,
    IntegerMin1,
    Flex,
    ResolvedColor,
    Param,
}

#[derive(Debug)]
enum Node {
    Keyword(String),
    Leaf(Leaf),
    Comma,
    Seq(Vec<Node>),
    Alt(Vec<Node>),
    AnyOf(Vec<Node>),
    AllOf(Vec<Node>),
    Optional(Box<Node>),
    Repeat { node: Box<Node>, min: usize, max: usize, comma: bool },
}

struct GrammarParser<'a> {
    text: &'a [u8],
    at: usize,
}

impl GrammarParser<'_> {
    fn skip(&mut self) {
        while self.text.get(self.at) == Some(&b' ') {
            self.at += 1;
        }
    }

    fn peek(&self, expected: &[u8]) -> bool {
        self.text[self.at..].starts_with(expected)
    }

    fn alternatives(&mut self) -> Node {
        let mut items = vec![self.any_of()];
        loop {
            self.skip();
            if self.peek(b"|") && !self.peek(b"||") {
                self.at += 1;
                items.push(self.any_of());
            } else {
                break;
            }
        }
        if items.len() == 1 { items.remove(0) } else { Node::Alt(items) }
    }

    fn any_of(&mut self) -> Node {
        let mut items = vec![self.all_of()];
        loop {
            self.skip();
            if self.peek(b"||") {
                self.at += 2;
                items.push(self.all_of());
            } else {
                break;
            }
        }
        if items.len() == 1 { items.remove(0) } else { Node::AnyOf(items) }
    }

    fn all_of(&mut self) -> Node {
        let mut items = vec![self.sequence()];
        loop {
            self.skip();
            if self.peek(b"&&") {
                self.at += 2;
                items.push(self.sequence());
            } else {
                break;
            }
        }
        if items.len() == 1 { items.remove(0) } else { Node::AllOf(items) }
    }

    fn sequence(&mut self) -> Node {
        let mut items = Vec::new();
        loop {
            self.skip();
            match self.text.get(self.at) {
                None | Some(b']') => break,
                Some(b'|') | Some(b'&') => break,
                _ => items.push(self.term()),
            }
        }
        if items.len() == 1 { items.remove(0) } else { Node::Seq(items) }
    }

    fn term(&mut self) -> Node {
        let atom = match self.text[self.at] {
            b'[' => {
                self.at += 1;
                let inner = self.alternatives();
                self.skip();
                self.at += 1;
                inner
            }
            b',' => {
                self.at += 1;
                Node::Comma
            }
            b'<' => {
                let end = self.at + self.text[self.at..].iter().position(|byte| *byte == b'>').unwrap_or(0);
                let name = core::str::from_utf8(&self.text[self.at + 1..end]).unwrap_or("");
                self.at = end + 1;
                Node::Leaf(leaf_named(name))
            }
            _ => {
                let start = self.at;
                while self.at < self.text.len() && !matches!(self.text[self.at], b' ' | b']' | b'?' | b'*' | b'+' | b'#' | b'{' | b'|' | b'&' | b',') {
                    self.at += 1;
                }
                Node::Keyword(String::from(core::str::from_utf8(&self.text[start..self.at]).unwrap_or("")))
            }
        };
        match self.text.get(self.at) {
            Some(b'?') => {
                self.at += 1;
                Node::Optional(Box::new(atom))
            }
            Some(b'*') => {
                self.at += 1;
                Node::Repeat { node: Box::new(atom), min: 0, max: 16, comma: false }
            }
            Some(b'+') => {
                self.at += 1;
                Node::Repeat { node: Box::new(atom), min: 1, max: 16, comma: false }
            }
            Some(b'#') => {
                self.at += 1;
                Node::Repeat { node: Box::new(atom), min: 1, max: 16, comma: true }
            }
            Some(b'{') => {
                let end = self.at + self.text[self.at..].iter().position(|byte| *byte == b'}').unwrap_or(0);
                let spec = core::str::from_utf8(&self.text[self.at + 1..end]).unwrap_or("");
                self.at = end + 1;
                let (min, max) = spec.split_once(',').map_or((spec, spec), |parts| parts);
                Node::Repeat { node: Box::new(atom), min: min.parse().unwrap_or(1), max: max.parse().unwrap_or(1), comma: false }
            }
            _ => atom,
        }
    }
}

fn leaf_named(name: &str) -> Leaf {
    match name {
        "length" => Leaf::Length,
        "length-nn" => Leaf::LengthNn,
        "length-percentage" => Leaf::LengthPercentage,
        "length-percentage-nn" => Leaf::LengthPercentageNn,
        "time-nn" => Leaf::TimeNn,
        "number" => Leaf::Number,
        "number-nn" => Leaf::NumberNn,
        "number-px" => Leaf::NumberPx,
        "number-px-nn" => Leaf::NumberPxNn,
        "url" => Leaf::Url,
        "custom-ident" => Leaf::CustomIdent,
        "container-ident" => Leaf::ContainerIdent,
        "will-change-ident" => Leaf::WillChangeIdent,
        "dashed-ident" => Leaf::DashedIdent,
        "color" => Leaf::Color,
        "string" => Leaf::String,
        "paint-order" => Leaf::PaintOrder,
        "path-function" => Leaf::PathFunction,
        "number-min1" => Leaf::NumberMin1,
        "integer-min1" => Leaf::IntegerMin1,
        "flex" => Leaf::Flex,
        "color-resolved" => Leaf::ResolvedColor,
        "param" => Leaf::Param,
        _ => panic!("unknown value-definition leaf"),
    }
}

fn grammar(index: usize) -> Node {
    let mut parser = GrammarParser { text: PROPS[index].syntax.as_bytes(), at: 0 };
    parser.alternatives()
}

#[derive(Clone, Copy)]
enum Token<'a> {
    Word(&'a str),
    Comma,
}

fn tokenize(raw: &str) -> Option<Vec<Token<'_>>> {
    let mut tokens = Vec::new();
    for (index, part) in top_level_split(raw, b',', 64)?.into_iter().enumerate() {
        if index > 0 {
            tokens.push(Token::Comma);
        }
        for word in components(part)? {
            tokens.push(Token::Word(word));
        }
    }
    Some(tokens)
}

type Matches = Vec<(usize, Vec<String>)>;
const MAX_MATCHES: usize = 64;

struct Matcher<'a> {
    tokens: &'a [Token<'a>],
    context: LengthContext,
    query: ContainerUnitContext,
    foreground: SourceColor,
    computed: bool,
}

impl Matcher<'_> {
    fn matches(&self, node: &Node, at: usize) -> Matches {
        match node {
            Node::Keyword(keyword) => match self.tokens.get(at) {
                Some(Token::Word(word)) if decoded_css_keyword(word, keyword) => vec![(at + 1, vec![keyword.clone()])],
                _ => Vec::new(),
            },
            Node::Comma => match self.tokens.get(at) {
                Some(Token::Comma) => vec![(at + 1, vec![String::from(",")])],
                _ => Vec::new(),
            },
            Node::Leaf(Leaf::PaintOrder) => self.paint_order(at),
            Node::Leaf(leaf) => match self.tokens.get(at) {
                Some(Token::Word(word)) => self.leaf(*leaf, word).map_or_else(Vec::new, |text| vec![(at + 1, vec![text])]),
                _ => Vec::new(),
            },
            Node::Seq(items) => {
                let mut current: Matches = vec![(at, Vec::new())];
                for item in items {
                    let mut next = Matches::new();
                    for (position, output) in &current {
                        for (end, part) in self.matches(item, *position) {
                            if next.len() >= MAX_MATCHES {
                                break;
                            }
                            let mut joined = output.clone();
                            joined.extend(part);
                            next.push((end, joined));
                        }
                    }
                    if next.is_empty() {
                        return next;
                    }
                    current = next;
                }
                current
            }
            Node::Alt(items) => items.iter().flat_map(|item| self.matches(item, at)).take(MAX_MATCHES).collect(),
            Node::Optional(inner) => {
                let mut result = self.matches(inner, at);
                result.push((at, Vec::new()));
                result
            }
            Node::Repeat { node, min, max, comma } => self.repeat(node, *min, *max, *comma, at),
            Node::AnyOf(items) => self.unordered(items, at, false),
            Node::AllOf(items) => self.unordered(items, at, true),
        }
    }

    fn repeat(&self, node: &Node, min: usize, max: usize, comma: bool, at: usize) -> Matches {
        let mut levels: Vec<Matches> = vec![vec![(at, Vec::new())]];
        for count in 1..=max {
            let mut next = Matches::new();
            for (position, output) in &levels[count - 1] {
                let mut start = *position;
                let mut prefix = output.clone();
                if comma && count > 1 {
                    match self.tokens.get(start) {
                        Some(Token::Comma) => {
                            start += 1;
                            prefix.push(String::from(","));
                        }
                        _ => continue,
                    }
                }
                for (end, part) in self.matches(node, start) {
                    if next.len() >= MAX_MATCHES {
                        break;
                    }
                    let mut joined = prefix.clone();
                    joined.extend(part);
                    next.push((end, joined));
                }
            }
            if next.is_empty() {
                break;
            }
            levels.push(next);
        }
        let mut result = Matches::new();
        for (count, level) in levels.into_iter().enumerate().rev() {
            if count >= min {
                result.extend(level);
            }
        }
        result.truncate(MAX_MATCHES);
        result
    }

    fn unordered(&self, items: &[Node], at: usize, all: bool) -> Matches {
        let mut results = Matches::new();
        let mut stack: Vec<(usize, u32, Vec<(usize, Vec<String>)>)> = vec![(at, 0, Vec::new())];
        while let Some((position, used, parts)) = stack.pop() {
            if results.len() >= MAX_MATCHES {
                break;
            }
            let complete = if all {
                items.iter().enumerate().all(|(index, item)| used & (1 << index) != 0 || matches!(item, Node::Optional(_)))
            } else {
                used != 0
            };
            if complete {
                let mut ordered = parts.clone();
                ordered.sort_by_key(|(index, _)| *index);
                results.push((position, ordered.into_iter().flat_map(|(_, part)| part).collect()));
            }
            for (index, item) in items.iter().enumerate() {
                if used & (1 << index) != 0 {
                    continue;
                }
                for (end, part) in self.matches(item, position) {
                    if end == position {
                        continue;
                    }
                    let mut next = parts.clone();
                    next.push((index, part));
                    stack.push((end, used | (1 << index), next));
                }
            }
        }
        results.sort_by_key(|(end, _)| core::cmp::Reverse(*end));
        results
    }

    fn paint_order(&self, at: usize) -> Matches {
        let mut seen: Vec<String> = Vec::new();
        let mut result = Matches::new();
        for (offset, token) in self.tokens[at..].iter().enumerate().take(3) {
            let Token::Word(word) = token else { break };
            let Some(keyword) = ["fill", "stroke", "markers"].into_iter().find(|keyword| decoded_css_keyword(word, keyword)) else { break };
            if seen.iter().any(|known| known == keyword) {
                break;
            }
            seen.push(String::from(keyword));
            result.push((at + offset + 1, seen.clone()));
        }
        result.reverse();
        result
    }

    fn compute(&self, syntax: &str, word: &str) -> Option<String> {
        compute_value_with_source(syntax, word, self.context, self.query, &self.foreground, None)
    }

    fn numeric(&self, syntax: &str, word: &str, non_negative: bool) -> Option<String> {
        if !accepts_value(syntax, word) {
            return None;
        }
        let math = math_function(word);
        let text = if self.computed {
            self.compute(syntax, word)?
        } else {
            let text = typed_numeric::parse_numeric_expression(word)?.serialize_specified()?;
            if text == "0" && syntax.starts_with("<length") { String::from("0px") } else { text }
        };
        if non_negative && text.starts_with('-') {
            if !math {
                return None;
            }
            let unit = text.find(|character: char| character.is_ascii_alphabetic() || character == '%').map_or("", |start| &text[start..]);
            return Some(alloc::format!("0{unit}"));
        }
        Some(text)
    }

    fn at_least_one(&self, integer: bool, word: &str) -> Option<String> {
        let text = self.numeric(if integer { "<integer>" } else { "<number>" }, word, false)?;
        match text.parse::<f64>() {
            Ok(value) if value >= 1.0 => Some(text),
            Ok(_) if math_function(word) => Some(String::from("1")),
            Ok(_) => None,
            Err(_) => math_function(word).then_some(text),
        }
    }

    fn leaf(&self, leaf: Leaf, word: &str) -> Option<String> {
        match leaf {
            Leaf::Length => self.numeric("<length>", word, false),
            Leaf::LengthNn => self.numeric("<length>", word, true),
            Leaf::LengthPercentage => self.numeric("<length-percentage>", word, false),
            Leaf::LengthPercentageNn => self.numeric("<length-percentage>", word, true),
            Leaf::TimeNn => self.numeric("<time>", word, true),
            Leaf::Number => self.numeric("<number>", word, false),
            Leaf::NumberNn => self.numeric("<number>", word, true),
            Leaf::NumberPx | Leaf::NumberPxNn => {
                let text = self.numeric("<number>", word, leaf == Leaf::NumberPxNn)?;
                if self.computed && !text.starts_with("calc(") {
                    Some(alloc::format!("{text}px"))
                } else {
                    Some(text)
                }
            }
            Leaf::Url => self.compute("<url>", word),
            Leaf::String => css_string(word).map(|value| serialize_string(&value)),
            Leaf::CustomIdent => self.compute("<custom-ident>", word),
            Leaf::ContainerIdent => {
                if ["none", "and", "not", "or"].iter().any(|keyword| decoded_css_keyword(word, keyword)) {
                    return None;
                }
                self.compute("<custom-ident>", word)
            }
            Leaf::WillChangeIdent => {
                if ["will-change", "none", "all", "auto"].iter().any(|keyword| decoded_css_keyword(word, keyword)) {
                    return None;
                }
                self.compute("<custom-ident>", word)
            }
            Leaf::DashedIdent => {
                if !word.starts_with("--") {
                    return None;
                }
                self.compute("<custom-ident>", word)
            }
            Leaf::Color => {
                if decoded_css_keyword(word, "currentcolor") {
                    return Some(String::from("currentcolor"));
                }
                if self.computed {
                    self.compute("<color>", word)
                } else {
                    accepts_value("<color>", word).then(|| word.trim().to_owned())
                }
            }
            Leaf::PathFunction => path_function(word),
            Leaf::NumberMin1 | Leaf::IntegerMin1 => self.at_least_one(leaf == Leaf::IntegerMin1, word),
            Leaf::PaintOrder => None,
            Leaf::Flex => {
                let number = word.trim().strip_suffix("fr").or_else(|| word.trim().strip_suffix("FR"))?;
                let value = number.parse::<f64>().ok().filter(|value| value.is_finite() && *value >= 0.0)?;
                Some(alloc::format!("{}fr", computed_values::number(value as f32)))
            }
            Leaf::Param => param_function(word),
            Leaf::ResolvedColor => {
                if self.computed {
                    self.compute("<color>", word)
                } else {
                    accepts_value("<color>", word).then(|| word.trim().to_owned())
                }
            }
        }
    }
}

fn param_function(word: &str) -> Option<String> {
    let word = word.trim();
    let open = word.find('(')?;
    if !word[..open].eq_ignore_ascii_case("param") || !word.ends_with(')') {
        return None;
    }
    let inner = &word[open + 1..word.len() - 1];
    let comma = syntax::top_level_find(inner, b',')??;
    let (head, value) = (inner[..comma].trim(), inner[comma + 1..].trim());
    let tokens = components(head)?;
    let (name, kind) = match tokens.as_slice() {
        [name] => (*name, None),
        [name, kind] => (*name, Some(*kind)),
        _ => return None,
    };
    let mut at = 0;
    if !name.starts_with("--") || consume_selector_identifier(name, &mut at).is_none_or(|_| at != name.len()) {
        return None;
    }
    let kind = match kind {
        None => None,
        Some(kind) if kind.starts_with('<') && kind.ends_with('>') && registered_properties::valid_typed_syntax(kind) => Some(String::from(kind)),
        Some(kind) if kind.len() > 6 && kind.as_bytes()[..5].eq_ignore_ascii_case(b"type(") && kind.ends_with(')') => {
            let syntax = kind[5..kind.len() - 1].trim();
            if !registered_properties::valid_typed_syntax(syntax) {
                return None;
            }
            let single = syntax.starts_with('<') && syntax.ends_with('>') && !syntax.contains(['|', ' ']);
            Some(if single { String::from(syntax) } else { alloc::format!("type({syntax})") })
        }
        Some(_) => return None,
    };
    Some(match kind {
        Some(kind) => alloc::format!("param({name} {kind}, {value})"),
        None => alloc::format!("param({name}, {value})"),
    })
}

fn path_function(word: &str) -> Option<String> {
    let (name, rest) = word.split_once('(')?;
    if !name.eq_ignore_ascii_case("path") {
        return None;
    }
    let inner = rest.strip_suffix(')')?.trim();
    let parts = top_level_split(inner, b',', 2)?;
    let (rule, string) = match parts.as_slice() {
        [string] => (None, *string),
        [rule, string] => (Some(rule.trim()), *string),
        _ => return None,
    };
    if rule.is_some_and(|rule| !decoded_css_keyword(rule, "nonzero") && !decoded_css_keyword(rule, "evenodd")) {
        return None;
    }
    let data = css_string(string.trim())?;
    let serialized = serialize_string(&data);
    Some(match rule {
        Some(rule) => alloc::format!("path({}, {serialized})", rule.to_ascii_lowercase()),
        None => alloc::format!("path({serialized})"),
    })
}

fn join(parts: &[String], separator: &str) -> String {
    let mut text = String::new();
    for part in parts {
        if part == "," {
            text.push(',');
            continue;
        }
        if !text.is_empty() {
            text.push_str(if text.ends_with(',') { " " } else { separator });
        }
        text.push_str(part);
    }
    text
}

fn finish(prop: &Prop, mut parts: Vec<String>, computed: bool) -> String {
    match prop.post {
        Post::None => {}
        Post::OffsetPairs => {
            let mut grouped: Vec<String> = Vec::new();
            let mut pair: Vec<String> = Vec::new();
            for part in parts.drain(..).chain(core::iter::once(String::from(","))) {
                if part != "," {
                    pair.push(part);
                    continue;
                }
                if pair.len() == 2 && pair[0] == pair[1] {
                    pair.pop();
                }
                if !grouped.is_empty() {
                    grouped.push(String::from(","));
                }
                grouped.append(&mut pair);
            }
            parts = grouped;
        }
        Post::InsetPair => {
            if parts.len() == 2 && parts[0] == parts[1] {
                parts.pop();
            }
        }
        Post::ScrollSnapAlign => {
            if parts.len() == 2 && parts[0] == parts[1] {
                parts.pop();
            }
        }
        Post::ScrollSnapType => {
            if parts.len() == 2 && parts[1] == "proximity" {
                parts.pop();
            }
        }
        Post::TextEmphasisPosition => {
            if parts.len() == 2 && parts[1] == "right" {
                parts.pop();
            }
        }
        Post::ScrollMarkerGroup => {
            if parts.len() == 2 && parts[1] == "links" && !computed {
                parts.pop();
            } else if parts.len() == 1 && matches!(parts[0].as_str(), "before" | "after") && computed {
                parts.push(String::from("links"));
            }
        }
        Post::OverflowClipMargin => {
            parts.retain(|part| part != "padding-box");
            if parts.len() == 2 && parts[1] == "0px" {
                parts.pop();
            }
            if parts.is_empty() {
                parts.push(String::from("0px"));
            }
        }
        Post::Dasharray => {
            parts.retain(|part| part != ",");
            return parts.join(", ");
        }
        Post::TextEdge => {
            if let [first, second] = parts.as_slice() {
                let collapsed = match (first.as_str(), second.as_str()) {
                    ("text", "alphabetic") => Some(second.clone()),
                    (first, "text") if matches!(first, "text" | "cap" | "ex") => Some(String::from(first)),
                    (first, second) if first == second => Some(String::from(first)),
                    _ => None,
                };
                if let Some(single) = collapsed {
                    parts = alloc::vec![single];
                }
            }
        }
        Post::StrokeWidth => {
            if computed {
                if let Some(width) = parts.first_mut() {
                    match width.as_str() {
                        "thin" => *width = String::from("1px"),
                        "medium" => *width = String::from("3px"),
                        "thick" => *width = String::from("5px"),
                        _ => {}
                    }
                }
            }
        }
        Post::EmphasisStyle => {
            let filled = parts.iter().any(|part| part == "filled");
            let open = parts.iter().any(|part| part == "open");
            let shape = parts.iter().find(|part| !matches!(part.as_str(), "filled" | "open")).cloned();
            if computed && parts.first().is_none_or(|part| !part.starts_with('"') && part != "none") {
                let shape = shape.unwrap_or_else(|| String::from("circle"));
                parts = if open { alloc::vec![String::from("open"), shape] } else { alloc::vec![shape] };
            } else if filled && shape.is_some() {
                parts.retain(|part| part != "filled");
            }
        }
    }
    join(&parts, " ")
}

fn evaluate(index: usize, raw: &str, context: LengthContext, foreground: Rgba, computed: bool) -> Option<String> {
    let prop = PROPS.get(index)?;
    let raw = raw.trim();
    if prop.name == "font-palette" && super::palette_mix::is_palette_mix(raw) {
        return super::palette_mix::canonicalize(raw, computed);
    }
    if prop.name == "dynamic-range-limit" && super::dynamic_range_limit::is_mix(raw) {
        return super::dynamic_range_limit::canonicalize(raw, computed, context);
    }
    let tokens = tokenize(raw)?;
    if tokens.is_empty() {
        return None;
    }
    let matcher = Matcher {
        tokens: &tokens,
        context,
        query: ContainerUnitContext::no_container(context.viewport),
        foreground: legacy_source_color(foreground),
        computed,
    };
    let (_, parts) = matcher.matches(&grammar(index), 0).into_iter().find(|(end, parts)| *end == tokens.len() && parts.last().is_none_or(|last| last != ","))?;
    Some(finish(prop, parts, computed))
}

pub(crate) fn compute(index: usize, raw: &str, context: LengthContext, foreground: Rgba) -> Option<String> {
    evaluate(index, raw, context, foreground, true)
}

pub(crate) fn serialize_specified(index: usize, raw: &str) -> Option<String> {
    evaluate(index, raw, static_length_context(), Style::initial().color, false)
}

pub(crate) fn interpolate(index: usize, from: &str, to: &str, progress: f64) -> Option<String> {
    if PROPS[index].name == "font-variation-settings" {
        return font_variations::interpolate(from, to, progress);
    }
    if PROPS[index].name == "text-decoration-inset" {
        return super::text_decoration_inset::interpolate(from, to, progress);
    }
    if PROPS[index].syntax == "<color>" {
        return super::text_props::interpolate_color(from, to, progress);
    }
    if PROPS[index].name == "font-palette" {
        return super::palette_mix::interpolate(from, to, progress);
    }
    if PROPS[index].name == "dynamic-range-limit" {
        return super::dynamic_range_limit::interpolate(from, to, progress);
    }
    if is_discrete(index) {
        return None;
    }
    let (a, unit_a) = syntax::split_number_unit(from)?;
    let (b, unit_b) = syntax::split_number_unit(to)?;
    if PROPS[index].syntax.contains("<time") {
        if unit_a != "s" || unit_b != "s" {
            return None;
        }
        let value = extra::mix(a, b, progress).max(0.0);
        return Some(alloc::format!("{}s", super::computed_values::number(value as f32)));
    }
    if unit_a != "px" || unit_b != "px" {
        return None;
    }
    let value = extra::mix(a, b, progress);
    Some(super::computed_values::px((if PROPS[index].syntax.contains("-nn") { value.max(0.0) } else { value }) as f32))
}

pub(crate) fn is_discrete(index: usize) -> bool {
    let syntax = PROPS[index].syntax;
    if PROPS[index].name == "overflow-clip-margin" {
        return true;
    }
    if PROPS[index].name == "dynamic-range-limit" {
        return false;
    }
    if syntax == "<color>" {
        return false;
    }
    syntax.contains("-min1>") || !(syntax.contains("<length") || syntax.contains("<number") || syntax.contains("<time"))
}

/// Whether a lone identifier reifies as a `CSSKeywordValue` for this property; `None` for
/// properties outside this table.
pub(crate) fn typed_keyword(name: &str, identifier: &str) -> Option<bool> {
    let prop = &PROPS[index_of(name)?];
    let lowered = identifier.to_ascii_lowercase();
    if let Some(keywords) = prop.typed {
        return Some(keywords.split(' ').any(|keyword| keyword == lowered));
    }
    let node = grammar(index_of(name)?);
    Some(contains_keyword(&node, &lowered) || prop.syntax.contains("<custom-ident>") || prop.syntax.contains("<container-ident>"))
}

fn contains_keyword(node: &Node, keyword: &str) -> bool {
    match node {
        Node::Keyword(known) => known == keyword,
        Node::Seq(items) | Node::Alt(items) | Node::AnyOf(items) | Node::AllOf(items) => items.iter().any(|item| contains_keyword(item, keyword)),
        Node::Optional(inner) => contains_keyword(inner, keyword),
        Node::Repeat { node, .. } => contains_keyword(node, keyword),
        Node::Leaf(Leaf::PaintOrder) => matches!(keyword, "fill" | "stroke" | "markers"),
        Node::Leaf(_) | Node::Comma => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(name: &str, raw: &str) -> Option<String> {
        compute(index_of(name).unwrap(), raw, static_length_context(), Style::initial().color)
    }

    fn specified(name: &str, raw: &str) -> Option<String> {
        serialize_specified(index_of(name).unwrap(), raw)
    }

    #[test]
    fn keyword_and_combination_grammars() {
        assert_eq!(value("object-fit", "COVER").as_deref(), Some("cover"));
        assert_eq!(value("font-variation-settings", "\"wght\" 4, \"wdth\" 2").as_deref(), Some("\"wght\" 4, \"wdth\" 2"));
        assert_eq!(value("touch-action", "pan-y pan-x").as_deref(), Some("pan-x pan-y"));
        assert_eq!(value("touch-action", "pan-x pan-left"), None);
        assert_eq!(value("scroll-snap-align", "start").as_deref(), Some("start"));
        assert_eq!(value("scroll-snap-align", "start end").as_deref(), Some("start end"));
        assert_eq!(specified("scroll-snap-align", "start").as_deref(), Some("start"));
        assert_eq!(value("scroll-snap-type", "x proximity").as_deref(), Some("x"));
        assert_eq!(value("text-emphasis-position", "over right").as_deref(), Some("over"));
        assert_eq!(value("paint-order", "stroke fill").as_deref(), Some("stroke fill"));
        assert_eq!(value("paint-order", "fill fill"), None);
    }

    #[test]
    fn numeric_leaves_clamp_and_keep_specified_units() {
        assert_eq!(value("scroll-padding-top", "2em").as_deref(), Some("32px"));
        assert_eq!(specified("scroll-padding-top", "2em").as_deref(), Some("2em"));
        assert_eq!(value("scroll-padding-top", "-1px"), None);
        assert_eq!(value("scroll-padding-top", "calc(-1px)").as_deref(), Some("0px"));
        assert_eq!(value("stroke-dasharray", "5 1px, 2%").as_deref(), Some("5px, 1px, 2%"));
        assert_eq!(value("stroke-dasharray", "5,"), None);
    }
}
