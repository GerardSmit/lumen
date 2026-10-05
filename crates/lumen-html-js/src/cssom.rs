//! CSSOM algorithms that build on the shared HTML CSS parser and cascade.
//!
//! The DOM adapter installs `CSS.supports` and `matchMedia` from these
//! declarations. Stylesheet text edits are validated by the same parser before
//! the owning `<style>` node is changed, so the renderer and CSSOM observe one
//! source of truth.
use super::*;
use crate::realm_services::RealmServices;
use lumen::embed::Promise;
use lumen::embed::{JsFunction, JsObject};
use lumen_bind::{Passed, This};
use lumen_html::css::{self, MediaEnvironment};
use std::rc::Weak;

fn css_error(error: css::CssError) -> OpError {
    OpError::new(
        "SyntaxError",
        format!("CSS error at {}: {}", error.offset, error.message),
    )
}

#[lumen_bind::class(name = "CSSStyleValue", hint(js(webidl)))]
pub struct DomCssStyleValue {
    serialized_value: RefCell<String>,
}

#[lumen_bind::class(name = "CSSKeywordValue", extends = DomCssStyleValue, hint(js(webidl)))]
pub struct DomCssKeywordValue {
    base: DomCssStyleValue,
}

#[lumen_bind::class(name = "StylePropertyMapReadOnly", hint(js(webidl)))]
pub struct DomStylePropertyMapReadOnly {
    realm: Rc<DomRealm>,
    node: NodeId,
    _owner: Value,
}

#[lumen_bind::class(name = "StylePropertyMap", extends = DomStylePropertyMapReadOnly, hint(js(webidl)))]
pub struct DomStylePropertyMap {
    base: DomStylePropertyMapReadOnly,
}

fn css_identifier_value(input: &str) -> Option<String> {
    fn name_start(character: char) -> bool {
        character == '_' || character.is_ascii_alphabetic() || character >= '\u{80}'
    }
    fn name_character(character: char) -> bool {
        name_start(character) || character == '-' || character.is_ascii_digit()
    }
    fn escape(iter: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<char> {
        let first = iter.next()?;
        if matches!(first, '\n' | '\r' | '\u{c}') {
            return None;
        }
        if !first.is_ascii_hexdigit() {
            return Some(first);
        }
        let mut value = first.to_digit(16)?;
        let mut digits = 1;
        while digits < 6
            && iter
                .peek()
                .is_some_and(|character| character.is_ascii_hexdigit())
        {
            value = value * 16 + iter.next()?.to_digit(16)?;
            digits += 1;
        }
        if iter
            .peek()
            .is_some_and(|character| matches!(character, '\t' | '\n' | '\u{c}' | '\r' | ' '))
        {
            iter.next();
        }
        Some(
            char::from_u32(value)
                .filter(|character| *character != '\0')
                .unwrap_or('\u{fffd}'),
        )
    }

    let mut iter = input.chars().peekable();
    let first = iter.next()?;
    let mut value = String::new();
    if first == '-' {
        value.push('-');
        match iter.next()? {
            '\\' => value.push(escape(&mut iter)?),
            '-' => value.push('-'),
            character if name_start(character) => value.push(character),
            _ => return None,
        }
    } else if first == '\\' {
        value.push(escape(&mut iter)?);
    } else if name_start(first) {
        value.push(first);
    } else {
        return None;
    }
    while let Some(character) = iter.next() {
        match character {
            '\\' => value.push(escape(&mut iter)?),
            character if name_character(character) => value.push(character),
            _ => return None,
        }
    }
    (!value.is_empty()).then_some(value)
}

fn serialize_css_identifier(value: &str) -> String {
    let characters = value.chars().collect::<Vec<_>>();
    let mut serialized = String::new();
    for (index, character) in characters.iter().copied().enumerate() {
        if character == '\0' {
            serialized.push('\u{fffd}');
        } else if (character as u32 >= 1 && character as u32 <= 0x1f)
            || character == '\u{7f}'
            || (index == 0 && character.is_ascii_digit())
            || (index == 1 && characters.first() == Some(&'-') && character.is_ascii_digit())
        {
            serialized.push('\\');
            serialized.push_str(&format!("{:x} ", character as u32));
        } else if index == 0 && character == '-' && characters.len() == 1 {
            serialized.push_str("\\-");
        } else if character >= '\u{80}'
            || character == '-'
            || character == '_'
            || character.is_ascii_alphanumeric()
        {
            serialized.push(character);
        } else {
            serialized.push('\\');
            serialized.push(character);
        }
    }
    serialized
}

fn css_keyword_for_property(property: &str, css_text: &str) -> OpResult<String> {
    if !css::supports_declaration(property, css_text) {
        return Err(OpError::new(
            "SyntaxError",
            "CSS text is invalid for the requested property",
        ));
    }
    css_identifier_value(css_text.trim()).ok_or_else(|| {
        OpError::new(
            "NotSupportedError",
            "the parsed CSS value is not a single keyword",
        )
    })
}

fn keyword_value(value: String) -> DomCssKeywordValue {
    DomCssKeywordValue {
        base: DomCssStyleValue {
            serialized_value: RefCell::new(value),
        },
    }
}

#[lumen_bind::methods]
impl DomCssStyleValue {
    #[classmethod(coerce)]
    fn parse(
        ctx: &mut Ctx,
        _class: This<Value>,
        property: &str,
        css_text: &str,
    ) -> OpResult<Value> {
        let value = css_keyword_for_property(property, css_text)?;
        Ok(ctx.new_instance(keyword_value(value)))
    }

    #[classmethod(coerce)]
    fn parse_all(
        ctx: &mut Ctx,
        _class: This<Value>,
        property: &str,
        css_text: &str,
    ) -> OpResult<Value> {
        let value = css_keyword_for_property(property, css_text)?;
        let global = ctx.global_object();
        let constructor = ctx
            .get_member(&global, "Array")
            .map_err(|_| OpError::new("Error", "Array constructor is unavailable"))?;
        let constructor = JsFunction::from_value(constructor)
            .ok_or_else(|| OpError::new("Error", "Array is not callable"))?;
        let array = constructor.call(ctx, Value::Undefined, &[Value::Num(1.0)])?;
        let item = ctx.new_instance(keyword_value(value));
        ctx.set_member(&array, "0", item)
            .map_err(|_| OpError::new("TypeError", "could not create CSSStyleValue array"))?;
        Ok(array)
    }

    #[method(name = "toString")]
    fn to_string(&self) -> String {
        serialize_css_identifier(&self.serialized_value.borrow())
    }
}

#[lumen_bind::methods]
impl DomCssKeywordValue {
    #[constructor(coerce)]
    fn new(value: &str) -> OpResult<Self> {
        if value.is_empty() {
            return Err(OpError::new(
                "TypeError",
                "CSSKeywordValue value must not be empty",
            ));
        }
        Ok(keyword_value(value.to_owned()))
    }

    #[getter]
    fn value(&self) -> String {
        self.base.serialized_value.borrow().clone()
    }

    #[setter(coerce)]
    fn set_value(&self, value: &str) -> OpResult<()> {
        if value.is_empty() {
            return Err(OpError::new(
                "TypeError",
                "CSSKeywordValue value must not be empty",
            ));
        }
        *self.base.serialized_value.borrow_mut() = value.to_owned();
        Ok(())
    }
}

fn inline_declaration(realm: &DomRealm, node: NodeId) -> OpResult<String> {
    let session = realm.session.borrow();
    match session.document().kind(node).map_err(dom_error)? {
        NodeKind::Element { attributes, .. } => Ok(attributes
            .iter()
            .find(|(name, _)| name == "style")
            .map_or_else(String::new, |(_, value)| value.clone())),
        _ => Err(OpError::new(
            "TypeError",
            "attributeStyleMap requires an Element",
        )),
    }
}

fn write_inline_property(
    realm: &DomRealm,
    node: NodeId,
    property: &str,
    value: &str,
) -> OpResult<()> {
    let mut declaration = CssDeclaration::parse(&inline_declaration(realm, node)?);
    declaration
        .set_property(property, value, false)
        .map_err(css_error)?;
    realm
        .session
        .borrow_mut()
        .document_mut()
        .set_attribute(node, "style", declaration.css_text())
        .map_err(dom_error)
}

fn keyword_from_inline_style(
    realm: &DomRealm,
    node: NodeId,
    property: &str,
) -> OpResult<Option<DomCssKeywordValue>> {
    let declaration = inline_declaration(realm, node)?;
    let Some((value, _)) = css::declaration_value(&declaration, property).map_err(css_error)?
    else {
        return Ok(None);
    };
    let keyword = css_keyword_for_property(property, &value)?;
    Ok(Some(keyword_value(keyword)))
}

fn value_array(ctx: &mut Ctx, values: impl IntoIterator<Item = Value>) -> OpResult<Value> {
    let values = values.into_iter().collect::<Vec<_>>();
    let global = ctx.global_object();
    let constructor = ctx
        .get_member(&global, "Array")
        .map_err(|_| OpError::new("Error", "Array constructor is unavailable"))?;
    let constructor = JsFunction::from_value(constructor)
        .ok_or_else(|| OpError::new("Error", "Array is not callable"))?;
    let array = constructor.call(ctx, Value::Undefined, &[Value::Num(values.len() as f64)])?;
    for (index, value) in values.into_iter().enumerate() {
        ctx.set_member(&array, &index.to_string(), value)
            .map_err(|_| OpError::new("TypeError", "could not create CSS value array"))?;
    }
    Ok(array)
}

#[lumen_bind::methods]
impl DomStylePropertyMapReadOnly {
    #[method(coerce)]
    fn get(&self, ctx: &mut Ctx, property: &str) -> OpResult<Value> {
        Ok(keyword_from_inline_style(&self.realm, self.node, property)?
            .map(|value| ctx.new_instance(value))
            .unwrap_or(Value::Null))
    }

    #[method(coerce)]
    fn get_all(&self, ctx: &mut Ctx, property: &str) -> OpResult<Value> {
        let value = keyword_from_inline_style(&self.realm, self.node, property)?
            .map(|value| ctx.new_instance(value));
        value_array(ctx, value.into_iter())
    }

    #[method(coerce)]
    fn has(&self, property: &str) -> OpResult<bool> {
        let declaration = inline_declaration(&self.realm, self.node)?;
        Ok(css::declaration_value(&declaration, property)
            .map_err(css_error)?
            .is_some())
    }
}

#[lumen_bind::methods]
impl DomStylePropertyMap {
    #[method(coerce)]
    fn set(&self, property: &str, value: &DomCssStyleValue) -> OpResult<()> {
        let value = value.serialized_value.borrow();
        if !css::supports_declaration(property, &value) {
            return Err(OpError::new(
                "TypeError",
                "CSSStyleValue is invalid for the requested property",
            ));
        }
        write_inline_property(&self.base.realm, self.base.node, property, &value)
    }

    #[method(coerce)]
    fn delete(&self, property: &str) -> OpResult<()> {
        write_inline_property(&self.base.realm, self.base.node, property, "")
    }
}

pub(crate) fn attribute_style_map(
    ctx: &mut Ctx,
    realm: Rc<DomRealm>,
    node: NodeId,
    owner: Value,
) -> Value {
    ctx.new_instance(DomStylePropertyMap {
        base: DomStylePropertyMapReadOnly {
            realm,
            node,
            _owner: owner,
        },
    })
}

/// `CSS.supports` overloads, evaluated by the shared parser used by stylesheets
/// and inline style. `Passed` distinguishes the conditionText overload from a
/// two-argument call whose second argument is explicitly null or undefined.
#[lumen_bind::op(name = "supports", coerce)]
pub fn supports(condition_or_property: &str, value: Passed<String>) -> bool {
    match value.0 {
        Some(value) => css::supports_property_value(condition_or_property, &value),
        None => css::supports_condition(condition_or_property),
    }
}

/// Text-backed CSSOM for a single style attribute or stylesheet rule.
/// Invalid declarations are discarded using the renderer's supported-property
/// registry, matching the existing `CSSStyleDeclaration` adapter.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CssDeclaration {
    text: String,
}

impl CssDeclaration {
    pub fn parse(text: &str) -> Self {
        Self {
            text: css::cssom_declaration_text(text),
        }
    }

    pub fn css_text(&self) -> &str {
        &self.text
    }

    pub fn get_property_value(&self, name: &str) -> Result<Option<(String, bool)>, css::CssError> {
        css::declaration_value(&self.text, name)
    }

    pub fn set_property(
        &mut self,
        name: &str,
        value: &str,
        important: bool,
    ) -> Result<(), css::CssError> {
        if value.is_empty() {
            self.text = css::set_declaration(&self.text, name, "", false)?;
        } else if css::supports_declaration(name, value) {
            self.text = css::set_declaration(&self.text, name, value, important)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CssRuleText {
    pub css_text: String,
    pub selector_text: Option<String>,
    pub style: CssDeclaration,
    pub nested: Vec<CssRuleText>,
}

/// Mutable stylesheet source associated with one `<style>` node. Every edit is
/// validated first, then callers commit the resulting text to the document;
/// `RenderSession` reparses that same node for cascade evaluation.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CssStyleSheetText {
    text: String,
}

impl CssStyleSheetText {
    pub fn parse(text: &str) -> Result<Self, css::CssError> {
        css::parse(text)?;
        Ok(Self { text: text.into() })
    }

    pub fn css_text(&self) -> &str {
        &self.text
    }

    pub fn css_rules(&self) -> Result<Vec<CssRuleText>, css::CssError> {
        rules_from_text(&self.text)
    }

    pub fn insert_rule(&mut self, rule: &str, index: usize) -> Result<(), css::CssError> {
        let mut current = self.css_rules()?;
        if index > current.len() {
            return Err(css::CssError {
                offset: index,
                message: "CSS rule index out of range",
            });
        }
        let proposed = rules_from_text(rule)?;
        if proposed.len() != 1 {
            return Err(css::CssError {
                offset: 0,
                message: "insertRule requires exactly one rule",
            });
        }
        let Some(rule) = proposed.into_iter().next() else {
            return Err(css::CssError {
                offset: 0,
                message: "insertRule requires exactly one rule",
            });
        };
        current.insert(index, rule);
        self.text = current
            .iter()
            .map(|rule| rule.css_text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        css::parse(&self.text)?;
        Ok(())
    }

    pub fn delete_rule(&mut self, index: usize) -> Result<(), css::CssError> {
        let mut current = self.css_rules()?;
        if index >= current.len() {
            return Err(css::CssError {
                offset: index,
                message: "CSS rule index out of range",
            });
        }
        current.remove(index);
        self.text = current
            .iter()
            .map(|rule| rule.css_text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        Ok(())
    }

    pub fn replace(&mut self, text: &str) -> Result<(), css::CssError> {
        let parsed = Self::parse(text)?;
        *self = parsed;
        Ok(())
    }

    pub fn set_style_rule_declarations(
        &mut self,
        index: usize,
        declarations: &str,
    ) -> Result<(), css::CssError> {
        let rules = self.css_rules()?;
        let Some(rule) = rules.get(index) else {
            return Err(css::CssError {
                offset: index,
                message: "CSS rule index out of range",
            });
        };
        let Some(selector) = rule.selector_text.as_deref() else {
            return Err(css::CssError {
                offset: index,
                message: "rule is not a style rule",
            });
        };
        let ranges = top_level_rule_ranges(&self.text)?;
        let mut output = String::new();
        for (rule_index, (start, end, open, close)) in ranges.into_iter().enumerate() {
            if !output.is_empty() {
                output.push('\n');
            }
            if rule_index == index {
                if open.is_none() || close.is_none() {
                    return Err(css::CssError {
                        offset: start,
                        message: "rule is not a style rule",
                    });
                }
                output.push_str(selector);
                output.push_str(" {");
                if !declarations.trim().is_empty() {
                    output.push(' ');
                    output.push_str(&css::cssom_declaration_text(declarations));
                    output.push(' ');
                }
                output.push('}');
            } else {
                output.push_str(self.text[start..end].trim());
            }
        }
        css::parse(&output)?;
        self.text = output;
        Ok(())
    }
}

fn rules_from_text(text: &str) -> Result<Vec<CssRuleText>, css::CssError> {
    css::parse(text)?;
    let ranges = top_level_rule_ranges(text)?;
    let mut result = Vec::with_capacity(ranges.len());
    for (start, end, open, close) in ranges {
        let raw = text[start..end].trim();
        if let (Some(open), Some(close)) = (open, close) {
            let prelude = text[start..open].trim();
            let body = &text[open + 1..close];
            let grouping = prelude.starts_with("@media")
                || prelude.starts_with("@layer")
                || prelude
                    .get(..9)
                    .is_some_and(|name| name.eq_ignore_ascii_case("@supports"));
            if prelude.starts_with('@') && !grouping {
                return Err(css::CssError {
                    offset: start,
                    message: "unsupported CSS rule type",
                });
            }
            if !prelude.starts_with('@') && css::parse(raw)?.is_empty() {
                continue;
            }
            result.push(CssRuleText {
                css_text: raw.to_owned(),
                selector_text: (!prelude.starts_with('@')).then(|| prelude.to_owned()),
                style: CssDeclaration::parse(body),
                nested: if grouping {
                    rules_from_text(body)?
                } else {
                    Vec::new()
                },
            });
        } else {
            result.push(CssRuleText {
                css_text: raw.to_owned(),
                selector_text: None,
                style: CssDeclaration::default(),
                nested: Vec::new(),
            });
        }
    }
    Ok(result)
}

fn top_level_rule_ranges(
    text: &str,
) -> Result<Vec<(usize, usize, Option<usize>, Option<usize>)>, css::CssError> {
    let bytes = text.as_bytes();
    let mut ranges = Vec::new();
    let mut start = 0usize;
    while start < bytes.len() {
        while start < bytes.len() && bytes[start].is_ascii_whitespace() {
            start += 1;
        }
        if start == bytes.len() {
            break;
        }
        if bytes.get(start..start + 2) == Some(b"/*") {
            let Some(end) = text[start + 2..].find("*/") else {
                return Err(css::CssError {
                    offset: start,
                    message: "unterminated comment",
                });
            };
            start += end + 4;
            continue;
        }
        let mut index = start;
        let mut quote = None;
        let mut parens = 0usize;
        let mut open = None;
        let mut statement = false;
        while index < bytes.len() {
            let byte = bytes[index];
            if let Some(delimiter) = quote {
                if byte == b'\\' {
                    index = index.saturating_add(2);
                    continue;
                }
                if byte == delimiter {
                    quote = None;
                }
                index += 1;
                continue;
            }
            if bytes.get(index..index + 2) == Some(b"/*") {
                let Some(end) = text[index + 2..].find("*/") else {
                    return Err(css::CssError {
                        offset: index,
                        message: "unterminated comment",
                    });
                };
                index += end + 4;
                continue;
            }
            match byte {
                b'\'' | b'"' => quote = Some(byte),
                b'(' => parens += 1,
                b')' => parens = parens.saturating_sub(1),
                b'{' if parens == 0 => {
                    open = Some(index);
                    break;
                }
                b';' if parens == 0 => {
                    ranges.push((start, index + 1, None, None));
                    start = index + 1;
                    statement = true;
                    break;
                }
                _ => {}
            }
            index += 1;
        }
        if statement {
            continue;
        }
        if let Some(open) = open {
            let mut depth = 1usize;
            index = open + 1;
            quote = None;
            while index < bytes.len() && depth != 0 {
                let byte = bytes[index];
                if let Some(delimiter) = quote {
                    if byte == b'\\' {
                        index = index.saturating_add(2);
                        continue;
                    }
                    if byte == delimiter {
                        quote = None;
                    }
                } else if bytes.get(index..index + 2) == Some(b"/*") {
                    let Some(end) = text[index + 2..].find("*/") else {
                        return Err(css::CssError {
                            offset: index,
                            message: "unterminated comment",
                        });
                    };
                    index += end + 4;
                    continue;
                } else {
                    match byte {
                        b'\'' | b'"' => quote = Some(byte),
                        b'{' => depth += 1,
                        b'}' => depth -= 1,
                        _ => {}
                    }
                }
                index += 1;
            }
            if depth != 0 {
                return Err(css::CssError {
                    offset: open,
                    message: "unterminated rule",
                });
            }
            let close = index - 1;
            ranges.push((start, index, Some(open), Some(close)));
            start = index;
        } else if start < bytes.len() {
            return Err(css::CssError {
                offset: start,
                message: "expected rule block",
            });
        }
    }
    Ok(ranges)
}

/// A viewport-bound MediaQueryList snapshot. The DOM wrapper can query it on
/// each `matches` read; dispatching `change` requires a host resize source.
#[derive(Clone, Debug, PartialEq)]
pub struct MediaQueryList {
    query: String,
    environment: MediaEnvironment,
}

impl MediaQueryList {
    pub fn new(query: &str, environment: MediaEnvironment) -> Self {
        Self {
            query: query.to_owned(),
            environment,
        }
    }

    pub fn media(&self) -> &str {
        &self.query
    }

    pub fn matches(&self) -> bool {
        css::media_query_matches(&self.query, self.environment)
    }
}

struct MediaQueryData {
    realm: Weak<DomRealm>,
    query: String,
    was_matching: Cell<bool>,
    target: DomEventTarget,
    wrapper: RefCell<Option<WeakValue>>,
}

struct MediaQueryRegistry {
    realm: Weak<DomRealm>,
    lists: RefCell<Vec<Weak<MediaQueryData>>>,
    sheets: RefCell<HashMap<NodeId, WeakValue>>,
    adopted: RefCell<Vec<(Option<NodeId>, Vec<AdoptedSheet>)>>,
}

struct ConstructedSheetData {
    realm: Weak<DomRealm>,
    registry: Weak<MediaQueryRegistry>,
    text: RefCell<String>,
}

#[derive(Clone)]
enum SheetSource {
    Element(NodeId),
    Constructed(Rc<ConstructedSheetData>),
}

#[derive(Clone)]
struct AdoptedSheet {
    data: Rc<ConstructedSheetData>,
    value: Value,
}

#[lumen_bind::class(name = "MediaQueryList", extends = DomEventTarget, hint(js(webidl)))]
pub struct DomMediaQueryList {
    base: DomEventTarget,
    data: Rc<MediaQueryData>,
}

#[lumen_bind::methods]
impl DomMediaQueryList {
    #[getter]
    fn media(&self) -> String {
        self.data.query.clone()
    }

    #[getter]
    fn matches(&self) -> bool {
        self.data.realm.upgrade().is_some_and(|realm| {
            let environment = realm.session.borrow().media_environment();
            css::media_query_matches(&self.data.query, environment)
        })
    }

    // The standard EventTarget methods are inherited. This override records
    // the wrapper so host-driven environment changes can dispatch `change`.
    fn add_event_listener(
        &self,
        ctx: &mut Ctx,
        this: This<Value>,
        kind: &str,
        callback: Value,
        options: Option<Value>,
    ) -> OpResult<()> {
        *self.data.wrapper.borrow_mut() = ctx.weak_value(&this.0);
        DomEventTarget::add_event_listener(ctx, this, kind, callback, options)
    }

    fn add_listener(&self, ctx: &mut Ctx, this: This<Value>, callback: Value) -> OpResult<()> {
        self.add_event_listener(ctx, this, "change", callback, None)
    }

    fn remove_listener(&self, ctx: &mut Ctx, this: This<Value>, callback: Value) -> OpResult<()> {
        DomEventTarget::remove_event_listener(ctx, this, "change", callback, None)
    }

    #[getter]
    fn onchange(&self) -> Option<JsFunction> {
        self.base.handler("change")
    }

    #[setter]
    fn set_onchange(&self, ctx: &mut Ctx, this: This<Value>, callback: Option<JsFunction>) {
        self.base.set_handler(ctx, &this.0, "change", callback);
        *self.data.wrapper.borrow_mut() = ctx.weak_value(&this.0);
    }
}

#[lumen_bind::op(name = "matchMedia")]
pub fn match_media(ctx: &mut Ctx, query: &str) -> OpResult<DomMediaQueryList> {
    let registry = RealmServices::<MediaQueryRegistry>::current(ctx)
        .ok_or_else(|| OpError::new("InvalidStateError", "CSSOM is not installed"))?;
    let realm = registry
        .realm
        .upgrade()
        .ok_or_else(|| OpError::new("InvalidStateError", "document realm was released"))?;
    let environment = realm.session.borrow().media_environment();
    let data = Rc::new(MediaQueryData {
        realm: Rc::downgrade(&realm),
        query: query.trim().to_owned(),
        was_matching: Cell::new(css::media_query_matches(query, environment)),
        target: DomEventTarget::independent(&realm),
        wrapper: RefCell::new(None),
    });
    registry.lists.borrow_mut().push(Rc::downgrade(&data));
    Ok(DomMediaQueryList {
        base: DomEventTarget::from_data(data.target.data_handle()),
        data,
    })
}

/// Install the standards-facing CSS namespace and `matchMedia` operation.
/// The browser host calls [`notify_media`] after changing its session's media
/// environment to deliver `MediaQueryList` change events.
pub fn install(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> OpResult<()> {
    ctx.class_constructor::<DomMediaQueryList>();
    ctx.class_constructor::<DomCssStyleValue>();
    ctx.class_constructor::<DomCssKeywordValue>();
    ctx.class_constructor::<DomStylePropertyMapReadOnly>();
    ctx.class_constructor::<DomStylePropertyMap>();
    ctx.class_constructor::<DomCssStyleSheet>();
    ctx.class_constructor::<DomCssRule>();
    ctx.class_constructor::<DomCssRuleList>();
    ctx.class_constructor::<DomCssStyleRule>();
    ctx.class_constructor::<DomCssRuleStyle>();
    ctx.class_constructor::<DomStyleSheetList>();
    RealmServices::replace_current(
        ctx,
        MediaQueryRegistry {
            realm: Rc::downgrade(realm),
            lists: RefCell::new(Vec::new()),
            sheets: RefCell::new(HashMap::new()),
            adopted: RefCell::new(Vec::new()),
        },
    );
    let css_namespace = Value::Obj(ctx.new_object());
    let supports = ctx.bound_function(&lumen_bind::FnItem::of::<supports::Op>());
    ctx.set_member(&css_namespace, "supports", supports)
        .map_err(|_| OpError::new("Error", "CSS.supports installation failed"))?;
    let global = ctx.global_object();
    let sheet_constructor = ctx.class_constructor::<DomCssStyleSheet>();
    crate::install_interface(ctx, &global, "CSSStyleSheet", sheet_constructor)
        .map_err(|_| OpError::new("Error", "CSSStyleSheet installation failed"))?;
    let style_value_constructor = ctx.class_constructor::<DomCssStyleValue>();
    crate::install_interface(ctx, &global, "CSSStyleValue", style_value_constructor)
        .map_err(|_| OpError::new("Error", "CSSStyleValue installation failed"))?;
    let keyword_value_constructor = ctx.class_constructor::<DomCssKeywordValue>();
    crate::install_interface(ctx, &global, "CSSKeywordValue", keyword_value_constructor)
        .map_err(|_| OpError::new("Error", "CSSKeywordValue installation failed"))?;
    let property_map_readonly_constructor = ctx.class_constructor::<DomStylePropertyMapReadOnly>();
    ctx.set_member(
        &global,
        "StylePropertyMapReadOnly",
        property_map_readonly_constructor,
    )
    .map_err(|_| OpError::new("Error", "StylePropertyMapReadOnly installation failed"))?;
    let property_map_constructor = ctx.class_constructor::<DomStylePropertyMap>();
    crate::install_interface(ctx, &global, "StylePropertyMap", property_map_constructor)
        .map_err(|_| OpError::new("Error", "StylePropertyMap installation failed"))?;
    ctx.set_member(&global, "CSS", css_namespace)
        .map_err(|_| OpError::new("Error", "CSS namespace installation failed"))?;
    let match_media = ctx.bound_function(&lumen_bind::FnItem::of::<match_media::Op>());
    ctx.set_member(&global, "matchMedia", match_media)
        .map_err(|_| OpError::new("Error", "matchMedia installation failed"))?;
    Ok(())
}

/// Re-evaluate live query lists and dispatch `change` when a host viewport or
/// display change crosses a media-query boundary.
pub fn notify_media(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> OpResult<()> {
    // The host must enter the target window's JS realm before dispatching. Apart from
    // isolating the registries, this ensures the UA-created Event comes from that
    // realm's intrinsics.
    let Some(registry) = registry_for_realm(ctx, realm) else {
        return Ok(());
    };
    let environment = realm.session.borrow().media_environment();
    // Never hold the registry borrow while running script: a `change`
    // listener may call matchMedia() and append another list reentrantly.
    let snapshot = registry.lists.borrow().clone();
    let mut live = Vec::with_capacity(snapshot.len());
    for weak in snapshot {
        let Some(data) = weak.upgrade() else {
            continue;
        };
        live.push(Rc::downgrade(&data));
        let matches = css::media_query_matches(&data.query, environment);
        let previous = data.was_matching.replace(matches);
        if previous != matches {
            if let Some(wrapper) = data.wrapper.borrow().as_ref().and_then(WeakValue::upgrade) {
                let options = Value::Obj(ctx.new_object());
                if ctx
                    .set_member(&options, "bubbles", Value::Bool(false))
                    .is_err()
                    || ctx
                        .set_member(&options, "cancelable", Value::Bool(false))
                        .is_err()
                {
                    continue;
                }
                let event = match DomEvent::new(ctx, "change", Some(options)) {
                    Ok(event) => ctx.new_instance(event),
                    Err(_) => continue,
                };
                if ctx
                    .set_member(&event, "matches", Value::Bool(matches))
                    .is_err()
                    || ctx
                        .set_member(&event, "media", Value::str(&data.query))
                        .is_err()
                {
                    continue;
                }
                if let Some(event) = JsObject::from_value(event) {
                    let target = DomEventTarget::from_data(data.target.data_handle());
                    let _ = DomEventTarget::dispatch_event(ctx, This(wrapper), event);
                }
            }
        }
    }
    *registry.lists.borrow_mut() = live;
    Ok(())
}

#[lumen_bind::class(name = "CSSStyleSheet", hint(js(webidl)))]
pub struct DomCssStyleSheet {
    realm: Rc<DomRealm>,
    source: SheetSource,
    owner: Value,
}

#[lumen_bind::class(name = "CSSRule", hint(js(webidl)))]
pub struct DomCssRule {
    realm: Rc<DomRealm>,
    source: SheetSource,
    index: usize,
    owner: Value,
}

#[lumen_bind::class(name = "CSSRuleList", hint(js(webidl)))]
pub struct DomCssRuleList {
    realm: Rc<DomRealm>,
    source: SheetSource,
    owner: Value,
}

#[lumen_bind::class(name = "StyleSheetList", hint(js(webidl)))]
pub struct DomStyleSheetList {
    realm: Rc<DomRealm>,
    document: NodeId,
    owner: Value,
}

#[lumen_bind::class(name = "CSSStyleRule", extends = DomCssRule, hint(js(webidl)))]
pub struct DomCssStyleRule {
    base: DomCssRule,
}

#[lumen_bind::class(name = "CSSRuleStyleDeclaration", extends = DomStyle, hint(js(webidl)))]
pub struct DomCssRuleStyle {
    base: DomStyle,
    realm: Rc<DomRealm>,
    source: SheetSource,
    index: usize,
    owner: Value,
}

fn sheet_text(realm: &DomRealm, node: NodeId) -> OpResult<String> {
    fn append(
        document: &lumen_html::Document,
        node: NodeId,
        output: &mut String,
    ) -> Result<(), Error> {
        let mut child = document.first_child(node)?;
        while let Some(id) = child {
            match document.kind(id)? {
                NodeKind::Text(text) => output.push_str(text),
                _ => append(document, id, output)?,
            }
            child = document.next_sibling(id)?;
        }
        Ok(())
    }
    let session = realm.session.borrow();
    let mut text = String::new();
    append(session.document(), node, &mut text).map_err(dom_error)?;
    Ok(text)
}

fn write_sheet_text(realm: &DomRealm, node: NodeId, text: &str) -> OpResult<()> {
    let mut session = realm.session.borrow_mut();
    let document = session.document_mut();
    let mut old = Vec::new();
    let mut child = document.first_child(node).map_err(dom_error)?;
    while let Some(id) = child {
        old.push(id);
        child = document.next_sibling(id).map_err(dom_error)?;
    }
    let existing_text = old
        .iter()
        .copied()
        .find(|id| matches!(document.kind(*id), Ok(NodeKind::Text(_))));
    let text_node = if let Some(existing_text) = existing_text {
        existing_text
    } else {
        document
            .create(NodeKind::Text(String::new()))
            .map_err(dom_error)?
    };
    document.replace_data(text_node, text).map_err(dom_error)?;
    document
        .replace_children_many(node, &[text_node])
        .map_err(dom_error)?;
    for removed in old {
        if removed != text_node {
            document.destroy_subtree(removed).map_err(dom_error)?;
        }
    }
    Ok(())
}

fn source_text(realm: &DomRealm, source: &SheetSource) -> OpResult<String> {
    match source {
        SheetSource::Element(node) => sheet_text(realm, *node),
        SheetSource::Constructed(data) => Ok(data.text.borrow().clone()),
    }
}

fn source_sheet(realm: &DomRealm, source: &SheetSource) -> OpResult<CssStyleSheetText> {
    CssStyleSheetText::parse(&source_text(realm, source)?).map_err(css_error)
}

fn source_dom_node(realm: &DomRealm, source: &SheetSource) -> NodeId {
    match source {
        SheetSource::Element(node) => *node,
        SheetSource::Constructed(_) => realm.session.borrow().document().root(),
    }
}

fn sync_adopted(registry: &MediaQueryRegistry, realm: &DomRealm) -> OpResult<()> {
    let adopted = registry.borrowed_adoptions();
    let mut session = realm.session.borrow_mut();
    for (scope, sheets) in adopted {
        session
            .set_adopted_stylesheets(scope, sheets)
            .map_err(|error| {
                OpError::new(
                    "SyntaxError",
                    format!("adopted stylesheet failed: {error:?}"),
                )
            })?;
    }
    Ok(())
}

pub fn adopted_stylesheets(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    scope: Option<NodeId>,
) -> OpResult<Value> {
    let registry = registry_for_realm(ctx, realm)
        .ok_or_else(|| OpError::new("InvalidStateError", "CSSOM is not installed"))?;
    let values = registry
        .adopted
        .borrow()
        .iter()
        .find(|(candidate, _)| *candidate == scope)
        .map(|(_, sheets)| {
            sheets
                .iter()
                .map(|sheet| sheet.value.clone())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let global = ctx.global_object();
    let constructor = ctx
        .get_member(&global, "Array")
        .map_err(|_| OpError::new("Error", "Array constructor is unavailable"))?;
    let constructor = JsFunction::from_value(constructor)
        .ok_or_else(|| OpError::new("Error", "Array is not callable"))?;
    let array = constructor.call(ctx, Value::Undefined, &[Value::Num(values.len() as f64)])?;
    for (index, value) in values.into_iter().enumerate() {
        ctx.set_member(&array, &index.to_string(), value)
            .map_err(|_| OpError::new("TypeError", "could not create adopted stylesheet array"))?;
    }
    Ok(array)
}

pub fn set_adopted_stylesheets(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    scope: Option<NodeId>,
    value: Value,
) -> OpResult<()> {
    if !ctx.is_array_value(&value).map_err(OpError::thrown)? {
        return Err(OpError::new(
            "TypeError",
            "adoptedStyleSheets must be an array",
        ));
    }
    let Value::Num(length) = ctx
        .get_member(&value, "length")
        .map_err(|_| OpError::new("TypeError", "could not read adoptedStyleSheets length"))?
    else {
        return Err(OpError::new(
            "TypeError",
            "invalid adoptedStyleSheets length",
        ));
    };
    if !length.is_finite() || length < 0.0 || length > 4096.0 || length.fract() != 0.0 {
        return Err(OpError::new(
            "RangeError",
            "invalid adoptedStyleSheets length",
        ));
    }
    let registry = registry_for_realm(ctx, realm)
        .ok_or_else(|| OpError::new("InvalidStateError", "CSSOM is not installed"))?;
    let mut sheets = Vec::with_capacity(length as usize);
    for index in 0..length as usize {
        let item = ctx
            .get_member(&value, &index.to_string())
            .map_err(|_| OpError::new("TypeError", "could not read adopted stylesheet"))?;
        let native = ctx
            .instance_data::<DomCssStyleSheet>(&item)
            .ok_or_else(|| {
                OpError::new(
                    "TypeError",
                    "adoptedStyleSheets entries must be CSSStyleSheet objects",
                )
            })?;
        let source = native.borrow().source.clone();
        let SheetSource::Constructed(data) = source else {
            return Err(OpError::new(
                "NotAllowedError",
                "only constructed stylesheets can be adopted",
            ));
        };
        if data
            .realm
            .upgrade()
            .is_none_or(|owner| !Rc::ptr_eq(&owner, realm))
        {
            return Err(OpError::new(
                "NotAllowedError",
                "stylesheet was constructed in another document",
            ));
        }
        if sheets
            .iter()
            .any(|sheet: &AdoptedSheet| Rc::ptr_eq(&sheet.data, &data))
        {
            return Err(OpError::new(
                "NotAllowedError",
                "a stylesheet cannot be adopted twice",
            ));
        }
        sheets.push(AdoptedSheet { data, value: item });
    }
    let previous = {
        let mut adopted = registry.adopted.borrow_mut();
        if let Some((_, current)) = adopted
            .iter_mut()
            .find(|(candidate, _)| *candidate == scope)
        {
            core::mem::replace(current, sheets)
        } else {
            adopted.push((scope, sheets));
            Vec::new()
        }
    };
    if let Err(error) = sync_adopted(&registry, realm) {
        let mut adopted = registry.adopted.borrow_mut();
        if let Some((_, current)) = adopted
            .iter_mut()
            .find(|(candidate, _)| *candidate == scope)
        {
            *current = previous;
        }
        return Err(error);
    }
    Ok(())
}

impl MediaQueryRegistry {
    fn borrowed_adoptions(&self) -> Vec<(Option<NodeId>, Vec<String>)> {
        self.adopted
            .borrow()
            .iter()
            .map(|(scope, sheets)| {
                (
                    *scope,
                    sheets
                        .iter()
                        .map(|sheet| sheet.data.text.borrow().clone())
                        .collect(),
                )
            })
            .collect()
    }
}

fn registry_for_realm(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> Option<Rc<MediaQueryRegistry>> {
    let registry = RealmServices::<MediaQueryRegistry>::current(ctx)?;
    registry
        .realm
        .upgrade()
        .is_some_and(|owner| Rc::ptr_eq(&owner, realm))
        .then_some(registry)
}

fn write_source_text(realm: &DomRealm, source: &SheetSource, text: &str) -> OpResult<()> {
    match source {
        SheetSource::Element(node) => write_sheet_text(realm, *node, text),
        SheetSource::Constructed(data) => {
            *data.text.borrow_mut() = text.to_owned();
            if let Some(registry) = data.registry.upgrade() {
                sync_adopted(&registry, realm)?;
            }
            Ok(())
        }
    }
}

/// Creates the live stylesheet object for a `<style>` node. The DOM adapter
/// uses this from `HTMLStyleElement.sheet` and caches the returned instance on
/// the element wrapper.
pub fn style_element_sheet(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    node: NodeId,
    owner: Value,
) -> OpResult<Value> {
    if !matches!(realm.session.borrow().document().kind(node), Ok(NodeKind::Element { name, .. }) if name == "style")
    {
        return Err(OpError::new(
            "TypeError",
            "sheet is only available on a style element",
        ));
    }
    if let Some(registry) = registry_for_realm(ctx, realm) {
        if let Some(sheet) = registry
            .sheets
            .borrow()
            .get(&node)
            .and_then(WeakValue::upgrade)
        {
            return Ok(sheet);
        }
    }
    let sheet = ctx.new_instance(DomCssStyleSheet {
        realm: realm.clone(),
        source: SheetSource::Element(node),
        owner,
    });
    if let Some(registry) = registry_for_realm(ctx, realm) {
        if let Some(weak) = ctx.weak_value(&sheet) {
            registry.sheets.borrow_mut().insert(node, weak);
        }
    }
    Ok(sheet)
}

/// Return the document's live `StyleSheetList`. Entries are discovered from
/// the current tree on each access, in document order, and share the cached
/// CSSStyleSheet wrapper returned by `HTMLStyleElement.sheet`.
pub fn document_style_sheets(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    document: NodeId,
    owner: Value,
) -> Value {
    ctx.new_instance(DomStyleSheetList {
        realm: realm.clone(),
        document,
        owner,
    })
}

fn style_elements(realm: &DomRealm, root: NodeId) -> Vec<NodeId> {
    let session = realm.session.borrow();
    let document = session.document();
    let mut result = Vec::new();
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        let mut children = Vec::new();
        let mut child = document.first_child(node).ok().flatten();
        while let Some(id) = child {
            children.push(id);
            child = document.next_sibling(id).ok().flatten();
        }
        pending.extend(children.iter().rev().copied());
        if matches!(document.kind(node), Ok(NodeKind::Element { name, .. }) if name == "style") {
            result.push(node);
        }
    }
    result
}

#[lumen_bind::methods]
impl DomCssStyleSheet {
    #[constructor]
    fn new(ctx: &mut Ctx) -> OpResult<Self> {
        let registry = RealmServices::<MediaQueryRegistry>::current(ctx)
            .ok_or_else(|| OpError::new("InvalidStateError", "CSSOM is not installed"))?;
        let realm = registry
            .realm
            .upgrade()
            .ok_or_else(|| OpError::new("InvalidStateError", "document realm was released"))?;
        Ok(Self {
            realm: realm.clone(),
            source: SheetSource::Constructed(Rc::new(ConstructedSheetData {
                realm: Rc::downgrade(&realm),
                registry: Rc::downgrade(&registry),
                text: RefCell::new(String::new()),
            })),
            owner: Value::Null,
        })
    }

    #[getter]
    fn css_rules(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        Ok(ctx.new_instance(DomCssRuleList {
            realm: self.realm.clone(),
            source: self.source.clone(),
            owner: this.0,
        }))
    }

    #[getter(name = "ownerNode")]
    fn owner_node(&self) -> Value {
        self.owner.clone()
    }

    #[getter(name = "parentStyleSheet")]
    fn parent_style_sheet(&self) -> Value {
        Value::Null
    }

    #[getter]
    fn href(&self) -> Value {
        Value::Null
    }

    #[method(coerce)]
    fn insert_rule(&self, rule: &str, index: Option<usize>) -> OpResult<usize> {
        let mut sheet = source_sheet(&self.realm, &self.source)?;
        let rules = sheet.css_rules().map_err(css_error)?;
        let position = index.unwrap_or(rules.len());
        sheet.insert_rule(rule, position).map_err(css_error)?;
        write_source_text(&self.realm, &self.source, sheet.css_text())?;
        Ok(position)
    }

    fn delete_rule(&self, index: usize) -> OpResult<()> {
        let mut sheet = source_sheet(&self.realm, &self.source)?;
        sheet.delete_rule(index).map_err(css_error)?;
        write_source_text(&self.realm, &self.source, sheet.css_text())
    }

    #[method(coerce)]
    fn replace_sync(&self, text: &str) -> OpResult<()> {
        if !matches!(&self.source, SheetSource::Constructed(_)) {
            return Err(OpError::new(
                "NotAllowedError",
                "replaceSync requires a constructed stylesheet",
            ));
        }
        let mut sheet = source_sheet(&self.realm, &self.source)?;
        sheet.replace(text).map_err(css_error)?;
        write_source_text(&self.realm, &self.source, sheet.css_text())
    }

    #[method(coerce)]
    fn replace(&self, this: This<Value>, text: &str) -> Promise<Value> {
        Promise::ready(self.replace_sync(text).map(|()| this.0))
    }
}

#[lumen_bind::methods]
impl DomCssRuleList {
    #[proto(len)]
    fn length(&self) -> OpResult<usize> {
        Ok(source_sheet(&self.realm, &self.source)?
            .css_rules()
            .map_err(css_error)?
            .len())
    }

    #[proto(getitem)]
    fn indexed(&self, ctx: &mut Ctx, index: usize) -> OpResult<Value> {
        let rules = source_sheet(&self.realm, &self.source)?
            .css_rules()
            .map_err(css_error)?;
        let Some(rule) = rules.get(index) else {
            return Ok(Value::Undefined);
        };
        let base = DomCssRule {
            realm: self.realm.clone(),
            source: self.source.clone(),
            index,
            owner: self.owner.clone(),
        };
        Ok(if rule.selector_text.is_some() {
            ctx.new_instance(DomCssStyleRule { base })
        } else {
            ctx.new_instance(base)
        })
    }

    fn item(&self, ctx: &mut Ctx, index: usize) -> OpResult<Value> {
        self.indexed(ctx, index)
    }
}

#[lumen_bind::methods]
impl DomStyleSheetList {
    #[proto(len)]
    fn length(&self) -> usize {
        style_elements(&self.realm, self.document).len()
    }

    #[proto(getitem)]
    fn indexed(&self, ctx: &mut Ctx, index: usize) -> OpResult<Value> {
        let Some(node) = style_elements(&self.realm, self.document)
            .get(index)
            .copied()
        else {
            return Ok(Value::Undefined);
        };
        let owner = self.realm.wrap(ctx, node);
        style_element_sheet(ctx, &self.realm, node, owner)
    }

    fn item(&self, ctx: &mut Ctx, index: usize) -> OpResult<Value> {
        self.indexed(ctx, index)
    }
}

#[lumen_bind::methods]
impl DomCssRule {
    #[getter(name = "cssText")]
    fn css_text(&self) -> OpResult<String> {
        source_sheet(&self.realm, &self.source)?
            .css_rules()
            .map_err(css_error)?
            .get(self.index)
            .map(|rule| rule.css_text.clone())
            .ok_or_else(|| OpError::new("InvalidStateError", "stylesheet rule was removed"))
    }

    #[getter(name = "selectorText")]
    fn selector_text(&self) -> OpResult<Option<String>> {
        Ok(source_sheet(&self.realm, &self.source)?
            .css_rules()
            .map_err(css_error)?
            .get(self.index)
            .and_then(|rule| rule.selector_text.clone()))
    }

    #[getter]
    fn style(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        let rule = source_sheet(&self.realm, &self.source)?
            .css_rules()
            .map_err(css_error)?
            .get(self.index)
            .cloned()
            .ok_or_else(|| OpError::new("InvalidStateError", "stylesheet rule was removed"))?;
        if rule.selector_text.is_none() {
            return Err(OpError::new(
                "InvalidStateError",
                "rule has no style declaration",
            ));
        }
        Ok(ctx.new_instance(DomCssRuleStyle {
            base: DomStyle {
                realm: self.realm.clone(),
                node: source_dom_node(&self.realm, &self.source),
                computed: false,
                _owner: this.0.clone(),
            },
            realm: self.realm.clone(),
            source: self.source.clone(),
            index: self.index,
            owner: this.0,
        }))
    }
}

#[lumen_bind::methods]
impl DomCssStyleRule {
    #[getter(name = "cssText")]
    fn css_text(&self) -> OpResult<String> {
        self.base.css_text()
    }
    #[getter(name = "selectorText")]
    fn selector_text(&self) -> OpResult<Option<String>> {
        self.base.selector_text()
    }
    #[getter]
    fn style(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        self.base.style(ctx, this)
    }
}

#[lumen_bind::methods]
impl DomCssRuleStyle {
    #[getter(name = "cssText")]
    fn css_text(&self) -> OpResult<String> {
        let sheet = source_sheet(&self.realm, &self.source)?;
        Ok(sheet
            .css_rules()
            .map_err(css_error)?
            .get(self.index)
            .ok_or_else(|| OpError::new("InvalidStateError", "stylesheet rule was removed"))?
            .style
            .css_text()
            .to_owned())
    }

    #[setter(name = "cssText", coerce)]
    fn set_css_text(&self, value: &str) -> OpResult<()> {
        let mut sheet = source_sheet(&self.realm, &self.source)?;
        sheet
            .set_style_rule_declarations(self.index, value)
            .map_err(css_error)?;
        write_source_text(&self.realm, &self.source, sheet.css_text())
    }

    fn get_property_value(&self, name: &str) -> OpResult<String> {
        let sheet = source_sheet(&self.realm, &self.source)?;
        Ok(sheet
            .css_rules()
            .map_err(css_error)?
            .get(self.index)
            .ok_or_else(|| OpError::new("InvalidStateError", "stylesheet rule was removed"))?
            .style
            .get_property_value(name)
            .map_err(css_error)?
            .map_or(String::new(), |(value, _)| value))
    }

    fn get_property_priority(&self, name: &str) -> OpResult<String> {
        let sheet = source_sheet(&self.realm, &self.source)?;
        Ok(if sheet
            .css_rules()
            .map_err(css_error)?
            .get(self.index)
            .ok_or_else(|| OpError::new("InvalidStateError", "stylesheet rule was removed"))?
            .style
            .get_property_value(name)
            .map_err(css_error)?
            .is_some_and(|(_, important)| important)
        {
            "important"
        } else {
            ""
        }
        .into())
    }

    #[method(coerce)]
    fn set_property(&self, name: &str, value: &str, priority: Option<&str>) -> OpResult<()> {
        let mut sheet = source_sheet(&self.realm, &self.source)?;
        let mut rules = sheet.css_rules().map_err(css_error)?;
        let rule = rules
            .get_mut(self.index)
            .ok_or_else(|| OpError::new("InvalidStateError", "stylesheet rule was removed"))?;
        if !value.is_empty() && !css::supports_declaration(name, value) {
            return Ok(());
        }
        let priority = priority.unwrap_or("");
        if !priority.is_empty() && !priority.eq_ignore_ascii_case("important") {
            return Ok(());
        }
        let text = css::set_declaration(rule.style.css_text(), name, value, !priority.is_empty())
            .map_err(css_error)?;
        sheet
            .set_style_rule_declarations(self.index, &text)
            .map_err(css_error)?;
        write_source_text(&self.realm, &self.source, sheet.css_text())
    }

    fn remove_property(&self, name: &str) -> OpResult<String> {
        let old = self.get_property_value(name)?;
        self.set_property(name, "", None)?;
        Ok(old)
    }

    #[getter]
    fn width(&self) -> OpResult<String> {
        self.get_property_value("width")
    }
    #[setter]
    fn set_width(&self, value: &str) -> OpResult<()> {
        self.set_property("width", value, None)
    }
    #[getter]
    fn height(&self) -> OpResult<String> {
        self.get_property_value("height")
    }
    #[setter]
    fn set_height(&self, value: &str) -> OpResult<()> {
        self.set_property("height", value, None)
    }
    #[getter]
    fn color(&self) -> OpResult<String> {
        self.get_property_value("color")
    }
    #[setter]
    fn set_color(&self, value: &str) -> OpResult<()> {
        self.set_property("color", value, None)
    }
    #[getter]
    fn display(&self) -> OpResult<String> {
        self.get_property_value("display")
    }
    #[setter]
    fn set_display(&self, value: &str) -> OpResult<()> {
        self.set_property("display", value, None)
    }
    #[getter]
    fn background_color(&self) -> OpResult<String> {
        self.get_property_value("background-color")
    }
    #[setter]
    fn set_background_color(&self, value: &str) -> OpResult<()> {
        self.set_property("background-color", value, None)
    }
    #[getter]
    fn font_size(&self) -> OpResult<String> {
        self.get_property_value("font-size")
    }
    #[setter]
    fn set_font_size(&self, value: &str) -> OpResult<()> {
        self.set_property("font-size", value, None)
    }
    #[getter]
    fn font_family(&self) -> OpResult<String> {
        self.get_property_value("font-family")
    }
    #[setter]
    fn set_font_family(&self, value: &str) -> OpResult<()> {
        self.set_property("font-family", value, None)
    }
    #[getter]
    fn font_weight(&self) -> OpResult<String> {
        self.get_property_value("font-weight")
    }
    #[setter]
    fn set_font_weight(&self, value: &str) -> OpResult<()> {
        self.set_property("font-weight", value, None)
    }
    #[getter]
    fn font_style(&self) -> OpResult<String> {
        self.get_property_value("font-style")
    }
    #[setter]
    fn set_font_style(&self, value: &str) -> OpResult<()> {
        self.set_property("font-style", value, None)
    }
    #[getter]
    fn font(&self) -> OpResult<String> {
        self.get_property_value("font")
    }
    #[setter]
    fn set_font(&self, value: &str) -> OpResult<()> {
        self.set_property("font", value, None)
    }
}

/// Validate that a proposed rule parses as a stylesheet before callers mutate
/// the owning style element. Returning an error leaves the original unchanged.
pub fn validate_stylesheet(text: &str) -> Result<(), css::CssError> {
    css::parse(text).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supports_uses_the_renderer_property_registry() {
        assert!(css::supports_property_value("display", "grid"));
        assert!(css::supports_property_value("width", "calc(50% - 2px)"));
        assert!(!css::supports_property_value("display", "spaceship"));
        assert!(!css::supports_property_value("unknown-property", "1px"));
    }

    #[test]
    fn declaration_cssom_preserves_importance_and_rejects_invalid_values() {
        let mut declaration = CssDeclaration::parse("color: red !important; width: 10px");
        declaration.set_property("width", "invalid", false).unwrap();
        assert_eq!(
            declaration.get_property_value("width").unwrap(),
            Some(("10px".into(), false))
        );
        declaration.set_property("width", "12px", true).unwrap();
        assert_eq!(
            declaration.get_property_value("width").unwrap(),
            Some(("12px".into(), true))
        );
    }

    #[test]
    fn stylesheet_statement_rules_can_precede_blocks_and_be_removed() {
        let mut sheet = CssStyleSheetText::parse("").unwrap();
        sheet
            .insert_rule(
                "@import url('data:text/css,div { background: red !important }');",
                0,
            )
            .unwrap();
        sheet.insert_rule("div { background: green }", 1).unwrap();
        let rules = sheet.css_rules().unwrap();
        assert_eq!(rules.len(), 2);
        assert!(rules[0].selector_text.is_none());
        assert_eq!(rules[1].selector_text.as_deref(), Some("div"));
        sheet.delete_rule(0).unwrap();
        assert_eq!(sheet.css_rules().unwrap().len(), 1);
        assert!(!sheet.css_text().contains("@import"));
        sheet.insert_rule("@layer base, override;", 0).unwrap();
        assert_eq!(sheet.css_rules().unwrap().len(), 2);
        sheet.delete_rule(0).unwrap();
        assert_eq!(
            sheet.css_rules().unwrap()[0].selector_text.as_deref(),
            Some("div")
        );
    }

    #[test]
    fn stylesheet_rule_edits_validate_before_replacement() {
        let mut sheet =
            CssStyleSheetText::parse(".a { color: red } @media screen { .b { width: 2px } }")
                .unwrap();
        let rules = sheet.css_rules().unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].selector_text.as_deref(), Some(".a"));
        assert_eq!(
            rules[0].style.get_property_value("color").unwrap(),
            Some(("red".into(), false))
        );
        assert_eq!(rules[1].nested.len(), 1);
        let before = sheet.css_text().to_owned();
        assert!(sheet.insert_rule(".broken { width: nope", 1).is_err());
        assert_eq!(sheet.css_text(), before);
        sheet.insert_rule(".c { display: grid }", 1).unwrap();
        assert_eq!(
            sheet.css_rules().unwrap()[1].selector_text.as_deref(),
            Some(".c")
        );
        sheet.delete_rule(1).unwrap();
        assert_eq!(sheet.css_rules().unwrap().len(), 2);

        let supports =
            CssStyleSheetText::parse("@supports (display: grid) { .supported { color: red } }")
                .unwrap();
        let rules = supports.css_rules().unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].nested.len(), 1);
        assert_eq!(
            rules[0].nested[0].selector_text.as_deref(),
            Some(".supported")
        );
    }

    #[test]
    fn media_query_list_reuses_cascade_media_matching() {
        let environment = MediaEnvironment {
            width: 800.0,
            height: 600.0,
            resolution: 2.0,
            print: false,
        };
        assert!(MediaQueryList::new("screen and (min-width: 700px)", environment).matches());
        assert!(!MediaQueryList::new("print, (max-width: 500px)", environment).matches());
    }

    #[test]
    fn public_css_supports_and_media_query_events_are_live_and_reentrant() {
        use lumen::Engine;

        let mut engine = Engine::new();
        let realm = crate::install(engine.ctx(), "<main></main>", 64).unwrap();
        realm
            .session
            .borrow_mut()
            .set_media_environment(MediaEnvironment {
                width: 600.0,
                height: 400.0,
                resolution: 1.0,
                print: false,
            })
            .unwrap();
        let supports = engine
            .eval_value("CSS.supports('display', 'grid') && !CSS.supports('display', 'spaceship')")
            .unwrap()
            .ok()
            .unwrap();
        assert!(
            matches!(supports, Value::Bool(true)),
            "CSS.supports rejected supported declarations"
        );
        for (name, source) in [
            ("conditionText declaration", "CSS.supports('color: red')"),
            (
                "conditionText logical condition",
                "CSS.supports('(display: grid) and (width: 1px)')",
            ),
            (
                "selector() valid selector",
                "CSS.supports('selector(div > .item)')",
            ),
            (
                "selector() rejects selector lists",
                "!CSS.supports('selector(div, .item)')",
            ),
            (
                "two-argument value string coercion",
                "CSS.supports('display', { toString() { return 'grid'; } })",
            ),
            (
                "required first argument explicit undefined coercion",
                "!CSS.supports(undefined)",
            ),
            (
                "explicit null is stringified",
                "CSS.supports('--explicit', null)",
            ),
            (
                "explicit undefined is stringified",
                "CSS.supports('--explicit', undefined)",
            ),
            (
                "explicit empty custom-property value",
                "CSS.supports('--explicit', '')",
            ),
            (
                "omitted second argument selects conditionText",
                "!CSS.supports('--omitted')",
            ),
        ] {
            let result = engine.eval_value(source).unwrap().ok();
            let passed = matches!(result, Some(Value::Bool(true)));
            assert!(passed, "CSS.supports {name} failed");
        }
        let coercion_throws = engine
            .eval_value(
                "let threw = false; \
                 try { CSS.supports('display', { toString() { throw new Error('coerce'); } }); } \
                 catch (error) { threw = error.message === 'coerce'; } \
                 threw",
            )
            .unwrap()
            .ok()
            .unwrap();
        assert!(
            matches!(coercion_throws, Value::Bool(true)),
            "CSS.supports swallowed a throwing string conversion"
        );
        let missing_required_throws = engine
            .eval_value(
                "let missingSupportsTypeError = false; \
                 try { CSS.supports(); } \
                 catch (error) { missingSupportsTypeError = error instanceof TypeError; } \
                 missingSupportsTypeError",
            )
            .unwrap()
            .ok()
            .unwrap();
        assert!(
            matches!(missing_required_throws, Value::Bool(true)),
            "CSS.supports did not reject its omitted required first argument"
        );
        let dom_stringified_undefined = engine
            .eval_value("document.getElementById(undefined) === null")
            .unwrap()
            .ok()
            .unwrap();
        assert!(
            matches!(dom_stringified_undefined, Value::Bool(true)),
            "Document.getElementById did not coerce an explicit undefined DOMString"
        );
        let optional_default = engine
            .eval_value(
                "const omittedTitleDocument = document.implementation.createHTMLDocument(); \
                 const explicitUndefinedTitleDocument = document.implementation.createHTMLDocument(undefined); \
                 omittedTitleDocument.getElementsByTagName('title').length === 0 && \
                 explicitUndefinedTitleDocument.getElementsByTagName('title').length === 0",
            )
            .unwrap()
            .ok()
            .unwrap();
        assert!(
            matches!(optional_default, Value::Bool(true)),
            "explicit undefined changed an optional/defaulted native argument"
        );
        let initial_match = engine.eval_value(
            "window.__changes = 0; window.__m = matchMedia('(min-width: 700px)'); !window.__m.matches"
        ).unwrap().ok().unwrap();
        assert!(
            matches!(initial_match, Value::Bool(true)),
            "initial matchMedia did not start false"
        );
        engine.eval_value(
            "window.__m.addEventListener('change', e => { window.__changes++; matchMedia('print'); })"
        ).unwrap().ok().unwrap();

        realm
            .session
            .borrow_mut()
            .set_media_environment(MediaEnvironment {
                width: 800.0,
                height: 400.0,
                resolution: 1.0,
                print: false,
            })
            .unwrap();
        crate::notify_media(engine.ctx(), &realm).unwrap();
        let result = engine
            .eval_value("window.__m.matches && window.__changes === 1")
            .unwrap()
            .ok()
            .unwrap();
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn media_query_registries_and_change_events_are_realm_scoped() {
        use lumen::Engine;

        let mut engine = Engine::new();
        let ctx = engine.ctx();
        let parent_realm = crate::install(ctx, "<main></main>", 64).unwrap();
        let parent_global = ctx.global_object();
        parent_realm
            .session
            .borrow_mut()
            .set_media_environment(MediaEnvironment {
                width: 500.0,
                height: 400.0,
                resolution: 1.0,
                print: false,
            })
            .unwrap();
        assert!(matches!(
            ctx.eval_in_realm(
                &parent_global,
                "window.parentList = matchMedia('(min-width: 700px)'); \
                 window.parentChanges = 0; \
                 parentList.addEventListener('change', () => parentChanges++); \
                 !parentList.matches",
            ),
            Ok(Value::Bool(true))
        ));

        let child_handle = ctx.create_host_realm();
        let child_global = child_handle.global();
        let child_realm = ctx
            .with_host_realm(&child_handle, |ctx| {
                let realm = crate::install(ctx, "<main></main>", 64).unwrap();
                realm
                    .session
                    .borrow_mut()
                    .set_media_environment(MediaEnvironment {
                        width: 400.0,
                        height: 300.0,
                        resolution: 1.0,
                        print: false,
                    })
                    .unwrap();
                assert!(matches!(
                    ctx.eval_in_realm(
                        &child_global,
                        "window.childList = matchMedia('(min-width: 700px)'); \
                         window.childChanges = 0; \
                         childList.addEventListener('change', () => childChanges++); \
                         !childList.matches",
                    ),
                    Ok(Value::Bool(true))
                ));
                realm
            })
            .expect("install the child browsing-context realm");

        // A child viewport update must only dispatch through the child's native
        // MQL and EventTarget. The parent registry remains installed in OpState.
        child_realm
            .session
            .borrow_mut()
            .set_media_environment(MediaEnvironment {
                width: 800.0,
                height: 300.0,
                resolution: 1.0,
                print: false,
            })
            .unwrap();
        ctx.with_host_realm(&child_handle, |ctx| {
            crate::notify_media(ctx, &child_realm).unwrap();
        })
        .expect("dispatch in the child browsing-context realm");
        assert!(matches!(
            ctx.eval_in_realm(&child_global, "childList.matches && childChanges === 1"),
            Ok(Value::Bool(true))
        ));
        assert!(matches!(
            ctx.eval_in_realm(&parent_global, "!parentList.matches && parentChanges === 0"),
            Ok(Value::Bool(true))
        ));

        parent_realm
            .session
            .borrow_mut()
            .set_media_environment(MediaEnvironment {
                width: 900.0,
                height: 400.0,
                resolution: 1.0,
                print: false,
            })
            .unwrap();
        crate::notify_media(ctx, &parent_realm).unwrap();
        assert!(matches!(
            ctx.eval_in_realm(&parent_global, "parentList.matches && parentChanges === 1"),
            Ok(Value::Bool(true))
        ));
        assert!(matches!(
            ctx.eval_in_realm(&child_global, "childList.matches && childChanges === 1"),
            Ok(Value::Bool(true))
        ));
    }

    #[test]
    fn constructed_stylesheets_are_adopted_scoped_and_mutate_the_real_cascade() {
        use lumen::Engine;

        let mut engine = Engine::new();
        let realm = crate::install(
            engine.ctx(),
            "<div id=outside></div><div id=host></div>",
            128,
        )
        .unwrap();
        let result = engine.eval_value(
            "(() => { const sheet = new CSSStyleSheet(); sheet.replaceSync('div { width: 12px }'); document.adoptedStyleSheets = [sheet]; const host = document.getElementById('host'); const root = host.attachShadow({mode:'open'}); root.innerHTML = '<p></p>'; const shadowSheet = new CSSStyleSheet(); shadowSheet.replaceSync('p { height: 9px }'); root.adoptedStyleSheets = [shadowSheet]; window.__promiseResult = false; const promise = shadowSheet.replace('p { height: 11px }').then(result => window.__promiseResult = result === shadowSheet); return document.adoptedStyleSheets[0] === sheet && root.adoptedStyleSheets[0] === shadowSheet && sheet.cssRules.length === 1 && sheet.ownerNode === null && promise instanceof Promise; })()"
        ).unwrap().ok().unwrap();
        assert!(matches!(result, Value::Bool(true)));
        engine.ctx().drain_microtasks_for_host();
        let resolved = engine
            .eval_value("window.__promiseResult")
            .unwrap()
            .ok()
            .unwrap();
        assert!(
            matches!(resolved, Value::Bool(true)),
            "replace() did not fulfill with its stylesheet"
        );
        realm.with_session(|session| {
            let document = session.document();
            let outside = selector::query_selector(document, document.root(), "#outside")
                .unwrap()
                .unwrap();
            let host = selector::query_selector(document, document.root(), "#host")
                .unwrap()
                .unwrap();
            let shadow = document.shadow_root(host).unwrap().unwrap();
            let paragraph = selector::query_selector(document, shadow, "p")
                .unwrap()
                .unwrap();
            assert_eq!(session.computed_style(outside).unwrap().width, Some(12.0));
            assert_eq!(
                session.computed_style(paragraph).unwrap().height,
                Some(11.0)
            );
        });
    }
}
