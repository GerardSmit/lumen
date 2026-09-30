#![cfg(feature = "embed")]
use lumen::{Completion, Engine};
#[test]
fn coroutine_created_views_belong_to_the_driver_registry() {
    let mut engine = Engine::new();
    let before = engine.string_view_stats().1;
    match engine.eval("function* make(){const root='abcdefgh'.repeat(1024);return root.slice(100,200);}globalThis.kept=make().next().value;kept.length",false).unwrap(){Completion::Value(value)=>assert_eq!(value,"100"),Completion::Throw{name,message}=>panic!("{name}: {message}")}
    assert_eq!(
        engine.string_view_stats().1,
        before + 1,
        "coroutine view must be visible to its owning driver and drop there"
    );
}

#[test]
fn parked_coroutine_views_stay_put_until_native_stack_finishes() {
    let mut engine = Engine::new();
    match engine.eval(r#"
 function* parked(){let root='abcdefgh'.repeat(1024);const view=root.slice(100,200);root=null;globalThis.kept=view;yield view.match(/defghabc/)[0];return view.replace(/def/g,'DEF').slice(10,40);}
 globalThis.generator=parked();generator.next().value;
 "#,false).unwrap(){Completion::Value(value)=>assert_eq!(value,"defghabc"),Completion::Throw{name,message}=>panic!("{name}: {message}")}
    let bytes = engine.string_view_stats().2;
    assert!(bytes >= 8192);
    for _ in 0..10 {
        engine.run_microtasks();
        assert_eq!(
            engine.string_view_stats().2,
            bytes,
            "parked native stack must prevent byte relocation"
        );
    }
    match engine
        .eval(
            "generator.next().value === kept.replace(/def/g,'DEF').slice(10,40)",
            false,
        )
        .unwrap()
    {
        Completion::Value(value) => assert_eq!(value, "true"),
        Completion::Throw { name, message } => panic!("{name}: {message}"),
    }
    engine.run_microtasks();
    assert!(
        engine.string_view_stats().2 < bytes,
        "completed coroutine permits compaction again"
    );
    match engine
        .eval("kept === 'abcdefgh'.repeat(1024).slice(100,200)", false)
        .unwrap()
    {
        Completion::Value(value) => assert_eq!(value, "true"),
        Completion::Throw { name, message } => panic!("{name}: {message}"),
    }
}

#[test]
fn pooled_coroutines_unregister_views_dropped_on_driver() {
    let mut engine = Engine::new();
    let source="function* create(){let root='abcdefgh'.repeat(1024);const view=root.slice(100,200);root=null;return view;}globalThis.views=[];for(let i=0;i<64;i++)views.push(create().next().value);views.length";
    match engine.eval(source, false).unwrap() {
        Completion::Value(value) => assert_eq!(value, "64"),
        Completion::Throw { name, message } => panic!("{name}: {message}"),
    }
    assert_eq!(engine.string_view_stats().1, 64);
    engine.run_microtasks();
    match engine.eval("views=null", false).unwrap() {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("{name}: {message}"),
    }
    assert_eq!(
        engine.string_view_stats().1,
        0,
        "driver drops must remove coroutine-created views"
    );
}

#[test]
fn awaited_async_native_stack_blocks_compaction_until_resolution() {
    let mut engine = Engine::new();
    match engine.eval(r#"
 let resolve;globalThis.pending=new Promise(r=>resolve=r);globalThis.resume=resolve;
 async function parked(){with({}){let root='abcdefgh'.repeat(1024);const view=root.slice(100,200);root=null;globalThis.kept=view;await pending;globalThis.result=view.replace(/def/g,'DEF').slice(10,40);}}
 globalThis.operation=parked();
 "#,false).unwrap(){Completion::Value(_)=>{},Completion::Throw{name,message}=>panic!("{name}: {message}")}
    let bytes = engine.string_view_stats().2;
    assert!(bytes >= 8192);
    for _ in 0..10 {
        engine.run_microtasks();
        assert_eq!(engine.string_view_stats().2, bytes);
    }
    match engine.eval("resume();", false).unwrap() {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("{name}: {message}"),
    }
    engine.run_microtasks();
    assert!(
        engine.string_view_stats().2 < bytes,
        "resolved async stack permits compaction"
    );
    match engine
        .eval("result===kept.replace(/def/g,'DEF').slice(10,40)", false)
        .unwrap()
    {
        Completion::Value(value) => assert_eq!(value, "true"),
        Completion::Throw { name, message } => panic!("{name}: {message}"),
    }
}
