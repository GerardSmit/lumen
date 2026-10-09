//! Shared source structure for CSS nesting, rendering and CSSOM.
use super::*;
use alloc::borrow::Cow;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceRuleKind {
    Style,
    Group,
    Scope,
    FontFace,
    Property,
    FontFeatureValues,
    Keyframes,
    Keyframe,
    Statement,
    NestedDeclarations,
}

/// All ranges are byte offsets in the original stylesheet. Declaration runs
/// retain their source pieces, including pieces separated by ignored rules.
#[derive(Clone, Debug)]
pub struct SourceRule {
    pub kind: SourceRuleKind,
    pub range: Range<usize>,
    pub prelude: Range<usize>,
    pub body: Option<Range<usize>>,
    pub declaration_offset: Option<usize>,
    pub declaration_ranges: Vec<Range<usize>>,
    pub children: Vec<SourceRule>,
    pub scope_prelude: Option<Box<ScopePrelude>>,
    pub(super) css_scope: Option<Arc<ScopeContext>>,
    pub(super) selectors: Option<Arc<[Selector]>>,
}

impl SourceRule {
    pub fn declaration_source<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let Some(first) = self.declaration_ranges.first() else {
            return Cow::Borrowed("");
        };
        let last = self.declaration_ranges.last().unwrap();
        if self.declaration_ranges.windows(2).all(|pair| {
            let mut position = pair[0].end;
            skip_css_space_comments(text, &mut position).is_some() && position == pair[1].start
        }) {
            return Cow::Borrowed(&text[first.start..last.end]);
        }
        Cow::Owned(self.declaration_text(text))
    }

    pub fn declaration_text(&self, text: &str) -> String {
        let mut result = String::new();
        for range in &self.declaration_ranges {
            if !result.is_empty() {
                result.push(' ');
            }
            result.push_str(&text[range.clone()]);
        }
        result
    }
}

pub type SourceRuleRange = (usize, usize, Option<usize>, Option<usize>);

/// Locate one rule's block without treating braces inside prelude components
/// as delimiters. CSSOM serialization uses the same lexer as source parsing.
pub fn source_rule_block_open(text: &str) -> Option<usize> {
    if text.len() > MAX_CSS_BYTES {
        return None;
    }
    let mut start = 0;
    skip_css_space_comments(text, &mut start)?;
    match rule_boundary(&text[start..])? {
        RuleBoundary::Block(open) => Some(start + open),
        RuleBoundary::Statement(_) | RuleBoundary::Discard(_) => None,
    }
}

/// Scan once per bounded body, using the same CSS component boundaries as
/// qualified rules and declarations. Braces in custom property values belong
/// to the declaration, not to a child style rule.
pub fn source_rule_ranges(
    text: &str,
    inside_style: bool,
) -> Result<Vec<SourceRuleRange>, CssError> {
    if text.len() > MAX_CSS_BYTES {
        return Err(error(0, "CSS input too large"));
    }
    let mut result = Vec::new();
    let mut position = 0;
    while position < text.len() {
        if skip_css_space_comments(text, &mut position).is_none() {
            break;
        }
        if position == text.len() {
            break;
        }
        if !inside_style
            && (text[position..].starts_with("<!--") || text[position..].starts_with("-->"))
        {
            position += if text[position..].starts_with("<!--") {
                4
            } else {
                3
            };
            continue;
        }
        if inside_style && text.as_bytes()[position] == b';' {
            position += 1;
            continue;
        }
        let start = position;
        let rest = &text[start..];
        if inside_style {
            if let Some(end) = declaration_end(rest)? {
                position += end;
                result.push((start, position, None, None));
                if result.len() > MAX_RULES {
                    return Err(error(start, "too many CSS source items"));
                }
                continue;
            }
        }
        match syntax::boundary(rest)? {
            Some(RuleBoundary::Block(open)) => {
                let boundary=syntax::block(rest,open)?;
                result.push((start,start+boundary.after,Some(start+open),Some(start+boundary.content_end)));
                position+=boundary.after;
            }
            Some(RuleBoundary::Statement(end)) => {
                position += (end + 1).min(rest.len());
                result.push((start, position, None, None));
            }
            Some(RuleBoundary::Discard(end)) => position += end,
            None => break,
        }
        if result.len() > MAX_RULES {
            return Err(error(start, "too many CSS source items"));
        }
    }
    Ok(result)
}

fn declaration_end(input:&str)->Result<Option<usize>,CssError> {
    let mut position=0;
    let Some(name)=consume_selector_identifier(input,&mut position) else {return Ok(None);};
    skip_css_space_comments(input,&mut position);
    if input.as_bytes().get(position)!=Some(&b':'){return Ok(None);}
    let custom=name.starts_with("--");
    let mut cursor=syntax::Cursor::new(input,position+1)?;
    while let Some(token)=cursor.next(){
        match token.kind {
            syntax::TokenKind::Open(b'{') if !custom=>return Ok(None),
            syntax::TokenKind::Open(_)=>cursor.position=syntax::block(input,token.start)?.after,
            syntax::TokenKind::Close(b'}')=>return Ok(Some(token.start)),
            syntax::TokenKind::Other if input.as_bytes()[token.start]==b';'=>return Ok(Some(token.end)),
            _=>{}
        }
    }
    Ok(Some(input.len()))
}

fn error(offset: usize, message: &'static str) -> CssError {
    CssError { offset, message }
}

#[derive(Clone)]
struct Parent {
    selectors: Arc<[Selector]>,
    work: usize,
    scoped: bool,
    scope: Option<Arc<ScopeContext>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScopePrelude {
    pub start: Option<Range<usize>>,
    pub end: Option<Range<usize>>,
}

#[derive(Clone, Copy, Debug)]
pub enum NestingContext<'a> {
    StartingStyle,
    Style(&'a str),
    Scope(&'a str),
}

pub fn parse_scope_prelude(text: &str, base: usize) -> Result<ScopePrelude, CssError> {
    let parsed = scope_prelude_ranges(text, base)?;
    for range in parsed.start.iter().chain(parsed.end.iter()) {
        let selectors = parse_selector_list_depth(&text[range.start - base..range.end - base], range.start, 0, false, true, &NamespaceMap::default())?;
        if selectors.iter().any(Selector::has_pseudo_element) {
            return Err(error(range.start, "pseudo-element in scope selector"));
        }
    }
    Ok(parsed)
}

fn scope_prelude_ranges(text: &str, base: usize) -> Result<ScopePrelude, CssError> {
    if text.len() > MAX_SELECTOR_BYTES * 2 + 32 || base.checked_add(text.len()).is_none() || !at_rule(text, "@scope") {
        return Err(error(base, "invalid scope prelude"));
    }
    let mut position = 6;
    skip_css_space_comments(text, &mut position).ok_or_else(|| error(base, "invalid scope prelude"))?;
    let selector = |position: &mut usize| -> Result<Range<usize>, CssError> {
        if text.as_bytes().get(*position) != Some(&b'(') {
            return Err(error(base + *position, "scope selector requires parentheses"));
        }
        let open = *position;
        let close = matching_css_block(text, open).ok_or_else(|| error(base + open, "unclosed scope selector"))?;
        let range = open + 1..close - 1;
        *position = close;
        Ok(base + range.start..base + range.end)
    };
    let start = if text.as_bytes().get(position) == Some(&b'(') { Some(selector(&mut position)?) } else { None };
    skip_css_space_comments(text, &mut position).ok_or_else(|| error(base, "invalid scope prelude"))?;
    let end = if position < text.len() {
        position = import_keyword_end(text, position, "to").ok_or_else(|| error(base + position, "invalid scope limit"))?;
        skip_css_space_comments(text, &mut position).ok_or_else(|| error(base, "invalid scope limit"))?;
        Some(selector(&mut position)?)
    } else { None };
    skip_css_space_comments(text, &mut position).ok_or_else(|| error(base, "invalid scope prelude"))?;
    if position != text.len() { return Err(error(base + position, "trailing scope prelude")); }
    Ok(ScopePrelude { start, end })
}

pub fn serialize_scope_prelude(text: &str) -> Result<String, CssError> {
    let parsed = parse_scope_prelude(text, 0)?;
    let mut result = String::from("@scope");
    if let Some(start) = parsed.start { result.push_str(" ("); result.push_str(text[start].trim()); result.push(')'); }
    if let Some(end) = parsed.end { result.push_str(" to ("); result.push_str(text[end].trim()); result.push(')'); }
    Ok(result)
}

fn scoped_parent(prelude: &str, base: usize, parent: Option<&Parent>, namespaces: &NamespaceMap) -> Result<(Parent, ScopePrelude), CssError> {
    let parsed = scope_prelude_ranges(prelude, base)?;
    for range in parsed.start.iter().chain(parsed.end.iter()) {
        let raw = &prelude[range.start - base..range.end - base];
        for (start, _) in selector_list_spans(raw, range.start, false)? {
            let mut position = start;
            skip_css_space_comments(raw, &mut position).ok_or_else(|| error(range.start, "invalid scope selector"))?;
            if matches!(raw.as_bytes().get(position), Some(b'>' | b'+' | b'~')) {
                return Err(error(range.start + position, "relative scope boundary selector"));
            }
        }
    }
    let start = parsed.start.as_ref().map(|range| {
        resolve_selectors(&prelude[range.start - base..range.end - base], range.start, parent, namespaces).map(|value| value.selectors)
    }).transpose()?;
    let mut anchor = parse_simple_selector_depth(":scope", base, 0, false, &NamespaceMap::default())?;
    anchor.specificity = (0, 0, 0);
    let selectors: Arc<[Selector]> = vec![anchor].into();
    let anchor_parent = Parent { selectors: selectors.clone(), work: 1, scoped: true, scope: None };
    let end = parsed.end.as_ref().map(|range| {
        resolve_selectors(&prelude[range.start - base..range.end - base], range.start, Some(&anchor_parent), namespaces).map(|value| value.selectors)
    }).transpose()?;
    if start.iter().chain(end.iter()).any(|selectors| selectors.iter().any(Selector::has_pseudo_element)) {
        return Err(error(base, "pseudo-element in scope selector"));
    }
    let scope = Arc::new(ScopeContext { start, end, outer: parent.and_then(|value| value.scope.clone()) });
    Ok((Parent { scope: Some(scope), ..anchor_parent }, parsed))
}
const MAX_NESTING_MATCH_WORK: usize = 16_384;

pub fn parse_source_rules(
    text: &str,
    parent_context: &[&str],
    strict: bool,
) -> Result<Vec<SourceRule>, CssError> {
    if text.len() > MAX_CSS_BYTES || parent_context.len() >= 32 {
        return Err(error(0, "CSS rule nesting limit"));
    }
    let namespaces=NamespaceMap::from_stylesheet(text)?;
    let mut parent = None;
    for selector in parent_context { parent = Some(resolve_selectors(selector, 0, parent.as_ref(), &namespaces)?); }
    parse_body(text, 0, parent.as_ref(), false, strict, 0, &mut 0, false, &namespaces)
}

pub fn parse_source_rules_in_context(text:&str,parent_context:&[NestingContext<'_>],strict:bool)->Result<Vec<SourceRule>,CssError> {
    let namespaces=NamespaceMap::from_stylesheet(text)?;
    parse_source_rules_in_namespace_context(text,parent_context,strict,&namespaces)
}

fn parse_source_rules_in_namespace_context(text: &str, parent_context: &[NestingContext<'_>], strict: bool, namespaces: &NamespaceMap) -> Result<Vec<SourceRule>, CssError> {
    if text.len() > MAX_CSS_BYTES || parent_context.len() >= 32 {
        return Err(error(0, "CSS rule nesting limit"));
    }
    let mut parent = None;
    let mut starting_context = false;
    for context in parent_context {
        if matches!(context, NestingContext::StartingStyle) { starting_context = true; continue; }
        parent = Some(match context {
            NestingContext::Style(selector) => resolve_selectors(selector, 0, parent.as_ref(), &namespaces)?,
            NestingContext::Scope(prelude) => scoped_parent(prelude, 0, parent.as_ref(), namespaces)?.0,
            NestingContext::StartingStyle => unreachable!(),
        });
    }
    let mut count = 0;
    parse_body(text, 0, parent.as_ref(), false, strict, 0, &mut count, starting_context, namespaces)
}

/// Sparse live CSSOM topology that serialization alone cannot preserve.
/// Ranges and parent offsets refer to the exact serialized stylesheet.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeclarationBoundary {
    pub parent_start: usize,
    pub child_index: usize,
    pub range: Range<usize>,
}

pub const MAX_DECLARATION_BOUNDARIES: usize = 1024;

pub fn parse_source_rules_with_boundaries(
    text: &str,
    parent_context: &[&str],
    strict: bool,
    boundaries: &[DeclarationBoundary],
) -> Result<Vec<SourceRule>, CssError> {
    parse_with_boundaries(text, boundaries, || parse_source_rules(text, parent_context, strict))
}

pub fn parse_source_rules_with_boundaries_in_namespace_context(
    text:&str,parent_context:&[NestingContext<'_>],strict:bool,
    boundaries:&[DeclarationBoundary],namespaces:&NamespaceMap,
)->Result<Vec<SourceRule>,CssError> {
    parse_with_boundaries(text,boundaries,||parse_source_rules_in_namespace_context(text,parent_context,strict,namespaces))
}

pub fn parse_source_rules_with_boundaries_in_context(
    text: &str,
    parent_context: &[NestingContext<'_>],
    strict: bool,
    boundaries: &[DeclarationBoundary],
) -> Result<Vec<SourceRule>, CssError> {
    parse_with_boundaries(text, boundaries, || parse_source_rules_in_context(text, parent_context, strict))
}

fn parse_with_boundaries(
    text: &str, boundaries: &[DeclarationBoundary],
    parse: impl FnOnce() -> Result<Vec<SourceRule>, CssError>,
) -> Result<Vec<SourceRule>, CssError> {
    if boundaries.is_empty() {
        return parse();
    }
    if boundaries.len() > MAX_DECLARATION_BOUNDARIES {
        return Err(error(0, "too many declaration boundaries"));
    }
    let mut ordered: Vec<&DeclarationBoundary> = boundaries.iter().collect();
    for boundary in &ordered {
        if boundary.range.start > boundary.range.end
            || boundary.range.end > text.len()
            || !text.is_char_boundary(boundary.range.start)
            || !text.is_char_boundary(boundary.range.end)
            || boundary.child_index >= MAX_RULES
        {
            return Err(error(boundary.range.start, "invalid declaration boundary"));
        }
    }
    ordered.sort_unstable_by_key(|boundary| (boundary.parent_start, boundary.child_index));
    if ordered.windows(2).any(|pair| {
        pair[0].parent_start == pair[1].parent_start && pair[0].child_index == pair[1].child_index
    }) {
        return Err(error(0, "duplicate declaration boundary index"));
    }
    let mut tree = parse()?;
    let mut matched = 0usize;
    apply_boundaries(text, &mut tree, None, &ordered, &mut matched)?;
    if matched != boundaries.len() {
        return Err(error(0, "declaration boundary parent was not found"));
    }
    fn count_rules(tree: &[SourceRule], count: &mut usize) -> Result<(), CssError> {
        for rule in tree {
            *count += 1;
            if *count > MAX_RULES {
                return Err(error(rule.range.start, "too many CSS rules"));
            }
            count_rules(&rule.children, count)?;
        }
        Ok(())
    }
    count_rules(&tree, &mut 0)?;
    Ok(tree)
}

fn apply_boundaries(
    text: &str,
    tree: &mut [SourceRule],
    inherited: Option<&Arc<[Selector]>>,
    boundaries: &[&DeclarationBoundary],
    matched: &mut usize,
) -> Result<(), CssError> {
    for node in tree {
        let selectors = node.selectors.as_ref().or(inherited).cloned();
        // Parent offsets are stable even when child indices change.
        apply_boundaries(
            text,
            &mut node.children,
            selectors.as_ref(),
            boundaries,
            matched,
        )?;
        let first = boundaries.partition_point(|boundary| boundary.parent_start < node.range.start);
        let last = boundaries.partition_point(|boundary| boundary.parent_start <= node.range.start);
        let local = &boundaries[first..last];
        if !local.is_empty() {
            let body = node.body.as_ref().ok_or_else(|| {
                error(
                    node.range.start,
                    "declaration boundary requires a rule body",
                )
            })?;
            if !matches!(node.kind, SourceRuleKind::Style | SourceRuleKind::Group | SourceRuleKind::Scope)
                || selectors.is_none()
            {
                return Err(error(
                    node.range.start,
                    "declaration boundary requires a style context",
                ));
            }
            for (index, boundary) in local.iter().enumerate() {
                if boundary.range.start < body.start
                    || boundary.range.end > body.end
                    || local[..index].iter().any(|previous| {
                        previous.range.start == boundary.range.start
                            || ranges_overlap(&previous.range, &boundary.range)
                    })
                    || node.children.iter().any(|child| {
                        child.kind != SourceRuleKind::NestedDeclarations
                            && ranges_overlap(&child.range, &boundary.range)
                    })
                {
                    return Err(error(
                        boundary.range.start,
                        "declaration boundary overlaps a rule",
                    ));
                }
                for piece in node.declaration_ranges.iter().chain(
                    node.children
                        .iter()
                        .filter(|child| child.kind == SourceRuleKind::NestedDeclarations)
                        .flat_map(|child| child.declaration_ranges.iter()),
                ) {
                    if ranges_overlap(piece, &boundary.range)
                        && !(boundary.range.start <= piece.start && boundary.range.end >= piece.end)
                    {
                        return Err(error(
                            boundary.range.start,
                            "declaration boundary splits a declaration",
                        ));
                    }
                }
            }
            let mut explicit = Vec::new();
            for boundary in local {
                let pieces: Vec<Range<usize>> = node
                    .declaration_ranges
                    .iter()
                    .chain(
                        node.children
                            .iter()
                            .filter(|child| child.kind == SourceRuleKind::NestedDeclarations)
                            .flat_map(|child| child.declaration_ranges.iter()),
                    )
                    .filter(|piece| {
                        boundary.range.start <= piece.start
                            && boundary.range.end >= piece.end
                            && !piece.is_empty()
                    })
                    .cloned()
                    .collect();
                if pieces.is_empty() {
                    let mut position = boundary.range.start;
                    if skip_css_space_comments(text, &mut position).is_none()
                        || position < boundary.range.end
                    {
                        return Err(error(
                            boundary.range.start,
                            "declaration boundary has no declarations",
                        ));
                    }
                }
                explicit.push(SourceRule {
                    kind: SourceRuleKind::NestedDeclarations,
                    range: boundary.range.clone(),
                    prelude: boundary.range.start..boundary.range.start,
                    body: None,
                    declaration_offset: Some(boundary.range.start),
                    declaration_ranges: pieces,
                    children: Vec::new(),
                    scope_prelude: None,
                    css_scope: node.css_scope.clone(),
                    selectors: selectors.clone(),
                });
            }
            node.declaration_ranges.retain(|piece| {
                !local.iter().any(|boundary| {
                    boundary.range.start <= piece.start && boundary.range.end >= piece.end
                })
            });
            let mut children = Vec::new();
            for child in core::mem::take(&mut node.children) {
                if child.kind != SourceRuleKind::NestedDeclarations {
                    children.push(child);
                    continue;
                }
                let mut pending = Vec::new();
                for piece in &child.declaration_ranges {
                    if local.iter().any(|boundary| {
                        boundary.range.start <= piece.start && boundary.range.end >= piece.end
                    }) {
                        push_residual_run(&child, &mut pending, &mut children);
                    } else {
                        pending.push(piece.clone());
                    }
                }
                push_residual_run(&child, &mut pending, &mut children);
            }
            for (boundary, child) in local.iter().zip(explicit) {
                if boundary.child_index > children.len() {
                    return Err(error(
                        boundary.range.start,
                        "declaration boundary child index out of range",
                    ));
                }
                children.insert(boundary.child_index, child);
            }
            if children
                .windows(2)
                .any(|pair| pair[0].range.start > pair[1].range.start)
            {
                return Err(error(
                    node.range.start,
                    "declaration boundary source order mismatch",
                ));
            }
            node.children = children;
            *matched += local.len();
        }
    }
    Ok(())
}

fn ranges_overlap(a: &Range<usize>, b: &Range<usize>) -> bool {
    if a.is_empty() {
        b.start < a.start && a.start < b.end
    } else if b.is_empty() {
        a.start < b.start && b.start < a.end
    } else {
        a.start < b.end && b.start < a.end
    }
}

fn push_residual_run(
    template: &SourceRule,
    pending: &mut Vec<Range<usize>>,
    output: &mut Vec<SourceRule>,
) {
    if pending.is_empty() {
        return;
    }
    let range = pending[0].start..pending.last().unwrap().end;
    let child = SourceRule {
        kind: SourceRuleKind::NestedDeclarations,
        prelude: range.start..range.start,
        declaration_offset: Some(range.start),
        range,
        body: None,
        declaration_ranges: core::mem::take(pending),
        children: Vec::new(),
        scope_prelude: None,
        css_scope: template.css_scope.clone(),
        selectors: template.selectors.clone(),
    };
    output.push(child);
}

/// CSSOM's parse-a-rule entry point rejects unconsumed non-trivia, while
/// stylesheet parsing continues to recover invalid neighboring rules.
pub fn parse_one_source_rule(text: &str, parent_context: &[&str]) -> Result<SourceRule, CssError> {
    one_source_rule(text, parse_source_rules(text, parent_context, true)?)
}

pub fn parse_one_source_rule_in_context(text: &str, parent_context: &[NestingContext<'_>]) -> Result<SourceRule, CssError> {
    one_source_rule(text, parse_source_rules_in_context(text, parent_context, true)?)
}

pub fn parse_one_source_rule_in_namespace_context(text: &str, parent_context: &[NestingContext<'_>], namespaces: &NamespaceMap) -> Result<SourceRule, CssError> {
    one_source_rule(text, parse_source_rules_in_namespace_context(text, parent_context, true, namespaces)?)
}

fn one_source_rule(text: &str, mut rules: Vec<SourceRule>) -> Result<SourceRule, CssError> {
    if rules.len() != 1 {
        return Err(error(0, "insertRule requires exactly one rule"));
    }
    let rule = rules.pop().unwrap();
    let mut position = rule.range.end;
    if skip_css_space_comments(text, &mut position).is_none() || position != text.len() {
        return Err(error(position, "unexpected input after CSS rule"));
    }
    let mut leading = 0;
    if skip_css_space_comments(text, &mut leading).is_none() || leading != rule.range.start {
        return Err(error(leading, "unexpected input before CSS rule"));
    }
    Ok(rule)
}

pub fn validate_nested_selector(input: &str, parent_context: &[&str]) -> Result<(), CssError> {
    if parent_context.len() > 32
        || parent_context
            .iter()
            .map(|value| value.len())
            .sum::<usize>()
            > MAX_CSS_BYTES
    {
        return Err(error(0, "CSS rule nesting limit"));
    }
    if parent_context.is_empty() {
        return parse_nested_selector_list_depth(input, 0, 0, false, &NamespaceMap::default()).map(|_| ());
    }
    let mut parent = None;
    for selector in parent_context {
        parent = Some(resolve_selectors(selector, 0, parent.as_ref(), &NamespaceMap::default())?);
    }
    resolve_selectors(input, 0, parent.as_ref(), &NamespaceMap::default()).map(|_| ())
}

fn flush_declarations(
    text: &str,
    pending: &mut Vec<Range<usize>>,
    output: &mut Vec<SourceRule>,
    count: &mut usize,
    force_leading: bool,
) -> Result<(), CssError> {
    if pending.is_empty() {
        return Ok(());
    }
    let first = pending[0].start;
    let last = pending.last().unwrap().end;
    let mut rule = SourceRule {
        kind: SourceRuleKind::NestedDeclarations,
        range: first..last,
        prelude: first..first,
        body: None,
        declaration_offset: Some(first),
        declaration_ranges: core::mem::take(pending),
        children: Vec::new(),
        scope_prelude: None,
        css_scope: None,
        selectors: None,
    };
    let has_declarations=if rule.declaration_ranges.len()>MAX_DECLARATIONS {
        let source=rule.declaration_source(text);
        let block=DeclarationBlock::parse(source.as_ref()).map_err(|mut failure|{
            if failure.message=="CSS declaration block exceeds limit" || failure.message=="too many declarations" {
                return error(first,"too many declarations");
            }
            failure.offset+=first;failure
        })?;
        !block.is_empty()
    }else{force_leading || !cssom_declaration_text(&rule.declaration_source(text)).is_empty()};
    if force_leading || has_declarations {
        *count += 1;
        if *count > MAX_RULES {
            return Err(error(first, "too many CSS rules"));
        }
        // The caller attaches the nearest parent, preserving its per-branch
        // specificity and pseudo-elements rather than using a nesting alias.
        rule.range = first..last;
        output.push(rule);
    }
    Ok(())
}

fn parse_body(
    text: &str,
    base: usize,
    parent: Option<&Parent>,
    style_body: bool,
    strict: bool,
    depth: usize,
    count: &mut usize,
    starting_context: bool,
    namespaces: &NamespaceMap,
) -> Result<Vec<SourceRule>, CssError> {
    if depth >= 32 && !style_body {
        return Err(error(base, "CSS rule nesting limit"));
    }
    let mut output = Vec::new();
    let mut pending = Vec::new();
    for (start, end, open, close) in source_rule_ranges(&text[base..], parent.is_some())? {
        // This function is called with a bounded body slice below; translating
        // its ranges once avoids rescanning ancestors for source-path lookup.
        let start = base + start;
        let end = base + end;
        let open = open.map(|value| base + value);
        let close = close.map(|value| base + value);
        if open.is_none() {
            if parent.is_some() && !text[start..end].trim_start().starts_with('@') {
                pending.push(start..end);
                // A partial `all` reset may serialize one registry-bounded
                // cohort. Validate its compact admission when the run ends;
                // ordinary authored declarations keep their existing quota.
                if pending.len() > MAX_DECLARATIONS + PROPERTIES.len() {
                    return Err(error(start, "too many declarations"));
                }
            } else if valid_statement(
                text[start..end].trim().trim_end_matches(';'),
                parent.is_some(),
                depth,
            ) {
                if depth >= 32 {
                    return Err(error(start, "CSS rule nesting limit"));
                }
                let force_leading = style_body && output.is_empty();
                flush_declarations(text, &mut pending, &mut output, count, force_leading)?;
                output.push(SourceRule {
                    kind: SourceRuleKind::Statement,
                    range: start..end,
                    prelude: start..if text.as_bytes().get(end.saturating_sub(1))==Some(&b';') {end-1} else {end},
                    body: None,
                    declaration_offset: None,
                    declaration_ranges: Vec::new(),
                    children: Vec::new(),
                    scope_prelude: None,
                    css_scope: parent.and_then(|value| value.scope.clone()),
                    selectors: None,
                });
                *count += 1;
                if *count > MAX_RULES {
                    return Err(error(start, "too many CSS rules"));
                }
            }
            continue;
        }
        if depth >= 32 {
            return Err(error(start, "CSS rule nesting limit"));
        }
        let (open, close) = (open.unwrap(), close.unwrap());
        let prelude = text[start..open].trim();
        let scope_body = parent.is_some_and(|value| value.scoped);
        let kind = if is_font_face_prelude(prelude) && (parent.is_none() || scope_body || starting_context) {
            SourceRuleKind::FontFace
        } else if at_rule_tail(prelude, "@property").is_some() && (parent.is_none() || scope_body || starting_context) {
            SourceRuleKind::Property
        } else if at_rule(prelude, "@font-feature-values") && (parent.is_none() || scope_body || starting_context) {
            SourceRuleKind::FontFeatureValues
        } else if at_rule(prelude, "@scope") {
            SourceRuleKind::Scope
        } else if at_rule(prelude, "@media")
            || at_rule(prelude, "@supports")
            || at_rule(prelude, "@layer")
            || at_rule(prelude, "@starting-style")
        {
            SourceRuleKind::Group
        } else if (at_rule(prelude, "@keyframes") || at_rule(prelude, "@-webkit-keyframes"))
            && (parent.is_none() || scope_body || starting_context)
        {
            SourceRuleKind::Keyframes
        } else if prelude.starts_with('@') {
            continue;
        } else {
            SourceRuleKind::Style
        };
        if kind == SourceRuleKind::FontFeatureValues &&
            !font_families(prelude["@font-feature-values".len()..].trim()).is_some_and(|families|
                !families.is_empty() && families.iter().all(|family|matches!(family,FontFamily::Named(_)))) {
            continue;
        }
        if kind == SourceRuleKind::Property && registered_properties::parse_rules(prelude, &text[open + 1..close])?.is_empty() { continue; }
        if kind == SourceRuleKind::Group && at_rule(prelude, "@starting-style")
            && !prelude[15..].trim().is_empty() { continue; }
        if kind == SourceRuleKind::Group
            && at_rule(prelude, "@layer")
            && !prelude[6..].trim().is_empty()
            && !valid_import_layer_name(prelude[6..].trim())
        {
            continue;
        }
        if kind == SourceRuleKind::Group
            && at_rule(prelude, "@supports")
            && parse_supports_condition(&prelude[9..], 0, namespaces).is_none()
        {
            continue;
        }
        let scope_parent = if kind == SourceRuleKind::Scope {
            match scoped_parent(prelude, start, parent, namespaces) {
                Ok(parsed) => Some(parsed),
                Err(error) if strict || selector_limit_error(&error) => return Err(error),
                Err(_) => continue,
            }
        } else { None };
        let resolved = if kind == SourceRuleKind::Style {
            match resolve_selectors(prelude, start, parent, namespaces) {
                Ok(selectors) => Some(selectors),
                Err(error)
                    if strict
                        || selector_limit_error(&error)
                        || error.message == "CSS nesting selector work limit" =>
                {
                    return Err(error)
                }
                Err(_) => continue,
            }
        } else {
            None
        };
        if kind == SourceRuleKind::Keyframes {
            match parse_keyframes_rule(&text[start..end]) {
                Ok(Some(_))=>{},
                Ok(None)=>continue,
                Err(error) if strict || css_resource_error(&error)=>return Err(error),
                Err(_)=>continue,
            }
        }

        let force_leading = style_body && output.is_empty();
        flush_declarations(text, &mut pending, &mut output, count, force_leading)?;
        *count += 1;
        if *count > MAX_RULES {
            return Err(error(start, "too many CSS rules"));
        }
        let mut node = SourceRule {
            kind,
            range: start..end,
            prelude: start..open,
            body: Some(open + 1..close),
            declaration_offset: matches!(kind, SourceRuleKind::Style | SourceRuleKind::FontFace | SourceRuleKind::FontFeatureValues | SourceRuleKind::Property)
                .then_some(open + 1),
            declaration_ranges: Vec::new(),
            children: Vec::new(),
            scope_prelude: scope_parent.as_ref().map(|value| Box::new(value.1.clone())),
            css_scope: scope_parent.as_ref().map(|value| value.0.scope.clone()).flatten()
                .or_else(|| parent.and_then(|value| value.scope.clone())),
            selectors: resolved.as_ref().map(|value| value.selectors.clone()),
        };
        match kind {
            SourceRuleKind::Style | SourceRuleKind::Group | SourceRuleKind::Scope => {
                let next_parent = scope_parent.as_ref().map(|value| &value.0).or(resolved.as_ref()).or(parent);
                if kind == SourceRuleKind::Scope { node.selectors = next_parent.map(|value| value.selectors.clone()); }
                // Keep absolute ranges while limiting this recursive scanner
                // to the current body, never to the rest of the stylesheet.
                node.children = parse_body(
                    &text[..close],
                    open + 1,
                    next_parent,
                    kind == SourceRuleKind::Style,
                    false,
                    depth + 1,
                    count,
                    starting_context || at_rule(prelude, "@starting-style"),
                    namespaces,
                )?;
                if kind == SourceRuleKind::Style
                    && node
                        .children
                        .first()
                        .is_some_and(|child| child.kind == SourceRuleKind::NestedDeclarations)
                {
                    let leading = node.children.remove(0);
                    node.declaration_ranges = leading.declaration_ranges;
                }
            }
            SourceRuleKind::FontFace | SourceRuleKind::FontFeatureValues | SourceRuleKind::Property => node.declaration_ranges.push(open + 1..close),
            SourceRuleKind::Keyframes => {
                for (frame_start, frame_end, frame_open, frame_close) in
                    source_rule_ranges(&text[open + 1..close], false)?
                {
                    if let (Some(frame_open), Some(frame_close)) = (frame_open, frame_close) {
                        if parse_keyframe_key_text(&text[open+1+frame_start..open+1+frame_open]).is_none() {continue;}
                        *count += 1;
                        if *count > MAX_RULES {
                            return Err(error(start, "too many CSS rules"));
                        }
                        let base = open + 1;
                        node.children.push(SourceRule {
                            kind: SourceRuleKind::Keyframe,
                            range: base + frame_start..base + frame_end,
                            prelude: base + frame_start..base + frame_open,
                            body: Some(base + frame_open + 1..base + frame_close),
                            declaration_offset: Some(base + frame_open + 1),
                            declaration_ranges: vec![base + frame_open + 1..base + frame_close],
                            children: Vec::new(),
                            scope_prelude: None,
                            css_scope: parent.and_then(|value| value.scope.clone()),
                            selectors: None,
                        });
                    }
                }
            }
            _ => {}
        }
        output.push(node);
    }
    let force_leading = style_body && output.is_empty();
    flush_declarations(text, &mut pending, &mut output, count, force_leading)?;
    for node in &mut output {
        if node.kind == SourceRuleKind::NestedDeclarations {
            node.selectors = parent.map(|value| value.selectors.clone());
            node.css_scope = parent.and_then(|value| value.scope.clone());
        }
    }
    Ok(output)
}

pub(super) fn at_rule_tail<'a>(prelude: &'a str, keyword: &str) -> Option<&'a str> {
    if !prelude.starts_with('@') { return None; }
    let mut at = 1;
    let name = consume_selector_identifier(prelude, &mut at)?;
    name.eq_ignore_ascii_case(keyword.strip_prefix('@')?).then(|| &prelude[at..])
}

pub fn at_rule(prelude: &str, keyword: &str) -> bool {
    prelude
        .get(..keyword.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(keyword))
        && prelude.get(keyword.len()..).is_some_and(|tail| {
            tail.is_empty()
                || tail.starts_with(char::is_whitespace)
                || tail.starts_with('(')
                || tail.starts_with('{')
                || tail.starts_with(';')
                || tail.starts_with("/*")
        })
}

fn valid_statement(prelude: &str, inside_style: bool, depth: usize) -> bool {
    if at_rule(prelude, "@import") {
        return !inside_style
            && depth == 0
            && parse_import_prelude(prelude, 0..prelude.len()).is_some();
    }
    if at_rule(prelude, "@layer") {
        return comma_components(&prelude[6..], 128).is_some_and(|names| {
            !names.is_empty()
                && names
                    .iter()
                    .all(|name| valid_import_layer_name(name.trim()))
        });
    }
    if !inside_style && depth==0 && namespaces::parse_prelude(prelude).is_some() {
        return true;
    }
    false
}

fn resolve_selectors(
    input: &str,
    offset: usize,
    parent: Option<&Parent>,
    namespaces: &NamespaceMap,
) -> Result<Parent, CssError> {
    let Some(parent) = parent else {
        let mut selectors = parse_selector_list_depth(input, offset, 0, false, true, namespaces)?;
        let work = scope_selectors(&mut selectors, offset)?;
        return Ok(Parent {
            selectors: selectors.into(),
            work,
            scoped: false,
            scope: None,
        });
    };
    let mut selectors = Vec::new();
    let mut work = 0usize;
    // Pseudo-elements make the reference contextually invalid as a whole.
    let invalid_parent = parent.selectors.iter().any(Selector::has_pseudo_element);
    let reference: Arc<[Selector]> = if invalid_parent {
        Arc::from([])
    } else {
        parent.selectors.clone()
    };
    let specificity = if invalid_parent {
        (0, 0, 0)
    } else {
        parent
            .selectors
            .iter()
            .map(|selector| selector.specificity)
            .max()
            .unwrap_or((0, 0, 0))
    };
    for (start, end) in selector_list_spans(input, offset, false)? {
        let item = &input[start..end];
        let mut leading = 0;
        skip_css_space_comments(item, &mut leading)
            .ok_or_else(|| error(offset + start, "unterminated selector comment"))?;
        let relation = match item.as_bytes().get(leading) {
            Some(b'>') => Some(Relation::Child),
            Some(b'+') => Some(Relation::Adjacent),
            Some(b'~') => Some(Relation::Following),
            _ => None,
        };
        let mut selector = parse_selector_depth(
            if relation.is_some() {
                &item[leading + 1..]
            } else {
                item
            },
            offset + start,
            0,
            true,
            namespaces,
        )?;
        let explicit = contains_nesting_token(item) || (parent.scoped && selector_contains_scope(&selector));
        let cost = resolve_nesting(&mut selector, &reference, specificity, parent.work)?;
        work = work.saturating_add(cost);
        if !explicit || relation.is_some() {
            let mut anchor = parse_simple_selector_depth("&", offset + start, 0, true, &NamespaceMap::default())?;
            resolve_nesting(&mut anchor, &reference, specificity, parent.work)?;
            attach_parent(
                &mut selector,
                anchor,
                relation.unwrap_or(Relation::Descendant),
            );
            add_specificity_chain(&mut selector, specificity);
            work = work.saturating_add(parent.work);
        }
        if work > MAX_NESTING_MATCH_WORK {
            return Err(error(offset, "CSS nesting selector work limit"));
        }
        // Substitution can introduce a :has() through a shared parent reference.
        if !validate_resolved_has(&mut selector) {
            selector.logical.push(LogicalPseudo::Is(Vec::new()));
            selector.specificity = (0, 0, 0);
        }
        selectors.push(selector);
    }
    Ok(Parent {
        selectors: selectors.into(),
        work: work.max(1),
        scoped: false,
        scope: parent.scope.clone(),
    })
}

fn selector_contains_scope(selector: &Selector) -> bool {
    selector.scope
        || selector.ancestor.as_ref().is_some_and(|(_, value)| selector_contains_scope(value))
        || selector.logical.iter().any(|value| value.selectors().iter().any(selector_contains_scope))
}

pub(super) fn contains_nesting_token(input: &str) -> bool {
    let mut position = 0;
    while position < input.len() {
        match input.as_bytes()[position] {
            b'&' => return true,
            b'\\' => {
                if selector_escape(input, &mut position).is_none() {
                    return false;
                }
            }
            b'\'' | b'"' => {
                let Some(end) = quoted_css_end(input, position) else {
                    return false;
                };
                position = end;
            }
            b'/' if input[position..].starts_with("/*") => {
                let Some(end) = input[position + 2..].find("*/") else {
                    return false;
                };
                position += end + 4;
            }
            _ => position += input[position..].chars().next().map_or(1, char::len_utf8),
        }
    }
    false
}

pub(super) fn scope_selectors(
    selectors: &mut [Selector],
    offset: usize,
) -> Result<usize, CssError> {
    if !selectors.iter().any(has_nesting) {
        return Ok(selectors.iter().map(authored_selector_work).sum());
    }
    let scope: Arc<[Selector]> =
        vec![parse_simple_selector_depth(":scope", offset, 0, false, &NamespaceMap::default())?].into();
    let mut work = 0usize;
    for selector in selectors {
        work = work.saturating_add(resolve_nesting(selector, &scope, (0, 0, 0), 1)?);
    }
    if work > MAX_NESTING_MATCH_WORK {
        return Err(error(offset, "CSS nesting selector work limit"));
    }
    Ok(work)
}

fn validate_resolved_has(selector: &mut Selector) -> bool {
    if selector
        .ancestor
        .as_mut()
        .is_some_and(|(_, ancestor)| !validate_resolved_has(ancestor))
    {
        return false;
    }
    for pseudo in &mut selector.logical {
        let old = pseudo
            .selectors()
            .iter()
            .map(|value| value.specificity)
            .max()
            .unwrap_or((0, 0, 0));
        match pseudo {
            LogicalPseudo::Has(values) => {
                if values
                    .iter_mut()
                    .any(|value| !value.selector.drop_nested_has_from_forgiving_lists())
                {
                    return false;
                }
            }
            LogicalPseudo::Is(values) | LogicalPseudo::Where(values) => {
                values.retain_mut(validate_resolved_has);
            }
            LogicalPseudo::Not(values) => {
                if !values.iter_mut().all(validate_resolved_has) {
                    return false;
                }
            }
            LogicalPseudo::State(_, _) | LogicalPseudo::Nesting(_) | LogicalPseudo::InvalidNesting | LogicalPseudo::Heading(_) => {}
        }
        if matches!(pseudo, LogicalPseudo::Is(_) | LogicalPseudo::Not(_)) {
            let new = pseudo
                .selectors()
                .iter()
                .map(|value| value.specificity)
                .max()
                .unwrap_or((0, 0, 0));
            selector.specificity = adjust_specificity(selector.specificity, old, new);
        }
    }
    for pseudo in &mut selector.structural {
        if let StructuralPseudo::Nth {
            of: Some(values), ..
        } = pseudo
        {
            if !values.iter_mut().all(validate_resolved_has) {
                return false;
            }
        }
    }
    true
}

fn authored_selector_work(selector: &Selector) -> usize {
    1 + selector
        .ancestor
        .as_ref()
        .map_or(0, |(_, value)| authored_selector_work(value))
        + selector
            .logical
            .iter()
            .map(|pseudo| match pseudo {
                LogicalPseudo::Has(values) => values
                    .iter()
                    .map(|value| authored_selector_work(&value.selector))
                    .sum::<usize>(),
                _ => pseudo
                    .selectors()
                    .iter()
                    .map(authored_selector_work)
                    .sum::<usize>(),
            })
            .sum::<usize>()
        + selector
            .structural
            .iter()
            .map(|pseudo| match pseudo {
                StructuralPseudo::Nth {
                    of: Some(values), ..
                } => values.iter().map(authored_selector_work).sum::<usize>(),
                _ => 0,
            })
            .sum::<usize>()
}

fn attach_parent(selector: &mut Selector, anchor: Selector, relation: Relation) {
    if let Some((_, ancestor)) = &mut selector.ancestor {
        attach_parent(ancestor, anchor, relation);
    } else {
        selector.ancestor = Some((relation, Box::new(anchor)));
    }
}

fn has_nesting(selector: &Selector) -> bool {
    selector.nesting_selector != 0 || selector.ancestor.as_ref().is_some_and(|(_, value)| has_nesting(value))
        || selector.logical.iter().any(|pseudo| match pseudo { LogicalPseudo::InvalidNesting => true, LogicalPseudo::Has(relative) => relative.iter().any(|value| has_nesting(&value.selector)), _ => pseudo.selectors().iter().any(has_nesting) })
        || selector.structural.iter().any(|pseudo| matches!(pseudo, StructuralPseudo::Nth { of: Some(values), .. } if values.iter().any(has_nesting)))
}

fn add_specificity_chain(selector: &mut Selector, value: (u16, u16, u16)) {
    selector.specificity = adjust_specificity(selector.specificity, (0, 0, 0), value);
    if let Some((_, ancestor)) = &mut selector.ancestor {
        add_specificity_chain(ancestor, value);
    }
}

fn resolve_nesting(
    selector: &mut Selector,
    parent: &Arc<[Selector]>,
    specificity: (u16, u16, u16),
    parent_work: usize,
) -> Result<usize, CssError> {
    let mut work = 1usize;
    if let Some((_, ancestor)) = &mut selector.ancestor {
        let old = ancestor.specificity;
        work = work.saturating_add(resolve_nesting(ancestor, parent, specificity, parent_work)?);
        selector.specificity = adjust_specificity(selector.specificity, old, ancestor.specificity);
    }
    for pseudo in &mut selector.logical {
        let old = pseudo
            .selectors()
            .iter()
            .map(|value| value.specificity)
            .max()
            .unwrap_or((0, 0, 0));
        match pseudo {
            LogicalPseudo::Not(values)
            | LogicalPseudo::Is(values)
            | LogicalPseudo::Where(values) => {
                for value in values {
                    work = work.saturating_add(resolve_nesting(
                        value,
                        parent,
                        specificity,
                        parent_work,
                    )?);
                }
            }
            LogicalPseudo::Has(values) => {
                let old = values
                    .iter()
                    .map(|value| value.selector.specificity)
                    .max()
                    .unwrap_or((0, 0, 0));
                for value in values.iter_mut() {
                    work = work.saturating_add(resolve_nesting(
                        &mut value.selector,
                        parent,
                        specificity,
                        parent_work,
                    )?);
                }
                let new = values
                    .iter()
                    .map(|value| value.selector.specificity)
                    .max()
                    .unwrap_or((0, 0, 0));
                selector.specificity = adjust_specificity(selector.specificity, old, new);
            }
            LogicalPseudo::State(_, _) | LogicalPseudo::Nesting(_) | LogicalPseudo::InvalidNesting | LogicalPseudo::Heading(_) => {}
        }
        if matches!(pseudo, LogicalPseudo::Is(_) | LogicalPseudo::Not(_)) {
            let new = pseudo
                .selectors()
                .iter()
                .map(|value| value.specificity)
                .max()
                .unwrap_or((0, 0, 0));
            selector.specificity = adjust_specificity(selector.specificity, old, new);
        }
    }
    for pseudo in &mut selector.structural {
        if let StructuralPseudo::Nth {
            of: Some(values), ..
        } = pseudo
        {
            let old = values
                .iter()
                .map(|value| value.specificity)
                .max()
                .unwrap_or((0, 0, 0));
            for value in values.iter_mut() {
                work =
                    work.saturating_add(resolve_nesting(value, parent, specificity, parent_work)?);
            }
            let new = values
                .iter()
                .map(|value| value.specificity)
                .max()
                .unwrap_or((0, 0, 0));
            selector.specificity = adjust_specificity(selector.specificity, old, new);
        }
    }
    let count = core::mem::take(&mut selector.nesting_selector);
    if count != 0 {
        work = work.saturating_add(parent_work.saturating_mul(usize::from(count)));
        let value = (
            specificity.0.saturating_mul(count),
            specificity.1.saturating_mul(count),
            specificity.2.saturating_mul(count),
        );
        selector.specificity = adjust_specificity(selector.specificity, (0, 0, 0), value);
        selector
            .logical
            .push(LogicalPseudo::Nesting(parent.clone()));
    }
    if work > MAX_NESTING_MATCH_WORK {
        return Err(error(0, "CSS nesting selector work limit"));
    }
    Ok(work)
}

pub fn absolutize_nested_selector(input: &str) -> String {
    let Ok(spans) = selector_list_spans(input, 0, false) else {
        return input.into();
    };
    let mut result = Vec::new();
    for (start, end) in spans {
        let item = input[start..end].trim();
        let mut position = 0;
        let _ = skip_css_space_comments(item, &mut position);
        let relative = matches!(item.as_bytes().get(position), Some(b'>' | b'+' | b'~'));
        let explicit = !relative && contains_nesting_token(item);
        result.push(if explicit {
            item.to_owned()
        } else {
            alloc::format!("& {item}")
        });
    }
    result.join(", ")
}

pub fn declaration_offsets(
    text: &str,
    paths: &[Arc<[usize]>],
) -> Result<Vec<Option<usize>>, CssError> {
    if paths.len() > 1024 || paths.iter().any(|path| path.len() > 32) {
        return Err(error(0, "CSS path limit"));
    }
    let tree = parse_source_rules(text, &[], false)?;
    Ok(paths
        .iter()
        .map(|path| source_rule_at_path(&tree, path).and_then(|node| node.declaration_offset))
        .collect())
}

/// Borrow a canonical CSSOM rule from an already parsed bounded source tree.
/// A stylesheet owner can retain one tree for its exact source generation;
/// declaration/name reads then touch only this path and its source ranges.
pub fn source_rule_at_path<'a>(tree: &'a [SourceRule], path: &[usize]) -> Option<&'a SourceRule> {
    if path.is_empty() || path.len() > 32 {
        return None;
    }
    let mut children = tree;
    let mut found = None;
    for &index in path {
        let node = children.get(index)?;
        found = Some(node);
        children = &node.children;
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn element(class: &str) -> NodeKind {
        NodeKind::Element {
            namespace: Namespace::Html,
            name: "div".into(),
            attributes: vec![("class".into(), class.into())],
        }
    }

    #[test]
    fn nesting_cascade_resolves_parent_lists_relative_selectors_and_ordered_tails() {
        let source = ".outer, #absent { color: blue; > .child { width: 17px; color: red; } &.outer { height: 31px; } @media screen { color: green; > .child { height: 23px; } } color: purple; } .outer { color: black; } .child { color: yellow; }";
        let index = StyleIndex::new(parse(source).unwrap());
        let mut document = Document::new(16);
        let outer = document.create(element("outer")).unwrap();
        let child = document.create(element("child")).unwrap();
        let outside = document.create(element("child")).unwrap();
        let host = document.create(element("host")).unwrap();
        document.append(document.root(), host).unwrap();
        document.append(host, outer).unwrap();
        document.append(outer, child).unwrap();
        document.append(host, outside).unwrap();
        let outer_style = compute_node(&document, outer, None, &index).unwrap();
        let child_style = compute_node(&document, child, Some(&outer_style), &index).unwrap();
        let outside_style = compute_node(&document, outside, None, &index).unwrap();
        assert_eq!(outer_style.height, Some(31.0));
        // Declaration tails have the matching .outer branch's specificity;
        // explicit nesting references use the entire parent list's maximum.
        assert_eq!(outer_style.color, color("black").unwrap());
        assert_eq!(child_style.width, Some(17.0));
        assert_eq!(child_style.height, Some(23.0));
        assert_eq!(child_style.color, color("red").unwrap());
        assert_eq!(outside_style.width, None);
        assert_eq!(outside_style.color, color("yellow").unwrap());
    }

    #[test]
    fn nesting_declaration_rules_preserve_pseudo_elements_and_individual_specificity() {
        let source = "#absent, .outer::before { content: 'x'; color: red; @media screen { color: green; } & { color: black; } color: blue; } .outer::before { color: orange; }";
        let index = StyleIndex::new(parse(source).unwrap());
        let mut document = Document::new(8);
        let outer = document.create(element("outer")).unwrap();
        document.append(document.root(), outer).unwrap();
        let style = compute_node(&document, outer, None, &index).unwrap();
        let generated = index
            .compute_pseudo(&document, outer, &style, PseudoElement::Before, None)
            .unwrap()
            .unwrap();
        assert_eq!(generated.style.color, color("orange").unwrap());
    }

    #[test]
    fn nesting_top_level_scope_has_zero_specificity_and_preserves_dom_selector_grammar() {
        let index = StyleIndex::new(
            parse("& { color:red; } div { color:blue; } & > .child { width:13px; }").unwrap(),
        );
        let mut document = Document::new(8);
        let root = document.create(element("root")).unwrap();
        let child = document.create(element("child")).unwrap();
        document.append(document.root(), root).unwrap();
        document.append(root, child).unwrap();
        let root_style = compute_node(&document, root, None, &index).unwrap();
        assert_eq!(root_style.color, color("blue").unwrap());
        assert_eq!(
            compute_node(&document, child, Some(&root_style), &index)
                .unwrap()
                .width,
            Some(13.0)
        );
        assert_eq!(
            crate::selector::query_selector_all(&document, root, "& > .child").unwrap(),
            vec![child]
        );
        assert!(parse_selector_list("&", 0).is_ok());
        assert!(supports_condition("selector(&)"));
    }

    #[test]
    fn nesting_scope_roots_limits_proximity_specificity_and_owner_are_real_cascade() {
        let mut document = Document::new(32);
        let fixture = document.create(element("fixture")).unwrap();
        let outer = document.create(element("outer")).unwrap();
        let inner = document.create(element("inner")).unwrap();
        let target = document.create(element("target")).unwrap();
        let limit = document.create(element("limit")).unwrap();
        let hole = document.create(element("target")).unwrap();
        let outside = document.create(element("target")).unwrap();
        let style_owner = document.create(NodeKind::Element {
            namespace: Namespace::Html, name: "style".into(), attributes: vec![],
        }).unwrap();
        document.append(document.root(), fixture).unwrap();
        document.append(fixture, outer).unwrap();
        document.append(outer, inner).unwrap();
        document.append(inner, target).unwrap();
        document.append(inner, limit).unwrap();
        document.append(limit, hole).unwrap();
        document.append(fixture, outside).unwrap();
        document.append(inner, style_owner).unwrap();
        let source = ".target { width:3px; } @scope (.inner) to (.limit) { .target { width:17px; } } @scope (.outer) { .target { width:11px; } }";
        let index = StyleIndex::new(parse(source).unwrap());
        assert_eq!(compute_node(&document, target, None, &index).unwrap().width, Some(17.0));
        assert_eq!(compute_node(&document, hole, None, &index).unwrap().width, Some(11.0));
        assert_eq!(compute_node(&document, outside, None, &index).unwrap().width, Some(3.0));
        let index = StyleIndex::new(parse(&alloc::format!("{source} div.target {{ width:23px; }}")).unwrap());
        assert_eq!(compute_node(&document, target, None, &index).unwrap().width, Some(23.0));
        let index = StyleIndex::new(parse("@scope (.outer) to (.limit) { @scope (.inner) { & > .target { height:31px; } } }").unwrap());
        assert_eq!(compute_node(&document, target, None, &index).unwrap().height, Some(31.0));
        assert_eq!(compute_node(&document, hole, None, &index).unwrap().height, None);
        assert_eq!(compute_node(&document, outside, None, &index).unwrap().height, None);
        for (selector, expected) in [(":scope .target", 31.0), ("& .target", 37.0)] {
            let index = StyleIndex::new(parse(&alloc::format!("@scope (.inner) {{ {selector} {{ width:31px; }} }} .inner .target {{ width:37px; }}")).unwrap());
            assert_eq!(compute_node(&document, target, None, &index).unwrap().width, Some(expected));
        }
        let mut rules = parse("@scope { :scope { width:21px; } .target { height:7px; } }").unwrap();
        for rule in &mut rules { rule.stylesheet_owner = Some(StylesheetIdentity::Dom(style_owner)); }
        let index = StyleIndex::new(rules);
        assert_eq!(compute_node(&document, inner, None, &index).unwrap().width, Some(21.0));
        assert_eq!(compute_node(&document, target, None, &index).unwrap().height, Some(7.0));
        assert_eq!(compute_node(&document, outside, None, &index).unwrap().height, None);
        document.append(outer, style_owner).unwrap();
        assert_eq!(compute_node(&document, outer, None, &index).unwrap().width, Some(21.0));
        assert_eq!(compute_node(&document, inner, None, &index).unwrap().width, None);
    }

    #[test]
    fn nesting_scope_context_boundaries_keep_root_declarations_offsets_and_nested_anchors() {
        let source = ".outer { @scope (&) { width:13px; @media screen { height:9px; } .target {} width:19px; } height:5px; }";
        let tree = parse_source_rules(source, &[], false).unwrap();
        let scope = &tree[0].children[0];
        assert_eq!(scope.kind, SourceRuleKind::Scope);
        assert_eq!(scope.children.len(), 4);
        assert_eq!(scope.children[0].kind, SourceRuleKind::NestedDeclarations);
        assert_eq!(scope.children[3].kind, SourceRuleKind::NestedDeclarations);
        assert_eq!(scope.children[3].declaration_offset, source.find("width:19px"));
        assert_eq!(tree[0].children[1].kind, SourceRuleKind::NestedDeclarations);
        let index = StyleIndex::new(parse(source).unwrap());
        let mut document = Document::new(8);
        let outer = document.create(element("outer")).unwrap();
        let target = document.create(element("target")).unwrap();
        document.append(document.root(), outer).unwrap();
        document.append(outer, target).unwrap();
        let root_style = compute_node(&document, outer, None, &index).unwrap();
        assert_eq!(root_style.width, Some(19.0));
        assert_eq!(root_style.height, Some(5.0));
        assert_eq!(compute_node(&document, target, None, &index).unwrap().width, None);
        let contexts = [NestingContext::Style(".outer"), NestingContext::Scope("@scope (&)")];
        let child = parse_one_source_rule_in_context("> .target { width:7px; }", &contexts).unwrap();
        assert_eq!(child.kind, SourceRuleKind::Style);
        assert_eq!(child.selectors.as_ref().unwrap()[0].specificity, (0, 1, 0));
        assert!(parse_scope_prelude("@scope (.a::before)", 0).is_err());
        assert!(parse_scope_prelude("@scope (.a) to (.b) trailing", 0).is_err());
        assert!(parse_scope_prelude("@scope (.a)", usize::MAX).is_err());
        let parsed = parse_scope_prelude("@scope (.a) to (.b)", 11).unwrap();
        assert_eq!(parsed.start, Some(19..21));
        assert_eq!(parsed.end, Some(27..29));
    }

    #[test]
    fn nesting_scope_featureless_shadow_hosts_share_matching() {
        let mut document = Document::new(16);
        let host = document.create(element("host")).unwrap();
        document.append(document.root(), host).unwrap();
        let shadow = document.attach_shadow(host, crate::shadow::ShadowMode::Open).unwrap();
        let target = document.create(element("target")).unwrap();
        let owner = document.create(NodeKind::Element { namespace: Namespace::Html, name: "style".into(), attributes: vec![] }).unwrap();
        document.append(shadow, owner).unwrap();
        document.append(shadow, target).unwrap();
        let mut rules = parse(":is(:scope, .target, .host) { height:99px; } @scope { :scope { width:21px; } > .target { width:17px; } :scope.host { height:99px; } } @scope (:host) { :is(:scope, .target) { color:green; } }").unwrap();
        for rule in &mut rules { rule.scope = Some(shadow); rule.stylesheet_owner = Some(StylesheetIdentity::Dom(owner)); }
        let index = StyleIndex::new(rules);
        assert_eq!(compute_node(&document, target, None, &index).unwrap().width, Some(17.0));
        let host_style = compute_node(&document, host, None, &index).unwrap();
        assert_eq!(host_style.width, Some(21.0));
        assert_eq!(host_style.height, None);
        assert_eq!(host_style.color, color("green").unwrap());
        assert_eq!(compute_node(&document, target, None, &index).unwrap().color, color("green").unwrap());
        assert!(!index.siblings_share);
    }

    #[test]
    fn nesting_scope_selector_quota_bounds_relational_visits_without_negation_false_positives() {
        let mut document = Document::new(64);
        let root = document.create(element("outer")).unwrap();
        document.append(document.root(), root).unwrap();
        for _ in 0..24 {
            let text = document.create(NodeKind::Text("text".into())).unwrap();
            document.append(root, text).unwrap();
        }
        let marker = document.create(element("marker")).unwrap();
        document.append(root, marker).unwrap();
        let validity = crate::forms::NoValidityOverrides;
        for raw in [".outer:has(.marker)", ".outer:not(:has(.absent))"] {
            let selector = parse_selector(raw, 0).unwrap();
            assert!(selector.matches_node_in_scope_with_validity(&document, root, Some(root), &validity));
            let mut small = 4;
            assert!(!selector.matches_in_context_with_work(&super::SelectorDocument::ordinary(&document), root, None, Some(root), &validity, &mut small));
            assert_eq!(small, 0);
            let mut sufficient = 256;
            assert!(selector.matches_in_context_with_work(&super::SelectorDocument::ordinary(&document), root, None, Some(root), &validity, &mut sufficient));
            assert!(sufficient > 0 && sufficient < 256);
        }
    }

    #[test]
    fn nesting_resolved_contexts_forgiving_markers_and_empty_semicolons() {
        let mut document = Document::new(16);
        let host = document.create(element("host")).unwrap();
        let child = document.create(element("target")).unwrap();
        document.append(document.root(), host).unwrap();
        document.append(host, child).unwrap();
        let grandchild = document.create(element("leaf")).unwrap();
        document.append(child, grandchild).unwrap();
        let source = ".target {color:green} .absent { :is(.target, !&) {color:blue} }";
        let index = StyleIndex::new(parse(source).unwrap());
        assert_eq!(
            compute_node(&document, child, None, &index).unwrap().color,
            color("blue").unwrap()
        );
        for source in [
            ".target {color:green} *, ::before { & * {color:red} }",
            ".target {color:green} :is(*, ::before) * {color:red}",
            ".target {color:green} .absent { :is(.target, :unknown(div, &)) {color:blue} }",
        ] {
            let index = StyleIndex::new(parse(source).unwrap());
            let expected = if source.contains(":unknown") {
                "blue"
            } else {
                "green"
            };
            assert_eq!(
                compute_node(&document, child, None, &index).unwrap().color,
                color(expected).unwrap()
            );
        }
        let source = ".host {color:green} .target:has(*) { :has(> &) {color:red} }";
        let index = StyleIndex::new(parse(source).unwrap());
        assert_eq!(
            compute_node(&document, host, None, &index).unwrap().color,
            color("green").unwrap()
        );
        let source = ".target { display:hover {}; ; color:green; }";
        let tree = parse_source_rules(source, &[], false).unwrap();
        // `display:hover` is a valid nested selector, so the following
        // declaration belongs to a synthetic child rather than the parent's
        // leading declaration block. Empty semicolons must not discard it.
        assert!(tree[0].declaration_ranges.is_empty());
        assert_eq!(tree[0].children.len(), 2);
        assert_eq!(tree[0].children[0].kind, SourceRuleKind::Style);
        let tail = &tree[0].children[1];
        assert_eq!(tail.kind, SourceRuleKind::NestedDeclarations);
        assert!(tail.declaration_source(source).contains("color:green"));
        assert_eq!(tail.declaration_offset, source.find("color:green"));
        let index = StyleIndex::new(parse(source).unwrap());
        assert_eq!(
            compute_node(&document, child, None, &index).unwrap().color,
            color("green").unwrap()
        );
        assert_eq!(
            absolutize_nested_selector("> & .bar, + .bar &, :is(!& .foo, .b)"),
            "& > & .bar, & + .bar &, :is(!& .foo, .b)"
        );
        assert!(!contains_nesting_token(r#"[title='&'] /* & */ .\&"#));
        assert!(contains_nesting_token(":is(:unknown(&), .b)"));
    }

    #[test]
    fn nesting_sparse_boundaries_preserve_adjacent_empty_grouped_cascade_and_offsets() {
        let source =
            ".host { width:3px; color:red; height:5px; @media screen { color:green; width:7px; } }";
        let color_start = source.find("color:red;").unwrap();
        let color_end = color_start + "color:red;".len();
        let height_start = source.find("height:5px;").unwrap();
        let group_start = source.find("@media").unwrap();
        let green_start = source.find("color:green;").unwrap();
        let green_end = green_start + "color:green;".len();
        let width_start = source.find("width:7px;").unwrap();
        let boundaries = vec![
            DeclarationBoundary {
                parent_start: 0,
                child_index: 0,
                range: color_start..color_end,
            },
            DeclarationBoundary {
                parent_start: 0,
                child_index: 1,
                range: color_end..color_end,
            },
            DeclarationBoundary {
                parent_start: 0,
                child_index: 2,
                range: height_start..height_start + "height:5px;".len(),
            },
            DeclarationBoundary {
                parent_start: group_start,
                child_index: 0,
                range: green_start..green_end,
            },
            DeclarationBoundary {
                parent_start: group_start,
                child_index: 1,
                range: green_end..green_end,
            },
            DeclarationBoundary {
                parent_start: group_start,
                child_index: 2,
                range: width_start..width_start + "width:7px;".len(),
            },
        ];
        let tree = parse_source_rules_with_boundaries(source, &[], false, &boundaries).unwrap();
        assert_eq!(tree[0].declaration_source(source).trim(), "width:3px;");
        assert_eq!(tree[0].children.len(), 4);
        assert!(tree[0].children[1].declaration_ranges.is_empty());
        assert_eq!(tree[0].children[1].declaration_offset, Some(color_end));
        assert_eq!(tree[0].children[3].children.len(), 3);
        assert_eq!(
            source_rule_at_path(&tree, &[0, 3, 2])
                .unwrap()
                .declaration_offset,
            Some(width_start)
        );
        let declaration = parse_one_source_rule("width:100px; height:200px", &[".host"]).unwrap();
        assert_eq!(declaration.kind, SourceRuleKind::NestedDeclarations);
        assert_eq!(declaration.declaration_ranges.len(), 2);
        let mut parsed = parse_stylesheet_with_boundaries(source, &boundaries).unwrap();
        let mut document = Document::new(8);
        let host = document.create(element("host")).unwrap();
        document.append(document.root(), host).unwrap();
        let owner = StylesheetIdentity::Dom(host);
        for rule in &mut parsed.rules {
            rule.stylesheet_owner = Some(owner);
        }
        let mut index = StyleIndex::new(parsed.rules);
        let overrides = vec![
            RuleDeclarationOverride {
                owner,
                source_text: Arc::from(source),
                source_url: None,
                declaration_offset: color_end,
                cssom_path: Arc::from([0usize, 1]),
                block: alloc::rc::Rc::new(DeclarationBlock::parse("height:11px").unwrap()),
            },
            RuleDeclarationOverride {
                owner,
                source_text: Arc::from(source),
                source_url: None,
                declaration_offset: green_end,
                cssom_path: Arc::from([0usize, 3, 1]),
                block: alloc::rc::Rc::new(DeclarationBlock::parse("color:orange").unwrap()),
            },
            RuleDeclarationOverride {
                owner,
                source_text: Arc::from(source),
                source_url: None,
                declaration_offset: width_start,
                cssom_path: Arc::from([0usize, 3, 2]),
                block: alloc::rc::Rc::new(DeclarationBlock::parse("width:13px").unwrap()),
            },
        ];
        index.apply_cssom_rule_overrides(&overrides).unwrap();
        let style = compute_node(&document, host, None, &index).unwrap();
        assert_eq!(style.width, Some(13.0));
        // The explicit adjacent height declaration follows the empty child.
        assert_eq!(style.height, Some(5.0));
        assert_eq!(style.color, color("orange").unwrap());
        let source = ".host {width:3px;}";
        let start = source.find("width").unwrap();
        let boundaries = [DeclarationBoundary {
            parent_start: 0,
            child_index: 0,
            range: start..start + "width:3px;".len(),
        }];
        let mut parsed = parse_stylesheet_with_boundaries(source, &boundaries).unwrap();
        assert_eq!(
            parsed.rules.len(),
            1,
            "empty parent must not share the synthetic child's override offset"
        );
        for rule in &mut parsed.rules {
            rule.stylesheet_owner = Some(owner);
        }
        let mut index = StyleIndex::new(parsed.rules);
        index
            .apply_cssom_rule_overrides(&[RuleDeclarationOverride {
                owner,
                source_text: Arc::from(source),
                source_url: None,
                declaration_offset: start,
                cssom_path: Arc::from([0usize, 0]),
                block: alloc::rc::Rc::new(DeclarationBlock::parse("width:19px").unwrap()),
            }])
            .unwrap();
        assert_eq!(
            compute_node(&document, host, None, &index).unwrap().width,
            Some(19.0)
        );
        assert!(parse_source_rules_with_boundaries(
            source,
            &[],
            false,
            &[DeclarationBoundary {
                parent_start: 0,
                child_index: 0,
                range: start + 1..start + 3
            }]
        )
        .is_err());
    }

    #[test]
    fn nesting_sparse_boundaries_count_restored_nodes_once() {
        let source = alloc::format!(".host {{ {} }}", "color:red; .child {} ".repeat(1537));
        let original = parse_source_rules(&source, &[], false).unwrap();
        let boundaries: Vec<_> = original[0]
            .children
            .iter()
            .enumerate()
            .filter(|(_, rule)| rule.kind == SourceRuleKind::NestedDeclarations)
            .take(MAX_DECLARATION_BOUNDARIES)
            .map(|(child_index, rule)| DeclarationBoundary {
                parent_start: original[0].range.start,
                child_index,
                range: rule.range.clone(),
            })
            .collect();
        assert_eq!(boundaries.len(), MAX_DECLARATION_BOUNDARIES);
        let restored =
            parse_source_rules_with_boundaries(&source, &[], false, &boundaries).unwrap();
        assert_eq!(restored[0].children.len(), original[0].children.len());
        assert_eq!(
            restored[0].children.last().unwrap().kind,
            SourceRuleKind::Style
        );
    }

    #[test]
    fn nesting_source_tree_shares_cssom_indices_and_exact_declaration_offsets() {
        let source = "@unknown ignored; @charset \"UTF-8\"; /*é*/ .a { --tokens: { value: ';{}' }; color: red; .b { width: 2px; } color: blue; @media screen { width: 9px; & .c { width: 3px; } height: 11px; } color: green; }";
        let tree = parse_source_rules(source, &[], false).unwrap();
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].children.len(), 4);
        assert!(tree[0].declaration_text(source).contains("--tokens:"));
        assert_eq!(tree[0].children[0].kind, SourceRuleKind::Style);
        assert_eq!(tree[0].children[1].kind, SourceRuleKind::NestedDeclarations);
        let paths: Vec<Arc<[usize]>> = [
            vec![0],
            vec![0, 1],
            vec![0, 2, 0],
            vec![0, 2, 2],
            vec![0, 3],
        ]
        .into_iter()
        .map(Arc::from)
        .collect();
        let offsets = declaration_offsets(source, &paths).unwrap();
        assert_eq!(
            offsets,
            vec![
                Some(source.find('{').unwrap() + 1),
                Some(source.find("color: blue").unwrap()),
                Some(source.find("width: 9px").unwrap()),
                Some(source.find("height: 11px").unwrap()),
                Some(source.find("color: green").unwrap())
            ]
        );
        assert_eq!(
            absolutize_nested_selector(".child, &.same, > .direct"),
            "& .child, &.same, & > .direct"
        );
    }

    #[test]
    fn nesting_recovery_preserves_valid_neighbors_and_enforces_depth_and_work_limits() {
        let index = StyleIndex::new(parse(".outer { color: blue; :unsupported { color: red; .child { height: 100px; } } @unknown { .child { height: 200px; } } color: green; }").unwrap());
        assert_eq!(
            compute(&element("outer"), None, &index).unwrap().color,
            color("green").unwrap()
        );
        for source in [
            "div { color: blue /* unfinished",
            "div { color: blue; width: calc(12px +",
        ] {
            let index = StyleIndex::new(parse(source).unwrap());
            assert_eq!(
                compute(&element(""), None, &index).unwrap().color,
                color("blue").unwrap()
            );
        }
        let deep = alloc::format!(
            ".outer {{{}color:red;{}}}",
            "& & {".repeat(18),
            "}".repeat(18)
        );
        assert_eq!(
            parse(&deep).unwrap_err().message,
            "CSS nesting selector work limit"
        );
        let deep = alloc::format!(
            "{}div{{color:red}}{}",
            "@media screen {".repeat(33),
            "}".repeat(33)
        );
        assert_eq!(parse(&deep).unwrap_err().message, "CSS rule nesting limit");
        let block =
            DeclarationBlock::parse("color: blue; @media screen { color:red; } color: orange;")
                .unwrap();
        assert_eq!(block.value("color").unwrap().0, "orange");
    }
}
