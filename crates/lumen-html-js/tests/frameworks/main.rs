use lumen_runtime::Runtime;
use std::{
    path::Path,
    time::{Duration, Instant},
};

fn eval(runtime: &mut Runtime, source: &str) -> lumen::embed::Value {
    runtime
        .engine()
        .eval_value(source)
        .unwrap()
        .unwrap_or_else(|error| {
            let message = runtime.engine().ctx().get_member(&error, "stack").ok();
            if let Some(lumen::embed::Value::Str(message)) = message {
                panic!("{message}");
            }
            panic!("JavaScript threw");
        })
}

#[test]
fn preact_todo() {
    std::thread::Builder::new()
        .stack_size(lumen::THREAD_STACK_SIZE)
        .spawn(|| run("preact.mjs"))
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn react_todo() {
    std::thread::Builder::new()
        .stack_size(lumen::THREAD_STACK_SIZE)
        .spawn(|| run("react.mjs"))
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn event_handler_properties_preserve_listener_order_and_custom_case() {
    std::thread::Builder::new().stack_size(lumen::THREAD_STACK_SIZE).spawn(|| {
        let mut runtime = Runtime::new();
        let _realm = lumen_html_js::install(runtime.engine().ctx(), "<button></button>", 32).unwrap();
        let result = eval(&mut runtime, "const b=document.querySelector('button'); const calls=[]; b.addEventListener('click',()=>calls.push('first')); b.onclick=()=>calls.push('old'); b.addEventListener('click',()=>calls.push('last')); b.onclick=()=>{calls.push('new');return false}; const canceled=!b.dispatchEvent(new Event('click',{cancelable:true})); b.onclick=null; b.dispatchEvent(new Event('click')); b.addEventListener('Input',()=>calls.push('custom')); b.dispatchEvent(new Event('input')); b.dispatchEvent(new Event('Input')); canceled && b.onclick===null && ('oninput' in b) && calls.join(',')==='first,new,last,first,last,custom'");
        assert!(matches!(result, lumen::embed::Value::Bool(true)));
    }).unwrap().join().unwrap();
}

fn run(entry: &str) {
    let start = Instant::now();
    let before = lumen_os::sysinfo::resource_usage().unwrap();
    let mut runtime = Runtime::new();
    runtime.set_deadline(Duration::from_secs(15));
    let realm = lumen_html_js::install(
        runtime.engine().ctx(),
        "<body style='margin:0'><div id=app></div></body>",
        1024,
    )
    .unwrap();
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/frameworks")
        .join(entry);
    runtime.install_module_loader(&path.to_string_lossy(), false);
    let path = path.to_string_lossy().replace('\\', "/");
    eval(&mut runtime, &format!("globalThis.result='pending'; import('{path}').then(()=>result='ready',e=>result=String(e)+' '+e.stack)" ));
    runtime.run_until_idle();
    let result = eval(&mut runtime, "result");
    if let lumen::embed::Value::Str(message) = &result {
        assert_eq!(message.as_str(), "ready");
    } else {
        panic!("module did not load");
    }
    eval(&mut runtime, "document.getElementById('draft').focus();");
    let field = realm.focused_node().unwrap();
    for key in "Buy milk".chars() {
        realm
            .dispatch(
                runtime.engine().ctx(),
                field,
                "keydown",
                true,
                true,
                &[("key", lumen::embed::Value::str(key.to_string()))],
            )
            .unwrap();
        runtime.run_until_idle();
    }
    realm
        .dispatch(runtime.engine().ctx(), field, "change", true, false, &[])
        .unwrap();
    runtime.run_until_idle();
    eval(&mut runtime, "document.getElementById('add').dispatchEvent(new Event('pointerdown',{bubbles:true})); document.getElementById('add').dispatchEvent(new Event('click',{bubbles:true}));");
    runtime.run_until_idle();
    let result = eval(
        &mut runtime,
        "document.querySelector('li').textContent + ':' + document.getElementById('draft').value",
    );
    assert!(
        matches!(&result, lumen::embed::Value::Str(s) if s.as_str()=="Buy milk:"),
        "controlled update failed"
    );
    assert!(!runtime.is_interrupted());
    let font =
        lumen_html_text::FontFace::new(std::sync::Arc::from(lumen_html_text::TEST_FONT_BYTES))
            .unwrap();
    let image = lumen_html_image::render_settled_shared(
        &realm.session_handle(),
        320,
        200,
        1.0,
        &font,
        &lumen_html_image::FileImages::new(Path::new(".")),
        lumen_html_image::SettleOptions::default(),
        || !runtime.run_until_idle().idle,
    )
    .unwrap();
    let expected = lumen_html_image::render_html_with_font("<body style='margin:0'><div id=app><main><input id=draft value=''><button id=add>Add</button><ul><li>Buy milk</li></ul></main></div></body>", 320, 200, 1.0, &font).unwrap();
    assert!(
        image == expected,
        "framework image differs from static golden"
    );
    let after = lumen_os::sysinfo::resource_usage().unwrap();
    let metrics = format!("{{\"entry\":\"{entry}\",\"wall_ms\":{},\"cpu_us\":{},\"process_peak_rss_kib\":{},\"image_bytes\":{},\"wrappers\":{},\"viewport\":[320,200],\"scale\":1,\"exact_golden\":true}}", start.elapsed().as_millis(), after.user_us+after.system_us-before.user_us-before.system_us, after.max_rss_kib, image.pixels.len(), realm.wrapper_count());
    let output = std::env::var_os("LUMEN_FRAMEWORK_OUTPUT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("lumen-framework-profiles"));
    std::fs::create_dir_all(&output).unwrap();
    std::fs::write(output.join(format!("{entry}.json")), &metrics).unwrap();
    std::fs::write(
        output.join(format!("{entry}.png")),
        lumen_html_image::encode_png(&image),
    )
    .unwrap();
    eprintln!("{metrics}");
}
