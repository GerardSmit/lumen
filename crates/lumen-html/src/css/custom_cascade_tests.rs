use super::*;

fn entry(name: &str, value: &str) -> CustomEntry {
    (CustomText::from(name), Some(CustomText::from(value)))
}

fn tokens() -> Vec<CustomEntry> {
    let mut entries = Vec::new();
    for at in 0..1000usize {
        let name = alloc::format!("--t{at}");
        let value = match at {
            0 => "4px".to_string(),
            _ if at % 50 == 0 => alloc::format!("var(--missing, var(--t{}))", at - 1),
            _ if at % 37 == 0 => alloc::format!("calc(var(--t{}) + var(--t{}))", at - 1, at / 2),
            _ if at % 11 == 0 => alloc::format!("{at}px var(--t{})", at - 5),
            _ if at % 7 == 0 => alloc::format!("var(--t{}, 1px)", at - 3),
            _ => alloc::format!("var(--t{})", at - 1),
        };
        entries.push((CustomText::from(name), Some(CustomText::from(value))));
    }
    entries.push(entry("--a", "var(--b)"));
    entries.push(entry("--b", "var(--c)"));
    entries.push(entry("--c", "var(--a)"));
    entries.push(entry("--user-of-cycle", "var(--a, 2px)"));
    entries.push(entry("--user-of-cycle-fallback", "var(--b, var(--t5))"));
    entries.push(entry("--self", "x var(--self)"));
    entries.push(entry("--fallback-cycle", "var(--missing, var(--self))"));
    entries.push(entry("--tail", "var(--a)"));
    entries.push(entry("--bad", "var(--)"));
    entries.push(entry("--unterminated", "/* var(--t1)"));
    entries.push(entry("--uses-bad", "var(--bad)"));
    entries.push(entry("--escape", "var(--t3) \\"));
    entries.push(entry("--none", "none"));
    entries.push((CustomText::from("--invalid"), None));
    entries.push(entry("--uses-invalid", "var(--invalid, 9px) var(--none)"));
    entries.push(entry("--quoted", "\"var(--t1)\" var(--t2)"));
    entries.push(entry("--comment", "var(--t4) /* c */ var(--t6)"));
    entries.push(entry("--l0", "1"));
    for level in 1..40usize {
        entries.push(entry(&alloc::format!("--l{level}"), &alloc::format!("var(--l{}) var(--l{})", level - 1, level - 1)));
    }
    entries.push(entry("--d0", "1"));
    for level in 1..45usize {
        entries.push(entry(&alloc::format!("--d{level}"), &alloc::format!("var(--d{})", level - 1)));
    }
    entries.push(entry("--top", "var(--d44)"));
    entries.push(entry("--top-fallback", "var(--d44, var(--t2))"));
    entries
}

fn run(mut entries: Vec<CustomEntry>, memoized: bool) -> (Vec<bool>, Vec<Option<Option<String>>>) {
    let index = NameIndex::new(&entries, entries.len());
    let cyclic: Vec<bool> = {
        let names = IndexedCustom(&entries, &index);
        let mut memo = CycleMemo::new(entries.len());
        let mut walk = CycleWalk { memo: memoized.then_some(&mut memo), deepest: 0, tainted: false };
        entries.iter().map(|(name, value)| custom_needs_work(value)
            && custom_cycle(name, &names, &mut Vec::new(), &mut 4096, &mut walk)).collect()
    };
    for ((_, value), cyclic) in entries.iter_mut().zip(&cyclic) {
        if *cyclic { *value = None; }
    }
    let expanded = {
        let names = IndexedCustom(&entries, &index);
        let mut memo = ExpandMemo::new(entries.len());
        let mut walk = if memoized { ExpandWalk::memoized(&mut memo) } else { ExpandWalk::plain() };
        let mut stack = Vec::new();
        entries.iter().enumerate().map(|(at, (name, value))| match value.as_ref() {
            Some(value) if value.expands() => Some(expand_entry(at, name, value, &names, &mut stack, &mut walk)),
            _ => None,
        }).collect()
    };
    (cyclic, expanded)
}

#[test]
fn memoized_custom_cascade_matches_the_unmemoized_walk() {
    let entries = tokens();
    let (cyclic, expanded) = run(entries.clone(), false);
    let (memo_cyclic, memo_expanded) = run(entries.clone(), true);
    for (at, (name, _)) in entries.iter().enumerate() {
        assert_eq!(cyclic[at], memo_cyclic[at], "cycle verdict of {name}");
        assert_eq!(expanded[at], memo_expanded[at], "expansion of {name}");
    }
    let position = |wanted: &str| entries.iter().position(|(name, _)| name.as_str() == wanted).unwrap();
    for cycle in ["--a", "--b", "--c", "--self", "--bad", "--unterminated"] {
        assert!(memo_cyclic[position(cycle)], "{cycle}");
    }
    assert!(!memo_cyclic[position("--t5")]);
    assert!(memo_cyclic[position("--l39")], "tree walk beyond the budget");
    assert!(memo_cyclic[position("--top")], "chain deeper than the nesting limit");
    assert_eq!(memo_expanded[position("--t1")], Some(Some("4px".to_string())));
    assert_eq!(memo_expanded[position("--none")], None);
    assert_eq!(memo_expanded[position("--uses-invalid")].clone().flatten().as_deref().map(str::trim), Some("9px none"));
}

#[test]
fn memoized_expansion_is_independent_of_entry_order() {
    let mut entries = tokens();
    let (_, expected) = run(entries.clone(), false);
    let names: Vec<_> = entries.iter().map(|(name, _)| name.clone()).collect();
    entries.reverse();
    let (_, reversed) = run(entries, true);
    for (at, name) in names.iter().enumerate() {
        assert_eq!(expected[at], reversed[names.len() - 1 - at], "expansion of {name}");
    }
}

#[test]
fn layered_custom_cascade_memoizes_the_overlay_only() {
    let base = CustomProperties::flat(alloc::vec![entry("--inherited", "7px")]);
    let overlay = alloc::vec![
        entry("--x", "var(--inherited) var(--y)"),
        entry("--y", "var(--inherited, 1px) var(--z)"),
        entry("--z", "var(--x)"),
        entry("--w", "var(--inherited) var(--inherited)"),
    ];
    let index = NameIndex::new(&overlay, 32);
    let names = LayeredCustom { overlay: &overlay, index: &index, base: Some(&base) };
    for memoized in [false, true] {
        let mut memo = CycleMemo::new(overlay.len());
        let mut walk = CycleWalk { memo: memoized.then_some(&mut memo), deepest: 0, tainted: false };
        let cyclic: Vec<bool> = overlay.iter().map(|(name, _)| custom_cycle(name, &names, &mut Vec::new(), &mut 4096, &mut walk)).collect();
        assert_eq!(cyclic, [true, true, true, false]);
        let mut memo = ExpandMemo::new(overlay.len());
        let mut walk = if memoized { ExpandWalk::memoized(&mut memo) } else { ExpandWalk::plain() };
        let expanded = expand_entry(3, "--w", "var(--inherited) var(--inherited)", &names, &mut Vec::new(), &mut walk);
        assert_eq!(expanded.as_deref(), Some("7px 7px"));
    }
}
