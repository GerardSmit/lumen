use super::*;
use super::extra_props as extra;

const MAX_ITEMS: usize = 1024;
const MAX_EXPANDED: usize = 4096;
const BREAK: &[&str] = &["normal", "none", "intersection"];
const VISIBILITY: &[&str] = &["all", "around", "between", "normal"];

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Part {
    Width,
    Color,
    Style,
    Inset,
}

#[derive(Clone, Copy)]
enum Check {
    List(Part),
    Words(&'static [&'static str]),
}

#[derive(Clone, Copy)]
enum Form {
    Rule,
    Mirror(Check),
    Inset,
    Cap,
    Junction,
    Edge,
}

impl Form {
    fn arity(self) -> usize {
        match self {
            Form::Rule => 3,
            Form::Mirror(_) => 1,
            Form::Inset => 4,
            Form::Cap | Form::Junction | Form::Edge => 2,
        }
    }
}

struct Short {
    name: &'static str,
    form: Form,
    longhands: &'static [&'static str],
}

impl Short {
    fn both(&self) -> bool {
        self.name.starts_with("rule")
    }
}

static TABLE: &[Short] = &[
    Short { name: "column-rule", form: Form::Rule, longhands: &["column-rule-width", "column-rule-style", "column-rule-color"] },
    Short { name: "row-rule", form: Form::Rule, longhands: &["row-rule-width", "row-rule-style", "row-rule-color"] },
    Short { name: "rule", form: Form::Rule, longhands: &["column-rule-width", "column-rule-style", "column-rule-color", "row-rule-width", "row-rule-style", "row-rule-color"] },
    Short { name: "column-rule-inset", form: Form::Inset, longhands: &["column-rule-inset-cap-start", "column-rule-inset-cap-end", "column-rule-inset-junction-start", "column-rule-inset-junction-end"] },
    Short { name: "row-rule-inset", form: Form::Inset, longhands: &["row-rule-inset-cap-start", "row-rule-inset-cap-end", "row-rule-inset-junction-start", "row-rule-inset-junction-end"] },
    Short { name: "rule-inset", form: Form::Inset, longhands: &["column-rule-inset-cap-start", "column-rule-inset-cap-end", "column-rule-inset-junction-start", "column-rule-inset-junction-end", "row-rule-inset-cap-start", "row-rule-inset-cap-end", "row-rule-inset-junction-start", "row-rule-inset-junction-end"] },
    Short { name: "column-rule-inset-cap", form: Form::Cap, longhands: &["column-rule-inset-cap-start", "column-rule-inset-cap-end"] },
    Short { name: "row-rule-inset-cap", form: Form::Cap, longhands: &["row-rule-inset-cap-start", "row-rule-inset-cap-end"] },
    Short { name: "rule-inset-cap", form: Form::Cap, longhands: &["column-rule-inset-cap-start", "column-rule-inset-cap-end", "row-rule-inset-cap-start", "row-rule-inset-cap-end"] },
    Short { name: "column-rule-inset-junction", form: Form::Junction, longhands: &["column-rule-inset-junction-start", "column-rule-inset-junction-end"] },
    Short { name: "row-rule-inset-junction", form: Form::Junction, longhands: &["row-rule-inset-junction-start", "row-rule-inset-junction-end"] },
    Short { name: "rule-inset-junction", form: Form::Junction, longhands: &["column-rule-inset-junction-start", "column-rule-inset-junction-end", "row-rule-inset-junction-start", "row-rule-inset-junction-end"] },
    Short { name: "column-rule-inset-start", form: Form::Edge, longhands: &["column-rule-inset-cap-start", "column-rule-inset-junction-start"] },
    Short { name: "row-rule-inset-start", form: Form::Edge, longhands: &["row-rule-inset-cap-start", "row-rule-inset-junction-start"] },
    Short { name: "rule-inset-start", form: Form::Edge, longhands: &["column-rule-inset-cap-start", "column-rule-inset-junction-start", "row-rule-inset-cap-start", "row-rule-inset-junction-start"] },
    Short { name: "column-rule-inset-end", form: Form::Edge, longhands: &["column-rule-inset-cap-end", "column-rule-inset-junction-end"] },
    Short { name: "row-rule-inset-end", form: Form::Edge, longhands: &["row-rule-inset-cap-end", "row-rule-inset-junction-end"] },
    Short { name: "rule-inset-end", form: Form::Edge, longhands: &["column-rule-inset-cap-end", "column-rule-inset-junction-end", "row-rule-inset-cap-end", "row-rule-inset-junction-end"] },
    Short { name: "rule-width", form: Form::Mirror(Check::List(Part::Width)), longhands: &["column-rule-width", "row-rule-width"] },
    Short { name: "rule-color", form: Form::Mirror(Check::List(Part::Color)), longhands: &["column-rule-color", "row-rule-color"] },
    Short { name: "rule-style", form: Form::Mirror(Check::List(Part::Style)), longhands: &["column-rule-style", "row-rule-style"] },
    Short { name: "rule-break", form: Form::Mirror(Check::Words(BREAK)), longhands: &["column-rule-break", "row-rule-break"] },
    Short { name: "rule-visibility-items", form: Form::Mirror(Check::Words(VISIBILITY)), longhands: &["column-rule-visibility-items", "row-rule-visibility-items"] },
];

fn short(name: &str) -> Option<&'static Short> {
    TABLE.iter().find(|short| short.name == name)
}

pub(crate) fn is_shorthand(name: &str) -> bool {
    short(name).is_some()
}

pub(crate) fn longhands(name: &str) -> &'static [&'static str] {
    short(name).map_or(&[], |short| short.longhands)
}

#[derive(Clone, Debug, PartialEq)]
enum Count {
    Auto,
    Times(u32),
}

impl Count {
    fn serialize(&self) -> String {
        match self {
            Count::Auto => String::from("auto"),
            Count::Times(times) => times.to_string(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Entry {
    One(String),
    Repeat(Count, Vec<String>),
}

fn repeat_body(part: &str) -> Option<&str> {
    let head = part.get(..7)?;
    (head.eq_ignore_ascii_case("repeat(") && part.ends_with(')')).then(|| &part[7..part.len() - 1])
}

fn parse_entries(raw: &str, context: LengthContext, element: &dyn Fn(&str) -> Option<String>) -> Option<Vec<Entry>> {
    let parts = top_level_split(raw.trim(), b',', MAX_ITEMS)?;
    let mut entries = Vec::with_capacity(parts.len());
    let mut autos = 0;
    for part in parts {
        if part.is_empty() {
            return None;
        }
        let Some(body) = repeat_body(part) else {
            entries.push(Entry::One(element(part)?));
            continue;
        };
        let args = top_level_split(body, b',', MAX_ITEMS)?;
        let (count, items) = args.split_first()?;
        let count = if decoded_css_keyword(count, "auto") {
            autos += 1;
            Count::Auto
        } else {
            let times = extra::integer(count, context)?;
            if times < 1 {
                return None;
            }
            Count::Times(u32::try_from(times).ok()?)
        };
        let items = items.iter().map(|item| element(item)).collect::<Option<Vec<_>>>()?;
        if items.is_empty() {
            return None;
        }
        entries.push(Entry::Repeat(count, items));
    }
    (autos <= 1).then_some(entries)
}

fn serialize_entries(entries: &[Entry]) -> String {
    entries
        .iter()
        .map(|entry| match entry {
            Entry::One(item) => item.clone(),
            Entry::Repeat(count, items) => alloc::format!("repeat({}, {})", count.serialize(), items.join(", ")),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn structure(raw: &str) -> Option<Vec<Entry>> {
    parse_entries(raw, static_length_context(), &|item| Some(item.to_owned()))
}

fn width_element(raw: &str, context: LengthContext) -> Option<String> {
    let keyword = extra::lower(raw);
    match keyword.as_ref() {
        "thin" => Some(String::from("1px")),
        "medium" => Some(String::from("3px")),
        "thick" => Some(String::from("5px")),
        _ => {
            let value = extra::parse_length(raw, context)?;
            if extra::negative_literal(raw, value) {
                return None;
            }
            Some(super::computed_values::px(value.max(0.0) as f32))
        }
    }
}

fn color_element(raw: &str, context: LengthContext, current: Rgba) -> Option<String> {
    let raw = raw.trim();
    let query = extra::query_for(context);
    let value = color_values::endpoint_with_current(raw, 0, color_values::from_rgba(current), context, query)?;
    let function = !decoded_css_keyword(raw, "currentcolor") && !color_values::legacy(raw);
    color_values::serialize(value, function)
}

fn style_element(raw: &str) -> Option<String> {
    BorderStyle::parse(raw.trim()).map(|style| String::from(style.as_str()))
}

fn plain_number(raw: &str) -> bool {
    raw.parse::<f64>().is_ok_and(|value| value != 0.0)
}

fn inset_element(raw: &str, context: LengthContext) -> Option<String> {
    let raw = raw.trim();
    if decoded_css_keyword(raw, "overlap-join") {
        return Some(String::from("overlap-join"));
    }
    if plain_number(raw) {
        return None;
    }
    let value = transform_length_with_query(raw, context, extra::query_for(context))?;
    Some(computed_values::percentage(value.pixels, value.percent))
}

fn specified_length(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw == "0" {
        return Some(String::from("0px"));
    }
    typed_numeric::parse_numeric_expression(raw)?.serialize_specified()
}

fn specified_element(part: Part, raw: &str) -> Option<String> {
    let context = static_length_context();
    let raw = raw.trim();
    match part {
        Part::Width => {
            width_element(raw, context)?;
            let keyword = extra::lower(raw);
            if matches!(keyword.as_ref(), "thin" | "medium" | "thick") {
                return Some(keyword.into_owned());
            }
            specified_length(raw)
        }
        Part::Color => {
            color_element(raw, context, Style::initial().color)?;
            Some(color_values::serialize_declared(raw, 0).unwrap_or_else(|| extra::lower(raw).into_owned()))
        }
        Part::Style => style_element(raw),
        Part::Inset => {
            inset_element(raw, context)?;
            if decoded_css_keyword(raw, "overlap-join") {
                return Some(String::from("overlap-join"));
            }
            specified_length(raw)
        }
    }
}

pub(crate) fn compute(part: Part, raw: &str, context: LengthContext, current: Rgba) -> Option<String> {
    let raw = raw.trim();
    if part == Part::Inset {
        return inset_element(raw, context);
    }
    let element = |item: &str| match part {
        Part::Width => width_element(item, context),
        Part::Color => color_element(item, context, current),
        _ => style_element(item),
    };
    let entries = parse_entries(raw, context, &element)?;
    Some(serialize_entries(&entries))
}

pub(crate) fn specified(part: Part, raw: &str) -> Option<String> {
    let raw = raw.trim();
    if part == Part::Inset {
        return specified_element(part, raw);
    }
    let entries = parse_entries(raw, static_length_context(), &|item| specified_element(part, item))?;
    Some(serialize_entries(&entries))
}

fn first_element(raw: &str) -> Option<String> {
    match structure(raw)?.into_iter().next()? {
        Entry::One(item) => Some(item),
        Entry::Repeat(_, items) => items.into_iter().next(),
    }
}

fn legacy_declarations(name: &str, raw: &str) -> Vec<Value> {
    if !matches!(name, "column-rule-width" | "column-rule-color" | "column-rule-style") {
        return Vec::new();
    }
    let Some(first) = first_element(raw) else { return Vec::new() };
    typed_declarations_legacy(name, &first, 0).map_or_else(|_| Vec::new(), |list| list.into_iter().map(|declaration| declaration.value).collect())
}

pub(crate) fn longhand_declarations(name: &str, raw: &str, extra_value: Vec<Value>) -> Vec<Value> {
    let mut values = legacy_declarations(name, raw);
    values.extend(extra_value);
    values
}

pub(crate) fn shorthand_declarations(name: &str, raw: &str) -> Option<Vec<Value>> {
    let mut values = Vec::new();
    for (longhand, text) in expand(name, raw)? {
        values.extend(extra::declarations(longhand, &text)?);
    }
    Some(values)
}

fn inset_token(raw: &str) -> Option<String> {
    specified_element(Part::Inset, raw)
}

fn inset_pair(raw: &str) -> Option<[String; 2]> {
    let tokens = components(raw)?;
    match tokens.as_slice() {
        [one] => {
            let value = inset_token(one)?;
            Some([value.clone(), value])
        }
        [first, second] => Some([inset_token(first)?, inset_token(second)?]),
        _ => None,
    }
}

fn expand_inset(raw: &str) -> Option<Vec<String>> {
    let halves = top_level_split(raw.trim(), b'/', 3)?;
    let (cap, junction) = match halves.as_slice() {
        [cap] => (*cap, *cap),
        [cap, junction] => (*cap, *junction),
        _ => return None,
    };
    let [cap_start, cap_end] = inset_pair(cap)?;
    let [junction_start, junction_end] = inset_pair(junction)?;
    Some(alloc::vec![cap_start, cap_end, junction_start, junction_end])
}

fn rule_component(token: &str, parts: &mut [Option<String>; 3]) -> Option<()> {
    let context = static_length_context();
    let (index, text) = if style_element(token).is_some() {
        (1, specified_element(Part::Style, token)?)
    } else if width_element(token, context).is_some() {
        (0, specified_element(Part::Width, token)?)
    } else {
        (2, specified_element(Part::Color, token)?)
    };
    if parts[index].is_some() {
        return None;
    }
    parts[index] = Some(text);
    Some(())
}

fn rule_triple(raw: &str) -> Option<[String; 3]> {
    let tokens = components(raw)?;
    if tokens.is_empty() {
        return None;
    }
    let mut parts = [None, None, None];
    for token in tokens {
        rule_component(token, &mut parts)?;
    }
    let [width, style, color] = parts;
    Some([
        width.unwrap_or_else(|| String::from("medium")),
        style.unwrap_or_else(|| String::from("none")),
        color.unwrap_or_else(|| String::from("currentcolor")),
    ])
}

fn expand_rule(raw: &str) -> Option<Vec<String>> {
    let parts = top_level_split(raw.trim(), b',', MAX_ITEMS)?;
    let mut lists: [Vec<String>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    let mut autos = 0;
    for part in parts {
        if part.is_empty() {
            return None;
        }
        let mut columns: [Vec<String>; 3] = [Vec::new(), Vec::new(), Vec::new()];
        let mut count = None;
        if let Some(body) = repeat_body(part) {
            let args = top_level_split(body, b',', MAX_ITEMS)?;
            let (head, items) = args.split_first()?;
            if items.is_empty() {
                return None;
            }
            if decoded_css_keyword(head, "auto") {
                autos += 1;
            } else if extra::integer(head, static_length_context()).map_or(true, |times| times < 1) {
                return None;
            }
            count = Some(String::from(*head));
            for item in items {
                for (column, value) in columns.iter_mut().zip(rule_triple(item)?) {
                    column.push(value);
                }
            }
        } else {
            for (column, value) in columns.iter_mut().zip(rule_triple(part)?) {
                column.push(value);
            }
        }
        for (list, column) in lists.iter_mut().zip(columns) {
            list.push(match &count {
                Some(count) => alloc::format!("repeat({}, {})", count.trim(), column.join(", ")),
                None => column.join(", "),
            });
        }
    }
    if autos > 1 {
        return None;
    }
    Some(lists.into_iter().map(|list| list.join(", ")).collect())
}

fn expand_axis(form: Form, raw: &str) -> Option<Vec<String>> {
    match form {
        Form::Rule => expand_rule(raw),
        Form::Mirror(Check::List(part)) => Some(alloc::vec![specified(part, raw)?]),
        Form::Mirror(Check::Words(words)) => {
            let keyword = extra::lower(raw);
            words.contains(&keyword.as_ref()).then(|| alloc::vec![keyword.into_owned()])
        }
        Form::Inset => expand_inset(raw),
        Form::Cap | Form::Junction => inset_pair(raw).map(Vec::from),
        Form::Edge => {
            let value = inset_token(raw)?;
            Some(alloc::vec![value.clone(), value])
        }
    }
}

pub(crate) fn expand(name: &str, raw: &str) -> Option<Vec<(&'static str, String)>> {
    let short = short(name)?;
    let values = expand_axis(short.form, raw.trim())?;
    let axes = if short.both() { 2 } else { 1 };
    let mut out = Vec::with_capacity(values.len() * axes);
    for (index, longhand) in short.longhands.iter().enumerate() {
        out.push((*longhand, values[index % values.len()].clone()));
    }
    Some(out)
}

fn rule_item(width: &str, style: &str, color: &str, computed: bool) -> String {
    if computed {
        return if style == "none" {
            alloc::format!("{width} {color}")
        } else {
            alloc::format!("{width} {style} {color}")
        };
    }
    let mut parts = Vec::new();
    if !width.eq_ignore_ascii_case("medium") {
        parts.push(width);
    }
    if !style.eq_ignore_ascii_case("none") {
        parts.push(style);
    }
    if !color.eq_ignore_ascii_case("currentcolor") {
        parts.push(color);
    }
    if parts.is_empty() {
        width.to_owned()
    } else {
        parts.join(" ")
    }
}

fn merge_rule(width: &str, style: &str, color: &str, computed: bool) -> Option<String> {
    let (width, style, color) = (structure(width)?, structure(style)?, structure(color)?);
    if width.len() != style.len() || width.len() != color.len() {
        return None;
    }
    let mut out = Vec::with_capacity(width.len());
    for ((width, style), color) in width.iter().zip(&style).zip(&color) {
        match (width, style, color) {
            (Entry::One(width), Entry::One(style), Entry::One(color)) => out.push(rule_item(width, style, color, computed)),
            (Entry::Repeat(count, widths), Entry::Repeat(style_count, styles), Entry::Repeat(color_count, colors))
                if count == style_count && count == color_count && widths.len() == styles.len() && widths.len() == colors.len() =>
            {
                let items: Vec<String> = widths
                    .iter()
                    .zip(styles)
                    .zip(colors)
                    .map(|((width, style), color)| rule_item(width, style, color, computed))
                    .collect();
                out.push(alloc::format!("repeat({}, {})", count.serialize(), items.join(", ")));
            }
            _ => return None,
        }
    }
    Some(out.join(", "))
}

fn synthesize_axis(form: Form, values: &[&str], computed: bool) -> Option<String> {
    match form {
        Form::Rule => merge_rule(values[0], values[1], values[2], computed),
        Form::Mirror(_) => Some(values[0].to_owned()),
        Form::Inset => {
            if values.iter().all(|value| *value == values[0]) {
                Some(values[0].to_owned())
            } else {
                Some(alloc::format!("{} {} / {} {}", values[0], values[1], values[2], values[3]))
            }
        }
        Form::Cap | Form::Junction => Some(if values[0] == values[1] {
            values[0].to_owned()
        } else {
            alloc::format!("{} {}", values[0], values[1])
        }),
        Form::Edge => (values[0] == values[1]).then(|| values[0].to_owned()),
    }
}

fn synthesize_values(name: &str, values: &[&str], computed: bool) -> Option<String> {
    let short = short(name)?;
    let arity = short.form.arity();
    if values.len() != short.longhands.len() {
        return None;
    }
    if short.both() {
        let (column, row) = values.split_at(values.len() / 2);
        if column != row {
            return None;
        }
        return synthesize_axis(short.form, column, computed);
    }
    synthesize_axis(short.form, &values[..arity], computed)
}

pub(crate) fn synthesize(name: &str, values: &[&str]) -> Option<String> {
    synthesize_values(name, values, false)
}

pub(crate) fn computed_shorthand(style: &Style, name: &str) -> String {
    let Some(short) = short(name) else { return String::new() };
    let values: Option<Vec<String>> = short.longhands.iter().map(|longhand| extra::computed(style, longhand)).collect();
    let Some(values) = values else { return String::new() };
    let refs: Vec<&str> = values.iter().map(String::as_str).collect();
    synthesize_values(name, &refs, true).unwrap_or_default()
}

pub(crate) fn computed(style: &Style, name: &str, part: Part) -> String {
    let stored = extra::stored(style, name);
    match part {
        Part::Color => match extra::keyword(style, name) {
            Some(value) => value.to_owned(),
            None => computed_values::color(style.color),
        },
        Part::Width => stored.unwrap_or_else(|| String::from("3px")),
        Part::Style | Part::Inset => stored.unwrap_or_default(),
    }
}

fn expand_list(entries: &[Entry]) -> Option<Vec<String>> {
    let mut out = Vec::new();
    for entry in entries {
        match entry {
            Entry::One(item) => out.push(item.clone()),
            Entry::Repeat(Count::Times(times), items) => {
                for _ in 0..*times {
                    out.extend(items.iter().cloned());
                    if out.len() > MAX_EXPANDED {
                        return None;
                    }
                }
            }
            Entry::Repeat(Count::Auto, _) => return None,
        }
        if out.len() > MAX_EXPANDED {
            return None;
        }
    }
    Some(out)
}

fn cycle(from: &[String], to: &[String], mix: &dyn Fn(&str, &str) -> Option<String>) -> Option<Vec<String>> {
    let length = lumen_common::numeric::lcm_capped(from.len(), to.len(), MAX_EXPANDED)?;
    (0..length).map(|index| mix(&from[index % from.len()], &to[index % to.len()])).collect()
}

fn split_auto(entries: &[Entry]) -> Option<(Vec<String>, Vec<String>, Vec<String>)> {
    let position = entries.iter().position(|entry| matches!(entry, Entry::Repeat(Count::Auto, _)))?;
    let Entry::Repeat(_, inner) = &entries[position] else { return None };
    Some((expand_list(&entries[..position])?, inner.clone(), expand_list(&entries[position + 1..])?))
}

/// Computed per-gap items of a rule list property, cycled or auto-filled to `count` entries.
pub(crate) fn resolve_items(part: Part, raw: &str, count: usize, context: LengthContext, current: Rgba) -> Vec<String> {
    let Some(computed) = compute(part, raw, context, current) else { return Vec::new() };
    let Some(entries) = structure(&computed) else { return Vec::new() };
    let items = match split_auto(&entries) {
        Some((lead, inner, trail)) => {
            let fill = count.saturating_sub(lead.len() + trail.len());
            let mut out = lead;
            out.extend((0..fill).map(|index| inner[index % inner.len()].clone()));
            out.extend(trail);
            out
        }
        None => expand_list(&entries).unwrap_or_default(),
    };
    if items.is_empty() {
        return items;
    }
    (0..count).map(|index| items[index % items.len()].clone()).collect()
}

fn width_keyword_px(raw: &str) -> &str {
    match raw.trim() {
        "thin" => "1px",
        "medium" => "3px",
        "thick" => "5px",
        other => other,
    }
}

fn mix_element(part: Part, from: &str, to: &str, progress: f64, snap: bool) -> Option<String> {
    match part {
        Part::Width => {
            let (from, to) = (width_keyword_px(from), width_keyword_px(to));
            let (a, unit_a) = syntax::split_number_unit(from)?;
            let (b, unit_b) = syntax::split_number_unit(to)?;
            if unit_a != "px" || unit_b != "px" {
                return None;
            }
            let width = extra::mix(a, b, progress).max(0.0);
            if !snap {
                return Some(super::computed_values::px(width as f32));
            }
            // Row rule widths snap like border widths; the epsilon absorbs float error such as 6.9999.
            let snapped = if width > 0.0 && width < 1.0 { 1.0 } else { (width + 1e-6).floor() };
            Some(super::computed_values::px(snapped as f32))
        }
        Part::Color => {
            let current = color_values::from_rgba(Style::initial().color);
            let a = parse_animation_unclipped_color(from, current)?;
            let b = parse_animation_unclipped_color(to, current)?;
            interpolate_source_colors(&a, &b, progress as f32).serialize()
        }
        Part::Inset => extra::mix_lp(from, to, progress),
        Part::Style => None,
    }
}

#[cfg(test)]
pub(crate) fn interpolate(part: Part, from: &str, to: &str, progress: f64) -> Option<String> {
    interpolate_snapping(part, from, to, progress, false)
}

/// Interpolates a rule property; `snap` rounds widths down like border widths (row rules only).
pub(crate) fn interpolate_snapping(part: Part, from: &str, to: &str, progress: f64, snap: bool) -> Option<String> {
    if !progress.is_finite() {
        return None;
    }
    if part == Part::Inset {
        if from.trim() == "overlap-join" || to.trim() == "overlap-join" {
            return None;
        }
        return mix_element(part, from, to, progress, snap);
    }
    let a = structure(from)?;
    let b = structure(to)?;
    let mix = |x: &str, y: &str| mix_element(part, x, y, progress, snap);
    match (split_auto(&a), split_auto(&b)) {
        (None, None) => Some(cycle(&expand_list(&a)?, &expand_list(&b)?, &mix)?.join(", ")),
        (Some((lead_a, inner_a, trail_a)), Some((lead_b, inner_b, trail_b))) => {
            if lead_a.len() != lead_b.len() || trail_a.len() != trail_b.len() {
                return None;
            }
            let lead = cycle(&lead_a, &lead_b, &mix).or_else(|| (lead_a.is_empty()).then(Vec::new))?;
            let trail = cycle(&trail_a, &trail_b, &mix).or_else(|| (trail_a.is_empty()).then(Vec::new))?;
            let inner = cycle(&inner_a, &inner_b, &mix)?;
            let mut out = lead;
            out.push(alloc::format!("repeat(auto, {})", inner.join(", ")));
            out.extend(trail);
            Some(out.join(", "))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compute_width(raw: &str) -> Option<String> {
        compute(Part::Width, raw, static_length_context(), Style::initial().color)
    }

    #[test]
    fn widths_keep_repeat_structure() {
        assert_eq!(compute_width("repeat(calc(5 + 3), 10px), thin").as_deref(), Some("repeat(8, 10px), 1px"));
        assert_eq!(compute_width("repeat(auto, 10px), thin, repeat(auto, thick)"), None);
        assert_eq!(compute_width("repeat(0, 10px)"), None);
        assert_eq!(compute_width("30%"), None);
    }

    #[test]
    fn auto_repeaters_need_matching_segments() {
        assert_eq!(
            interpolate(Part::Width, "20px, repeat(auto, 20px)", "30px, repeat(auto, 30px, 30px)", 0.5).as_deref(),
            Some("25px, repeat(auto, 25px, 25px)")
        );
        assert_eq!(interpolate(Part::Width, "20px, repeat(auto, 20px)", "30px, 30px, repeat(auto, 30px)", 0.5), None);
    }

    #[test]
    fn integer_repeaters_expand_to_common_multiple() {
        assert_eq!(interpolate(Part::Width, "repeat(2, 20px)", "30px", 0.5).as_deref(), Some("25px, 25px"));
        assert_eq!(interpolate(Part::Width, "10px, 20px", "30px, 40px, 50px", 0.0).as_deref(), Some("10px, 20px, 10px, 20px, 10px, 20px"));
    }

    #[test]
    fn rule_shorthand_round_trips_through_longhands() {
        let expanded = expand("rule", "repeat(auto, blue 6px, 5px solid red)").unwrap();
        assert_eq!(expanded[0], ("column-rule-width", String::from("repeat(auto, 6px, 5px)")));
        assert_eq!(expanded[1], ("column-rule-style", String::from("repeat(auto, none, solid)")));
        let values: Vec<&str> = expanded.iter().take(3).map(|(_, value)| value.as_str()).collect();
        assert_eq!(synthesize("column-rule", &values).as_deref(), Some("repeat(auto, 6px blue, 5px solid red)"));
    }

    #[test]
    fn inset_shorthand_serialization() {
        let expanded = expand("column-rule-inset", "10px 20px / -5px").unwrap();
        let values: Vec<&str> = expanded.iter().map(|(_, value)| value.as_str()).collect();
        assert_eq!(synthesize("column-rule-inset", &values).as_deref(), Some("10px 20px / -5px -5px"));
        assert_eq!(expand("column-rule-inset", "10px / 20px / 10px"), None);
    }

    #[test]
    fn registered_slots_follow_property_order() {
        for name in ["column-rule-width", "row-rule-width", "rule-overlap", "row-rule-visibility-items"] {
            let index = extra::property_index(name).unwrap();
            assert!(slots(name).contains(&(extra::BASE + index)), "{name}");
        }
    }
}
