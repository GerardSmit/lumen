use super::*;
use crate::limits::{HeapBudget, InterruptHandle, StopFlags};

fn ch(c: char) -> Node {
    Node::Char(c as u32)
}

fn build(node: Node, ngroups: usize, options: Options) -> Regex {
    Regex::build(&node, ngroups, Vec::new(), options).unwrap()
}

fn py(node: Node, ngroups: usize) -> Regex {
    build(node, ngroups, Options::python())
}

fn span(re: &Regex, text: &str, opts: ExecOptions) -> Option<(usize, usize)> {
    re.exec_str(text, opts).unwrap().and_then(|c| c[0])
}

fn find(re: &Regex, text: &str) -> Option<(usize, usize)> {
    span(re, text, ExecOptions::search(0))
}

fn groups(re: &Regex, text: &str) -> Vec<Option<(usize, usize)>> {
    re.exec_str(text, ExecOptions::search(0))
        .unwrap()
        .expect("match")
        .to_vec()
}

fn word(flavor: Flavor, negated: bool, set: BuiltinSet) -> Node {
    Node::Class(CharClass::new().with_builtin(Builtin::new(set, negated, flavor)))
}

fn plus(inner: Node) -> Node {
    Node::repeat(inner, 1, None, true)
}

fn star(inner: Node) -> Node {
    Node::repeat(inner, 0, None, true)
}

#[test]
fn atomic_group_discards_alternatives() {
    let ab = Node::concat(vec![Node::atomic(plus(ch('a'))), ch('a'), ch('b')]);
    assert_eq!(find(&py(ab, 0), "aaab"), None);
    let plain = Node::concat(vec![plus(ch('a')), ch('a'), ch('b')]);
    assert_eq!(find(&py(plain, 0), "aaab"), Some((0, 4)));
}

#[test]
fn atomic_group_keeps_first_alternative_only() {
    let re = py(
        Node::concat(vec![
            Node::atomic(Node::alt(vec![ch('a'), Node::literal("ab")])),
            ch('c'),
        ]),
        0,
    );
    assert_eq!(find(&re, "abc"), None);
    assert_eq!(find(&re, "ac"), Some((0, 2)));
}

#[test]
fn atomic_group_captures_survive_and_unwind() {
    let re = py(
        Node::concat(vec![
            Node::atomic(Node::capture(1, plus(ch('a')))),
            Node::alt(vec![ch('x'), ch('b')]),
        ]),
        1,
    );
    assert_eq!(groups(&re, "aab"), vec![Some((0, 3)), Some((0, 2))]);
}

#[test]
fn possessive_quantifiers() {
    let single = Node::concat(vec![Node::possessive(ch('a'), 0, None), ch('a')]);
    assert_eq!(find(&py(single, 0), "aaa"), None);

    let body = Node::concat(vec![
        Node::possessive(Node::literal("ab"), 1, None),
        Node::literal("ab"),
    ]);
    assert_eq!(find(&py(body, 0), "ababab"), None);

    let ok = Node::concat(vec![Node::possessive(ch('a'), 1, Some(2)), ch('b')]);
    assert_eq!(find(&py(ok, 0), "aab"), Some((0, 3)));
    let capped = Node::concat(vec![Node::possessive(ch('a'), 1, Some(2)), ch('a')]);
    assert_eq!(find(&py(capped, 0), "aaa"), Some((0, 3)));
}

#[test]
fn conditional_group_by_number() {
    let re = py(
        Node::concat(vec![
            Node::repeat(Node::capture(1, ch('a')), 0, Some(1), true),
            Node::conditional(1, ch('b'), ch('c')),
        ]),
        1,
    );
    assert_eq!(find(&re, "ab"), Some((0, 2)));
    assert_eq!(find(&re, "c"), Some((0, 1)));
    assert_eq!(find(&re, "ac"), Some((1, 2)));
    assert_eq!(find(&re, "b"), None);
}

#[test]
fn conditional_without_else_branch() {
    let re = py(
        Node::concat(vec![
            Node::repeat(Node::capture(1, ch('<')), 0, Some(1), true),
            ch('x'),
            Node::conditional(1, ch('>'), Node::Empty),
        ]),
        1,
    );
    assert_eq!(find(&re, "<x>"), Some((0, 3)));
    assert_eq!(find(&re, "x"), Some((0, 1)));
    assert_eq!(find(&re, "<x"), Some((1, 2)));
}

#[test]
fn conditional_rejects_unknown_group() {
    let node = Node::conditional(2, ch('a'), ch('b'));
    assert!(Regex::build(&node, 1, Vec::new(), Options::python()).is_err());
}

#[test]
fn absolute_anchors_ignore_multiline() {
    let mut opts = Options::python();
    opts.multiline = true;
    let re = build(
        Node::concat(vec![Node::StartText, Node::literal("ab"), Node::EndText]),
        0,
        opts,
    );
    assert_eq!(find(&re, "ab"), Some((0, 2)));
    assert_eq!(find(&re, "x\nab"), None);
    assert_eq!(find(&re, "ab\n"), None);
    let line = build(
        Node::concat(vec![Node::Start, Node::literal("ab")]),
        0,
        opts,
    );
    assert_eq!(find(&line, "x\nab"), Some((2, 4)));
}

#[test]
fn python_dollar_matches_before_final_newline() {
    let re = py(Node::concat(vec![ch('a'), Node::End]), 0);
    assert_eq!(find(&re, "a"), Some((0, 1)));
    assert_eq!(find(&re, "a\n"), Some((0, 1)));
    assert_eq!(find(&re, "a\n\n"), None);
    assert_eq!(find(&re, "a\nb"), None);

    let mut opts = Options::python();
    opts.multiline = true;
    let multi = build(Node::concat(vec![ch('a'), Node::End]), 0, opts);
    assert_eq!(find(&multi, "a\nb"), Some((0, 1)));
    assert_eq!(find(&multi, "a\n\n"), Some((0, 1)));
    assert_eq!(find(&multi, "ab"), None);
}

#[test]
fn js_dollar_stays_strict() {
    let re = build(
        Node::concat(vec![ch('a'), Node::End]),
        0,
        Options::default(),
    );
    assert_eq!(find(&re, "a\n"), None);
    assert_eq!(find(&re, "a"), Some((0, 1)));
}

#[test]
fn unset_backref_fails_in_python_and_matches_empty_in_js() {
    let node = |dialect_node: Node| {
        Node::concat(vec![
            Node::repeat(Node::capture(1, ch('a')), 0, Some(1), true),
            dialect_node,
            ch('b'),
        ])
    };
    let python = py(node(Node::Backref(1)), 1);
    assert_eq!(find(&python, "b"), None);
    assert_eq!(find(&python, "aab"), Some((0, 3)));
    let js = build(node(Node::Backref(1)), 1, Options::default());
    assert_eq!(find(&js, "b"), Some((0, 1)));
}

#[test]
fn python_repeat_keeps_captures_of_empty_iteration() {
    let node = star(Node::capture(1, star(ch('a'))));
    let python = py(node.clone(), 1);
    assert_eq!(groups(&python, "b"), vec![Some((0, 0)), Some((0, 0))]);
    let js = build(node, 1, Options::default());
    assert_eq!(groups(&js, "b"), vec![Some((0, 0)), None]);
}

#[test]
fn python_repeat_does_not_clear_captures_between_iterations() {
    let node = star(Node::non_capture(Node::alt(vec![
        Node::capture(1, ch('a')),
        ch('b'),
    ])));
    let python = py(node.clone(), 1);
    assert_eq!(groups(&python, "ab")[1], Some((0, 1)));
    let js = build(node, 1, Options::default());
    assert_eq!(groups(&js, "ab")[1], None);
}

#[test]
fn python_ignore_case_uses_lowercase_and_equivalences() {
    let mut opts = Options::python();
    opts.ignore_case = true;
    let s = build(ch('s'), 0, opts);
    assert_eq!(find(&s, "S"), Some((0, 1)));
    assert_eq!(find(&s, "\u{17f}"), Some((0, 1)));
    let k = build(ch('k'), 0, opts);
    assert_eq!(find(&k, "\u{212a}"), Some((0, 1)));
    let dotted = build(ch('\u{130}'), 0, opts);
    assert_eq!(find(&dotted, "i"), Some((0, 1)));
    let sigma = build(ch('\u{3c3}'), 0, opts);
    assert_eq!(find(&sigma, "\u{3c2}"), Some((0, 1)));
    assert_eq!(find(&sigma, "\u{3a3}"), Some((0, 1)));
    let sharp = build(ch('\u{df}'), 0, opts);
    assert_eq!(find(&sharp, "\u{1e9e}"), Some((0, 1)));
    assert_eq!(find(&sharp, "ss"), None);
}

#[test]
fn python_ignore_case_in_classes_and_backrefs() {
    let mut opts = Options::python();
    opts.ignore_case = true;
    let class = build(
        Node::Class(CharClass::new().with_range('a' as u32, 'f' as u32)),
        0,
        opts,
    );
    assert_eq!(find(&class, "D"), Some((0, 1)));
    assert_eq!(find(&class, "G"), None);
    let negated = build(
        Node::Class(
            CharClass::new()
                .negated(true)
                .with_range('a' as u32, 'f' as u32),
        ),
        0,
        opts,
    );
    assert_eq!(find(&negated, "D"), None);
    assert_eq!(find(&negated, "G"), Some((0, 1)));
    let backref = build(
        Node::concat(vec![Node::capture(1, plus(ch('a'))), Node::Backref(1)]),
        1,
        opts,
    );
    assert_eq!(find(&backref, "aAa"), Some((0, 2)));
}

#[test]
fn python_ascii_ignore_case_does_not_use_unicode_folds() {
    let mut opts = Options::python();
    opts.ignore_case = true;
    opts.fold = CaseFold::PythonAscii;
    let s = build(ch('s'), 0, opts);
    assert_eq!(find(&s, "S"), Some((0, 1)));
    assert_eq!(find(&s, "\u{17f}"), None);
    let class = build(Node::Class(CharClass::new().with_char('k' as u32)), 0, opts);
    assert_eq!(find(&class, "K"), Some((0, 1)));
    assert_eq!(find(&class, "\u{212a}"), None);
}

#[test]
fn python_word_digit_space_definitions() {
    let digit = |flavor| py(word(flavor, false, BuiltinSet::Digit), 0);
    assert_eq!(find(&digit(Flavor::PyUnicode), "x\u{663}"), Some((1, 2)));
    assert_eq!(find(&digit(Flavor::PyAscii), "x\u{663}"), None);
    assert_eq!(find(&digit(Flavor::Js), "x\u{663}"), None);

    let word_re = |flavor| py(word(flavor, false, BuiltinSet::Word), 0);
    assert_eq!(find(&word_re(Flavor::PyUnicode), "-\u{e9}"), Some((1, 2)));
    assert_eq!(find(&word_re(Flavor::PyUnicode), "-_"), Some((1, 2)));
    assert_eq!(find(&word_re(Flavor::PyUnicode), "-\u{bd}"), Some((1, 2)));
    assert_eq!(find(&word_re(Flavor::PyUnicode), "- \u{2014}"), None);
    assert_eq!(find(&word_re(Flavor::PyAscii), "-\u{e9}"), None);
    assert_eq!(find(&word_re(Flavor::PyAscii), "-_"), Some((1, 2)));

    let space = |flavor| py(word(flavor, false, BuiltinSet::Space), 0);
    assert_eq!(find(&space(Flavor::PyUnicode), "x\u{1c}"), Some((1, 2)));
    assert_eq!(find(&space(Flavor::PyUnicode), "x\u{85}"), Some((1, 2)));
    assert_eq!(find(&space(Flavor::PyUnicode), "x\u{feff}"), None);
    assert_eq!(find(&space(Flavor::PyAscii), "x\u{a0}"), None);
    assert_eq!(find(&space(Flavor::PyAscii), "x\u{b}"), Some((1, 2)));
    assert_eq!(find(&space(Flavor::Js), "x\u{feff}"), Some((1, 2)));

    let not_word = py(word(Flavor::PyUnicode, true, BuiltinSet::Word), 0);
    assert_eq!(find(&not_word, "\u{e9}-"), Some((1, 2)));
}

#[test]
fn python_word_boundary() {
    let b = |flavor| py(Node::concat(vec![Node::WordB(true, flavor), ch('x')]), 0);
    assert_eq!(find(&b(Flavor::PyUnicode), "\u{e9}x"), None);
    assert_eq!(find(&b(Flavor::PyAscii), "\u{e9}x"), Some((1, 2)));
    let not_b = py(Node::WordB(false, Flavor::PyUnicode), 0);
    assert_eq!(find(&not_b, ""), None);
    assert_eq!(find(&not_b, "ab"), Some((1, 1)));
    let boundary = py(Node::WordB(true, Flavor::PyUnicode), 0);
    assert_eq!(find(&boundary, ""), None);
    assert_eq!(find(&boundary, " a"), Some((1, 1)));
}

#[test]
fn python_dot_excludes_only_newline() {
    let dot = py(Node::Any, 0);
    assert_eq!(find(&dot, "\n\r"), Some((1, 2)));
    let js = build(Node::Any, 0, Options::default());
    assert_eq!(find(&js, "\n\r"), None);
    let mut opts = Options::python();
    opts.dotall = true;
    assert_eq!(find(&build(Node::Any, 0, opts), "\n"), Some((0, 1)));
    let scoped = py(
        Node::Modifier {
            add: (false, false, true),
            remove: (false, false, false),
            inner: Box::new(Node::Any),
        },
        0,
    );
    assert_eq!(find(&scoped, "\n"), Some((0, 1)));
}

#[test]
fn match_mode_is_anchored_at_start() {
    let re = py(plus(ch('a')), 0);
    assert_eq!(span(&re, "baa", ExecOptions::anchored(0)), None);
    assert_eq!(span(&re, "baa", ExecOptions::anchored(1)), Some((1, 3)));
    assert_eq!(span(&re, "baa", ExecOptions::search(0)), Some((1, 3)));
}

#[test]
fn fullmatch_backtracks_to_reach_the_end() {
    let re = py(Node::alt(vec![ch('a'), Node::literal("ab")]), 0);
    assert_eq!(span(&re, "ab", ExecOptions::anchored(0)), Some((0, 1)));
    assert_eq!(span(&re, "ab", ExecOptions::full(0)), Some((0, 2)));
    assert_eq!(span(&re, "abc", ExecOptions::full(0)), None);
    let greedy = py(Node::Concat(vec![plus(ch('a')), star(ch('a'))]), 0);
    assert_eq!(span(&greedy, "aaa", ExecOptions::full(0)), Some((0, 3)));
}

#[test]
fn fullmatch_ignores_lookaround_bodies() {
    let re = py(
        Node::concat(vec![Node::lookahead(false, ch('a')), ch('a')]),
        0,
    );
    assert_eq!(span(&re, "ab", ExecOptions::anchored(0)), Some((0, 1)));
    assert_eq!(span(&re, "a", ExecOptions::full(0)), Some((0, 1)));
    assert_eq!(span(&re, "ab", ExecOptions::full(0)), None);
}

#[test]
fn end_bound_truncates_the_input() {
    let re = py(Node::concat(vec![ch('a'), Node::End]), 0);
    let bounded = ExecOptions {
        end: Some(1),
        ..ExecOptions::search(0)
    };
    assert_eq!(span(&re, "ab", bounded), Some((0, 1)));
    assert_eq!(find(&re, "ab"), None);
    let tail = py(Node::concat(vec![ch('b'), Node::EndText]), 0);
    let to_two = ExecOptions {
        end: Some(2),
        ..ExecOptions::search(0)
    };
    assert_eq!(span(&tail, "abc", to_two), Some((1, 2)));
    let literal = py(Node::literal("bc"), 0);
    let to_two_literal = ExecOptions {
        end: Some(2),
        ..ExecOptions::search(0)
    };
    assert_eq!(span(&literal, "abc", to_two_literal), None);
    let full = ExecOptions {
        end: Some(2),
        ..ExecOptions::full(0)
    };
    assert_eq!(span(&py(Node::literal("ab"), 0), "abc", full), Some((0, 2)));
}

#[test]
fn start_position_still_sees_earlier_text() {
    let line_start = py(Node::Start, 0);
    assert_eq!(span(&line_start, "ab", ExecOptions::search(1)), None);
    let behind = py(
        Node::concat(vec![Node::lookbehind(false, ch('a')), ch('b')]),
        0,
    );
    assert_eq!(span(&behind, "ab", ExecOptions::anchored(1)), Some((1, 2)));
}

#[test]
fn must_advance_rejects_an_empty_match_at_start() {
    let re = py(star(ch('a')), 0);
    let advance = |start| ExecOptions {
        must_advance: true,
        ..ExecOptions::search(start)
    };
    assert_eq!(span(&re, "aab", advance(0)), Some((0, 2)));
    assert_eq!(span(&re, "aab", advance(2)), Some((3, 3)));
    assert_eq!(span(&re, "aab", ExecOptions::search(2)), Some((2, 2)));
    let literal = py(Node::literal("a"), 0);
    assert_eq!(span(&literal, "aa", advance(0)), Some((0, 1)));
}

#[test]
fn exec_str_indexes_by_code_point() {
    let re = py(Node::capture(1, plus(ch('\u{e9}'))), 1);
    assert_eq!(
        groups(&re, "\u{1f600}\u{1f600}\u{e9}\u{e9}x"),
        vec![Some((2, 4)), Some((2, 4))]
    );
}

#[test]
fn interrupt_flag_aborts_a_match() {
    let handle = InterruptHandle::new();
    handle.interrupt();
    set_host_poll(StopFlags::from_handle(&handle), HeapBudget::NONE);
    let re = py(
        Node::concat(vec![plus(Node::capture(1, plus(ch('a')))), ch('b')]),
        1,
    );
    let subject = "a".repeat(40);
    let result = re.exec_str(&subject, ExecOptions::search(0));
    set_host_poll(StopFlags::new(), HeapBudget::NONE);
    assert!(result.is_err());
    assert_eq!(take_abort(), Abort::Interrupt);
    assert_eq!(take_abort(), Abort::None);
}

#[test]
fn deadline_flag_aborts_a_match() {
    let mut stop = StopFlags::new();
    stop.deadline_flag()
        .store(true, std::sync::atomic::Ordering::Relaxed);
    set_host_poll(stop, HeapBudget::NONE);
    let re = py(
        Node::concat(vec![plus(Node::capture(1, plus(ch('a')))), ch('b')]),
        1,
    );
    let result = re.exec_str(&"a".repeat(40), ExecOptions::search(0));
    set_host_poll(StopFlags::new(), HeapBudget::NONE);
    assert!(result.is_err());
    assert_eq!(take_abort(), Abort::Deadline);
}

#[test]
fn backtracking_budget_stops_catastrophic_patterns() {
    set_host_poll(StopFlags::new(), HeapBudget::NONE);
    let re = py(
        Node::concat(vec![plus(Node::capture(1, plus(ch('a')))), ch('b')]),
        1,
    );
    assert!(re
        .exec_str(&"a".repeat(64), ExecOptions::search(0))
        .is_err());
    assert_eq!(take_abort(), Abort::None);
}

#[test]
fn atomic_groups_inside_lookbehind() {
    let re = py(
        Node::concat(vec![
            Node::lookbehind(false, Node::atomic(Node::literal("ab"))),
            ch('c'),
        ]),
        0,
    );
    assert_eq!(find(&re, "abc"), Some((2, 3)));
    assert_eq!(find(&re, "aac"), None);
}

#[test]
fn js_front_end_still_parses_and_matches() {
    let flags = js::Flags::parse("gi").unwrap();
    let elems: Vec<char> = r"(\w+)@(\w+)".chars().collect();
    let re = js::compile(elems, &flags).unwrap();
    assert_eq!(re.ngroups, 2);
    assert_eq!(
        groups(&re, "mail Bob@Example.com"),
        vec![Some((5, 16)), Some((5, 8)), Some((9, 16)),]
    );
    assert!(js::Flags::parse("gg").is_err());
    assert!(js::compile("(".chars().collect(), &flags).is_err());
}

#[test]
fn js_overflowing_decimal_escape_never_panics_or_truncates() {
    let digits = "8".repeat(64);
    let pattern = format!("\\{digits}");

    // In Unicode mode this is a decimal back-reference, whose index cannot be represented or
    // refer to a capture in the pattern. It must be rejected, never overflow while parsing.
    let unicode = js::Flags::parse("u").unwrap();
    assert!(js::compile(pattern.chars().collect(), &unicode).is_err());

    // Annex B legacy mode interprets an out-of-range decimal escape as an identity escape followed
    // by the remaining literal digits, rather than truncating it into some unrelated capture.
    let legacy = js::Flags::parse("").unwrap();
    let re = js::compile(pattern.chars().collect(), &legacy).unwrap();
    assert_eq!(find(&re, &digits), Some((0, digits.len())));
}

#[test]
fn js_unicode_flag_selects_full_folding() {
    let flags = js::Flags::parse("iu").unwrap();
    let re = js::compile("\u{17f}".chars().collect(), &flags).unwrap();
    assert_eq!(find(&re, "S"), Some((0, 1)));
    let legacy = js::compile("\u{17f}".chars().collect(), &js::Flags::parse("i").unwrap()).unwrap();
    assert_eq!(find(&legacy, "S"), None);
}
