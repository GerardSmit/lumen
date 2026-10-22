//! `@font-palette-values`: prelude, descriptor grammar and CSSOM serialization.
//! Each descriptor keeps its last valid declaration.
use super::*;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FontPaletteValues {
    pub name: String,
    pub font_family: Option<String>,
    pub base_palette: Option<String>,
    pub override_colors: Vec<(u32, String)>,
}

impl FontPaletteValues {
    pub fn override_colors_text(&self) -> String {
        self.override_colors
            .iter()
            .map(|(index, color)| alloc::format!("{index} {color}"))
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub fn serialize(&self) -> String {
        let mut text = alloc::format!("@font-palette-values {} {{", self.name);
        if let Some(family) = &self.font_family {
            text.push_str(&alloc::format!(" font-family: {family};"));
        }
        if let Some(base) = &self.base_palette {
            text.push_str(&alloc::format!(" base-palette: {base};"));
        }
        if !self.override_colors.is_empty() {
            text.push_str(&alloc::format!(" override-colors: {};", self.override_colors_text()));
        }
        text.push_str(" }");
        text
    }
}

/// Serialized dashed identifier of a `@font-palette-values` prelude.
pub fn rule_name(prelude: &str) -> Option<String> {
    let tail = nesting::at_rule_tail(prelude.trim(), "@font-palette-values")?.trim();
    let mut position = 0;
    let identifier = consume_selector_identifier(tail, &mut position)?;
    (position == tail.len() && identifier.starts_with("--")).then(|| serialize_css_identifier(&identifier))
}

fn parse_family(value: &str) -> Option<String> {
    let families = parse_font_family_list(value)?;
    if families.is_empty() {
        return None;
    }
    let mut names = Vec::new();
    for family in families.iter() {
        let name = family.named()?;
        if name.is_empty() {
            return None;
        }
        names.push(name.clone());
    }
    Some(font_feature_values::serialize_family_names(&names))
}

fn parse_index(token: &str) -> Option<u32> {
    (!token.is_empty() && token.bytes().all(|byte| byte.is_ascii_digit())).then(|| token.parse().ok()).flatten()
}

fn parse_base(value: &str) -> Option<String> {
    let value = value.trim();
    if matches!(&*ascii_lower(value), "light" | "dark") {
        return Some(ascii_lower(value).into_owned());
    }
    parse_index(value.strip_prefix('+').unwrap_or(value)).map(|index| index.to_string())
}

fn color_is_context_free(color: &str) -> bool {
    let lowered = ascii_lower(color);
    if lowered.contains("light-dark(") {
        return false;
    }
    let mut word = String::new();
    let flush = |word: &mut String| {
        let blocked = super::color_scheme::is_system_color(&word);
        word.clear();
        !blocked
    };
    for character in lowered.chars() {
        if character.is_ascii_alphanumeric() || character == '-' {
            word.push(character);
        } else if !flush(&mut word) {
            return false;
        }
    }
    flush(&mut word)
}

fn parse_overrides(value: &str) -> Option<Vec<(u32, String)>> {
    let mut colors = Vec::new();
    for part in top_level_split(value, b',', usize::MAX)? {
        let part = part.trim();
        let split = part.find(|character: char| character.is_ascii_whitespace())?;
        let index = parse_index(&part[..split])?;
        let color = part[split..].trim();
        if color.is_empty() || !color_is_context_free(color) {
            return None;
        }
        colors.push((index, color_values::serialize_declared(color, 0)?));
    }
    (!colors.is_empty()).then_some(colors)
}

/// Parse a complete `@font-palette-values` rule; `None` for an invalid prelude.
pub fn parse(rule: &str) -> Option<FontPaletteValues> {
    let rule = rule.trim();
    let open = nesting::source_rule_block_open(rule)?;
    let name = rule_name(&rule[..open])?;
    let close = rule.rfind('}')?;
    let body = rule.get(open + 1..close)?;
    let mut values = FontPaletteValues { name, ..FontPaletteValues::default() };
    for (start, end) in declaration_spans(body).ok()? {
        let Some((property, raw)) = declaration_pair(&body[start..end]) else { continue };
        let (value, important) = important_value(raw);
        if important {
            continue;
        }
        let value = value.trim();
        match &*ascii_lower(property.trim()) {
            "font-family" => {
                if let Some(family) = parse_family(value) {
                    values.font_family = Some(family);
                }
            }
            "base-palette" => {
                if let Some(base) = parse_base(value) {
                    values.base_palette = Some(base);
                }
            }
            "override-colors" => {
                if let Some(colors) = parse_overrides(value) {
                    values.override_colors = colors;
                }
            }
            _ => {}
        }
    }
    Some(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_valid_descriptor_wins_and_colors_serialize() {
        let rule = parse("@font-palette-values --C { font-family: foo; font-family: bar; base-palette: 1; base-palette: \"x\"; base-palette: 2; override-colors: \"a\" #123; override-colors: 3 #123; }").unwrap();
        assert_eq!(rule.font_family.as_deref(), Some("bar"));
        assert_eq!(rule.base_palette.as_deref(), Some("2"));
        assert_eq!(rule.override_colors_text(), "3 rgb(17, 34, 51)");
    }

    #[test]
    fn invalid_values_and_context_dependent_colors_are_dropped() {
        let rule = parse("@font-palette-values --A { font-family: a, serif; base-palette: ident; override-colors: 0 currentcolor; }").unwrap();
        assert_eq!(rule.serialize(), "@font-palette-values --A { }");
        assert!(parse("@font-palette-values A { }").is_none());
    }
}
