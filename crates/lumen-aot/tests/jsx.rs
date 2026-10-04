use lumen::{Completion, Engine, Precompiled};

static TSX: Precompiled = lumen_aot::include_js!(entry = "tests/tsx/main.tsx");
static COMPILED: Precompiled = lumen_aot::include_js!(entry = "tests/tsx/compiled.tsx");
static STACK: Precompiled = lumen_aot::include_js!(entry = "tests/tsx/stack.tsx");

#[test]
fn native_compiled_tsx_aot_preserves_template_slots() {
    let mut engine = Engine::new();
    engine.eval("globalThis.slots=[]; globalThis.__lumen={template:s=>s, instantiate:s=>s, nodeAt:(root,path)=>path, bindAttribute:(node,name,fn)=>slots.push([node,name,fn()]), bindChild:(node,fn)=>slots.push([node,fn()])};", false).unwrap();
    match engine.load_precompiled(&COMPILED).unwrap() {
        Completion::Value(_) => {}
        Completion::Throw { message, .. } => panic!("{message}"),
    }
    match engine.eval("JSON.stringify(slots)", false).unwrap() {
        Completion::Value(value) => assert_eq!(value, r#"[[[],"title","count 4"],[[0,0],4]]"#),
        Completion::Throw { message, .. } => panic!("{message}"),
    }
}

#[test]
fn native_tsx_aot_module_loads_in_bare_engine() {
    let mut engine = Engine::new();
    match engine.load_precompiled(&TSX).unwrap() {
        Completion::Value(_) => {}
        Completion::Throw { message, .. } => panic!("{message}"),
    }
    match engine.eval("tsxResult", false).unwrap() {
        Completion::Value(value) => assert_eq!(
            value,
            r#"{"type":"section","props":{"count":4,"children":5}}"#
        ),
        Completion::Throw { message, .. } => panic!("{message}"),
    }
}

#[test]
fn native_tsx_aot_keeps_jsx_expression_source_positions() {
    let mut engine = Engine::new();
    match engine.load_precompiled(&STACK).unwrap() {
        Completion::Value(_) => {}
        Completion::Throw { message, .. } => panic!("{message}"),
    }
    match engine
        .eval(
            "if (!jsxStack.props.children.includes('stack.tsx:3:29')) throw new Error(jsxStack.props.children)",
            false,
        )
        .unwrap()
    {
        Completion::Value(_) => {}
        Completion::Throw { message, .. } => panic!("{message}"),
    }
}
