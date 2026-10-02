use super::{Limits, Parcel, parcel::HeapGuard};
use crate::{Engine, Completion, value::{Value, set_data, live_objects}};

fn value(engine: &mut Engine, name: &str) -> Value {
    let global = Value::Obj(engine.interp.global.clone());
    engine.interp.get_member(&global, name).unwrap_or_else(|_| panic!("global lookup"))
}
fn evaluate(engine: &mut Engine, source: &str) -> String {
    match engine.eval(source, false).unwrap() {
        Completion::Value(value) => value,
        Completion::Throw { name, message } => panic!("{name}: {message}"),
    }
}

#[test]
fn graph_copy_preserves_identity_holes_keys_and_snapshot_time() {
    let mut sender = Engine::new();
    evaluate(&mut sender, "var shared={v:42}; var root={a:shared,b:shared,array:[,shared,,]}; root.self=root; root.array.extra='yes'; root.text='a'.repeat(1024).slice(200,700); root.big=123456789012345678901234567890n;");
    let root = value(&mut sender, "root");
    let parcel = Parcel::build(&mut sender.interp, &root, Limits::default()).unwrap_or_else(|_| panic!("build"));
    evaluate(&mut sender, "shared.v=99; root.array.extra='changed';");
    sender.collect_garbage();
    std::thread::spawn(move || {
        let mut receiver = Engine::new();
        let root = receiver.interp.adopt(parcel);
        set_data(&receiver.interp.global, "root", root);
        assert_eq!(evaluate(&mut receiver, "root.a===root.b && root.self===root && root.array[1]===root.a"), "true");
        assert_eq!(evaluate(&mut receiver, "JSON.stringify([root.a.v,root.array.length,0 in root.array,2 in root.array,root.array.extra,root.text.length,String(root.big)])"), "[42,3,false,false,\"yes\",500,\"123456789012345678901234567890\"]");
        assert_eq!(evaluate(&mut receiver, "Object.getPrototypeOf(root)===Object.prototype && Object.getPrototypeOf(root.array)===Array.prototype && !Object.keys(root.array).includes('length')"), "true");
        receiver.collect_garbage();
        assert_eq!(evaluate(&mut receiver, "root.a.v"), "42");
        evaluate(&mut receiver, "root=undefined;");
        receiver.collect_garbage();
    }).join().unwrap();
    assert_eq!(evaluate(&mut sender, "shared.v"), "99");
}

#[test]
fn unadopted_cyclic_graph_is_freed_on_another_thread() {
    let mut sender = Engine::new();
    evaluate(&mut sender, "var root={};root.self=root;root.text='x'.repeat(2000);");
    let root = value(&mut sender, "root");
    let parcel = Parcel::build(&mut sender.interp, &root, Limits::default()).unwrap_or_else(|_| panic!("build"));
    let heap = parcel.heap.clone();
    std::thread::spawn(move || {
        drop(parcel);
        let _entered = HeapGuard::enter(&heap);
        assert_eq!(live_objects(), 0);
    }).join().unwrap();
}

#[test]
fn clone_limits_and_getter_failure_restore_sender_state() {
    let mut sender = Engine::new();
    evaluate(&mut sender, "var root={a:{}};Object.defineProperty(root,'fail',{enumerable:true,get:function(){throw Error('getter failed');}});");
    let root = value(&mut sender, "root");
    let heap = crate::value::gc_state_handle();
    let error = Parcel::build(&mut sender.interp, &root, Limits::default()).err().expect("throw");
    assert!(sender.interp.is_error_value(&error));
    assert!(std::sync::Arc::ptr_eq(&heap, &crate::value::gc_state_handle()));
    let limit = Limits { objects: 0, ..Limits::default() };
    assert!(Parcel::build(&mut sender.interp, &root, limit).is_err());
    evaluate(&mut sender, "var symbol=Symbol('x');");
    let symbol = value(&mut sender, "symbol");
    assert!(Parcel::build(&mut sender.interp, &symbol, Limits::default()).is_err());
}

#[test]
fn builtin_slots_and_views_survive_adoption_and_gc() {
    let mut sender = Engine::new();
    evaluate(&mut sender, "var key={v:42};var buffer=new ArrayBuffer(32);var typed=new Uint16Array(buffer,4,3);typed[1]=1234;var view=new DataView(buffer,4,6);var root={key:key,map:new Map([[key,key]]),set:new Set([key]),date:new Date(123456),regex:/a+/gi,num:new Number(7),str:new String('ok'),bool:new Boolean(true),big:Object(123n),error:new TypeError('bad',{cause:key}),buffer:buffer,typed:typed,view:view};root.regex.lastIndex=2;");
    let root = value(&mut sender, "root");
    let parcel = Parcel::build(&mut sender.interp, &root, Limits::default()).unwrap_or_else(|_| panic!("build builtins"));
    std::thread::spawn(move || {
        let mut receiver = Engine::new();
        let root = receiver.interp.adopt(parcel);
        set_data(&receiver.interp.global, "root", root);
        receiver.collect_garbage();
        assert_eq!(evaluate(&mut receiver, "root.map.get(root.key)===root.key && root.set.has(root.key) && root.typed.buffer===root.buffer && root.view.buffer===root.buffer"), "true");
        assert_eq!(evaluate(&mut receiver, "JSON.stringify([root.date.getTime(),root.regex.source,root.regex.flags,root.regex.lastIndex,root.num.valueOf(),root.str.valueOf(),root.bool.valueOf(),String(root.big.valueOf()),root.error.name,root.error.message,root.typed[1],root.view.getUint16(2,true)])"), "[123456,\"a+\",\"gi\",2,7,\"ok\",true,\"123\",\"TypeError\",\"bad\",1234,1234]");
        assert_eq!(evaluate(&mut receiver, "root.error.cause===root.key && root.error instanceof TypeError"), "true");
    }).join().unwrap();
}

#[test]
fn transfer_moves_backing_pointer_and_detaches_only_after_success() {
    let mut sender = Engine::new();
    evaluate(&mut sender, "var buffer=new ArrayBuffer(65536);var root={buffer:buffer,view:new Uint8Array(buffer)};root.view[0]=42;");
    let root = value(&mut sender, "root");
    let buffer = value(&mut sender, "buffer");
    let pointer = crate::value::Gc::as_ptr(buffer.as_obj().unwrap()) as usize;
    let backing = sender.interp.array_buffers[&pointer].as_ptr() as usize;
    evaluate(&mut sender, "var unsupported=Symbol('x');");
    let unsupported = value(&mut sender, "unsupported");
    assert!(Parcel::build_with_transfer(&mut sender.interp, &unsupported, std::slice::from_ref(&buffer), Limits::default()).is_err());
    assert!(sender.interp.is_transferable_array_buffer(&buffer));
    let parcel = Parcel::build_with_transfer(&mut sender.interp, &root, std::slice::from_ref(&buffer), Limits::default()).unwrap_or_else(|_| panic!("transfer"));
    assert!(!sender.interp.is_transferable_array_buffer(&buffer));
    std::thread::spawn(move || {
        let mut receiver = Engine::new();
        let root = receiver.interp.adopt(parcel);
        set_data(&receiver.interp.global, "root", root);
        let buffer = value(&mut receiver, "root");
        let buffer = receiver.interp.get_member(&buffer, "buffer").unwrap_or_else(|_| panic!("buffer"));
        let pointer = crate::value::Gc::as_ptr(buffer.as_obj().unwrap()) as usize;
        assert_eq!(receiver.interp.array_buffers[&pointer].as_ptr() as usize, backing);
        assert_eq!(evaluate(&mut receiver, "root.view[0]"), "42");
    }).join().unwrap();
}

#[test]
fn shared_buffer_aliases_backing_without_sharing_js_objects() {
    let mut sender = Engine::new();
    evaluate(&mut sender, "var shared=new SharedArrayBuffer(16);var root={buffer:shared,view:new Int32Array(shared)};root.view[0]=7;");
    let root = value(&mut sender, "root");
    let parcel = Parcel::build(&mut sender.interp, &root, Limits::default()).unwrap_or_else(|_| panic!("shared build"));
    std::thread::spawn(move || {
        let mut receiver = Engine::new();
        let root = receiver.interp.adopt(parcel);
        set_data(&receiver.interp.global, "root", root);
        assert_eq!(evaluate(&mut receiver, "root.view[0]"), "7");
        evaluate(&mut receiver, "Atomics.store(root.view,0,42);");
        receiver.collect_garbage();
        assert_eq!(evaluate(&mut receiver, "root.buffer instanceof SharedArrayBuffer && root.view.buffer===root.buffer"), "true");
    }).join().unwrap();
    assert_eq!(evaluate(&mut sender, "root.view[0]"), "42");
}

#[test]
fn unsupported_objects_are_rejected_and_repeated_drop_is_steady() {
    let mut sender = Engine::new();
    for source in ["Symbol('x')", "new Proxy({}, {})", "Promise.resolve(1)", "new WeakMap()", "new WeakSet()", "new WeakRef({})", "(function*(){yield 1})()", "Math.sin", "(function(){}).bind(null)", "new (class { #secret=1; })()"] {
        evaluate(&mut sender, &format!("var root={source};"));
        let root = value(&mut sender, "root");
        assert!(Parcel::build(&mut sender.interp, &root, Limits::default()).is_err(), "accepted {source}");
    }
    evaluate(&mut sender, "var root={child:{text:'abc'}};root.self=root;");
    let root = value(&mut sender, "root");
    sender.collect_garbage();
    let before = live_objects();
    for _ in 0..1000 {
        let parcel = Parcel::build(&mut sender.interp, &root, Limits::default()).unwrap_or_else(|_| panic!("build"));
        drop(parcel);
    }
    sender.collect_garbage();
    assert_eq!(live_objects(), before);
}

#[test]
fn functions_use_receiver_globals_and_fresh_code() {
    let mut sender = Engine::new();
    evaluate(&mut sender, "var root={arrow:(x)=>Math.max(x,7),async:async function(x){return x+1;},generator:function*(x){yield x;},strict:function(x){'use strict';return this===undefined?x:0;},recursive:function factorial(n){return n<2?1:n*factorial(n-1);},shadow:(x)=>{let outer=9;return {outer:x}.outer+outer;}};");
    let root = value(&mut sender, "root");
    let parcel = Parcel::build(&mut sender.interp, &root, Limits::default()).unwrap_or_else(|_| panic!("functions"));
    std::thread::spawn(move || {
        let mut receiver = Engine::new();
        let root = receiver.interp.adopt(parcel);
        set_data(&receiver.interp.global, "root", root);
        assert_eq!(evaluate(&mut receiver, "JSON.stringify([root.arrow(2),root.generator(13).next().value,(0,root.strict)(42),root.recursive(5),root.shadow(3)])"), "[7,13,42,120,12]");
        evaluate(&mut receiver, "var asyncResult;root.async(41).then(x=>asyncResult=x);");
        assert_eq!(evaluate(&mut receiver, "asyncResult"), "42");
        receiver.collect_garbage();
        assert_eq!(evaluate(&mut receiver, "root.recursive(6)"), "720");
    }).join().unwrap();
}

#[test]
fn outer_bindings_are_rejected_but_properties_and_shadowing_are_allowed() {
    let mut sender = Engine::new();
    for (source, name) in [
        ("(()=>{const config=7;return ()=>config;})()", "config"),
        ("(()=>{let config=7;return ()=>config;})()", "config"),
        ("(()=>this)", "this"),
        ("(function(){return ()=>arguments;})()", "arguments"),
        ("(function(){return ()=>new.target;})()", "new.target"),
    ] {
        evaluate(&mut sender, &format!("var root={source};"));
        let root = value(&mut sender, "root");
        let error = Parcel::build(&mut sender.interp, &root, Limits::default()).err().expect("capture error");
        set_data(&sender.interp.global, "error", error);
        assert_eq!(evaluate(&mut sender, &format!("error instanceof TypeError && error.message.includes('{name}') && error.message.includes('args')")), "true");
    }
    evaluate(&mut sender, "const userGlobal=7;var root=()=>userGlobal;");
    let root = value(&mut sender, "root");
    assert!(Parcel::build(&mut sender.interp, &root, Limits::default()).is_err());
    evaluate(&mut sender, "var outer=7;var root=(config)=>{const outer=3;return config.outer+outer;};");
    let root = value(&mut sender, "root");
    assert!(Parcel::build(&mut sender.interp, &root, Limits::default()).is_ok());
}
