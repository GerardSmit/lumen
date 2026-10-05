//! Official Unicode corpora, supplied by the host tooling rather than vendored
//! into the renderer. Every case is checked; failures retain source line numbers.
use std::{collections::BTreeSet, path::PathBuf};

fn corpus() -> PathBuf {
    PathBuf::from(
        std::env::var_os("LUMEN_UNICODE_CORPUS")
            .expect("run these ignored corpus tests with cargo xtask test html-corpus"),
    )
}

enum BoundaryMode {
    Grapheme,
    Word,
    Line,
}

fn boundary_cases(path: &std::path::Path, mode: BoundaryMode) {
    let source = std::fs::read_to_string(path).expect("pinned Unicode corpus missing");
    let mut cases = 0;
    let mut failures = Vec::new();
    for (line, raw) in source.lines().enumerate() {
        let raw = raw.split('#').next().unwrap().trim();
        if raw.is_empty() {
            continue;
        }
        let mut text = String::new();
        let mut expected = BTreeSet::new();
        for token in raw.split_whitespace() {
            match token {
                "÷" => {
                    expected.insert(text.len());
                }
                "×" => (),
                _ => text.push(
                    char::from_u32(u32::from_str_radix(token, 16).unwrap())
                        .expect("corpus contains a non-scalar code point"),
                ),
            }
        }
        let actual: BTreeSet<usize> = match mode {
            BoundaryMode::Grapheme => lumen_common::ucd::graphemes(&text)
                .map(|(index, _)| index)
                .chain(std::iter::once(text.len()))
                .collect(),
            BoundaryMode::Word => lumen_common::ucd::word_boundaries(&text)
                .map(|(index, _)| index)
                .chain(std::iter::once(text.len()))
                .collect(),
            BoundaryMode::Line => {
                // UAX #14 forbids a break before the first character; the iterator
                // reports only opportunities after characters, including end-of-text.
                expected.remove(&0);
                lumen_common::ucd::line_breaks(&text)
                    .map(|(index, _)| index)
                    .collect()
            }
        };
        cases += 1;
        if actual != expected {
            failures.push((line + 1, expected, actual));
        }
    }
    eprintln!(
        "{}: {} passed / {} cases",
        path.display(),
        cases - failures.len(),
        cases
    );
    for (line, _, _) in &failures {
        eprintln!("line {line}: {}", source.lines().nth(line - 1).unwrap());
    }
    assert!(
        failures.is_empty(),
        "{} failures; first twenty: {:?}",
        failures.len(),
        &failures[..failures.len().min(20)]
    );
}

#[test]
#[ignore = "requires the pinned external Unicode corpus"]
fn uax14_line_break_test_15() {
    boundary_cases(
        &corpus().join("15.0.0/auxiliary/LineBreakTest.txt"),
        BoundaryMode::Line,
    );
}

#[test]
#[ignore = "requires the pinned external Unicode corpus"]
fn uax29_grapheme_break_test_16() {
    boundary_cases(
        &corpus().join("16.0.0/auxiliary/GraphemeBreakTest.txt"),
        BoundaryMode::Grapheme,
    );
}

#[test]
#[ignore = "requires the pinned external Unicode corpus"]
fn uax29_word_break_test_16() {
    assert_eq!(lumen_common::ucd::WORD_UNICODE_VERSION, (16, 0, 0));
    boundary_cases(
        &corpus().join("16.0.0/auxiliary/WordBreakTest.txt"),
        BoundaryMode::Word,
    );
}

fn bidi_matches(
    text: &str,
    rtl: Option<bool>,
    levels: &[Option<u8>],
    order: &[usize],
    base: Option<u8>,
) -> bool {
    let info = lumen_common::bidi::resolve(text, rtl).unwrap();
    let mut resolved = info.levels.clone();
    for paragraph in &info.paragraphs {
        let reordered = info.reordered_levels(paragraph, paragraph.range.clone());
        resolved[paragraph.range.clone()].copy_from_slice(&reordered[paragraph.range.clone()]);
    }
    let characters: Vec<_> = text
        .char_indices()
        .map(|(index, _)| resolved[index])
        .collect();
    if characters.len() != levels.len() {
        return false;
    }
    let actual_order = lumen_common::bidi::BidiInfo::reorder_visual(&characters)
        .into_iter()
        .filter(|index| levels[*index].is_some())
        .collect::<Vec<_>>();
    info.paragraphs
        .iter()
        .all(|p| base.is_none_or(|base| p.level.number() == base))
        && characters
            .iter()
            .zip(levels)
            .all(|(actual, expected)| expected.is_none_or(|level| level == actual.number()))
        && actual_order == order
}

#[test]
#[ignore = "requires the pinned external Unicode corpus"]
fn uax9_bidi_test_16() {
    let path = corpus().join("16.0.0/ucd/BidiTest.txt");
    let source = std::fs::read_to_string(&path).expect("pinned bidi corpus missing");
    let mut levels = Vec::new();
    let mut order = Vec::new();
    let mut cases = 0;
    let mut failures = Vec::new();
    for (line, raw) in source.lines().enumerate() {
        let raw = raw.split('#').next().unwrap().trim();
        if raw.is_empty() {
            continue;
        }
        if let Some(raw) = raw.strip_prefix("@Levels:") {
            levels = raw
                .split_whitespace()
                .map(|level| {
                    if level == "x" {
                        None
                    } else {
                        Some(level.parse().unwrap())
                    }
                })
                .collect();
        } else if let Some(raw) = raw.strip_prefix("@Reorder:") {
            order = raw.split_whitespace().map(|v| v.parse().unwrap()).collect();
        } else if !raw.starts_with('@') {
            let (classes, bitset) = raw.split_once(';').unwrap();
            // The corpus explicitly allows representative characters for an API
            // that accepts text. These characters carry exactly the named class.
            let text: String = classes
                .split_whitespace()
                .map(|class| match class {
                    "L" => 'A',
                    "R" => '\u{05d0}',
                    "AL" => '\u{0627}',
                    "EN" => '0',
                    "ES" => '+',
                    "ET" => '$',
                    "AN" => '\u{0660}',
                    "CS" => ',',
                    "B" => '\u{2029}',
                    "S" => '\t',
                    "WS" => ' ',
                    "ON" => '!',
                    "BN" => '\u{00ad}',
                    "NSM" => '\u{0300}',
                    "LRE" => '\u{202a}',
                    "RLE" => '\u{202b}',
                    "LRO" => '\u{202d}',
                    "RLO" => '\u{202e}',
                    "PDF" => '\u{202c}',
                    "LRI" => '\u{2066}',
                    "RLI" => '\u{2067}',
                    "FSI" => '\u{2068}',
                    "PDI" => '\u{2069}',
                    _ => panic!("unknown bidi class {class}"),
                })
                .collect();
            let bitset = u8::from_str_radix(bitset.trim(), 16).unwrap();
            for (bit, rtl) in [(1, None), (2, Some(false)), (4, Some(true))] {
                if bitset & bit == 0 {
                    continue;
                }
                cases += 1;
                if !bidi_matches(&text, rtl, &levels, &order, None) {
                    failures.push((line + 1, bit));
                }
            }
        }
    }
    eprintln!(
        "{}: {} passed / {} cases",
        path.display(),
        cases - failures.len(),
        cases
    );
    assert!(
        failures.is_empty(),
        "{} failures; first twenty source lines/directions: {:?}",
        failures.len(),
        &failures[..failures.len().min(20)]
    );
}

#[test]
#[ignore = "requires the pinned external Unicode corpus"]
fn uax9_bidi_character_test_16() {
    let path = corpus().join("16.0.0/ucd/BidiCharacterTest.txt");
    let source = std::fs::read_to_string(&path).expect("pinned bidi corpus missing");
    let mut cases = 0;
    let mut failures = Vec::new();
    for (line, raw) in source.lines().enumerate() {
        let raw = raw.split('#').next().unwrap().trim();
        if raw.is_empty() {
            continue;
        }
        let fields: Vec<_> = raw.split(';').collect();
        assert_eq!(fields.len(), 5);
        let text: String = fields[0]
            .split_whitespace()
            .map(|cp| char::from_u32(u32::from_str_radix(cp, 16).unwrap()).unwrap())
            .collect();
        let rtl = match fields[1] {
            "0" => Some(false),
            "1" => Some(true),
            "2" => None,
            _ => panic!("bad direction"),
        };
        let expected_base: u8 = fields[2].parse().unwrap();
        let expected_levels: Vec<Option<u8>> = fields[3]
            .split_whitespace()
            .map(|level| {
                if level == "x" {
                    None
                } else {
                    Some(level.parse().unwrap())
                }
            })
            .collect();
        let expected_order: Vec<usize> = fields[4]
            .split_whitespace()
            .map(|v| v.parse().unwrap())
            .collect();
        let passed = bidi_matches(
            &text,
            rtl,
            &expected_levels,
            &expected_order,
            Some(expected_base),
        );
        cases += 1;
        if !passed {
            failures.push(line + 1);
        }
    }
    eprintln!(
        "{}: {} passed / {} cases",
        path.display(),
        cases - failures.len(),
        cases
    );
    assert!(
        failures.is_empty(),
        "{} failures; first twenty source lines: {:?}",
        failures.len(),
        &failures[..failures.len().min(20)]
    );
}
