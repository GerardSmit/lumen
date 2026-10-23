//! Heap ownership, shape remapping, allocator, and snapshot isolation tests.
use super::*;

struct Enter(Arc<GcState>);
impl Enter {
    fn new(state: Arc<GcState>) -> Self {
        Self(enter_gc_state(state))
    }
}
impl Drop for Enter {
    fn drop(&mut self) {
        enter_gc_state(self.0.clone());
    }
}

#[test]
fn absorb_preserves_addresses_and_rehomes_free_lists() {
    let parcel = GcState::new();
    let receiver = GcState::new();
    let objects = {
        let _entered = Enter::new(parcel.clone());
        (0..6000)
            .map(|i| match i % 7 {
                0 => Object::new_bare(None),
                1 => Object::new_with_capacity(None, 2),
                2 => Object::new_with_capacity(None, 4),
                3 => Object::new_with_capacity(None, 8),
                4 => Object::new_array_from_vec(None, vec![Value::Null; 10]),
                5 => Object::new_array_from_vec(None, vec![Value::Null; 2]),
                _ => Object::new_array_from_vec(None, vec![Value::Null; 4]),
            })
            .collect::<Vec<_>>()
    };
    let addresses = objects.iter().map(Gc::as_ptr).collect::<Vec<_>>();
    // Free slots in several chunks before moving: both lists must remain usable.
    let mut kept = Vec::new();
    for (index, object) in objects.into_iter().enumerate() {
        if index % 3 == 0 {
            kept.push(object);
        }
    }
    assert_eq!(parcel.counters.live().get(), 2000);
    let _entered = Enter::new(receiver.clone());
    let existing = Object::new(None);
    receiver.heap.absorb(&parcel.heap, receiver.counters.live());
    receiver.counters.live()
        .set(receiver.counters.live().get() + parcel.counters.live().replace(0));
    assert_eq!(parcel.heap.chunk_count(), 0);
    assert_eq!(live_objects(), 2001);
    assert!(
        kept.iter()
            .enumerate()
            .all(|(i, object)| Gc::as_ptr(object) == addresses[i * 3])
    );
    let mut walked = 0;
    receiver.heap.for_each_live(|_| walked += 1);
    assert_eq!(walked, 2001);
    drop(parcel);
    let more = (0..6000).map(|_| Object::new(None)).collect::<Vec<_>>();
    assert_eq!(live_objects(), 8001);
    drop(kept);
    drop(more);
    drop(existing);
    assert_eq!(live_objects(), 0);
    receiver.heap.trim(1);
    assert_eq!(receiver.heap.chunk_count(), 7);
}

#[test]
fn fastalloc_frees_sender_allocations_on_receiver_thread() {
    use std::alloc::{GlobalAlloc, Layout};
    let allocator = crate::fastalloc::ClassAlloc;
    let blocks = (1..=128)
        .map(|n| {
            let layout = Layout::from_size_align(n * 17, 8).unwrap();
            let pointer = unsafe { allocator.alloc(layout) };
            assert!(!pointer.is_null());
            unsafe {
                pointer.write_bytes(0x5a, layout.size());
            }
            (pointer as usize, layout)
        })
        .collect::<Vec<_>>();
    std::thread::spawn(move || {
        for (address, layout) in blocks {
            let pointer = address as *mut u8;
            let bytes = unsafe { std::slice::from_raw_parts(pointer, layout.size()) };
            assert!(bytes.iter().all(|byte| *byte == 0x5a));
            unsafe {
                allocator.dealloc(pointer, layout);
            }
        }
        crate::fastalloc::trim();
    })
    .join()
    .unwrap();
}

#[test]
fn remapped_shapes_survive_interpreter_bytecode_and_collection() {
    let mut engine = crate::Engine::new();
    let receiver = gc_state_handle();
    // Occupy ids in R so a parcel id is known to collide with unrelated keys.
    engine
        .eval("var unrelated = {a: 1, b: 2, c: 3};", false)
        .unwrap();
    let parcel = GcState::new();
    let (shared, shared_again, owned, cycle) = {
        let _entered = Enter::new(parcel.clone());
        let shared = Object::new(None);
        set_data(&shared, "value", Value::Num(42.0));
        let shared_again = Object::new(None);
        set_data(&shared_again, "value", Value::Num(7.0));
        let owned = Object::new(None);
        set_data(&owned, "removed", Value::Null);
        set_data(&owned, "value", Value::Num(13.0));
        owned.borrow_mut().props.remove("removed");
        assert!(owned.borrow().props.census().owned_shape_keys > 0);
        let cycle = Object::new(None);
        set_data(&cycle, "self", Value::Obj(cycle.clone()));
        (shared, shared_again, owned, cycle)
    };
    let mut memo = std::collections::HashMap::new();
    for object in [&shared, &shared_again, &owned, &cycle] {
        object.borrow_mut().props.remap_shape(&mut memo);
    }
    assert_eq!(
        shared.borrow().props.shape,
        shared_again.borrow().props.shape
    );
    assert_ne!(shared.borrow().props.shape, owned.borrow().props.shape);
    receiver.heap.absorb(&parcel.heap, receiver.counters.live());
    receiver.counters.live()
        .set(receiver.counters.live().get() + parcel.counters.live().replace(0));
    drop(parcel);
    let global = engine.interp.global.clone();
    set_data(&global, "parcel", Value::Obj(shared));
    set_data(&global, "parcelOwned", Value::Obj(owned));
    for tier in [
        crate::bytecode::Tier::Interp,
        crate::bytecode::Tier::Bytecode,
    ] {
        engine.set_tier(tier);
        let result = engine.eval("function read(x) { return x.value; } var sum = 0; for (var i=0; i<300; i++) sum += read(parcel) + read(parcelOwned); sum", false).unwrap();
        assert!(matches!(result, crate::Completion::Value(ref value) if value == "16500"));
        engine.collect_garbage();
    }
    // A cycle in a moved chunk is swept by the receiver's collector.
    let before = live_objects();
    drop(cycle);
    engine.collect_garbage();
    assert!(live_objects() < before);
}

#[test]
fn function_snapshot_owns_source_and_resets_tier_state() {
    let source = String::from("function copied(x) { return x * 3 + 1; }");
    let body = crate::parser::parse_script_lazy(&source).unwrap();
    let crate::ast::Stmt::FuncDecl(sender) = &body[0] else {
        panic!("function")
    };
    sender.calls.set(777);
    sender.ensure_body().unwrap();
    let snapshot = crate::snapshot::encode(&body, &source);
    drop(body);
    // Only owned bytes and text cross the thread; decode creates fresh Rc nodes.
    std::thread::spawn(move || {
        let decoded = crate::snapshot::decode(&snapshot, &source).unwrap();
        let crate::ast::Stmt::FuncDecl(function) = &decoded[0] else { panic!("function") };
        assert_eq!(function.calls.get(), 0);
        assert!(function.parsed_body().is_none());
        function.ensure_body().unwrap();
        assert!(function.release_cold_body());
        assert!(function.parsed_body().is_none());
        function.ensure_body().unwrap();
        let mut engine = crate::Engine::new();
        engine.eval_snapshot(&snapshot, &source, false).unwrap();
        engine.set_tier(crate::bytecode::Tier::Bytecode);
        assert!(matches!(engine.eval("copied.toString()", false).unwrap(), crate::Completion::Value(value) if value == source));
        assert!(matches!(engine.eval("var answer; for (var i=0;i<300;i++) answer=copied(7); answer", false).unwrap(), crate::Completion::Value(value) if value == "22"));
        engine.collect_garbage();
        engine.collect_garbage();
        assert!(matches!(engine.eval("copied(13)", false).unwrap(), crate::Completion::Value(value) if value == "40"));
    }).join().unwrap();
}

#[test]
fn getters_run_in_sender_heap_and_partial_parcel_cleans_up() {
    let mut engine = crate::Engine::new();
    let sender = gc_state_handle();
    engine.eval("var graph={}; Object.defineProperty(graph,'value',{enumerable:true,get:function(){graph.added={v:7}; return 42;}}); Object.defineProperty(graph,'fail',{enumerable:true,get:function(){graph.other={}; throw Error('getter failure');}});", false).unwrap();
    let global = Value::Obj(engine.interp.global.clone());
    let graph = engine
        .interp
        .get_member(&global, "graph")
        .unwrap_or_else(|_| panic!("graph lookup"));
    let parcel = GcState::new();
    let root = {
        let _entered = Enter::new(parcel.clone());
        Object::new(None)
    };
    let before = sender.counters.live().get();
    let copied = engine
        .interp
        .get_member(&graph, "value")
        .unwrap_or_else(|_| panic!("getter failed"));
    assert!(
        sender.counters.live().get() > before,
        "getter allocation stayed in sender"
    );
    assert_eq!(parcel.counters.live().get(), 1);
    {
        let _entered = Enter::new(parcel.clone());
        set_data(&root, "value", copied);
    }
    assert!(engine.interp.get_member(&graph, "fail").is_err());
    assert_eq!(
        parcel.counters.live().get(),
        1,
        "throwing getter did not allocate in parcel"
    );
    {
        let _entered = Enter::new(parcel.clone());
        drop(root);
        assert_eq!(live_objects(), 0);
    }
    assert!(Arc::ptr_eq(&gc_state_handle(), &sender));
}

#[test]
fn mid_inline_class_holds_five_and_six_properties_in_the_box() {
    assert_eq!(heap::SlotClass::inline_for(5), heap::SlotClass::Inline6);
    assert_eq!(heap::SlotClass::inline_for(6), heap::SlotClass::Inline6);
    assert_eq!(heap::SlotClass::inline_for(7), heap::SlotClass::Inline8);
    assert!(heap::SlotClass::Inline6.size() < heap::SlotClass::Inline8.size());
    let _entered = Enter::new(GcState::new());
    for wanted in [5usize, 6, 7, 8, 9] {
        let object = Object::new_with_capacity(None, wanted);
        for i in 0..wanted {
            object.borrow_mut().props.insert(
                format!("k{i}"),
                Property::data(Value::Num(i as f64), true, true, true),
            );
        }
        let borrowed = object.borrow();
        for i in 0..wanted {
            let got = borrowed.props.get(&format!("k{i}")).map(|p| p.value());
            assert!(matches!(got, Some(Value::Num(n)) if n == i as f64));
        }
        let keys: Vec<_> = borrowed.props.keys().into_iter().map(|k| k.to_string()).collect();
        let expected: Vec<_> = (0..wanted).map(|i| format!("k{i}")).collect();
        assert_eq!(keys, expected);
    }
}

fn num_of(v: Value) -> f64 {
    match v {
        Value::Num(n) => n,
        _ => panic!("not a number"),
    }
}

fn used_slots() -> usize {
    with_gc_state(|state| state.heap.census().iter().map(|c| c.1).sum())
}

#[test]
fn compaction_keeps_every_property_and_objects_stay_mutable() {
    let _entered = Enter::new(GcState::new());
    let getter = Object::new(None);
    let objects: Vec<Gc> = (0..40usize)
        .map(|n| {
            let o = Object::new_bare(None);
            {
                let mut b = o.borrow_mut();
                for i in 0..9 + n % 5 {
                    b.props.insert(
                        format!("k{i}"),
                        Property::data(Value::Num((i + n) as f64), i % 2 == 0, i % 3 == 0, i % 4 != 0),
                    );
                }
                b.props.insert(
                    "acc",
                    Property::accessor_prop(Some(Value::Obj(getter.clone())), None, true, false),
                );
                b.props.insert("odd key!", Property::plain(Value::Num(-1.0)));
            }
            o
        })
        .collect();
    let before: Vec<(usize, usize)> = objects
        .iter()
        .map(|o| {
            let c = o.borrow().props.census();
            (c.entries_len, c.entries_cap)
        })
        .collect();
    assert!(before.iter().any(|&(len, cap)| cap > len));
    assert!(gc_compact_storage() > 0);
    for (n, o) in objects.iter().enumerate() {
        let b = o.borrow();
        let c = b.props.census();
        assert_eq!(c.entries_len, before[n].0);
        assert_eq!(c.entries_cap, c.entries_len);
        let count = 9 + n % 5;
        let mut expected: Vec<String> = (0..count).map(|i| format!("k{i}")).collect();
        expected.extend(["acc".to_string(), "odd key!".to_string()]);
        let keys: Vec<String> = b.props.keys().iter().map(|k| k.to_string()).collect();
        assert_eq!(keys, expected);
        for i in 0..count {
            let p = b.props.get(&format!("k{i}")).expect("kept");
            assert_eq!(num_of(p.value()), (i + n) as f64);
            assert_eq!(p.writable(), i % 2 == 0);
            assert_eq!(p.enumerable(), i % 3 == 0);
            assert_eq!(p.configurable(), i % 4 != 0);
        }
        let acc = b.props.get("acc").expect("accessor kept");
        assert!(acc.accessor() && acc.enumerable() && !acc.configurable());
        assert_eq!(num_of(b.props.get("odd key!").unwrap().value()), -1.0);
    }
    assert_eq!(gc_compact_storage(), 0);
    for (n, o) in objects.iter().enumerate() {
        let mut b = o.borrow_mut();
        b.props.insert("added", Property::plain(Value::Num(n as f64)));
        assert!(b.props.remove("k1"));
        assert!(b.props.get("k1").is_none());
        assert_eq!(num_of(b.props.get("added").unwrap().value()), n as f64);
        assert_eq!(num_of(b.props.get("k2").unwrap().value()), (2 + n) as f64);
        assert_eq!(b.props.keys().last().map(|k| k.to_string()).as_deref(), Some("added"));
    }
}

#[test]
fn compaction_keeps_array_elements_and_holes() {
    let _entered = Enter::new(GcState::new());
    let arr = Object::new_array_from_vec(None, (0..6).map(|i| Value::Num(i as f64)).collect());
    let small = Object::new_array_from_vec(None, vec![Value::Num(1.0), Value::Num(2.0)]);
    {
        let mut b = arr.borrow_mut();
        for i in 6..100 {
            assert!(b.props.push_array_element(Value::Num(i as f64)).is_ok());
        }
        assert!(b.props.remove("3"));
    }
    assert!(small.borrow_mut().props.push_array_element(Value::Num(3.0)).is_ok());
    assert!(gc_compact_storage() > 0);
    {
        let b = arr.borrow();
        for n in 0..100u32 {
            match b.props.get_index(n) {
                Some(p) => {
                    assert_ne!(n, 3);
                    assert_eq!(num_of(p.value()), n as f64);
                }
                None => assert_eq!(n, 3),
            }
        }
        assert_eq!(num_of(b.props.length_property().unwrap().value()), 100.0);
    }
    for i in 100..130 {
        assert!(arr.borrow_mut().props.push_array_element(Value::Num(i as f64)).is_ok());
    }
    let b = arr.borrow();
    assert_eq!(num_of(b.props.get_index(129).unwrap().value()), 129.0);
    assert_eq!(num_of(b.props.length_property().unwrap().value()), 130.0);
    let s = small.borrow();
    for (n, want) in [1.0, 2.0, 3.0].into_iter().enumerate() {
        assert_eq!(num_of(s.props.get_index(n as u32).unwrap().value()), want);
    }
}

#[test]
fn release_unused_heap_returns_chunks_and_pages_and_allocation_continues() {
    let _entered = Enter::new(GcState::new());
    let make = |n: usize| {
        let o = Object::new_with_capacity(None, 2);
        o.borrow_mut().props.insert("a", Property::plain(Value::Num(n as f64)));
        o
    };
    let mut objects: Vec<Option<Gc>> = (0..60_000).map(|n| Some(make(n))).collect();
    let chunks_full = gc_heap_chunks();
    for (n, slot) in objects.iter_mut().enumerate() {
        if n % 1000 != 0 && n >= 2000 {
            *slot = None;
        }
    }
    let live = live_objects();
    let released = gc_release_unused_heap();
    assert!(gc_heap_chunks() <= chunks_full);
    // Without page release (granule 0, e.g. Windows) only whole chunks go back, and the
    // survivors pin every chunk here.
    if heap::granule() != 0 {
        assert!(released.slab_bytes > 0);
        assert!(gc_released_slab_bytes() > 0);
    }
    assert_eq!(live_objects(), live);
    assert_eq!(used_slots() as i64, live);
    let check = |objects: &[Option<Gc>]| {
        for (n, slot) in objects.iter().enumerate() {
            if let Some(o) = slot {
                assert_eq!(num_of(o.borrow().props.get("a").unwrap().value()), n as f64);
            }
        }
    };
    check(&objects);
    objects.extend((0..60_000).map(|n| Some(make(60_000 + n))));
    assert_eq!(used_slots() as i64, live_objects());
    check(&objects);
    let second = gc_release_unused_heap();
    assert_eq!(second.slack_bytes, 0);
    check(&objects);
    drop(objects);
    assert_eq!(live_objects(), 0);
    assert_eq!(used_slots(), 0);
    gc_release_unused_heap();
    assert_eq!(gc_heap_chunks(), 0);
    let again = make(7);
    assert_eq!(num_of(again.borrow().props.get("a").unwrap().value()), 7.0);
}
