//! Computed `grid-template-columns/rows` text for track lists that contain an
//! integer `repeat()`, which the expanded track vectors cannot reproduce.
use super::computed_values::{named_tracks, specified_grid_tracks};
use super::*;
use alloc::{
    string::{String, ToString},
    vec::Vec,
};

pub(super) fn computed_source(raw: &str, context: LengthContext) -> Option<Arc<str>> {
    let tokens = grid_components(raw)?;
    let subgrid = tokens
        .first()
        .is_some_and(|token| token.eq_ignore_ascii_case("subgrid"));
    let body = if subgrid { &tokens[1..] } else { &tokens[..] };
    if !body.iter().any(|token| integer_repeat(token)) {
        return None;
    }
    let text = if subgrid {
        specified_grid_tracks(raw)?
    } else {
        track_list(body, context, 0)?
    };
    Some(Arc::from(text))
}

fn integer_repeat(token: &str) -> bool {
    grid_function_args(token, "repeat")
        .and_then(|args| comma_components(args, 2))
        .is_some_and(|parts| parts.len() == 2 && parts[0].trim().bytes().all(|byte| byte.is_ascii_digit()))
}

fn push_separator(output: &mut String) {
    if !output.is_empty() {
        output.push(' ');
    }
}

fn flush_names(output: &mut String, names: &mut Vec<&str>) {
    if names.is_empty() {
        return;
    }
    push_separator(output);
    output.push('[');
    output.push_str(&names.join(" "));
    output.push(']');
    names.clear();
}

fn track_list(tokens: &[&str], context: LengthContext, depth: usize) -> Option<String> {
    if depth > 1 {
        return None;
    }
    let mut output = String::new();
    let mut names: Vec<&str> = Vec::new();
    for &token in tokens {
        if let Some(inner) = token.strip_prefix('[').and_then(|value| value.strip_suffix(']')) {
            names.extend(grid_word_slices(inner, 256)?);
            continue;
        }
        flush_names(&mut output, &mut names);
        push_separator(&mut output);
        if let Some(args) = grid_function_args(token, "repeat") {
            let parts = comma_components(args, 2)?;
            if parts.len() != 2 {
                return None;
            }
            let count = parts[0].trim();
            let count = if count.eq_ignore_ascii_case("auto-fill") || count.eq_ignore_ascii_case("auto-fit") {
                count.to_ascii_lowercase()
            } else {
                count.parse::<usize>().ok()?.to_string()
            };
            let inner = track_list(&grid_components(parts[1])?, context, depth + 1)?;
            output.push_str("repeat(");
            output.push_str(&count);
            output.push_str(", ");
            output.push_str(&inner);
            output.push(')');
        } else {
            let (tracks, _, _) = grid_template(token, context)?;
            if tracks.len() != 1 {
                return None;
            }
            output.push_str(&named_tracks(&tracks, &[], None)?);
        }
    }
    flush_names(&mut output, &mut names);
    Some(output)
}
