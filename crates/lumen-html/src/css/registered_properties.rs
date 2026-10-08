//! Document-scoped registered custom properties, shared by DOM adapters and
//! stylesheet cascade, animation sampling and Typed OM adapters.
use super::*;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegisteredCustomProperty {
    pub name: String,
    pub inherits: bool,
    pub initial_value: Option<String>,
    pub syntax: String,
    /// Base URL frozen when this registration was created.
    pub source_url: Option<Arc<str>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistrationError {
    InvalidName,
    Duplicate,
    InvalidSyntax,
    UnsupportedSyntax,
    InvalidInitialValue,
    Capacity,
}

/// Recognize the bounded registered-property descriptor grammar.
pub fn valid_typed_syntax(input: &str) -> bool {
    input.split('|').all(|component| {
        let component = component.trim();
        if component.strip_suffix('+').or_else(|| component.strip_suffix('#')) == Some("<transform-list>") { return false; }
        let component = component
            .strip_suffix('+')
            .or_else(|| component.strip_suffix('#'))
            .unwrap_or(component);
        if let Some(name) = component
            .strip_prefix('<')
            .and_then(|value| value.strip_suffix('>'))
        {
            matches!(
                name,
                "length"
                    | "number"
                    | "percentage"
                    | "length-percentage"
                    | "integer"
                    | "angle"
                    | "time"
                    | "resolution"
                    | "color"
                    | "image"
                    | "url"
                    | "transform-function"
                    | "transform-list"
                    | "custom-ident"
                    | "string"
            )
        } else {
            let mut at = 0;
            consume_selector_identifier(component, &mut at).is_some_and(|name| {
                at == component.len()
                    && !matches!(
                        name.to_ascii_lowercase().as_str(),
                        "initial" | "inherit" | "unset" | "revert" | "revert-layer" | "default"
                    )
            })
        }
    })
}

/// Match the registered numeric grammar using the canonical bounded math AST.
/// Percent hints are admitted only by <length-percentage>; <length> cannot
/// silently acquire a percentage basis from a dimensionally compatible sum.
fn numeric_expression(syntax: &str, value: &str) -> Option<typed_numeric::NumericExpression> {
    use typed_numeric::{NumericDimension as D, NumericType as T, NumericUnit as U};
    let expression = typed_numeric::parse_numeric_expression(value)?;
    let kind = expression.numeric_type()?;
    let mut plain = kind;
    plain.percent_hint = None;
    let accepted = match syntax {
        "<length>" => {
            kind == T::from_unit(U::Px)
                || !math_function(value)
                    && expression
                        .single_numeric_value()
                        .is_some_and(|v| v.unit == U::Number && v.value == 0.0)
        }
        "<length-percentage>" => {
            plain == T::from_unit(U::Px) && kind.percent_hint.is_none_or(|hint| hint == D::Length)
                || kind == T::from_unit(U::Percent)
                || !math_function(value)
                    && expression
                        .single_numeric_value()
                        .is_some_and(|v| v.unit == U::Number && v.value == 0.0)
        }
        "<number>" | "<integer>" => kind == T::default(),
        "<percentage>" => kind == T::from_unit(U::Percent),
        "<angle>" => kind == T::from_unit(U::Deg),
        "<time>" => kind == T::from_unit(U::S),
        "<resolution>" => kind == T::from_unit(U::Dppx),
        _ => false,
    };
    if !accepted {
        return None;
    }
    if syntax == "<integer>"
        && !math_function(value)
        && expression
            .single_numeric_value()
            .is_none_or(|v| v.value.fract() != 0.0)
    {
        return None;
    }
    Some(expression)
}

fn items<'a>(syntax: &str, value: &'a str) -> Option<Vec<&'a str>> {
    if syntax.ends_with('#') {
        top_level_split(value, b',', 64)
    } else if syntax.ends_with('+') {
        components(value)
    } else {
        Some(alloc::vec![value])
    }
}

fn component_syntax(syntax: &str) -> &str {
    syntax
        .strip_suffix('+')
        .or_else(|| syntax.strip_suffix('#'))
        .unwrap_or(syntax)
        .trim()
}

/// Syntax validation does not resolve font, viewport, query or percentage units.
/// Those are computed only after the declaring element's context is established.
pub fn accepts_value(syntax: &str, value: &str) -> bool {
    if syntax.trim() == "*" {
        return supports_property_value("--registered-value", value);
    }
    syntax.split('|').any(|syntax| {
        let syntax = syntax.trim();
        let Some(items) = items(syntax, value) else {
            return false;
        };
        let syntax = component_syntax(syntax);
        !items.is_empty()
            && items.into_iter().all(|value| match syntax {
                "<color>" => color_with_context(value, Style::initial().color).is_some(),
                "<string>" => css_string(value).is_some(),
                "<url>" => background_url(value).is_some(),
                "<image>" => !decoded_css_keyword(value,"none") && background_image_part_with_source(value,&legacy_source_color(Style::initial().color),Some(static_length_context()),0,false,ContainerUnitContext::default()).is_some(),
                "<transform-function>" => typed_transforms::validated_function_tokens(value).is_some_and(|functions| functions.len()==1),
                "<transform-list>" => typed_transforms::validated_function_tokens(value).is_some_and(|functions| !functions.is_empty()),
                "<custom-ident>" => {
                    let mut at = 0;
                    consume_selector_identifier(value.trim(), &mut at).is_some_and(|name| at == value.trim().len()
                        && !matches!(name.to_ascii_lowercase().as_str(), "initial" | "inherit" | "unset" | "revert" | "revert-layer" | "default"))
                }
                literal if !literal.starts_with('<') => {
                    let mut at = 0;
                    consume_selector_identifier(value.trim(), &mut at)
                        .is_some_and(|name| {
                            let mut syntax_at = 0;
                            at == value.trim().len() && consume_selector_identifier(literal, &mut syntax_at)
                                .is_some_and(|expected| syntax_at == literal.len() && name == expected)
                        })
                }
                _ => numeric_expression(syntax, value).is_some(),
            })
    })
}

/// Computed Typed OM uses the matched registered list grammar, rather than
/// returning the original unparsed custom-property token sequence.
pub fn computed_value_items<'a>(syntax: &str, value: &'a str) -> Option<Vec<&'a str>> {
    if syntax.trim() == "*" {
        return None;
    }
    for alternative in syntax.split('|') {
        if accepts_value(alternative, value) {
            return items(alternative.trim(), value);
        }
    }
    None
}

/// Registered lists combine corresponding computed components. A changed
/// component type or list length uses discrete interpolation, rather than the
/// least common multiple repetition used by ordinary repeatable list properties.
#[derive(Clone,Copy,Debug)]
pub enum ComputedValueOperation { Interpolate(f64), Add, Accumulate(f64) }

pub fn combine_computed_values(syntax:&str,from:&str,to:&str,operation:ComputedValueOperation)->Option<String> {
    combine_computed_values_with_reference(syntax,from,to,operation,None)
}

/// Computed values retain percentages. Used-value consumers can supply their
/// actual transform reference dimensions through this same combination path.
pub fn combine_computed_values_with_reference(syntax:&str,from:&str,to:&str,operation:ComputedValueOperation,reference:Option<[f64;2]>)->Option<String> {
    if syntax.trim()=="*"{return None;}
    // Each endpoint keeps the type selected by its first matching clause.
    // Searching for a later common clause would reparse an already computed
    // value and turn distinct parsed types into an interpolable pair.
    let left_type=syntax.split('|').map(str::trim).find(|alternative|accepts_value(alternative,from))?;
    let right_type=syntax.split('|').map(str::trim).find(|alternative|accepts_value(alternative,to))?;
    if left_type!=right_type{return None;}
    {
        let alternative=left_type;
        if matches!(alternative,"<transform-list>"|"<transform-function>"|"<transform-function>+") {
            return typed_transforms::combine_computed(from,to,operation,alternative!="<transform-function>",reference);
        }
        let left=items(alternative,from)?;let right=items(alternative,to)?;
        if left.len()!=right.len(){return None;}
        // CSS Values §3: interpolation at 0 and 1 produces the computed
        // endpoint itself, including its color space and calculation tree.
        if let ComputedValueOperation::Interpolate(progress)=operation {
            if progress==0.0{return Some(from.to_string());}
            if progress==1.0{return Some(to.to_string());}
        }
        let component=component_syntax(alternative);
        let mut result=String::new();
        for (left_value,right_value) in left.into_iter().zip(right) {
            let computed=if numeric_expression(component,left_value).is_some()&&numeric_expression(component,right_value).is_some(){
                let raw=match operation {
                    ComputedValueOperation::Interpolate(progress)=>crate::animation::interpolate_numeric(left_value,right_value,progress),
                    ComputedValueOperation::Add=>crate::animation::add_numeric_values(left_value,right_value,None),
                    ComputedValueOperation::Accumulate(iterations)=>crate::animation::combine_numeric_values(&[(left_value,iterations),(right_value,1.0)]),
                }?;
                if component=="<integer>" {
                    let mut expression=typed_numeric::parse_numeric_expression(&raw)?;expression.simplify_absolute_units();
                    let value=expression.single_numeric_value()?;
                    typed_numeric::serialize_numeric_value(libm::floor(value.value+0.5),typed_numeric::NumericUnit::Number)
                }else{raw}
            }else if component=="<transform-function>" {
                typed_transforms::combine_computed(left_value,right_value,operation,false,reference)?
            }else if component=="<color>" {
                match operation {
                    ComputedValueOperation::Interpolate(progress)=>{
                        if !progress.is_finite(){return None;}
                        let current=lumen_common::color::Color::rgba8([0,0,0,255]);
                        let left_value=parse_animation_unclipped_color(left_value,current)?;let right_value=parse_animation_unclipped_color(right_value,current)?;
                        interpolate_source_colors(&left_value,&right_value,progress as f32).serialize()?
                    },
                    // CSS Values defines color addition and accumulation as V_B.
                    ComputedValueOperation::Add|ComputedValueOperation::Accumulate(_)=>right_value.to_string(),
                }
            }else{return None;};
            let separator=if result.is_empty(){""}else if alternative.ends_with('#'){", "}else{" "};
            let length=result.len().checked_add(separator.len())?.checked_add(computed.len())?;
            if length>MAX_VARIABLE_BYTES{return None;}
            result.try_reserve(separator.len()+computed.len()).ok()?;result.push_str(separator);result.push_str(&computed);
        }
        Some(result)
    }
}

/// Registered numeric dimensions compute to canonical units. Percentages and
/// nonlinear percentage math retain their basis until the consuming property.
fn compute_component(
    syntax: &str,
    value: &str,
    context: LengthContext,
    query: ContainerUnitContext,
    foreground: &SourceColor,
    base: Option<&str>,
) -> Option<String> {
    use typed_numeric::NumericUnit as U;
    if syntax == "<color>" {
        let source = color_values::endpoint_with_current(
            value,
            0,
            foreground.value,
            context,
            query,
        )?;
        return color_values::serialize(source, color_values::color_function_in_scheme(value,context.viewport.color_schemes.page().scheme,foreground.color_function));
    }
    if syntax == "<url>" { let url=background_url(value)?;let url=resolve_css_url(&url,base)?;return Some(alloc::format!("url({})",serialize_css_string(&url))); }
    if syntax == "<image>" {
        if !accepts_value(syntax,value) {return None;}
        return image_source::computed(value,context,query,foreground,base);
    }
    if matches!(syntax,"<transform-function>"|"<transform-list>") {
        if !accepts_value(syntax,value) {return None;}
        return typed_transforms::computed(value,|value|if value.unit.dimension()==typed_numeric::NumericDimension::Length {typed_numeric::computed_numeric_value(value,context,query)}else{Some(value)});
    }
    if syntax == "<string>" { return css_string(value).map(|value| serialize_string(&value)); }
    if syntax == "<custom-ident>" {
        if !accepts_value(syntax, value) { return None; }
        let mut at = 0;
        return consume_selector_identifier(value.trim(), &mut at).map(|value| serialize_identifier(&value));
    }
    if !syntax.starts_with('<') {
        if !accepts_value(syntax, value) { return None; }
        let mut at = 0;
        consume_selector_identifier(syntax, &mut at).map(|value| serialize_identifier(&value))
    } else {
        let mut expression = numeric_expression(syntax, value)?;
        expression.map_numeric_values(|value|typed_numeric::computed_numeric_value(value,context,query))?;
        expression.simplify_absolute_units();
        if let Some(mut value) = expression.single_numeric_value() {
            if matches!(syntax, "<length>" | "<length-percentage>") && value.unit == U::Number {
                value.unit = U::Px;
            }
            if syntax == "<integer>" {
                value.value = libm::floor(value.value + 0.5);
            }
            if !value.value.is_finite() {
                return None;
            }
            Some(typed_numeric::serialize_numeric_value(
                value.value,
                value.unit,
            ))
        } else {
            expression.serialize()
        }
    }
}

#[cfg(test)]
pub(super) fn compute_value(syntax: &str, value: &str, context: LengthContext, query: ContainerUnitContext, foreground: Rgba) -> Option<String> {
    compute_value_precise(syntax, value, context, query, &legacy_source_color(foreground))
}

#[cfg(test)]
pub(super) fn compute_value_precise(syntax:&str,value:&str,context:LengthContext,query:ContainerUnitContext,foreground:&SourceColor)->Option<String>{
    compute_value_with_source(syntax,value,context,query,foreground,None)
}

pub(super) fn compute_value_with_source(
    syntax: &str,
    value: &str,
    context: LengthContext,
    query: ContainerUnitContext,
    foreground: &SourceColor,
    base: Option<&str>,
) -> Option<String> {
    if syntax.trim() == "*" {
        return Some(value.to_string());
    }
    for alternative in syntax.split('|') {
        let alternative = alternative.trim();
        let Some(parts) = items(alternative, value) else {
            continue;
        };
        let mut result = String::new();
        let mut valid = !parts.is_empty();
        for part in parts {
            let Some(computed) = compute_component(
                component_syntax(alternative),
                part,
                context,
                query,
                foreground,
                base,
            ) else {
                valid = false;
                break;
            };
            if !result.is_empty() {
                result.push_str(if alternative.ends_with('#') {
                    ", "
                } else {
                    " "
                });
            }
            if result
                .len()
                .checked_add(computed.len())
                .is_none_or(|size| size > MAX_VARIABLE_BYTES)
            {
                valid = false;
                break;
            }
            result.push_str(&computed);
        }
        if valid {
            return Some(result);
        }
    }
    None
}

fn has_variables(value:&str)->bool {
    component_values::parse_unparsed_value(value).is_ok_and(|components|components.iter().any(|component|matches!(component,component_values::UnparsedComponent::Variable{..})))
}

fn computationally_independent(syntax: &str, value: &str) -> bool {
    if typed_numeric::substitute_sibling_functions(value,1,1).is_none_or(|(_,dependent)|dependent){return false;}
    if color_values::uses_current_color(value) || has_variables(value) {
        return false;
    }
    syntax.split('|').any(|alternative| {
        let Some(parts) = items(alternative.trim(), value) else {
            return false;
        };
        let syntax = component_syntax(alternative.trim());
        parts.into_iter().all(|part| {
            if let Some(expression) = numeric_expression(syntax, part) {
                !expression.contains_tree_functions()&&!expression.contains_unit(|unit| !typed_numeric::computationally_independent_unit(unit))
            } else if matches!(syntax,"<transform-function>"|"<transform-list>") {
                typed_transforms::computationally_independent(part)
            } else if syntax=="<image>" {image_source::independent(part)}
            else {accepts_value(syntax, part)}
        })
    })
}

pub(super) fn font_dependency(syntax: &str, value: &str, slot: usize, root: bool) -> bool {
    syntax.split('|').any(|alternative| {
        let syntax = component_syntax(alternative.trim());
        if !matches!(syntax, "<length>" | "<length-percentage>") {
            return false;
        }
        items(alternative.trim(), value).is_some_and(|parts| {
            parts.into_iter().any(|part| {
                numeric_expression(syntax, part).is_some_and(|expression| {
                    use typed_numeric::NumericUnit::*;
                    match slot {
                        7 => {
                            expression
                                .contains_unit(|unit| matches!(unit, Em | Ex | Cap | Ch | Ic | Lh))
                                || root
                                    && expression.contains_unit(|unit| {
                                        matches!(unit, Rem | Rex | Rcap | Rch | Ric | Rlh)
                                    })
                        }
                        13 => {
                            expression.contains_unit(|unit| unit == Lh)
                                || root && expression.contains_unit(|unit| unit == Rlh)
                        }
                        _ => false,
                    }
                })
            })
        })
    })
}

/// Resolve dependencies before substitution so a registered property contributes
/// its computed serialization even through an unregistered token-list alias.
pub(super) fn resolve_computed(
    values: &mut [(String, Option<String>)],
    registrations: &[RegisteredCustomProperty],
    defaults: &[(String, Option<String>)],
    sources: &[(String, Option<Arc<str>>)],
    context: LengthContext,
    query: ContainerUnitContext,
    foreground: &SourceColor,
    sibling:(usize,usize),
) -> Option<()> {
    fn dependencies(
        components: &[component_values::UnparsedComponent],
        names: &mut Vec<String>,
    ) -> Option<()> {
        for component in components {
            if let component_values::UnparsedComponent::Variable { name, fallback } = component {
                let mut at = 0;
                let decoded =
                    consume_selector_identifier(name, &mut at).filter(|_| at == name.len())?;
                if !names.iter().any(|other| *other == decoded) {
                    if names.len() >= MAX_CUSTOM_PROPERTIES {
                        return None;
                    }
                    names.try_reserve(1).ok()?;
                    names.push(decoded);
                }
                if let Some(fallback) = fallback {
                    dependencies(fallback, names)?;
                }
            }
        }
        Some(())
    }
    fn resolve(
        at: usize,
        depth: usize,
        values: &mut [(String, Option<String>)],
        states: &mut [u8],
        registrations: &[RegisteredCustomProperty],
        defaults: &[(String, Option<String>)],
        sources: &[(String, Option<Arc<str>>)],
        context: LengthContext,
        query: ContainerUnitContext,
        foreground: &SourceColor,
        sibling:(usize,usize),
    ) -> Option<()> {
        if states[at] == 2 {
            return Some(());
        }
        if states[at] == 1 || depth >= MAX_VARIABLE_DEPTH {
            values[at].1=defaults.iter().find(|(name,_)|name==&values[at].0).and_then(|(_,value)|value.clone());
            states[at]=2;
            return Some(());
        }
        states[at] = 1;
        let raw = values[at].1.clone();
        if let Some(raw) = raw.as_deref().filter(|raw| needs_expansion(raw)) {
            let components = component_values::parse_unparsed_value(raw).ok()?;
            let mut names = Vec::new();
            dependencies(&components, &mut names)?;
            for name in names {
                if let Some(dependency) = values.iter().position(|(key, _)| *key == name) {
                    resolve(
                        dependency,
                        depth + 1,
                        values,
                        states,
                        registrations,
                        defaults,
                        sources,
                        context,
                        query,
                        foreground,
                        sibling,
                    )?;
                }
            }
        }
        let expanded = raw.as_deref().and_then(|raw| {
            if needs_expansion(raw) {
                expand_variables(raw, values, &mut Vec::new())
            } else {
                Some(raw.to_string())
            }
        });
        let registration = registrations
            .iter()
            .find(|registration| registration.name == values[at].0);
        values[at].1 = match registration {
            Some(registration) if registration.syntax.trim() != "*" => expanded
                .as_deref()
                .and_then(|value| {
                    let (value,_)=typed_numeric::substitute_sibling_functions(value,sibling.0,sibling.1)?;
                    compute_value_with_source(&registration.syntax, &value, context, query, foreground, sources.iter().find(|(name,_)| name==&values[at].0).and_then(|(_,url)|url.as_deref()))
                })
                .or_else(|| {
                    defaults
                        .iter()
                        .find(|(key, _)| *key == values[at].0)
                        .and_then(|(_, value)| value.clone())
                }),
            _ => expanded,
        };
        states[at] = 2;
        Some(())
    }
    let mut states = alloc::vec![0; values.len()];
    for at in 0..values.len() {
        resolve(
            at,
            0,
            values,
            &mut states,
            registrations,
            defaults,
            sources,
            context,
            query,
            foreground,
            sibling,
        )?;
    }
    Some(())
}

/// Parse stylesheet descriptors through the ordinary recovery/token pipeline.
/// A malformed rule is discarded without replacing an earlier valid carrier.
/// Parse all names from one property rule using the shared CSS identifier lexer.
pub fn parse_rules(prelude: &str, input: &str) -> Result<Vec<RegisteredCustomProperty>, CssError> {
    let Some(raw_names) = nesting::at_rule_tail(prelude, "@property") else { return Ok(Vec::new()); };
    let Some(raw_names) = top_level_split(raw_names.trim(), b',', MAX_CUSTOM_PROPERTIES) else { return Ok(Vec::new()); };
    if raw_names.is_empty() || raw_names.len() > MAX_CUSTOM_PROPERTIES { return Ok(Vec::new()); }
    let mut names = Vec::new();
    for raw_name in raw_names {
        let raw_name = raw_name.trim();
        let mut at = 0;
        let Some(name) = consume_selector_identifier(raw_name, &mut at).filter(|name| at == raw_name.len() && name.starts_with("--") && name.len() > 2) else { return Ok(Vec::new()); };
        if !names.contains(&name) { names.push(name); }
    }
    let mut syntax = "*".to_string();
    let mut inherits = true;
    let mut initials = Vec::new();
    let mut initial_bytes=0usize;
    for (start, end) in declaration_spans_with_recovery(input, true)? {
        let Some((name, raw)) = declaration_pair(&input[start..end]) else { continue; };
        let Some(name) = parsed_declaration_name(name) else { continue; };
        let Some(raw) = syntax::complete(raw)? else { continue; };
        if important_value(&raw).1 { continue; }
        let cleaned = if needs_expansion(&raw) { raw.into_owned() } else {
            let Some(cleaned) = expand_variables_bounded(&raw, &[], &mut Vec::new(), false, MAX_VARIABLE_BYTES) else { continue; };
            cleaned
        };
        let raw = cleaned.trim();
        match name.as_ref().to_ascii_lowercase().as_str() {
            "syntax" => if let Some(value) = css_string(raw).filter(|value| value == "*" || valid_typed_syntax(value)) { syntax = value; },
            "inherits" => match raw.to_ascii_lowercase().as_str() { "true" => inherits = true, "false" => inherits = false, _ => {} },
            "initial-value" => {
                initial_bytes=initial_bytes.checked_add(raw.len()).unwrap_or(usize::MAX);
                if initial_bytes>MAX_CSS_BYTES{return Ok(Vec::new());}
                initials.try_reserve(1).map_err(|_|syntax::error(start,"too many property descriptors"))?;
                initials.push(raw.to_string());
            },
            _ => {}
        }
    }
    // Invalid descriptors are ignored; choose the last initial which validates
    // against the winning syntax descriptor, regardless of descriptor order.
    let initial_value=initials.into_iter().rev().find(|value|value.len()<=MAX_VARIABLE_BYTES
        && accepts_value(&syntax,value) && (syntax.trim()=="*" || computationally_independent(&syntax,value)) && !has_variables(value));
    let mut parsed = Vec::new();
    for name in names {
        if register(&mut parsed, name, &syntax, inherits, initial_value.clone()).is_err() { return Ok(Vec::new()); }
        parsed.last_mut().unwrap().syntax = syntax.clone();
    }
    Ok(parsed)
}

/// CSSPropertyRule's current single-name IDL predates multi-name registration.
pub fn parse_rule(prelude: &str, input: &str) -> Result<Option<RegisteredCustomProperty>, CssError> {
    Ok(parse_rules(prelude, input)?.into_iter().next())
}

pub fn register(
    properties: &mut Vec<RegisteredCustomProperty>,
    name: String,
    syntax: &str,
    inherits: bool,
    initial_value: Option<String>,
) -> Result<(), RegistrationError> {
    if name.len() > 1024 || initial_value.as_ref().is_some_and(|value| value.len() > MAX_VARIABLE_BYTES) {
        return Err(RegistrationError::Capacity);
    }
    let mut retained = name.len().checked_add(syntax.len()).and_then(|bytes| bytes.checked_add(initial_value.as_ref().map_or(0, String::len))).ok_or(RegistrationError::Capacity)?;
    for property in properties.iter() {
        retained = retained.checked_add(property.name.len()).and_then(|bytes| bytes.checked_add(property.syntax.len()))
            .and_then(|bytes| bytes.checked_add(property.initial_value.as_ref().map_or(0, String::len))).ok_or(RegistrationError::Capacity)?;
    }
    if retained > MAX_CSS_BYTES { return Err(RegistrationError::Capacity); }
    // The DOM descriptor supplies a literal name, not a serialized CSS token.
    if !name.starts_with("--") || name.len() == 2 {
        return Err(RegistrationError::InvalidName);
    }
    if properties.iter().any(|property| property.name == name) {
        return Err(RegistrationError::Duplicate);
    }
    if syntax.len() > 1024 || syntax.trim().is_empty() {
        return Err(RegistrationError::InvalidSyntax);
    }
    if syntax.trim() != "*" && !valid_typed_syntax(syntax) {
        return Err(RegistrationError::InvalidSyntax);
    }
    if syntax.trim() != "*"
        && syntax.split('|').any(|part| {
            let part = part.trim();
            part.starts_with('<')
                && !matches!(
                    component_syntax(part),
                    "<number>"
                        | "<integer>"
                        | "<color>"
                        | "<length>"
                        | "<length-percentage>"
                        | "<percentage>"
                        | "<angle>"
                        | "<time>"
                        | "<resolution>"
                        | "<string>"
                        | "<url>" | "<image>" | "<transform-function>" | "<transform-list>"
                        | "<custom-ident>"
                )
        })
    {
        return Err(RegistrationError::UnsupportedSyntax);
    }
    if let Some(value) = &initial_value {
        // Validate the value independently of the literal DOM name. The declaration
        // parser accepts CSS source identifiers, while registry names are strings.
        if !accepts_value(syntax, value) {
            return Err(RegistrationError::InvalidInitialValue);
        }
        if syntax.trim() != "*" && !computationally_independent(syntax, value) {
            return Err(RegistrationError::InvalidInitialValue);
        }
        let components = component_values::parse_unparsed_value(value)
            .map_err(|_| RegistrationError::InvalidInitialValue)?;
        // Initial values may not depend on another custom property. Reuse the
        // Typed OM component parser, including its token and nesting limits.
        if components.iter().any(|component| {
            matches!(
                component,
                component_values::UnparsedComponent::Variable { .. }
            )
        }) {
            return Err(RegistrationError::InvalidInitialValue);
        }
    }
    if properties.len() >= MAX_CUSTOM_PROPERTIES {
        return Err(RegistrationError::Capacity);
    }
    properties
        .try_reserve(1)
        .map_err(|_| RegistrationError::Capacity)?;
    properties.push(RegisteredCustomProperty {
        name,
        inherits,
        initial_value,
        syntax: syntax.trim().into(),
        source_url: None,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        html,
        paint::{ShapedRun, TextShaper},
        selector,
        session::RenderSession,
    };
    struct NoText;
    impl TextShaper for NoText {
        fn shape(&self, _: &str, _: f32) -> Result<ShapedRun, ()> {
            Err(())
        }
        fn ascent(&self, _: f32) -> f32 {
            0.0
        }
        fn line_height(&self, _: f32) -> f32 {
            0.0
        }
    }

    #[test]
    fn specification_registered_interpolation_keeps_exact_computed_endpoints(){
        use ComputedValueOperation::Interpolate;
        for(syntax,from,to)in [
            ("<length-percentage>","calc(15% + 24px)","calc(7% + 13px)"),
            ("<color>","color(srgb 0 0.6 0)","color(srgb 0.8 0 0)"),
            ("<length-percentage>#","calc(15% + 24px), 10px","calc(7% + 13px), 20%"),
        ]{
            assert_eq!(combine_computed_values(syntax,from,to,Interpolate(0.0)).as_deref(),Some(from));
            assert_eq!(combine_computed_values(syntax,from,to,Interpolate(1.0)).as_deref(),Some(to));
        }
        assert!(combine_computed_values("<length> | <number>","1px","2",Interpolate(0.0)).is_none());
    }

    #[test]
    fn specification_registered_sibling_owner_and_query_replay() {
        let document=html::parse(r#"<style>@property --n{syntax:"<length>";inherits:false;initial-value:0px} #scope{container-type:inline-size;width:200px} div{--n:calc(10cqw * sibling-index());width:var(--n)}</style><section id=scope><div></div>
 <div id=target></div></section>"#,128).unwrap();
        let target=selector::query_selector(&document,document.root(),"#target").unwrap().unwrap();
        let scope=selector::query_selector(&document,document.root(),"#scope").unwrap().unwrap();
        let mut session=RenderSession::new(document);session.display_list(500,200,&NoText).unwrap();
        assert_eq!(session.layout_rect(target).unwrap().width,40.0);
        let style=session.computed_style(target).unwrap();assert!(style.sibling_position_dependent);
        assert_eq!(style.custom_properties().iter().find(|(name,_)|name=="--n").unwrap().1.as_deref(),Some("40px"));
        session.document_mut().set_attribute(scope,"style","width:300px").unwrap();
        session.display_list(500,200,&NoText).unwrap();assert_eq!(session.layout_rect(target).unwrap().width,60.0);
        let mut registrations=Vec::new();
        assert_eq!(register(&mut registrations,"--bad".into(),"<number>",false,Some("sibling-index()".into())),Err(RegistrationError::InvalidInitialValue));
        assert_eq!(register(&mut registrations,"--bad-image".into(),"<image>",false,Some(r#"image-set(url("x") calc(1dppx * sibling-index()))"#.into())),Err(RegistrationError::InvalidInitialValue));
    }

    #[test]
    fn specification_registered_alternatives_keep_first_parsed_type() {
        use ComputedValueOperation::Interpolate;
        let single="translateX(100px)";
        let list="translateX(200px) rotate(90deg)";
        assert!(combine_computed_values("<transform-function> | <transform-list>",single,list,Interpolate(0.5)).is_none());
        assert!(combine_computed_values("<transform-function> | <transform-list>",list,single,Interpolate(0.5)).is_none());
        assert_eq!(combine_computed_values("<transform-list> | <transform-function>",single,list,Interpolate(0.5)).as_deref(),Some("translateX(150px) rotate(45deg)"));
        assert!(combine_computed_values("<length> | <length-percentage>","10px","20%",Interpolate(0.5)).is_none());
        assert!(combine_computed_values("<number> | <integer>","1","3",Interpolate(0.5)).is_some());
        assert!(combine_computed_values("<transform-function># | <transform-list>",single,list,Interpolate(0.5)).is_none());
    }

    #[test]
    fn specification_registered_precise_currentcolor_scheme_and_single_gradient_source() {
        let context=static_length_context();let query=ContainerUnitContext::default();
        let source=SourceColor{value:color_values::endpoint("color(srgb 0.123 0.456 0.789)",0).unwrap(),color_function:true,expression:None};
        let value=compute_value_with_source("<color>","currentColor",context,query,&source,None).unwrap();
        assert!(value.starts_with("color(srgb "));
        let dark=LengthContext{viewport:context.viewport.for_used_scheme(UsedColorScheme::Dark),..context};
        assert_eq!(compute_value_with_source("<color>","light-dark(red,blue)",dark,query,&source,None).as_deref(),Some("rgb(0, 0, 255)"));
        assert_eq!(compute_value_with_source("<image>","light-dark(url(light.png),url(dark.png))",dark,query,&source,None).as_deref(),Some("url(\"dark.png\")"));
        let gradient=background_image_part_with_source("linear-gradient(blue)",&source,Some(context),0,false,query).unwrap();
        assert_eq!(computed_values::image(&gradient,0).as_deref(),Some("linear-gradient(rgb(0, 0, 255))"));
        let gradient=background_image_part_with_source("linear-gradient(blue,blue)",&source,Some(context),0,false,query).unwrap();
        assert_eq!(computed_values::image(&gradient,0).as_deref(),Some("linear-gradient(rgb(0, 0, 255), rgb(0, 0, 255))"));
    }

    #[test]
    fn specification_registered_codecs_url_origins_image_candidates_transform_identity_and_math() {
        let context=LengthContext{font:10.0,..static_length_context()};let query=ContainerUnitContext::default();let foreground=legacy_source_color(Style::initial().color);
        let compute=|syntax,value|compute_value_with_source(syntax,value,context,query,&foreground,Some("https://example.test/sheet/main.css"));
        assert_eq!(compute("<url>","url(../image.svg)").as_deref(),Some("url(\"https://example.test/image.svg\")"));
        assert_eq!(compute("<transform-function>","translateX(10em)").as_deref(),Some("translateX(100px)"));
        assert_eq!(compute("<transform-list>","translateX(calc(11em + 10%)) rotate(1turn)").as_deref(),Some("translateX(calc(10% + 110px)) rotate(1turn)"));
        let image=compute("<image>","image-set(url(one.png) 1x, url(two.png) 2x)").unwrap();
        assert!(image.contains("https://example.test/sheet/one.png"));assert!(image.contains("https://example.test/sheet/two.png"));assert!(image.contains("2dppx"));
        assert_eq!(compute("<image>","linear-gradient(red 2em, blue 50%)").as_deref(),Some("linear-gradient(rgb(255, 0, 0) 20px, rgb(0, 0, 255) 50%)"));
        assert!(!accepts_value("<image>","none"));assert!(!accepts_value("<transform-function>","translateX(1px) scale(2)"));assert!(!accepts_value("<transform-list>","unknown()"));
        let mut registrations=Vec::new();register(&mut registrations,"--comment".into(),"<length>",false,Some("10px /*independent*/".into())).unwrap();
        assert_eq!(register(&mut registrations,"--dependent-image".into(),"<image>",false,Some("linear-gradient(red 2em,blue)".into())),Err(RegistrationError::InvalidInitialValue));
        let numeric=compute("<number>","calc(1 * tan(atan2(1em, 1px)))").unwrap().parse::<f64>().unwrap();assert!((numeric-10.0).abs()<0.0001);
        assert!(!accepts_value("<number>","tan(1px)"));

        let document=html::parse("<div id=target></div>",128).unwrap();let target=selector::query_selector(&document,document.root(),"#target").unwrap().unwrap();
        let mut rules=parse(r#"@property --url{syntax:"<url>";inherits:false;initial-value:url(initial.svg)}div{--url:url(authored.svg);--alias:var(--url)}"#).unwrap();
        for rule in &mut rules{rule.source_url=Some(Arc::from("https://example.test/stylesheet/main.css"));}
        let index=StyleIndex::new_with_document_base_url(rules,Some(Arc::from("https://example.test/document/page.html")));
        let style=compute_node(&document,target,None,&index).unwrap();
        assert_eq!(style.custom_properties().iter().find(|(name,_)|name=="--alias").unwrap().1.as_deref(),Some("url(\"https://example.test/stylesheet/authored.svg\")"));
    }

    #[test]
    fn specification_registered_font_cycle_invalidates_intermediate_unregistered_alias() {
        let document=html::parse(r#"<style>@property --x{syntax:"<length>";inherits:false;initial-value:3px}div{--x:3em;--y:var(--x);font-size:var(--y);--zx:var(--x,5px);--zy:var(--y,5px)}</style><div id=target></div>"#,128).unwrap();
        let target=selector::query_selector(&document,document.root(),"#target").unwrap().unwrap();let index=crate::layout::stylesheets(&document).unwrap();let style=compute_node(&document,target,None,&index).unwrap();
        let value=|key|style.custom_properties().iter().find(|(name,_)|name==key).and_then(|(_,value)|value.as_deref());
        assert_eq!(value("--zx"),Some("3px"));assert_eq!(value("--zy"),Some("5px"));
    }

    #[test]
    fn specification_property_modern_defaults_names_escapes_and_independent_viewport() {
        let rules = parse_rules(r"@pr\6f perty --one, --two", r#"syntax: nope; inherits: nonsense"#).unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].name, "--one");
        assert_eq!(rules[1].name, "--two");
        assert_eq!(rules[0].syntax, "*");
        assert!(rules[0].inherits);
        assert!(rules[0].initial_value.is_none());
        let typed = parse_rules("@property --missing", r#"syntax:"<length>""#).unwrap();
        assert!(typed[0].initial_value.is_none());
        let invalid_initial=parse_rules("@property --invalid-initial",r#"initial-value:1px;initial-value:red;syntax:"<length>""#).unwrap();
        assert_eq!(invalid_initial[0].initial_value.as_deref(),Some("1px"));
        let invalid_initial=parse_rules("@property --invalid-initial",r#"initial-value:red;syntax:"<length>""#).unwrap();
        assert!(invalid_initial[0].initial_value.is_none());
        let mut api = Vec::new();
        register(&mut api, "--viewport".into(), "<length>", true, Some("2svw".into())).unwrap();
        register(&mut api, "--missing".into(), "<length>", true, None).unwrap();
        assert_eq!(register(&mut api, "--font".into(), "<length>", true, Some("2em".into())), Err(RegistrationError::InvalidInitialValue));
        let index = StyleIndex::new(parse(r#"@pr\6f perty --one, --two {initial-value:green}"#).unwrap());
        assert_eq!(index.registered_snapshot().len(), 2);
    }

    #[test]
    fn specification_property_carrier_order_media_shared_snapshot_and_removal() {
        let document = html::parse(r#"<style id=first>@property --x{syntax:"<length>";inherits:false;initial-value:1px}</style><style id=last>@media(min-width:300px){@property --x{syntax:"<length>";inherits:false;initial-value:2px}} @property --{syntax:"<length>";initial-value:red}</style><style>div{width:var(--x)}</style><div id=target></div>"#,128).unwrap();
        let target = selector::query_selector(&document,document.root(),"#target").unwrap().unwrap();
        let last = selector::query_selector(&document,document.root(),"#last").unwrap().unwrap();
        let first = selector::query_selector(&document,document.root(),"#first").unwrap().unwrap();
        let mut session = RenderSession::new(document);
        session.display_list(400,100,&NoText).unwrap();
        assert_eq!(session.layout_rect(target).unwrap().width,2.0);
        session.display_list(200,100,&NoText).unwrap();
        assert_eq!(session.layout_rect(target).unwrap().width,1.0);
        session.document_mut().remove(last).unwrap();
        session.display_list(400,100,&NoText).unwrap();
        assert_eq!(session.layout_rect(target).unwrap().width,1.0);
        session.document_mut().remove(first).unwrap();
        let style = session.computed_style(target).unwrap();
        assert!(!style.custom_properties().iter().any(|(name,_)|name=="--x"));
        session.register_custom_property("--x".into(),"<length>",false,Some("7px".into())).unwrap();
        session.display_list(400,100,&NoText).unwrap();
        assert_eq!(session.layout_rect(target).unwrap().width,7.0);

        let mut index = StyleIndex::new(parse(r#"@property --x{syntax:"<length>";inherits:false;initial-value:2px}"#).unwrap());
        let first = index.registered_snapshot();
        assert!(Arc::ptr_eq(&first,&index.registered_snapshot()));
        let mut api = Vec::new();
        register(&mut api,"--x".into(),"<length>",false,Some("7px".into())).unwrap();
        index.set_registered_properties(&api);
        assert_eq!(index.registered_snapshot()[0].initial_value.as_deref(),Some("7px"));
        assert!(!Arc::ptr_eq(&first,&index.registered_snapshot()));
    }

    #[test]
    fn specification_property_query_demand_keeps_unrelated_color_read_sparse() {
        let document = html::parse(r#"<style>@property --measure{syntax:"<length>";inherits:false;initial-value:1px} div{--measure:2cqw;width:var(--measure);color:green}</style><div id=target></div>"#,128).unwrap();
        let target = selector::query_selector(&document,document.root(),"#target").unwrap().unwrap();
        let index = crate::layout::stylesheets(&document).unwrap();
        let style = compute_node(&document,target,None,&index).unwrap();
        assert!(style.property_query_context_pending("--measure"));
        assert!(style.property_query_context_pending("width"));
        assert!(!style.property_query_context_pending("color"));
    }

    #[test]
    fn specification_registered_computed_numeric_grammar_initials_and_lists() {
        let mut properties = Vec::new();
        register(
            &mut properties,
            "--measure".into(),
            "<length-percentage>",
            true,
            Some("calc(1in + 10%)".into()),
        )
        .unwrap();
        assert!(accepts_value("<length-percentage>", "min(1em + 10%, 20px)"));
        assert!(!accepts_value("<length>", "calc(1px + 10%)"));
        assert!(!accepts_value("<length>", "calc(0)"));
        assert!(accepts_value("<length>#", "1px, calc(2em + 3px)"));
        assert!(!accepts_value("<length>#", "1px,,2px"));
        assert!(!accepts_value("<integer>", "1.5"));
        assert!(accepts_value("<integer>", "calc(1.5)"));
        assert_eq!(
            register(
                &mut properties,
                "--dependent".into(),
                "<length>",
                false,
                Some("calc(1em + 1px)".into())
            ),
            Err(RegistrationError::InvalidInitialValue)
        );
        let context = static_length_context();
        assert_eq!(
            compute_value(
                "<length-percentage>",
                "calc(1in + 10%)",
                context,
                ContainerUnitContext::no_container(context.viewport),
                Style::initial().color
            )
            .as_deref(),
            Some("calc(10% + 96px)")
        );
        assert_eq!(
            compute_value(
                "<integer>",
                "calc(-1.5)",
                context,
                ContainerUnitContext::no_container(context.viewport),
                Style::initial().color
            )
            .as_deref(),
            Some("-1")
        );
        assert_eq!(
            computed_value_items("<length>#", "1px, 2px").unwrap(),
            ["1px", "2px"]
        );
    }

    #[test]
    fn specification_registered_computed_font_inheritance_aliases_and_invalid_defaults() {
        let mut document = html::parse("<style>#p{font-size:20px;line-height:30px;--length:2em;--line:2lh;--ordinary:var(--length)}#c{font-size:10px;width:var(--ordinary);height:var(--line);--own:2em}</style><div id=p><div id=c></div></div>", 128).unwrap();
        for (name, inherits) in [("--length", true), ("--line", true), ("--own", false)] {
            register(
                &mut document.registered_custom_properties,
                name.into(),
                "<length>",
                inherits,
                Some("3px".into()),
            )
            .unwrap();
        }
        let mut session = RenderSession::new(document);
        session.display_list(300, 200, &NoText).unwrap();
        let child = selector::query_selector(session.document(), session.document().root(), "#c")
            .unwrap()
            .unwrap();
        let style = session.computed_style(child).unwrap();
        let value = |name| {
            style
                .custom_properties()
                .iter()
                .find(|(key, _)| key == name)
                .and_then(|(_, value)| value.as_deref())
        };
        assert_eq!(value("--length"), Some("40px"));
        assert_eq!(value("--ordinary"), Some("40px"));
        assert_eq!(value("--line"), Some("60px"));
        assert_eq!(value("--own"), Some("20px"));
        assert_eq!(session.layout_rect(child).unwrap().width, 40.0);
        assert_eq!(session.layout_rect(child).unwrap().height, 60.0);
        session
            .document_mut()
            .set_attribute(child, "style", "--length:bad;--line:initial;--own:bad")
            .unwrap();
        let style = session.computed_style(child).unwrap();
        assert_eq!(
            style
                .custom_properties()
                .iter()
                .find(|(key, _)| key == "--length")
                .unwrap()
                .1
                .as_deref(),
            Some("40px")
        );
        assert_eq!(
            style
                .custom_properties()
                .iter()
                .find(|(key, _)| key == "--line")
                .unwrap()
                .1
                .as_deref(),
            Some("3px")
        );
        assert_eq!(
            style
                .custom_properties()
                .iter()
                .find(|(key, _)| key == "--own")
                .unwrap()
                .1
                .as_deref(),
            Some("3px")
        );
    }

    #[test]
    fn specification_registered_computed_relative_unit_cycles_unset_font_and_initial_custom() {
        let mut document = html::parse("<style>html{font-size:20px;--root:2rem}#p{font-size:30px;line-height:40px}#a{--length:2em;font-size:var(--length)}#b{--line:2lh;line-height:var(--line)}</style><div id=p><div id=a></div><div id=b></div></div>", 128).unwrap();
        for name in ["--root", "--length", "--line"] {
            register(
                &mut document.registered_custom_properties,
                name.into(),
                "<length>",
                false,
                Some("5px".into()),
            )
            .unwrap();
        }
        let mut session = RenderSession::new(document);
        let a = selector::query_selector(session.document(), session.document().root(), "#a")
            .unwrap()
            .unwrap();
        let b = selector::query_selector(session.document(), session.document().root(), "#b")
            .unwrap()
            .unwrap();
        let style = session.computed_style(a).unwrap();
        assert_eq!(style.font_size, 30.0);
        assert_eq!(
            style
                .custom_properties()
                .iter()
                .find(|(name, _)| name == "--length")
                .unwrap()
                .1
                .as_deref(),
            Some("5px")
        );
        let style = session.computed_style(b).unwrap();
        assert_eq!(style.line_height, LineHeight::Pixels(40.0));
        assert_eq!(
            style
                .custom_properties()
                .iter()
                .find(|(name, _)| name == "--line")
                .unwrap()
                .1
                .as_deref(),
            Some("5px")
        );
    }

    #[test]
    fn specification_registered_computed_declaring_query_percentage_nonlinear_and_replay() {
        let mut document = html::parse("<style>body{margin:0}#container{container-type:inline-size;width:300px}#p{--length:min(calc(10cqw + 10%),80px);width:var(--length);height:20px}#c{width:var(--length);height:10px}</style><div id=container><div id=p><div id=c></div></div></div>", 128).unwrap();
        register(
            &mut document.registered_custom_properties,
            "--length".into(),
            "<length-percentage>",
            true,
            Some("0px".into()),
        )
        .unwrap();
        let parent = selector::query_selector(&document, document.root(), "#p")
            .unwrap()
            .unwrap();
        let child = selector::query_selector(&document, document.root(), "#c")
            .unwrap()
            .unwrap();
        let mut session = RenderSession::new(document);
        session.display_list(600, 200, &NoText).unwrap();
        assert_eq!(session.layout_rect(parent).unwrap().width, 60.0);
        assert_eq!(session.layout_rect(child).unwrap().width, 36.0);
        let first = session.display_list(600, 200, &NoText).unwrap().0.clone();
        assert_eq!(session.display_list(600, 200, &NoText).unwrap().0, first);
        let style = session.computed_style(child).unwrap();
        let value = style
            .custom_properties()
            .iter()
            .find(|(name, _)| name == "--length")
            .unwrap()
            .1
            .as_deref()
            .unwrap();
        let expression = typed_numeric::parse_numeric_expression(value).unwrap();
        assert!(!expression.contains_unit(container_unit));
        assert!(expression.contains_unit(|unit| unit == typed_numeric::NumericUnit::Percent));
        let container =
            selector::query_selector(session.document(), session.document().root(), "#container")
                .unwrap()
                .unwrap();
        session
            .document_mut()
            .set_attribute(container, "style", "width:500px")
            .unwrap();
        session.display_list(600, 200, &NoText).unwrap();
        assert_eq!(session.layout_rect(parent).unwrap().width, 80.0);
        assert_eq!(session.layout_rect(child).unwrap().width, 58.0);
    }

    #[test]
    fn registered_property_names_are_literal_strings() {
        let mut properties = Vec::new();
        register(
            &mut properties,
            "--name, no escapes needed".into(),
            "*",
            true,
            Some("green".into()),
        )
        .unwrap();
        assert_eq!(properties[0].name, "--name, no escapes needed");
        assert_eq!(
            register(&mut properties, "--".into(), "*", true, None),
            Err(RegistrationError::InvalidName)
        );
        assert_eq!(
            register(&mut properties, "name".into(), "*", true, None),
            Err(RegistrationError::InvalidName)
        );
    }

    #[test]
    fn registered_property_universal_changes_real_layout_without_dom_mutation() {
        let document = html::parse("<style>#target{display:var(--my-display,block);width:40px;height:30px}</style><div id=target></div>", 96).unwrap();
        let target = selector::query_selector(&document, document.root(), "#target")
            .unwrap()
            .unwrap();
        let mut session = RenderSession::new(document);
        session.display_list(200, 100, &NoText).unwrap();
        assert_eq!(session.layout_rect(target).unwrap().width, 40.0);
        let document_version = session.document().version();
        session
            .register_custom_property("--my-display".into(), "*", false, Some("none".into()))
            .unwrap();
        assert_eq!(
            session.document().version(),
            document_version,
            "registration is not a DOM mutation"
        );
        session.display_list(200, 100, &NoText).unwrap();
        assert!(
            session.layout_rect(target).is_none(),
            "display:none suppresses the actual layout box"
        );
        assert_eq!(
            session.register_custom_property("--my-display".into(), "**", true, None),
            Err(RegistrationError::Duplicate)
        );
    }
}

#[cfg(test)]
mod modern_math_guards {
    use super::*;
    #[test]
    fn specification_registered_stepped_math_percentage_basis_and_angle_zero() {
        assert!(!accepts_value("<angle>", "0"));
        assert!(!accepts_value("<angle>", "calc(0)"));
        assert!(accepts_value("<angle>", "0deg"));
        assert!(!valid_typed_syntax("<transform-list>+"));
        assert!(!valid_typed_syntax("<transform-list>#"));
        let context=static_length_context();let query=ContainerUnitContext::default();let color=Style::initial().color;
        for (syntax,input,expected) in [
            ("<length>","mod(-18px, 5px)","2px"),
            ("<length>","rem(-18px, 5px)","-3px"),
            ("<angle>","mod(140deg, -90deg)","-40deg"),
            ("<angle>","rem(140deg, -90deg)","50deg"),
            ("<number>","round(2.5)","3"),
            ("<number>","round(-2.5)","-2"),
            ("<length>","round(up, 11px, 5px)","15px"),
            ("<length>","round(down, -11px, 5px)","-15px"),
            ("<length>","round(to-zero, -11px, -5px)","-10px"),
            ("<length>","round(nearest, 12.5px, 5px)","15px"),
        ] { assert_eq!(compute_value(syntax,input,context,query,color).as_deref(),Some(expected),"{input}"); }
        assert!(!accepts_value("<length>","round(3px)"));
        assert!(!accepts_value("<length>","mod(3px, 1deg)"));
        assert!(!accepts_value("<length>","round(up 3px, 1px)"));
        for input in ["abs(10%)","hypot(10%, 20%)","round(up, 10%, 3%)"] {
            let computed=compute_value("<percentage>",input,context,query,color).unwrap();
            assert!(computed.contains('('),"must retain unresolved percentage basis: {input}: {computed}");
        }
    }
}

#[cfg(test)]
mod registered_list_animation_guards {
    use super::*;
    #[test]
    fn specification_registered_list_animation_equal_length_integer_and_nonadditive_color() {
        use ComputedValueOperation::*;
        for (syntax,from,to,operation,expected) in [
            ("<angle>#","100deg, 150deg","200deg, 250deg",Interpolate(0.5),"150deg, 200deg"),
            ("<number>+","1 2","3 4",Add,"4 6"),
            ("<integer>#","-2, 1","-1, 2",Interpolate(0.5),"-1, 2"),
            ("<length>#","100px, 100px","50px, 75px",Accumulate(2.0),"250px, 275px"),
            ("<color>+","rgb(0, 0, 0) rgb(100, 100, 100)","rgb(200, 200, 200) rgb(200, 200, 200)",Interpolate(0.5),"rgb(100, 100, 100) rgb(150, 150, 150)"),
            ("<color>","red","blue",Add,"blue"),
            ("<color>","red","blue",Accumulate(5.0),"blue"),
        ] {assert_eq!(combine_computed_values(syntax,from,to,operation).as_deref(),Some(expected));}
        assert!(combine_computed_values("<length>#","10px, 20px","30px",Interpolate(0.5)).is_none());
        assert!(combine_computed_values("<length> | <number>","10px","2",Interpolate(0.5)).is_none());
        assert!(combine_computed_values("<number>","1","2",Interpolate(f64::NAN)).is_none());
    }
}
