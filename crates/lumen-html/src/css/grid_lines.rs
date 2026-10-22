//! Grid placement longhands (`grid-row-start` and friends) and the
//! `grid-row`, `grid-column` and `grid-area` shorthands over them (CSS Grid
//! Layout 2 section 8). The four longhands are the source of truth; the legacy
//! per-axis placement fields that layout consumes are mirrored from them.
use super::*;
use super::extra_props as extra;

const ROW_START: &str = "grid-row-start";
const COLUMN_START: &str = "grid-column-start";
const ROW_END: &str = "grid-row-end";
const COLUMN_END: &str = "grid-column-end";

pub(crate) fn is_shorthand(name: &str) -> bool {
    matches!(name, "grid-area" | "grid-row" | "grid-column")
}

pub(crate) fn longhands(name: &str) -> &'static [&'static str] {
    match name {
        "grid-area" => &[ROW_START, COLUMN_START, ROW_END, COLUMN_END],
        "grid-row" => &[ROW_START, ROW_END],
        "grid-column" => &[COLUMN_START, COLUMN_END],
        _ => &[],
    }
}

fn canonical_integer(word: &str) -> String {
    let negative = word.starts_with('-');
    let digits = word.trim_start_matches(['+', '-']).trim_start_matches('0');
    let mut text = String::new();
    if negative {
        text.push('-');
    }
    text.push_str(digits);
    text
}

/// Canonical form of one `<grid-line>`: `auto`, a custom identifier, or
/// `[<integer> || <custom-ident>]` / `span && [<integer> || <custom-ident>]`
/// with the integer first and an implied `span 1` dropped beside a name.
pub(crate) fn line(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.eq_ignore_ascii_case("auto") {
        return Some(String::from("auto"));
    }
    if !grid_line_spec(raw, 1) {
        return None;
    }
    let (mut span, mut integer, mut name) = (false, None, None);
    for word in grid_words(raw)? {
        if word.eq_ignore_ascii_case("span") {
            span = true;
        } else if grid_integer(word).is_some() {
            integer = Some(canonical_integer(word));
        } else {
            name = Some(word);
        }
    }
    let mut out = String::new();
    if span {
        out.push_str("span");
    }
    if let Some(integer) = integer.filter(|integer| !(span && name.is_some() && integer == "1")) {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&integer);
    }
    if let Some(name) = name {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(name);
    }
    Some(out)
}

fn is_identifier(line: &str) -> bool {
    line != "auto" && !line.contains(' ') && grid_integer(line).is_none()
}

fn omitted(start: &str) -> &str {
    if is_identifier(start) { start } else { "auto" }
}

pub(crate) fn expand(name: &str, raw: &str) -> Option<Vec<(&'static str, String)>> {
    let max = if name == "grid-area" { 4 } else { 2 };
    let mut lines = Vec::new();
    for segment in grid_line_segments(raw.trim(), max)? {
        lines.push(line(segment)?);
    }
    let first = lines.first()?.clone();
    let line_at = |index: usize, fallback: &str| lines.get(index).cloned().unwrap_or_else(|| fallback.into());
    Some(match name {
        "grid-row" => alloc::vec![(ROW_START, first.clone()), (ROW_END, line_at(1, omitted(&first)))],
        "grid-column" => alloc::vec![(COLUMN_START, first.clone()), (COLUMN_END, line_at(1, omitted(&first)))],
        "grid-area" => {
            let column_start = line_at(1, omitted(&first));
            let row_end = line_at(2, omitted(&first));
            let column_end = line_at(3, omitted(&column_start));
            alloc::vec![
                (ROW_START, first),
                (COLUMN_START, column_start),
                (ROW_END, row_end),
                (COLUMN_END, column_end),
            ]
        }
        _ => return None,
    })
}

pub(crate) fn shorthand_declarations(name: &str, raw: &str) -> Option<Vec<Value>> {
    let mut values = Vec::new();
    if name == "grid-area" {
        let segments = grid_line_segments(raw.trim(), 4)?;
        let named = match segments.as_slice() {
            [single] => grid_words(single)
                .filter(|words| words.len() == 1 && grid_identifier(words[0]))
                .map(|words| Arc::from(words[0])),
            _ => None,
        };
        values.push(Value::GridArea(named));
    }
    for (longhand, text) in expand(name, raw)? {
        values.extend(extra::declarations(longhand, &text)?);
    }
    Some(values)
}

/// Shortest serialization: trailing lines equal to what omission would imply
/// are dropped, working from the last line backwards.
pub(crate) fn synthesize(name: &str, values: &[&str]) -> Option<String> {
    match (name, values) {
        ("grid-row" | "grid-column", &[start, end]) => Some(if end == omitted(start) {
            start.into()
        } else {
            alloc::format!("{start} / {end}")
        }),
        ("grid-area", &[row_start, column_start, row_end, column_end]) => {
            let mut parts = alloc::vec![row_start, column_start, row_end, column_end];
            if column_end == omitted(column_start) {
                parts.pop();
                if row_end == omitted(row_start) {
                    parts.pop();
                    if column_start == omitted(row_start) {
                        parts.pop();
                    }
                }
            }
            Some(parts.join(" / "))
        }
        _ => None,
    }
}

pub(crate) fn computed_shorthand(style: &Style, name: &str) -> String {
    let values: Option<Vec<String>> = longhands(name).iter().map(|longhand| extra::stored(style, longhand)).collect();
    let Some(values) = values else { return String::new() };
    let refs: Vec<&str> = values.iter().map(String::as_str).collect();
    synthesize(name, &refs).unwrap_or_default()
}

fn mirror_axis(style: &mut Style, column: bool) {
    let (start_name, end_name) = if column { (COLUMN_START, COLUMN_END) } else { (ROW_START, ROW_END) };
    let start = extra::stored(style, start_name).unwrap_or_else(|| "auto".into());
    let end = extra::stored(style, end_name).unwrap_or_else(|| "auto".into());
    let joined = alloc::format!("{start} / {end}");
    let simple = grid_placement(if end == "auto" { &start } else { &joined });
    let (placement, spec) = match simple {
        Some(placement) => (placement, None),
        None => (GridPlacement { start: None, span: 1 }, Some(Arc::<str>::from(joined))),
    };
    if column {
        style.grid_column = placement;
        style.grid_column_spec = spec;
    } else {
        style.grid_row = placement;
        style.grid_row_spec = spec;
    }
}

/// Recompute the placement fields layout reads from the placement longhands,
/// and drop a named-area placement the longhands no longer all spell.
pub(crate) fn mirror(style: &mut Style) {
    mirror_axis(style, true);
    mirror_axis(style, false);
    if let Some(area) = style.grid_area.clone() {
        let spells_area = longhands("grid-area")
            .iter()
            .all(|longhand| extra::stored(style, longhand).is_some_and(|value| value == *area));
        if !spells_area {
            style.grid_area = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_line_serialization_is_canonical() {
        assert_eq!(line("AuTo").as_deref(), Some("auto"));
        assert_eq!(line("+90 -a-").as_deref(), Some("90 -a-"));
        assert_eq!(line("-A0 33").as_deref(), Some("33 -A0"));
        assert_eq!(line("i 2 SpAn").as_deref(), Some("span 2 i"));
        assert_eq!(line("span 1 i").as_deref(), Some("span i"));
        assert_eq!(line("span 1").as_deref(), Some("span 1"));
        assert_eq!(line("1 / 2"), None);
    }

    #[test]
    fn shorthand_expansion_and_shortest_serialization_round_trip() {
        let expanded = expand("grid-area", "i").unwrap();
        assert!(expanded.iter().all(|(_, value)| value == "i"));
        let expanded = expand("grid-area", "2").unwrap();
        assert_eq!(expanded[1].1, "auto");
        assert_eq!(synthesize("grid-row", &["i", "i"]).as_deref(), Some("i"));
        assert_eq!(synthesize("grid-row", &["i", "auto"]).as_deref(), Some("i / auto"));
        assert_eq!(synthesize("grid-column", &["10", "auto"]).as_deref(), Some("10"));
        assert_eq!(synthesize("grid-area", &["1", "auto", "2", "auto"]).as_deref(), Some("1 / auto / 2"));
        assert_eq!(synthesize("grid-area", &["i", "i", "auto", "auto"]).as_deref(), Some("i / i / auto / auto"));
        assert_eq!(synthesize("grid-area", &["auto", "auto", "auto", "auto"]).as_deref(), Some("auto"));
    }
}
