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
    assert_eq!(parcel.live.get(), 2000);
    let _entered = Enter::new(receiver.clone());
    let existing = Object::new(None);
    receiver.heap.absorb(&parcel.heap, &receiver.live);
    receiver
        .live
        .set(receiver.live.get() + parcel.live.replace(0));
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
    receiver.heap.absorb(&parcel.heap, &receiver.live);
    receiver
        .live
        .set(receiver.live.get() + parcel.live.replace(0));
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
    let before = sender.live.get();
    let copied = engine
        .interp
        .get_member(&graph, "value")
        .unwrap_or_else(|_| panic!("getter failed"));
    assert!(
        sender.live.get() > before,
        "getter allocation stayed in sender"
    );
    assert_eq!(parcel.live.get(), 1);
    {
        let _entered = Enter::new(parcel.clone());
        set_data(&root, "value", copied);
    }
    assert!(engine.interp.get_member(&graph, "fail").is_err());
    assert_eq!(
        parcel.live.get(),
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
