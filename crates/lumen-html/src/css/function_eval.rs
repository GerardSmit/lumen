//! CSS Mixins: collecting `@function` rules and substituting `--f()` calls in custom property
//! values. A call evaluates in a frame of its parameters and local custom properties, which
//! shadows the caller's frame, which shadows the element's custom properties.
use super::function_rule::{self, FunctionPrelude};
use super::*;
use alloc::collections::BTreeMap;
use core::cell::{Cell, OnceCell, RefCell};

const MAX_CALL_DEPTH: usize = 48;
const MAX_CALLS: usize = 4096;
const MAX_ROUNDS: usize = 8;

#[derive(Clone, Debug)]
pub struct FunctionRule {
    pub prelude: FunctionPrelude,
    pub items: Vec<Item>,
    pub layer: Option<usize>,
    pub media: Vec<String>,
}

/// A body entry: a local custom property or `result` descriptor, or a conditional block.
#[derive(Clone, Debug)]
pub enum Item {
    Declaration(String, String),
    Media(String, Vec<Item>),
}

#[derive(Default)]
pub struct FunctionTable {
    rules: BTreeMap<String, Vec<(usize, Arc<FunctionRule>)>>,
}

impl FunctionTable {
    pub fn from_rules(rules: &[FunctionRule]) -> Self {
        let mut table = Self::default();
        for (order, rule) in rules.iter().enumerate() {
            table.rules.entry(rule.prelude.name.clone()).or_default().push((order, Arc::new(rule.clone())));
        }
        table
    }

    /// The winning definition of `name`: unlayered over layered, later layers and later rules first.
    fn lookup(&self, name: &str, environment: MediaEnvironment) -> Option<Arc<FunctionRule>> {
        self.rules
            .get(name)?
            .iter()
            .filter(|(_, rule)| rule.media.iter().all(|query| media_queries::query_matches(query, environment)))
            .max_by_key(|(order, rule)| (rule.layer.map_or(usize::MAX, |layer| layer), *order))
            .map(|(_, rule)| rule.clone())
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

struct Collector<'a> {
    input: &'a str,
    namespaces: NamespaceMap,
    layers: Vec<String>,
    anonymous: usize,
    rules: Vec<FunctionRule>,
}

impl Collector<'_> {
    fn layer_index(&mut self, parent: Option<usize>, name: &str) -> usize {
        let full = match parent {
            Some(parent) => alloc::format!("{}.{name}", self.layers[parent]),
            None => String::from(name),
        };
        match self.layers.iter().position(|known| *known == full) {
            Some(found) => found,
            None => {
                self.layers.push(full);
                self.layers.len() - 1
            }
        }
    }

    fn collect(&mut self, nodes: &[nesting::SourceRule], layer: Option<usize>, media: &[String]) {
        for node in nodes {
            let prelude = self.input[node.prelude.clone()].trim();
            if node.kind == nesting::SourceRuleKind::Statement {
                if nesting::at_rule(prelude, "@layer") {
                    for name in prelude[6..].split(',').map(str::trim).filter(|name| !name.is_empty()) {
                        self.layer_index(layer, name);
                    }
                }
                continue;
            }
            if node.kind != nesting::SourceRuleKind::Group {
                continue;
            }
            if nesting::at_rule(prelude, "@function") {
                if let Some(parsed) = function_rule::parse_prelude(&prelude["@function".len()..]) {
                    let body = node.body.clone().map_or("", |body| &self.input[body]);
                    let items = parse_block(body, &self.namespaces);
                    self.rules.push(FunctionRule { prelude: parsed, items, layer, media: media.to_vec() });
                }
            } else if nesting::at_rule(prelude, "@layer") {
                let name = prelude[6..].trim();
                let inner = if name.is_empty() {
                    self.anonymous += 1;
                    let anonymous = alloc::format!("\u{0}{}", self.anonymous);
                    self.layer_index(layer, &anonymous)
                } else {
                    self.layer_index(layer, name)
                };
                self.collect(&node.children, Some(inner), media);
            } else if nesting::at_rule(prelude, "@media") {
                let mut nested = media.to_vec();
                nested.push(String::from(prelude[6..].trim()));
                self.collect(&node.children, layer, &nested);
            } else if nesting::at_rule(prelude, "@supports") {
                if parse_supports_condition(&prelude[9..], 0, &self.namespaces) == Some(true) {
                    self.collect(&node.children, layer, media);
                }
            }
        }
    }
}

/// The valid `@function` rules of a stylesheet, in source order, with their layer and media context.
pub fn collect_rules(input: &str) -> Vec<FunctionRule> {
    if !input.as_bytes().windows(9).any(|window| window.eq_ignore_ascii_case(b"@function")) {
        return Vec::new();
    }
    let Ok(nodes) = nesting::parse_source_rules(input, &[], false) else {
        return Vec::new();
    };
    let Ok(namespaces) = NamespaceMap::from_stylesheet(input) else {
        return Vec::new();
    };
    let mut collector = Collector { input, namespaces, layers: Vec::new(), anonymous: 0, rules: Vec::new() };
    collector.collect(&nodes, None, &[]);
    collector.rules
}

fn skip_string(bytes: &[u8], mut position: usize) -> usize {
    let quote = bytes[position];
    position += 1;
    while position < bytes.len() && bytes[position] != quote {
        if bytes[position] == b'\\' {
            position += 1;
        }
        position += 1;
    }
    position
}

fn push_declaration(piece: &str, items: &mut Vec<Item>) {
    let Some(colon) = piece.find(':') else { return };
    let name = piece[..colon].trim();
    let value = piece[colon + 1..].trim();
    let key = if name.eq_ignore_ascii_case("result") {
        String::from("result")
    } else if name.starts_with("--") && name.len() > 2 {
        String::from(name)
    } else {
        return;
    };
    items.push(Item::Declaration(key, String::from(value)));
}

fn push_conditional(prelude: &str, body: &str, namespaces: &NamespaceMap, items: &mut Vec<Item>) {
    if nesting::at_rule(prelude, "@supports") {
        if parse_supports_condition(&prelude[9..], 0, namespaces) == Some(true) {
            items.extend(parse_block(body, namespaces));
        }
    } else if nesting::at_rule(prelude, "@media") {
        items.push(Item::Media(String::from(prelude[6..].trim()), parse_block(body, namespaces)));
    }
}

fn parse_block(text: &str, namespaces: &NamespaceMap) -> Vec<Item> {
    let bytes = text.as_bytes();
    let mut items = Vec::new();
    let (mut start, mut position, mut declaration) = (0, 0, false);
    while position < bytes.len() {
        match bytes[position] {
            b'(' | b'[' => match function_rule::matching_close(text, position) {
                Some(close) => position = close,
                None => break,
            },
            b'{' => {
                let Some(close) = function_rule::matching_close(text, position) else { break };
                if !declaration {
                    push_conditional(text[start..position].trim(), &text[position + 1..close], namespaces, &mut items);
                    start = close + 1;
                }
                position = close;
            }
            b'"' | b'\'' => position = skip_string(bytes, position),
            b'\\' => position += 1,
            b':' => declaration = true,
            b';' => {
                push_declaration(&text[start..position], &mut items);
                start = position + 1;
                declaration = false;
            }
            _ => {}
        }
        position += 1;
    }
    if start < text.len() {
        push_declaration(&text[start..], &mut items);
    }
    items
}

fn flatten<'a>(items: &'a [Item], environment: MediaEnvironment, out: &mut Vec<(&'a str, &'a str)>) {
    for item in items {
        match item {
            Item::Declaration(name, value) => out.push((name, value)),
            Item::Media(query, nested) => {
                if media_queries::query_matches(query, environment) {
                    flatten(nested, environment, out);
                }
            }
        }
    }
}

/// The canonical text of a function body's declarations: only `result` and custom properties,
/// without priorities, the last declaration of a name winning.
pub fn descriptor_text(input: &str) -> String {
    let mut entries: Vec<(String, String)> = Vec::new();
    for (start, end) in declaration_spans_with_recovery(input, true).unwrap_or_default() {
        let Some((name, raw)) = declaration_pair(&input[start..end]) else { continue };
        let Some(decoded) = parsed_declaration_name(name) else { continue };
        let name = if decoded.starts_with("--") {
            decoded.to_string()
        } else if decoded.eq_ignore_ascii_case("result") {
            String::from("result")
        } else {
            continue;
        };
        let (value, important) = important_value(raw);
        if important {
            continue;
        }
        entries.retain(|(known, _)| *known != name);
        entries.push((name, String::from(value.trim())));
    }
    let mut text = String::new();
    for (name, value) in entries {
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(&serialize_identifier_name(&name));
        text.push_str(": ");
        text.push_str(&value);
        text.push(';');
    }
    text
}

fn serialize_identifier_name(name: &str) -> String {
    if name.starts_with("--") {
        super::identifier::serialize_identifier(name)
    } else {
        String::from(name)
    }
}

/// `@function` followed by the canonical prelude, or `None` when the prelude is invalid.
pub fn serialize_prelude(prelude: &str) -> Option<String> {
    let rest = prelude.trim().strip_prefix("@function")?;
    let parsed = function_rule::parse_prelude(rest)?;
    Some(alloc::format!("@function {}", parsed.serialize()))
}

pub(super) struct Env<'e> {
    table: &'e FunctionTable,
    context: &'e CustomContext,
    environment: MediaEnvironment,
    stack: RefCell<Vec<String>>,
    calls: Cell<usize>,
    registrations: &'e [registered_properties::RegisteredCustomProperty],
    inherited: Option<&'e dyn CustomNames>,
    current: RefCell<Option<String>>,
}

impl<'e> Env<'e> {
    pub(super) fn new(table: &'e FunctionTable, context: &'e CustomContext, environment: MediaEnvironment) -> Self {
        Self { table, context, environment, stack: RefCell::new(Vec::new()), calls: Cell::new(0),
            registrations: &[], inherited: None, current: RefCell::new(None) }
    }

    fn scope<'s>(&'s self, names: &'s dyn CustomNames, current: Option<&'s str>) -> if_function::IfScope<'s> {
        if_function::IfScope {
            names, base: self.inherited, registrations: self.registrations, context: self.context,
            environment: self.environment, current,
        }
    }
}

struct Slot {
    name: CustomText,
    raw: Option<String>,
    initial: Option<CustomText>,
    done: OnceCell<Option<CustomText>>,
    busy: Cell<bool>,
}

struct Frame<'f> {
    slots: Vec<Slot>,
    parent: &'f dyn CustomNames,
    env: &'f Env<'f>,
}

impl<'f> Frame<'f> {
    fn find(&self, name: &str) -> Option<&Slot> {
        self.slots.iter().rev().find(|slot| slot.name.as_str() == name)
    }

    fn push_value(&mut self, name: &str, value: Option<String>) {
        let value = value.map(CustomText::from);
        let done = OnceCell::new();
        let _ = done.set(value.clone());
        self.slots.push(Slot { name: CustomText::from(name), raw: None, initial: value, done, busy: Cell::new(false) });
    }

    fn push_local(&mut self, name: &str, raw: &str) {
        let initial = self.find(name).and_then(|slot| slot.initial.clone());
        self.slots.push(Slot {
            name: CustomText::from(name),
            raw: Some(String::from(raw)),
            initial,
            done: OnceCell::new(),
            busy: Cell::new(false),
        });
    }

    fn resolve(&self, slot: &Slot, raw: &str) -> Option<CustomText> {
        let text = substitute(raw, self, self.env)?;
        let text = text.trim();
        match text.to_ascii_lowercase().as_str() {
            "initial" => slot.initial.clone(),
            "inherit" => self.parent.value(slot.name.as_str()).cloned(),
            "unset" | "revert" | "revert-layer" | "revert-rule" => None,
            _ => Some(CustomText::from(text)),
        }
    }
}

impl CustomNames for Frame<'_> {
    fn value(&self, name: &str) -> Option<&CustomText> {
        let Some(slot) = self.find(name) else {
            return self.parent.value(name);
        };
        if let Some(done) = slot.done.get() {
            return done.as_ref();
        }
        let raw = slot.raw.as_deref()?;
        if slot.busy.replace(true) {
            return None;
        }
        let resolved = self.resolve(slot, raw);
        slot.busy.set(false);
        let _ = slot.done.set(resolved);
        slot.done.get().and_then(Option::as_ref)
    }
}

fn typed(syntax: &str, text: &str, env: &Env<'_>) -> Option<String> {
    if syntax.trim() == "*" {
        return Some(String::from(text));
    }
    if !registered_properties::accepts_value(syntax, text) {
        return None;
    }
    let (length, query, color) = env.context;
    registered_properties::compute_value_with_source(syntax, text, *length, query.clone(), color, None)
}

fn unwrap_braces(argument: &str) -> &str {
    let argument = argument.trim();
    if argument.starts_with('{') && function_rule::matching_close(argument, 0) == Some(argument.len() - 1) {
        return &argument[1..argument.len() - 1];
    }
    argument
}

fn bind_argument(argument: &str, syntax: &str, caller: &dyn CustomNames, env: &Env<'_>) -> Option<String> {
    let text = substitute(unwrap_braces(argument), caller, env)?;
    typed(syntax, text.trim(), env)
}

fn bind_default(default: &str, name: &str, syntax: &str, frame: &Frame<'_>) -> Option<String> {
    let text = substitute(unwrap_braces(default), frame, frame.env)?;
    let text = text.trim();
    match text.to_ascii_lowercase().as_str() {
        "initial" => None,
        "inherit" => frame.parent.value(name).map(|value| String::from(value.as_str())),
        _ => typed(syntax, text, frame.env),
    }
}

fn run(rule: &FunctionRule, bound: Vec<Option<String>>, caller: &dyn CustomNames, env: &Env<'_>) -> Option<String> {
    let mut frame = Frame { slots: Vec::new(), parent: caller, env };
    let mut bound = bound.into_iter();
    for parameter in &rule.prelude.parameters {
        let given = bound.next();
        let value = match given {
            Some(Some(value)) => Some(value),
            other => match &parameter.default {
                Some(default) => bind_default(default, &parameter.name, &parameter.syntax, &frame),
                None if other.is_some() => None,
                None => return None,
            },
        };
        frame.push_value(&parameter.name, value);
    }
    let mut result = None;
    let mut declarations = Vec::new();
    flatten(&rule.items, env.environment, &mut declarations);
    for (name, value) in declarations {
        if name == "result" {
            result = Some(value);
        } else {
            frame.push_local(name, value);
        }
    }
    let text = substitute(result?, &frame, env)?;
    let text = text.trim();
    match rule.prelude.return_type.as_str() {
        "*" => Some(String::from(text)),
        syntax => typed(syntax, text, env),
    }
}

fn call(name: &str, arguments: &[&str], caller: &dyn CustomNames, env: &Env<'_>) -> Option<String> {
    let rule = env.table.lookup(name, env.environment)?;
    if arguments.len() > rule.prelude.parameters.len() || env.calls.get() >= MAX_CALLS {
        return None;
    }
    {
        let stack = env.stack.borrow();
        if stack.len() >= MAX_CALL_DEPTH || stack.iter().any(|entry| entry == name) {
            return None;
        }
    }
    env.calls.set(env.calls.get() + 1);
    let bound: Vec<Option<String>> = arguments
        .iter()
        .zip(&rule.prelude.parameters)
        .map(|(argument, parameter)| bind_argument(argument, &parameter.syntax, caller, env))
        .collect();
    env.stack.borrow_mut().push(String::from(name));
    let result = run(&rule, bound, caller, env);
    env.stack.borrow_mut().pop();
    result
}

fn replace_calls(raw: &str, names: &dyn CustomNames, env: &Env<'_>) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    let mut last = 0;
    for (start, open) in function_rule::call_positions(raw) {
        if start < last {
            continue;
        }
        let close = function_rule::matching_close(raw, open)?;
        let inner = &raw[open + 1..close];
        let arguments = if inner.trim().is_empty() { Vec::new() } else { syntax::split_top_level(inner, b',', usize::MAX, usize::MAX)? };
        let value = call(&raw[start..open], &arguments, names, env)?;
        out.push_str(&raw[last..start]);
        out.push_str(&value);
        last = close + 1;
    }
    out.push_str(&raw[last..]);
    Some(out)
}

/// `raw` with its function calls evaluated and its `var()` references substituted from `names`;
/// `None` is the guaranteed-invalid value.
pub(super) fn substitute(raw: &str, names: &dyn CustomNames, env: &Env<'_>) -> Option<String> {
    let mut text = String::from(raw);
    for _ in 0..MAX_ROUNDS {
        if if_function::contains_if(&text) {
            let current = env.current.borrow().clone();
            text = if_function::replace_ifs(&text, &env.scope(names, current.as_deref()))?;
        }
        if function_rule::contains_call(&text) {
            text = replace_calls(&text, names, env)?;
        }
        if !needs_expansion(&text) {
            if if_function::contains_if(&text) {
                continue;
            }
            break;
        }
        text = var_template::expand_variables_in(&text, names, MAX_VARIABLE_BYTES)?;
        if !function_rule::contains_call(&text) {
            break;
        }
    }
    if if_function::contains_if(&text) || function_rule::contains_call(&text) {
        return None;
    }
    Some(text)
}

/// Evaluates the function calls in the declared custom properties of an element. A call that is
/// undefined, cyclic or malformed makes its property guaranteed-invalid; a CSS-wide keyword
/// result applies to the property.
pub(super) fn evaluate_entries(
    entries: &mut [CustomEntry],
    index: &NameIndex,
    base: Option<&dyn CustomNames>,
    table: &FunctionTable,
    context: &CustomContext,
    environment: MediaEnvironment,
    registrations: &[registered_properties::RegisteredCustomProperty],
) {
    let mut env = Env::new(table, context, environment);
    env.registrations = registrations;
    env.inherited = base;
    let updates: Vec<(usize, Option<CustomText>)> = {
        let names = LayeredCustom { overlay: entries, index, base };
        entries
            .iter()
            .enumerate()
            .filter_map(|(at, (name, value))| {
                let raw = value.as_ref().filter(|value| function_rule::contains_call(value))?;
                *env.current.borrow_mut() = Some(String::from(name.as_str()));
                let resolved = substitute(raw, &names, &env).and_then(|text| {
                    let text = text.trim();
                    match text.to_ascii_lowercase().as_str() {
                        "initial" | "revert-layer" | "revert-rule" => None,
                        "inherit" | "unset" | "revert" => base.and_then(|base| base.value(name.as_str()).cloned()),
                        _ => Some(CustomText::from(text)),
                    }
                });
                Some((at, resolved))
            })
            .collect()
    };
    for (at, value) in updates {
        entries[at].1 = value;
    }
}

/// Substitutes the calls and `var()` references of a non-custom property's value against the
/// element's custom properties.
pub(super) fn substitute_declaration(
    raw: &str,
    names: &dyn CustomNames,
    table: &FunctionTable,
    context: &CustomContext,
    environment: MediaEnvironment,
) -> Option<String> {
    substitute(raw, names, &Env::new(table, context, environment))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_locals_and_result() {
        let rules = collect_rules("@function --f(--x) { --y: 1px; result: calc(var(--x) + var(--y)); }");
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].items.len(), 2);
        assert!(matches!(&rules[0].items[1], Item::Declaration(name, _) if name == "result"));
    }
}
