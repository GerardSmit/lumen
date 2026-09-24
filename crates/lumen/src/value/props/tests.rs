use super::shapes::{INDEX_THRESHOLD, OWNED_THRESHOLD};
use super::Props;
use crate::value::{Property, Value};

fn num(n: f64) -> Property {
    Property::plain(Value::Num(n))
}

fn value_of(props: &Props, key: &str) -> f64 {
    match props.get(key).expect("present").value() {
        Value::Num(n) => n,
        _ => panic!("not a number"),
    }
}

fn keys(props: &Props) -> Vec<String> {
    props.keys().iter().map(|k| k.to_string()).collect()
}

#[test]
fn shared_shapes_are_memoized_per_key_sequence() {
    let mut a = Props::new();
    let mut b = Props::new();
    for p in [&mut a, &mut b] {
        p.insert("x", num(1.0));
        p.insert("y", num(2.0));
    }
    assert_eq!(a.shape(), b.shape());
    assert!(a.shape_is_shared());
    assert_eq!(a.slot_of("y"), Some(1));
    assert_eq!(b.slot_of("y"), Some(1));
    let mut c = Props::new();
    c.insert("y", num(2.0));
    c.insert("x", num(1.0));
    assert_ne!(a.shape(), c.shape());
    assert_eq!(c.slot_of("y"), Some(0));
}

#[test]
fn delete_detaches_to_an_owned_shape_and_reids_every_mutation() {
    let mut a = Props::new();
    for k in ["x", "y", "z"] {
        a.insert(k, num(1.0));
    }
    let shared = a.shape();
    assert!(a.remove("y"));
    let owned = a.shape();
    assert_ne!(owned, shared);
    assert!(!a.shape_is_shared());
    assert_eq!(keys(&a), ["x", "z"]);
    assert_eq!(a.slot_of("z"), Some(1));
    // A stale (shared-shape, slot 2) cache entry can never validate against this map again.
    a.insert("w", num(4.0));
    assert_ne!(a.shape(), owned);
    assert!(!a.shape_is_shared());
    assert_eq!(keys(&a), ["x", "z", "w"]);
    assert_eq!(a.slot_of("w"), Some(2));
    assert!(!a.remove("nope"));
    // A second map with the same key history gets its own owned shape.
    let mut b = Props::new();
    for k in ["x", "y", "z"] {
        b.insert(k, num(1.0));
    }
    b.remove("y");
    assert_ne!(a.shape(), b.shape());
}

#[test]
fn dictionary_growth_leaves_the_transition_tree_past_the_threshold() {
    let mut p = Props::new();
    let mut ids = Vec::new();
    for i in 0..(OWNED_THRESHOLD + 8) {
        p.insert(format!("k{i}"), num(i as f64));
        ids.push(p.shape());
        assert_eq!(p.shape_is_shared(), i < OWNED_THRESHOLD);
    }
    assert_eq!(p.census().owned_shape_keys, OWNED_THRESHOLD + 8);
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), ids.len(), "every insert changes the shape id");
    for i in 0..(OWNED_THRESHOLD + 8) {
        assert_eq!(value_of(&p, &format!("k{i}")), i as f64);
        assert_eq!(p.slot_of(&format!("k{i}")), Some(i));
    }
    assert_eq!(p.slot_of("k999"), None);
    assert_eq!(
        keys(&p),
        (0..(OWNED_THRESHOLD + 8))
            .map(|i| format!("k{i}"))
            .collect::<Vec<_>>()
    );
}

#[test]
fn large_shared_shapes_answer_through_the_hash_index() {
    let mut p = Props::new();
    for i in 0..(INDEX_THRESHOLD * 2) {
        p.insert(format!("p{i}"), num(i as f64));
    }
    assert!(p.shape_is_shared());
    for i in 0..(INDEX_THRESHOLD * 2) {
        assert_eq!(p.slot_of(&format!("p{i}")), Some(i));
    }
    assert_eq!(p.slot_of("p"), None);
    assert_eq!(p.slot_of(&format!("p{}", INDEX_THRESHOLD * 2)), None);
}

#[test]
fn array_named_insert_displaces_the_element_at_its_slot() {
    let mut p = Props::new();
    p.mark_array();
    p.insert("length", num(0.0));
    let shape = p.shape();
    for n in 0..3 {
        p.insert(n.to_string(), num(n as f64));
    }
    assert_eq!(p.shape(), shape, "elements never transition an array shape");
    assert_eq!(p.slot_of("length"), Some(0));
    p.insert("tag", num(9.0));
    assert_ne!(p.shape(), shape);
    assert_eq!(p.slot_of("tag"), Some(1));
    for n in 0..3 {
        assert_eq!(value_of(&p, &n.to_string()), n as f64);
        assert!(p.slot_of(&n.to_string()).unwrap() >= 2);
    }
    assert_eq!(keys(&p), ["0", "1", "2", "length", "tag"]);
    let entries: Vec<(String, f64)> = p
        .iter()
        .map(|(k, v)| {
            (
                k.to_string(),
                match v.value() {
                    Value::Num(n) => n,
                    _ => unreachable!(),
                },
            )
        })
        .collect();
    assert_eq!(
        entries,
        [
            ("length".to_string(), 0.0),
            ("tag".to_string(), 9.0),
            ("0".to_string(), 0.0),
            ("1".to_string(), 1.0),
            ("2".to_string(), 2.0)
        ]
    );
    // Removing an element leaves the shape alone; removing a named key detaches.
    let named = p.shape();
    assert!(p.remove("1"));
    assert_eq!(p.shape(), named);
    assert_eq!(keys(&p), ["0", "2", "length", "tag"]);
    assert!(p.remove("tag"));
    assert_ne!(p.shape(), named);
    assert_eq!(keys(&p), ["0", "2", "length"]);
    assert_eq!(value_of(&p, "2"), 2.0);
}

#[test]
fn far_indices_become_named_keys_on_arrays() {
    let mut p = Props::new();
    p.mark_array();
    p.insert("length", num(0.0));
    let shape = p.shape();
    p.insert("100000", num(5.0));
    assert_ne!(p.shape(), shape);
    assert_eq!(value_of(&p, "100000"), 5.0);
    assert_eq!(
        keys(&p),
        ["length", "100000"],
        "named keys stay in insertion order"
    );
    assert_eq!(
        p.ordered_keys()
            .iter()
            .map(|k| k.to_string())
            .collect::<Vec<_>>(),
        ["100000", "length"]
    );
}

#[test]
fn truncation_removes_named_and_element_indices_in_one_pass() {
    let mut p = Props::new();
    p.mark_array();
    p.insert("length", num(0.0));
    for n in 0..5 {
        p.insert(n.to_string(), num(n as f64));
    }
    p.insert("name", num(7.0));
    p.insert("500", num(8.0));
    let shape = p.shape();
    p.remove_indices_from(2);
    assert_ne!(p.shape(), shape, "a named index key was removed");
    assert_eq!(keys(&p), ["0", "1", "length", "name"]);
    assert_eq!(value_of(&p, "1"), 1.0);
    assert_eq!(value_of(&p, "name"), 7.0);
    assert!(p.get("500").is_none());
    assert!(p.get("3").is_none());
    p.insert("3", num(3.0));
    assert_eq!(keys(&p), ["0", "1", "3", "length", "name"]);
}

#[test]
fn cloning_an_owned_shape_gives_each_map_its_own_key_list() {
    let mut a = Props::new();
    for k in ["x", "y", "z"] {
        a.insert(k, num(1.0));
    }
    a.remove("y");
    let mut b = a.clone();
    assert_ne!(a.shape(), b.shape());
    assert_eq!(keys(&b), ["x", "z"]);
    b.insert("q", num(2.0));
    assert_eq!(keys(&a), ["x", "z"]);
    assert_eq!(keys(&b), ["x", "z", "q"]);
    assert_eq!(a.slot_of("q"), None);
    // Shared shapes clone by reference.
    let mut c = Props::new();
    c.insert("x", num(1.0));
    let d = c.clone();
    assert_eq!(c.shape(), d.shape());
    assert!(d.shape_is_shared());
}

#[test]
fn instantiate_plain_keeps_the_template_shape() {
    let mut template = Props::new();
    template.insert("a", num(0.0));
    template.insert("b", num(0.0));
    let inst = template.instantiate_plain([Value::Num(1.0), Value::Num(2.0)].into_iter());
    assert_eq!(inst.shape(), template.shape());
    assert_eq!(value_of(&inst, "a"), 1.0);
    assert_eq!(value_of(&inst, "b"), 2.0);
    assert_eq!(keys(&inst), ["a", "b"]);
}

#[test]
fn shared_shapes_store_one_key_each_and_materialise_lists_on_demand() {
    // A shape allocation must not grow back toward a flat key list per shape.
    assert!(std::mem::size_of::<super::Shape>() <= 64);
    // Checked on this map's own shape chain rather than by diffing whole-table censuses, so
    // nothing else touching the shape table can disturb it.
    fn flat_lists(p: &Props) -> Vec<usize> {
        let leaf = p.shape_rc.as_ref().expect("a shared shape");
        leaf.chain()
            .filter(|(s, _)| s.flat_built())
            .map(|(s, _)| s.len())
            .collect()
    }
    let mut p = Props::new();
    let names: Vec<String> = (0..OWNED_THRESHOLD)
        .map(|i| format!("chain{i}_{}", std::process::id()))
        .collect();
    for (i, k) in names.iter().enumerate() {
        p.insert(k.clone(), num(i as f64));
        // Lookups (hits and misses) never build the ordered list.
        assert_eq!(p.slot_of(k), Some(i));
        assert_eq!(p.slot_of(&names[0]), Some(0));
        assert_eq!(p.slot_of("absent"), None);
    }
    {
        // One shared shape per insert, each storing only its own key.
        let leaf = p.shape_rc.as_ref().expect("a shared shape");
        assert!(!leaf.owned());
        let chain: Vec<_> = leaf.chain().collect();
        assert_eq!(chain.len(), OWNED_THRESHOLD);
        for (depth, (s, key)) in chain.iter().enumerate() {
            assert_eq!(s.len(), OWNED_THRESHOLD - depth);
            assert_eq!(&***key, names[OWNED_THRESHOLD - 1 - depth].as_str());
        }
    }
    assert_eq!(flat_lists(&p), Vec::<usize>::new(), "no iteration yet");
    assert_eq!(keys(&p), names);
    assert_eq!(flat_lists(&p), vec![OWNED_THRESHOLD], "only the iterated leaf");
    // The materialised list answers the same lookups.
    for (i, k) in names.iter().enumerate() {
        assert_eq!(p.slot_of(k), Some(i));
    }
    assert_eq!(p.slot_of("absent"), None);
}

#[test]
fn chain_shapes_answer_key_at_and_special_slots_without_a_list() {
    let mut p = Props::new();
    p.insert("a", num(1.0));
    p.insert("length", num(2.0));
    p.insert("b", num(3.0));
    p.insert("prototype", num(4.0));
    let s = p.shape_rc.as_ref().unwrap();
    assert_eq!(s.len(), 4);
    assert_eq!(&**s.key_at(0), "a");
    assert_eq!(&**s.key_at(1), "length");
    assert_eq!(&**s.key_at(3), "prototype");
    assert_eq!(s.last_key().map(|k| &**k), Some("prototype"));
    assert_eq!(p.slot_of("length"), Some(1));
    assert_eq!(p.prototype_slot(), Some(3));
    // An owned copy (a delete) carries the memos and the full ordered list.
    assert!(p.remove("a"));
    assert!(!p.shape_is_shared());
    assert_eq!(keys(&p), ["length", "b", "prototype"]);
    assert_eq!(p.slot_of("length"), Some(0));
    assert_eq!(p.prototype_slot(), Some(2));
    // Cloning a map on a shared chain shape shares the shape; iterating the clone builds the
    // list once for both.
    let mut q = Props::new();
    q.insert("a", num(1.0));
    q.insert("length", num(2.0));
    let r = q.clone();
    assert_eq!(q.shape(), r.shape());
    assert_eq!(keys(&r), ["a", "length"]);
    assert_eq!(r.slot_of("length"), Some(1));
    assert_eq!(r.slot_of("a"), Some(0));
}
