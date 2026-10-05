use std::{
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

#[cfg(not(target_arch = "wasm32"))]
#[global_allocator]
static APPLICATION_PROFILE_ALLOCATOR: lumen::fastalloc::ClassAlloc = lumen::fastalloc::ClassAlloc;

const VIEWPORT: (u32, u32) = (320, 200);
const MAX_FRAME_WALL_NS: u128 = 250_000_000;
const MAX_FRAME_CPU_US: u64 = 250_000;
const MAX_TODOS: usize = 128;
const MAX_DOCUMENT_NODES: usize = 2_048;
const MAX_WRAPPERS: usize = 2_048;
const MAX_HTML_PROFILE_PEAK_BYTES: isize = 8 * 1024 * 1024;
const MAX_FONT_FACE_CACHE_BYTES: usize = 1024 * 1024;
const MAX_DISPLAY_LIST_REFERENCED_BYTES: usize = 16 * 1024 * 1024;

const GOLDENS: &[(&str, &str)] = &[
    (
        "initial",
        include_str!("application-profile/goldens/initial.html"),
    ),
    (
        "added",
        include_str!("application-profile/goldens/added.html"),
    ),
    (
        "reversed",
        include_str!("application-profile/goldens/reversed.html"),
    ),
    (
        "editing",
        include_str!("application-profile/goldens/editing.html"),
    ),
    (
        "edit-draft",
        include_str!("application-profile/goldens/edit-draft.html"),
    ),
    (
        "edited",
        include_str!("application-profile/goldens/edited.html"),
    ),
    (
        "toggled",
        include_str!("application-profile/goldens/toggled.html"),
    ),
    (
        "active-filter",
        include_str!("application-profile/goldens/active-filter.html"),
    ),
    (
        "completed-filter",
        include_str!("application-profile/goldens/completed-filter.html"),
    ),
    (
        "deleted",
        include_str!("application-profile/goldens/deleted.html"),
    ),
    (
        "all-filter",
        include_str!("application-profile/goldens/all-filter.html"),
    ),
];

struct Golden {
    name: &'static str,
    pixels: Vec<u8>,
}

struct StageImage {
    name: &'static str,
    pixels: Vec<u8>,
    png: Vec<u8>,
}

struct FrameSample {
    stage: &'static str,
    wall_ns: u128,
    process_cpu_us: Option<u64>,
    html_category_live_bytes: Option<isize>,
    slab_category_live_bytes: Option<isize>,
    html_category_live_allocations: Option<isize>,
    html_class_allocator_overhead_live_bytes: Option<isize>,
    html_overaligned_request_count: Option<usize>,
    html_overaligned_request_bytes: Option<usize>,
    html_overaligned_live_allocations: Option<isize>,
    html_overaligned_live_bytes: Option<isize>,
    html_overaligned_ledger_overflow: Option<bool>,
    style_cache: lumen_html::css::StyleCacheStats,
    font_cache: lumen_html_text::ShapeCacheStats,
    display_list_referenced_bytes: usize,
    template_static_nodes: usize,
    template_live_wrappers: usize,
}

struct ProfileResult {
    stages: Vec<StageImage>,
}

#[test]
fn compiled_jsx_preact_and_react_todomvc_profile() {
    let goldens = Arc::new(render_static_goldens());
    let compiled = run_profile("compiled", "compiled.tsx", true, Arc::clone(&goldens));
    let preact = run_profile("preact", "preact.mjs", false, Arc::clone(&goldens));
    let react = run_profile("react", "react.mjs", false, goldens);

    assert_eq!(compiled.stages.len(), preact.stages.len());
    assert_eq!(compiled.stages.len(), react.stages.len());
    for index in 0..compiled.stages.len() {
        let stage = compiled.stages[index].name;
        assert_eq!(stage, preact.stages[index].name);
        assert_eq!(stage, react.stages[index].name);
        assert_pixels_equal(
            &compiled.stages[index].pixels,
            &preact.stages[index].pixels,
            &format!("{stage}: compiled JSX and Preact differ"),
        );
        assert_pixels_equal(
            &compiled.stages[index].pixels,
            &react.stages[index].pixels,
            &format!("{stage}: compiled JSX and React differ"),
        );
    }
}

#[test]
fn html_allocation_category_guard_restores_the_prior_thread_tag() {
    let before = lumen_common::memcat::current();
    let counting_enabled = lumen::memstats::categories().is_some();
    {
        let _guard = lumen_common::memcat::enter(lumen_common::memcat::CategoryTag::HTML);
        assert_eq!(
            lumen_common::memcat::current(),
            if counting_enabled {
                lumen_common::memcat::HTML_CATEGORY_ID
            } else {
                before
            }
        );
    }
    assert_eq!(lumen_common::memcat::current(), before);
}

fn render_static_goldens() -> Vec<Golden> {
    let font = lumen_html_text::FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
    GOLDENS
        .iter()
        .map(|(name, html)| {
            let image =
                lumen_html_image::render_html_with_font(html, VIEWPORT.0, VIEWPORT.1, 1.0, &font)
                    .unwrap_or_else(|error| {
                        panic!("static {name} golden did not render: {error:?}")
                    });
            Golden {
                name,
                pixels: image.pixels,
            }
        })
        .collect()
}

fn run_profile(
    name: &'static str,
    entry: &str,
    compiled_jsx: bool,
    goldens: Arc<Vec<Golden>>,
) -> ProfileResult {
    let thread_name = format!("html-profile-{name}");
    let entry = entry.to_owned();
    std::thread::Builder::new()
        .name(thread_name)
        .stack_size(lumen::THREAD_STACK_SIZE)
        .spawn(move || run_profile_inner(name, &entry, compiled_jsx, goldens))
        .unwrap()
        .join()
        .unwrap()
}

fn run_profile_inner(
    name: &'static str,
    entry: &str,
    compiled_jsx: bool,
    goldens: Arc<Vec<Golden>>,
) -> ProfileResult {
    let profile_start = Instant::now();
    let process_cpu_start = process_cpu_us();
    let process_peak_start = process_peak_rss_kib();
    let process_rss_start = lumen_os::sysinfo::resident_set_bytes();
    let html_category_start = html_category_live_bytes();
    let slab_category_start = slab_category_live_bytes();
    let html_category_overhead_start = html_category_allocation_overhead();
    let html_overaligned_start = html_overaligned_requests();
    let html_overaligned_live_start = html_overaligned_live();

    let mut runtime = lumen_runtime::Runtime::new();
    runtime.set_deadline(Duration::from_secs(30));
    let realm = lumen_html_js::install(
        runtime.engine().ctx(),
        "<body style='margin:0'><style>body{margin:0;font-family:Arial,sans-serif;color:#333}main{width:280px;margin:8px auto}h1{margin:0 0 6px;font-size:24px;text-align:center}#new-todo{width:150px}button{margin:2px;padding:2px 5px;font-size:12px}ul{list-style:none;margin:8px 0;padding:0}li{padding:3px 0;border-bottom:1px solid #ddd}li.completed label{text-decoration:line-through;color:#888}.editor.hidden{display:none}nav{margin-top:6px}#count{margin:4px 0;font-size:12px}</style><div id='app'></div><div id='template-probe' style='display:none'></div></body>",
        4_096,
    )
    .unwrap();
    let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/application-profile")
        .join(entry);
    let fixture = std::fs::read_to_string(&fixture_path)
        .unwrap_or_else(|error| panic!("could not read {}: {error}", fixture_path.display()));

    if compiled_jsx {
        let key = fixture_path.to_string_lossy().replace('\\', "/");
        match runtime
            .engine()
            .eval_module_jsx(&fixture, &key, true, |_, _| None)
            .unwrap_or_else(|error| panic!("compiled JSX parse failed: {error:?}"))
        {
            lumen::Completion::Value(_) => {}
            lumen::Completion::Throw { name, message } => {
                panic!("compiled JSX threw: {name}: {message}")
            }
        }
        runtime.run_until_idle();
    } else {
        runtime.install_module_loader(&fixture_path.to_string_lossy(), false);
        let key = fixture_path
            .to_string_lossy()
            .replace('\\', "/")
            .replace('\'', "\\'");
        eval(
            &mut runtime,
            &format!(
                "globalThis.__profileLoad='pending'; import('{key}').then(()=>__profileLoad='ready',error=>__profileLoad=String(error)+' '+error.stack)"
            ),
        );
        runtime.run_until_idle();
        match eval(&mut runtime, "__profileLoad") {
            lumen::embed::Value::Str(value) if value.as_str() == "ready" => {}
            lumen::embed::Value::Str(error) => {
                panic!("{name} module failed to load: {error}")
            }
            _ => panic!("{name} module loader returned a non-string value"),
        }
    }

    if compiled_jsx {
        eval(
            &mut runtime,
            "globalThis.__profileStaticTemplate=__lumen.template('<section><h2 class=template-static>Compiled template</h2><div><span class=template-static>Untouched label</span><b class=template-static>Untouched badge</b></div><small class=template-static>Untouched note</small></section>'); globalThis.__profileStaticClone=__lumen.instantiate(__profileStaticTemplate); document.getElementById('template-probe').appendChild(__profileStaticClone);",
        );
        runtime.run_until_idle();
    }

    let font_setup_before = (
        html_category_live_bytes(),
        html_category_allocation_overhead(),
        html_overaligned_live(),
    );
    let font = {
        let _html_allocations =
            lumen_common::memcat::enter(lumen_common::memcat::CategoryTag::HTML);
        lumen_html_text::FontFace::new(Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap()
    };
    let font_setup_after = (
        html_category_live_bytes(),
        html_category_allocation_overhead(),
        html_overaligned_live(),
    );
    let mut frames = Vec::with_capacity(GOLDENS.len());
    let mut stages = Vec::with_capacity(GOLDENS.len());
    capture_stage(
        name,
        "initial",
        &mut runtime,
        &realm,
        &font,
        &goldens,
        &mut frames,
        &mut stages,
    );

    eval(&mut runtime, "document.getElementById('new-todo').focus()");
    dispatch_tab(&mut runtime, &realm, false);
    assert_js_true(
        &mut runtime,
        "document.activeElement.id==='add'",
        "Tab moves from the entry field to the next focusable control",
    );
    dispatch_tab(&mut runtime, &realm, true);
    assert_js_true(
        &mut runtime,
        "document.activeElement.id==='new-todo'",
        "Shift+Tab returns focus to the entry field",
    );

    type_text(&mut runtime, &realm, "new-todo", "Buy milk", false);
    click(&mut runtime, "add");
    type_text(&mut runtime, &realm, "new-todo", "Read book", false);
    click(&mut runtime, "add");
    type_text(&mut runtime, &realm, "new-todo", "Take a walk", false);
    click(&mut runtime, "add");
    assert_js_true(
        &mut runtime,
        "document.querySelectorAll('#todo-list li').length===3 && document.getElementById('count').textContent==='3 items left'",
        "add three todos",
    );
    capture_stage(
        name,
        "added",
        &mut runtime,
        &realm,
        &font,
        &goldens,
        &mut frames,
        &mut stages,
    );

    eval(
        &mut runtime,
        "globalThis.__profileMoveNodes=[document.getElementById('todo-1'),document.getElementById('todo-2'),document.getElementById('todo-3')];",
    );
    click(&mut runtime, "reverse");
    assert_js_true(
        &mut runtime,
        "document.querySelector('#todo-list li').id==='todo-3' && document.getElementById('todo-1')===__profileMoveNodes[0] && document.getElementById('todo-2')===__profileMoveNodes[1] && document.getElementById('todo-3')===__profileMoveNodes[2]",
        "reverse keyed rows without replacing their DOM identities",
    );
    capture_stage(
        name,
        "reversed",
        &mut runtime,
        &realm,
        &font,
        &goldens,
        &mut frames,
        &mut stages,
    );

    click(&mut runtime, "edit-2");
    assert_js_true(
        &mut runtime,
        "document.getElementById('todo-2').className==='todo editing' && document.getElementById('editor-2').value==='Read book'",
        "enter edit mode",
    );
    capture_stage(
        name,
        "editing",
        &mut runtime,
        &realm,
        &font,
        &goldens,
        &mut frames,
        &mut stages,
    );
    type_text(&mut runtime, &realm, "editor-2", "Read more", true);
    assert_js_true(
        &mut runtime,
        "document.getElementById('editor-2').value==='Read more'",
        "update the edit draft",
    );
    capture_stage(
        name,
        "edit-draft",
        &mut runtime,
        &realm,
        &font,
        &goldens,
        &mut frames,
        &mut stages,
    );
    dispatch_key(&mut runtime, &realm, "Enter");
    assert_js_true(
        &mut runtime,
        "profileLastEditKey==='Enter'",
        "deliver Enter to the TodoMVC edit handler",
    );
    assert_js_true(
        &mut runtime,
        "document.getElementById('label-2').textContent==='Read more' && document.getElementById('todo-2').className==='todo'",
        "commit the edit",
    );
    capture_stage(
        name,
        "edited",
        &mut runtime,
        &realm,
        &font,
        &goldens,
        &mut frames,
        &mut stages,
    );

    eval(
        &mut runtime,
        "document.getElementById('toggle-3').setAttribute('checked',''); document.getElementById('toggle-3').dispatchEvent(new Event('click',{bubbles:true})); document.getElementById('toggle-3').dispatchEvent(new Event('change',{bubbles:true}));",
    );
    runtime.run_until_idle();
    assert_js_true(
        &mut runtime,
        "document.getElementById('todo-3').className==='todo completed' && document.getElementById('count').textContent==='2 items left'",
        "toggle a todo complete",
    );
    capture_stage(
        name,
        "toggled",
        &mut runtime,
        &realm,
        &font,
        &goldens,
        &mut frames,
        &mut stages,
    );

    click(&mut runtime, "filter-active");
    assert_js_true(
        &mut runtime,
        "document.querySelectorAll('#todo-list li').length===2 && document.querySelector('#todo-3')===null",
        "filter active todos",
    );
    capture_stage(
        name,
        "active-filter",
        &mut runtime,
        &realm,
        &font,
        &goldens,
        &mut frames,
        &mut stages,
    );
    click(&mut runtime, "filter-completed");
    assert_js_true(
        &mut runtime,
        "document.querySelectorAll('#todo-list li').length===1 && document.querySelector('#todo-list li').id==='todo-3'",
        "filter completed todos",
    );
    capture_stage(
        name,
        "completed-filter",
        &mut runtime,
        &realm,
        &font,
        &goldens,
        &mut frames,
        &mut stages,
    );
    click(&mut runtime, "delete-3");
    assert_js_true(
        &mut runtime,
        "document.querySelectorAll('#todo-list li').length===0 && document.getElementById('count').textContent==='2 items left'",
        "delete a completed todo",
    );
    capture_stage(
        name,
        "deleted",
        &mut runtime,
        &realm,
        &font,
        &goldens,
        &mut frames,
        &mut stages,
    );
    click(&mut runtime, "filter-all");
    assert_js_true(
        &mut runtime,
        "document.querySelectorAll('#todo-list li').length===2 && document.querySelector('#todo-list li').id==='todo-2'",
        "show all remaining todos",
    );
    capture_stage(
        name,
        "all-filter",
        &mut runtime,
        &realm,
        &font,
        &goldens,
        &mut frames,
        &mut stages,
    );

    eval(
        &mut runtime,
        "globalThis.__profileFillResult=profileFillToLimit()",
    );
    runtime.run_until_idle();
    assert_js_true(
        &mut runtime,
        &format!("__profileFillResult==={MAX_TODOS} && profileCount()==={MAX_TODOS}"),
        "fill the bounded todo list",
    );
    assert_js_true(
        &mut runtime,
        "profileAddTodo('overflow')===false && profileCount()===128",
        "reject items beyond the app limit",
    );
    runtime.run_until_idle();
    assert!(
        !runtime.is_interrupted(),
        "{name} profile exceeded its runtime deadline"
    );

    let (node_count, frame_id) =
        realm.with_session(|session| (session.document().node_count(), session.frame_id()));
    let wrapper_count = realm.wrapper_count();
    assert!(
        node_count <= MAX_DOCUMENT_NODES,
        "{name}: {node_count} DOM nodes exceed the {MAX_DOCUMENT_NODES} profile limit"
    );
    assert!(
        wrapper_count <= MAX_WRAPPERS,
        "{name}: {wrapper_count} wrappers exceed the {MAX_WRAPPERS} profile limit"
    );

    let process_cpu_end = process_cpu_us();
    let process_peak_end = process_peak_rss_kib();
    let process_rss_end = lumen_os::sysinfo::resident_set_bytes();
    let process_cpu_total = process_cpu_start
        .zip(process_cpu_end)
        .map(|(before, after)| after.saturating_sub(before));
    let engine_allocator = lumen::memstats::allocator();
    let html_category_end = html_category_live_bytes();
    let slab_category_end = slab_category_live_bytes();
    let html_category_overhead_end = html_category_allocation_overhead();
    let html_overaligned_end = html_overaligned_requests();
    let html_overaligned_live_end = html_overaligned_live();
    let html_category_peak = frames
        .iter()
        .filter_map(|frame| frame.html_category_live_bytes)
        .max();
    let html_profile_end_delta = html_category_end
        .zip(html_category_start)
        .map(|(end, baseline)| end - baseline);
    let html_frame_peak_delta = html_category_peak
        .zip(html_category_start)
        .map(|(peak, baseline)| peak - baseline);
    let html_profile_peak_delta = match (html_frame_peak_delta, html_profile_end_delta) {
        (Some(frame_peak), Some(final_delta)) => Some(frame_peak.max(final_delta)),
        (Some(frame_peak), None) => Some(frame_peak),
        (None, Some(final_delta)) => Some(final_delta),
        (None, None) => None,
    };
    // Normal ClassAlloc blocks add a 16-byte category header and, for cached sizes, class
    // rounding beyond the payload bytes in Cat::Html. Add their sampled live overhead and the
    // exact requested live bytes of HTML-tagged over-aligned blocks from the allocator ledger.
    // The process-wide Slab delta remains diagnostic only because it includes unrelated
    // categories and object-slab chunks.
    let html_overaligned_count_delta = html_overaligned_end
        .zip(html_overaligned_start)
        .map(|(end, start)| end.0.saturating_sub(start.0));
    let html_overaligned_bytes_delta = html_overaligned_end
        .zip(html_overaligned_start)
        .map(|(end, start)| end.1.saturating_sub(start.1));
    let html_overaligned_live_count_delta = html_overaligned_live_end
        .zip(html_overaligned_live_start)
        .map(|(end, start)| end.0.saturating_sub(start.0));
    let html_overaligned_live_bytes_delta = html_overaligned_live_end
        .zip(html_overaligned_live_start)
        .map(|(end, start)| end.1.saturating_sub(start.1));
    let html_memory_bound_at = |html_live: Option<isize>,
                                class_overhead: Option<isize>,
                                overaligned_live: Option<isize>,
                                ledger_overflow: Option<bool>| {
        let payload = html_live.zip(html_category_start);
        let overhead =
            class_overhead.zip(html_category_overhead_start.map(|(_, overhead)| overhead));
        let overaligned =
            overaligned_live.zip(html_overaligned_live_start.map(|(_, bytes, _)| bytes));
        match (payload, overhead, overaligned, ledger_overflow) {
            (
                Some((live, baseline)),
                Some((overhead, overhead_baseline)),
                Some((aligned, aligned_baseline)),
                Some(false),
            ) => Some(
                live.saturating_sub(baseline)
                    .saturating_add(overhead.saturating_sub(overhead_baseline))
                    .saturating_add(aligned.saturating_sub(aligned_baseline)),
            ),
            _ => None,
        }
    };
    let html_memory_bound_frame_peak = frames
        .iter()
        .filter_map(|frame| {
            html_memory_bound_at(
                frame.html_category_live_bytes,
                frame.html_class_allocator_overhead_live_bytes,
                frame.html_overaligned_live_bytes,
                frame.html_overaligned_ledger_overflow,
            )
        })
        .max();
    let html_memory_bound_final_delta = html_memory_bound_at(
        html_category_end,
        html_category_overhead_end.map(|(_, overhead)| overhead),
        html_overaligned_live_end.map(|(_, bytes, _)| bytes),
        html_overaligned_live_end.map(|(_, _, overflow)| overflow),
    );
    let html_memory_bound_peak_delta =
        match (html_memory_bound_frame_peak, html_memory_bound_final_delta) {
            (Some(frame_peak), Some(final_delta)) => Some(frame_peak.max(final_delta)),
            (Some(frame_peak), None) => Some(frame_peak),
            (None, Some(final_delta)) => Some(final_delta),
            (None, None) => None,
        };
    let font_setup_deltas = (
        font_setup_before
            .0
            .zip(font_setup_after.0)
            .map(|(before, after)| after - before),
        font_setup_before
            .1
            .zip(font_setup_after.1)
            .map(|(before, after)| (after.0 - before.0, after.1 - before.1)),
        font_setup_before
            .2
            .zip(font_setup_after.2)
            .map(|(before, after)| (after.0 - before.0, after.1 - before.1, after.2)),
    );
    let peak_payload_frame = frames
        .iter()
        .filter_map(|frame| {
            frame
                .html_category_live_bytes
                .zip(html_category_start)
                .map(|(live, baseline)| (frame.stage, live - baseline, live))
        })
        .max_by_key(|(_, delta, _)| *delta);
    let peak_bound_frame = frames
        .iter()
        .filter_map(|frame| {
            html_memory_bound_at(
                frame.html_category_live_bytes,
                frame.html_class_allocator_overhead_live_bytes,
                frame.html_overaligned_live_bytes,
                frame.html_overaligned_ledger_overflow,
            )
            .map(|delta| (frame.stage, delta))
        })
        .max_by_key(|(_, delta)| *delta);
    if std::env::var_os("LUMEN_APPLICATION_PROFILE_REQUIRE_MEM_STATS").is_some() {
        let stage_memory_samples = frames
            .iter()
            .map(|frame| {
                (
                    frame.stage,
                    frame
                        .html_category_live_bytes
                        .zip(html_category_start)
                        .map(|(live, baseline)| live - baseline),
                    html_memory_bound_at(
                        frame.html_category_live_bytes,
                        frame.html_class_allocator_overhead_live_bytes,
                        frame.html_overaligned_live_bytes,
                        frame.html_overaligned_ledger_overflow,
                    ),
                    frame.html_category_live_allocations,
                    frame.html_class_allocator_overhead_live_bytes,
                    frame.font_cache.bytes,
                )
            })
            .collect::<Vec<_>>();
        eprintln!(
            "html-profile-memory-debug application={name} font_setup_before={font_setup_before:?} font_setup_after={font_setup_after:?} font_setup_deltas={font_setup_deltas:?} stage_memory_samples={stage_memory_samples:?} peak_payload_frame={peak_payload_frame:?} peak_bound_frame={peak_bound_frame:?} final_payload_delta={html_profile_end_delta:?} final_bound_delta={html_memory_bound_final_delta:?}"
        );
    }
    if std::env::var_os("LUMEN_APPLICATION_PROFILE_REQUIRE_MEM_STATS").is_some() {
        assert!(
            html_category_start.is_some()
                && html_category_end.is_some()
                && slab_category_start.is_some()
                && slab_category_end.is_some()
                && html_category_overhead_start.is_some()
                && html_category_overhead_end.is_some()
                && html_overaligned_start.is_some()
                && html_overaligned_end.is_some()
                && html_overaligned_live_start.is_some_and(|(_, _, overflow)| !overflow)
                && html_overaligned_live_end.is_some_and(|(_, _, overflow)| !overflow)
                && frames.iter().all(|frame| {
                    frame.html_category_live_bytes.is_some()
                        && frame.slab_category_live_bytes.is_some()
                        && frame.html_category_live_allocations.is_some()
                        && frame.html_class_allocator_overhead_live_bytes.is_some()
                        && frame.html_overaligned_request_count.is_some()
                        && frame.html_overaligned_request_bytes.is_some()
                        && frame.html_overaligned_live_allocations.is_some()
                        && frame.html_overaligned_live_bytes.is_some()
                        && frame.html_overaligned_ledger_overflow == Some(false)
                })
                && engine_allocator.is_some_and(|(_, _, _, allocations)| allocations > 0)
                && html_profile_peak_delta.is_some_and(|bytes| bytes > 0)
                && html_memory_bound_peak_delta.is_some()
                && html_overaligned_count_delta.is_some()
                && html_overaligned_bytes_delta.is_some()
                && html_overaligned_live_count_delta.is_some()
                && html_overaligned_live_bytes_delta.is_some(),
            "the dedicated HTML profile gate requires lumen/mem-stats category counters"
        );
    }
    if let Some(peak_delta) = html_profile_peak_delta {
        assert!(
            peak_delta <= MAX_HTML_PROFILE_PEAK_BYTES,
            "{name}: sampled HTML category live-byte delta {peak_delta} exceeds the {MAX_HTML_PROFILE_PEAK_BYTES}-byte regression limit"
        );
    }
    if let Some(peak_delta) = html_memory_bound_peak_delta {
        assert!(
            peak_delta <= MAX_HTML_PROFILE_PEAK_BYTES,
            "{name}: sampled HTML payload, exact known allocator overhead, and live over-aligned payload bound {peak_delta} exceeds the {MAX_HTML_PROFILE_PEAK_BYTES}-byte regression limit"
        );
    }
    if let (Some(count), Some(bytes)) = (html_overaligned_count_delta, html_overaligned_bytes_delta)
    {
        assert_eq!(
            count == 0,
            bytes == 0,
            "{name}: over-aligned HTML request counters disagree: {count} requests, {bytes} logical bytes"
        );
    }
    if std::env::var_os("LUMEN_APPLICATION_PROFILE_REQUIRE_MEM_STATS").is_some() {
        assert!(
            html_overaligned_live_start.is_some_and(|(_, _, overflow)| !overflow)
                && html_overaligned_live_end.is_some_and(|(_, _, overflow)| !overflow)
                && frames
                    .iter()
                    .all(|frame| frame.html_overaligned_ledger_overflow == Some(false)),
            "{name}: over-aligned allocation ledger overflowed; refusing to claim a complete HTML live-byte bound"
        );
    }
    let profile_wall_ns = profile_start.elapsed().as_nanos();
    assert!(
        profile_wall_ns < Duration::from_secs(30).as_nanos(),
        "{name} full application profile exceeded 30 seconds"
    );

    write_profile_evidence(
        name,
        &frames,
        &stages,
        profile_wall_ns,
        process_cpu_total,
        process_peak_start,
        process_peak_end,
        process_rss_start,
        process_rss_end,
        engine_allocator,
        html_category_start,
        html_category_end,
        html_profile_peak_delta,
        html_profile_end_delta,
        slab_category_start,
        slab_category_end,
        html_category_overhead_start,
        html_category_overhead_end,
        html_overaligned_start,
        html_overaligned_end,
        html_overaligned_live_start,
        html_overaligned_live_end,
        html_overaligned_live_count_delta,
        html_overaligned_live_bytes_delta,
        html_memory_bound_peak_delta,
        html_memory_bound_final_delta,
        node_count,
        wrapper_count,
        frame_id,
    );

    ProfileResult { stages }
}

fn capture_stage(
    profile: &'static str,
    stage: &'static str,
    runtime: &mut lumen_runtime::Runtime,
    realm: &Rc<lumen_html_js::DomRealm>,
    font: &lumen_html_text::FontFace,
    goldens: &[Golden],
    frames: &mut Vec<FrameSample>,
    stages: &mut Vec<StageImage>,
) {
    let golden = goldens
        .iter()
        .find(|golden| golden.name == stage)
        .unwrap_or_else(|| panic!("missing static golden {stage}"));
    let cpu_before = process_cpu_us();
    let start = Instant::now();
    let image = lumen_html_image::render_settled_shared(
        &realm.session_handle(),
        VIEWPORT.0,
        VIEWPORT.1,
        1.0,
        font,
        &lumen_html_image::FileImages::new(Path::new(".")),
        lumen_html_image::SettleOptions::default(),
        || !runtime.run_until_idle().idle,
    )
    .unwrap_or_else(|error| panic!("{profile}/{stage} render failed: {error:?}"));
    let wall_ns = start.elapsed().as_nanos();
    let process_cpu_us = cpu_before
        .zip(process_cpu_us())
        .map(|(before, after)| after.saturating_sub(before));
    assert!(
        wall_ns <= MAX_FRAME_WALL_NS,
        "{profile}/{stage} frame took {wall_ns} ns (limit {MAX_FRAME_WALL_NS} ns)"
    );
    if let Some(cpu_us) = process_cpu_us {
        assert!(
            cpu_us <= MAX_FRAME_CPU_US,
            "{profile}/{stage} process CPU took {cpu_us} us (limit {MAX_FRAME_CPU_US} us)"
        );
    }
    let style_cache = realm.with_session(|session| session.style_cache_stats());
    let font_cache = lumen_html_text::FontProvider::shape_cache_stats(font);
    let display_list_referenced_bytes = realm
        .with_session(|session| {
            session
                .cached_display_list()
                .map(lumen_html::paint::DisplayList::referenced_bytes)
        })
        .unwrap_or_else(|| panic!("{profile}/{stage}: rendered frame has no cached display list"));
    let template_static_nodes = if profile == "compiled" {
        realm.with_session(|session| {
            lumen_html::selector::query_selector_all(
                session.document(),
                session.document().root(),
                ".template-static",
            )
            .unwrap_or_else(|error| {
                panic!("{profile}/{stage}: template probe query failed: {error:?}")
            })
        })
    } else {
        Vec::new()
    };
    let template_live_wrappers = template_static_nodes
        .iter()
        .filter(|&&node| realm.has_live_wrapper(node))
        .count();
    if profile == "compiled" {
        assert!(
            template_static_nodes.len() >= 4,
            "{profile}/{stage}: compiled template probe found only {} untouched descendants",
            template_static_nodes.len()
        );
        assert_eq!(
            template_live_wrappers, 0,
            "{profile}/{stage}: untouched compiled-template descendants were wrapped"
        );
    }
    assert!(
        font_cache.bytes <= MAX_FONT_FACE_CACHE_BYTES,
        "{profile}/{stage}: font-face shape/measure caches retain {} bytes (limit {MAX_FONT_FACE_CACHE_BYTES})",
        font_cache.bytes
    );
    assert!(
        display_list_referenced_bytes <= MAX_DISPLAY_LIST_REFERENCED_BYTES,
        "{profile}/{stage}: display list conservatively references {display_list_referenced_bytes} bytes (limit {MAX_DISPLAY_LIST_REFERENCED_BYTES})"
    );
    assert!(
        style_cache.unique_styles <= style_cache.styled_nodes,
        "{profile}/{stage}: {} unique computed styles exceed {} styled nodes",
        style_cache.unique_styles,
        style_cache.styled_nodes
    );
    if profile == "compiled" && stage == "added" {
        assert!(
            style_cache.unique_styles < style_cache.styled_nodes
                && style_cache.shared_hits + style_cache.intern_hits > 0,
            "compiled profile did not share computed styles across repeated todo siblings: {style_cache:?}"
        );
    }
    assert_pixels_equal(
        &image.pixels,
        &golden.pixels,
        &format!("{profile}/{stage} differs from its static HTML golden"),
    );
    let png = lumen_html_image::encode_png(&image);
    let html_overaligned_requests = html_overaligned_requests();
    let html_overaligned_live = html_overaligned_live();
    let html_class_allocation_overhead = html_category_allocation_overhead();
    frames.push(FrameSample {
        stage,
        wall_ns,
        process_cpu_us,
        html_category_live_bytes: html_category_live_bytes(),
        slab_category_live_bytes: slab_category_live_bytes(),
        html_category_live_allocations: html_class_allocation_overhead.map(|stats| stats.0),
        html_class_allocator_overhead_live_bytes: html_class_allocation_overhead
            .map(|stats| stats.1),
        html_overaligned_request_count: html_overaligned_requests.map(|stats| stats.0),
        html_overaligned_request_bytes: html_overaligned_requests.map(|stats| stats.1),
        html_overaligned_live_allocations: html_overaligned_live.map(|stats| stats.0),
        html_overaligned_live_bytes: html_overaligned_live.map(|stats| stats.1),
        html_overaligned_ledger_overflow: html_overaligned_live.map(|stats| stats.2),
        style_cache,
        font_cache,
        display_list_referenced_bytes,
        template_static_nodes: template_static_nodes.len(),
        template_live_wrappers,
    });
    stages.push(StageImage {
        name: stage,
        pixels: image.pixels,
        png,
    });
}

fn assert_pixels_equal(actual: &[u8], expected: &[u8], context: &str) {
    if actual == expected {
        return;
    }
    let first_channel = actual
        .iter()
        .zip(expected)
        .position(|(actual, expected)| actual != expected);
    let mismatched_channels = actual
        .iter()
        .zip(expected)
        .filter(|(actual, expected)| actual != expected)
        .count();
    let mismatched_pixels = actual
        .chunks_exact(4)
        .zip(expected.chunks_exact(4))
        .filter(|(actual, expected)| actual != expected)
        .count();
    let first_difference = first_channel.map_or_else(
        || "no common byte differs".to_owned(),
        |channel| {
            let pixel = channel / 4;
            let x = pixel % VIEWPORT.0 as usize;
            let y = pixel / VIEWPORT.0 as usize;
            let actual_pixel = actual.get(pixel * 4..pixel * 4 + 4);
            let expected_pixel = expected.get(pixel * 4..pixel * 4 + 4);
            format!(
                "first at ({x},{y}) channel {}: actual {actual_pixel:?}, expected {expected_pixel:?}",
                channel % 4
            )
        },
    );
    panic!(
        "{context}: {} actual bytes vs {} expected bytes; {mismatched_channels} channel mismatches across {mismatched_pixels} pixels; {first_difference}",
        actual.len(),
        expected.len(),
    );
}

fn type_text(
    runtime: &mut lumen_runtime::Runtime,
    realm: &Rc<lumen_html_js::DomRealm>,
    input_id: &str,
    text: &str,
    replace: bool,
) {
    eval(
        runtime,
        &format!("document.getElementById('{input_id}').focus();"),
    );
    let node = realm.focused_node().expect("focused text input");
    if replace {
        let length = realm.control_value(node).unwrap().encode_utf16().count();
        realm.set_selection(node, 0, length, "forward").unwrap();
    }
    for key in text.chars() {
        dispatch_key_at(runtime, realm, node, &key.to_string());
    }
    runtime.run_until_idle();
}

fn dispatch_key(
    runtime: &mut lumen_runtime::Runtime,
    realm: &Rc<lumen_html_js::DomRealm>,
    key: &str,
) {
    let node = realm.focused_node().expect("focused text input");
    dispatch_key_at(runtime, realm, node, key);
    runtime.run_until_idle();
}

fn dispatch_tab(
    runtime: &mut lumen_runtime::Runtime,
    realm: &Rc<lumen_html_js::DomRealm>,
    backwards: bool,
) {
    let node = realm.focused_node().expect("focused element before Tab");
    realm
        .dispatch(
            runtime.engine().ctx(),
            node,
            "keydown",
            true,
            true,
            &[
                ("key", lumen::embed::Value::str("Tab")),
                ("shiftKey", lumen::embed::Value::Bool(backwards)),
            ],
        )
        .unwrap();
    runtime.run_until_idle();
}

fn dispatch_key_at(
    runtime: &mut lumen_runtime::Runtime,
    realm: &Rc<lumen_html_js::DomRealm>,
    node: lumen_html::NodeId,
    key: &str,
) {
    realm
        .dispatch(
            runtime.engine().ctx(),
            node,
            "keydown",
            true,
            true,
            &[("key", lumen::embed::Value::str(key))],
        )
        .unwrap();
    runtime.run_until_idle();
}

fn click(runtime: &mut lumen_runtime::Runtime, id: &str) {
    eval(
        runtime,
        &format!(
            "document.getElementById('{id}').dispatchEvent(new Event('click',{{bubbles:true}}));"
        ),
    );
    runtime.run_until_idle();
}

fn assert_js_true(runtime: &mut lumen_runtime::Runtime, source: &str, operation: &str) {
    if matches!(eval(runtime, source), lumen::embed::Value::Bool(true)) {
        return;
    }
    let details = eval(
        runtime,
        "'label=' + ((document.getElementById('label-2')||{}).textContent||'?') + '; row=' + ((document.getElementById('todo-2')||{}).className||'?') + '; editor=' + ((document.getElementById('editor-2')||{}).value||'?')",
    );
    let details = match details {
        lumen::embed::Value::Str(details) => details.to_string(),
        _ => "unavailable".to_string(),
    };
    panic!(
        "{operation} did not produce its expected DOM state; assertion: {source}; state: {details}"
    );
}

fn eval(runtime: &mut lumen_runtime::Runtime, source: &str) -> lumen::embed::Value {
    runtime
        .engine()
        .eval_value(source)
        .unwrap()
        .unwrap_or_else(|error| {
            let message = runtime.engine().ctx().get_member(&error, "stack").ok();
            if let Some(lumen::embed::Value::Str(message)) = message {
                panic!("{message}\nwhile evaluating: {source}");
            }
            panic!("JavaScript threw while evaluating: {source}");
        })
}

fn process_cpu_us() -> Option<u64> {
    if cfg!(any(unix, windows)) {
        lumen_os::sysinfo::resource_usage()
            .ok()
            .map(|usage| usage.user_us.saturating_add(usage.system_us))
    } else {
        None
    }
}

fn process_peak_rss_kib() -> Option<u64> {
    if cfg!(any(unix, windows)) {
        lumen_os::sysinfo::resource_usage()
            .ok()
            .map(|usage| usage.max_rss_kib)
    } else {
        None
    }
}

fn html_category_live_bytes() -> Option<isize> {
    lumen::memstats::categories().map(|categories| categories[lumen::memstats::Cat::Html as usize])
}

fn slab_category_live_bytes() -> Option<isize> {
    lumen::memstats::categories().map(|categories| categories[lumen::memstats::Cat::Slab as usize])
}

fn html_category_allocation_overhead() -> Option<(isize, isize)> {
    let (counts, overhead) = lumen::memstats::category_allocation_overhead()?;
    let category = lumen::memstats::Cat::Html as usize;
    Some((counts[category], overhead[category]))
}

fn html_overaligned_requests() -> Option<(usize, usize)> {
    let (counts, bytes) = lumen::memstats::overaligned_requests_by_category()?;
    let category = lumen::memstats::Cat::Html as usize;
    Some((counts[category], bytes[category]))
}

fn html_overaligned_live() -> Option<(isize, isize, bool)> {
    let (counts, bytes, overflow) = lumen::memstats::overaligned_live_by_category()?;
    let category = lumen::memstats::Cat::Html as usize;
    Some((counts[category], bytes[category], overflow))
}

#[allow(clippy::too_many_arguments)]
fn write_profile_evidence(
    name: &str,
    frames: &[FrameSample],
    stages: &[StageImage],
    profile_wall_ns: u128,
    process_cpu_total: Option<u64>,
    process_peak_start: Option<u64>,
    process_peak_end: Option<u64>,
    process_rss_start: Option<u64>,
    process_rss_end: Option<u64>,
    engine_allocator: Option<(usize, usize, usize, usize)>,
    html_category_start: Option<isize>,
    html_category_end: Option<isize>,
    html_profile_peak_delta: Option<isize>,
    html_profile_end_delta: Option<isize>,
    slab_category_start: Option<isize>,
    slab_category_end: Option<isize>,
    html_category_overhead_start: Option<(isize, isize)>,
    html_category_overhead_end: Option<(isize, isize)>,
    html_overaligned_start: Option<(usize, usize)>,
    html_overaligned_end: Option<(usize, usize)>,
    html_overaligned_live_start: Option<(isize, isize, bool)>,
    html_overaligned_live_end: Option<(isize, isize, bool)>,
    html_overaligned_live_count_delta: Option<isize>,
    html_overaligned_live_bytes_delta: Option<isize>,
    html_memory_bound_peak_delta: Option<isize>,
    html_memory_bound_final_delta: Option<isize>,
    node_count: usize,
    wrapper_count: usize,
    rendered_frame_id: u64,
) {
    let output = std::env::var_os("LUMEN_APPLICATION_PROFILE_OUTPUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .nth(4)
                .expect("lumen-html-js lives below the repository root")
                .join("target/html-application-profile")
        });
    std::fs::create_dir_all(&output).unwrap();
    for stage in stages {
        std::fs::write(
            output.join(format!("{name}-{}.png", stage.name)),
            &stage.png,
        )
        .unwrap();
    }
    let frame_json = frames
        .iter()
        .map(|frame| {
            format!(
                "{{\"stage\":\"{}\",\"wall_ns\":{},\"process_cpu_us\":{},\"html_category_live_bytes\":{},\"slab_category_live_bytes_diagnostic\":{},\"html_category_live_allocations_le16\":{},\"html_class_allocator_overhead_live_bytes\":{},\"html_overaligned_request_count\":{},\"html_overaligned_request_bytes\":{},\"html_overaligned_live_allocations\":{},\"html_overaligned_live_bytes\":{},\"html_overaligned_ledger_overflow\":{},\"display_list_referenced_bytes\":{},\"template_static_nodes\":{},\"template_live_wrappers\":{},\"style_cache\":{{\"styled_nodes\":{},\"unique_styles\":{},\"computed_styles\":{},\"cache_hits\":{},\"shared_hits\":{},\"intern_hits\":{}}},\"font_cache\":{{\"bytes\":{},\"entries\":{},\"hits\":{},\"misses\":{},\"evictions\":{},\"limit_bytes\":{MAX_FONT_FACE_CACHE_BYTES}}}}}",
                frame.stage,
                frame.wall_ns,
                option_number(frame.process_cpu_us),
                option_signed_number(frame.html_category_live_bytes),
                option_signed_number(frame.slab_category_live_bytes),
                option_signed_number(frame.html_category_live_allocations),
                option_signed_number(frame.html_class_allocator_overhead_live_bytes),
                option_usize(frame.html_overaligned_request_count),
                option_usize(frame.html_overaligned_request_bytes),
                option_signed_number(frame.html_overaligned_live_allocations),
                option_signed_number(frame.html_overaligned_live_bytes),
                option_bool(frame.html_overaligned_ledger_overflow),
                frame.display_list_referenced_bytes,
                frame.template_static_nodes,
                frame.template_live_wrappers,
                frame.style_cache.styled_nodes,
                frame.style_cache.unique_styles,
                frame.style_cache.computed_styles,
                frame.style_cache.cache_hits,
                frame.style_cache.shared_hits,
                frame.style_cache.intern_hits,
                frame.font_cache.bytes,
                frame.font_cache.entries,
                frame.font_cache.hits,
                frame.font_cache.misses,
                frame.font_cache.evictions,
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let allocator_json = engine_allocator.map_or_else(
        || "null".to_string(),
        |(live, peak, cached, allocations)| {
            format!(
                "{{\"live_bytes\":{live},\"peak_live_bytes\":{peak},\"cached_bytes\":{cached},\"allocations\":{allocations}}}"
            )
        },
    );
    let html_attribution_json = match (
        html_category_start,
        html_category_end,
        html_profile_peak_delta,
        html_profile_end_delta,
        html_category_overhead_start,
        html_category_overhead_end,
        html_memory_bound_peak_delta,
        html_memory_bound_final_delta,
    ) {
        (
            Some(start),
            Some(end),
            Some(peak_delta),
            Some(end_delta),
            Some((start_allocations, start_overhead)),
            Some((end_allocations, end_overhead)),
            Some(memory_peak),
            Some(memory_final),
        ) => format!(
            "{{\"available\":true,\"category_id\":{},\"category\":\"HTML DOM and rendering\",\"scope\":\"process-wide sampled requested live payload bytes for alignments <=16 + exact ClassAlloc inner-layout overhead (16-byte category header plus size-class rounding) + exact ledger-tracked requested live bytes for alignments >16 by original allocation category. It excludes system allocator metadata/pages and is not realm-local\",\"baseline_live_payload_bytes_le16\":{start},\"final_live_payload_bytes_le16\":{end},\"baseline_outstanding_allocations_le16\":{start_allocations},\"final_outstanding_allocations_le16\":{end_allocations},\"baseline_classalloc_overhead_bytes_le16\":{start_overhead},\"final_classalloc_overhead_bytes_le16\":{end_overhead},\"max_sampled_payload_delta_bytes_le16\":{peak_delta},\"final_payload_delta_bytes_le16\":{end_delta},\"max_sampled_payload_overhead_and_overaligned_live_delta_bytes\":{memory_peak},\"final_payload_overhead_and_overaligned_live_delta_bytes\":{memory_final},\"max_sampled_delta_limit_bytes\":{MAX_HTML_PROFILE_PEAK_BYTES}}}",
            lumen::memstats::Cat::Html as u8,
        ),
        _ => "{\"available\":false,\"reason\":\"lumen/mem-stats feature is not enabled\"}"
            .to_string(),
    };
    let over_aligned_attribution_json = match (html_overaligned_start, html_overaligned_end) {
        (Some((start_count, start_bytes)), Some((end_count, end_bytes))) => format!(
            "{{\"available\":true,\"category_id\":{},\"alignment_greater_than_bytes\":16,\"scope\":\"process-wide cumulative HTML-tagged allocation and reallocation request traffic; diagnostic only, not included in the live-byte bound\",\"baseline_request_count\":{start_count},\"final_request_count\":{end_count},\"request_count_delta\":{},\"baseline_requested_bytes\":{start_bytes},\"final_requested_bytes\":{end_bytes},\"requested_bytes_delta\":{}}}",
            lumen::memstats::Cat::Html as u8,
            end_count.saturating_sub(start_count),
            end_bytes.saturating_sub(start_bytes)
        ),
        _ => "{\"available\":false,\"reason\":\"lumen/mem-stats feature is not enabled\"}"
            .to_string(),
    };
    let over_aligned_live_attribution_json = match (
        html_overaligned_live_start,
        html_overaligned_live_end,
        html_overaligned_live_count_delta,
        html_overaligned_live_bytes_delta,
    ) {
        (
            Some((start_count, start_bytes, start_overflow)),
            Some((end_count, end_bytes, end_overflow)),
            Some(count_delta),
            Some(bytes_delta),
        ) => format!(
            "{{\"available\":true,\"category_id\":{},\"alignment_greater_than_bytes\":16,\"scope\":\"exact outstanding requested payload bytes, retaining the original allocation category through reallocation\",\"ledger_capacity_blocks\":4096,\"ledger_overflow\":{},\"baseline_outstanding_allocations\":{start_count},\"final_outstanding_allocations\":{end_count},\"outstanding_allocation_delta\":{count_delta},\"baseline_live_requested_bytes\":{start_bytes},\"final_live_requested_bytes\":{end_bytes},\"live_requested_bytes_delta\":{bytes_delta}}}",
            lumen::memstats::Cat::Html as u8,
            start_overflow || end_overflow
        ),
        _ => "{\"available\":false,\"reason\":\"lumen/mem-stats feature is not enabled\"}"
            .to_string(),
    };
    let slab_category_live_delta = slab_category_end
        .zip(slab_category_start)
        .map(|(end, start)| end - start);
    let metrics = format!(
        "{{\"application\":\"{name}\",\"viewport\":[{},{}],\"scale\":1,\"frame_count\":{},\"frames\":[{frame_json}],\"profile_wall_ns\":{profile_wall_ns},\"profile_process_cpu_us\":{},\"process_peak_rss_kib_start\":{},\"process_peak_rss_kib_end\":{},\"process_rss_bytes_start\":{},\"process_rss_bytes_end\":{},\"lumen_allocator_process_wide\":{allocator_json},\"html_allocation_attribution\":{html_attribution_json},\"html_overaligned_requests\":{over_aligned_attribution_json},\"html_overaligned_live\":{over_aligned_live_attribution_json},\"slab_category_live_bytes_diagnostic\":{{\"baseline\":{},\"final\":{},\"delta\":{}}},\"physical_heap_reservation_bytes\":null,\"physical_heap_reservation_available\":false,\"physical_heap_reservation_reason\":\"The HTML bound covers requested live payload and known ClassAlloc inner-layout overhead. It excludes system allocator metadata, page rounding, cached freed blocks, and the static mem-stats ledger/counter storage; those bytes are not attributed as HTML live payload\",\"todo_limit\":{MAX_TODOS},\"document_nodes\":{node_count},\"wrapper_count\":{wrapper_count},\"rendered_frame_id\":{rendered_frame_id}}}",
        VIEWPORT.0,
        VIEWPORT.1,
        frames.len(),
        option_number(process_cpu_total),
        option_number(process_peak_start),
        option_number(process_peak_end),
        option_number(process_rss_start),
        option_number(process_rss_end),
        option_signed_number(slab_category_start),
        option_signed_number(slab_category_end),
        option_signed_number(slab_category_live_delta)
    );
    std::fs::write(output.join(format!("{name}.json")), &metrics).unwrap();
    eprintln!("application-profile {metrics}");
}

fn option_number(value: Option<u64>) -> String {
    value.map_or_else(|| "null".to_string(), |value| value.to_string())
}

fn option_usize(value: Option<usize>) -> String {
    value.map_or_else(|| "null".to_string(), |value| value.to_string())
}

fn option_signed_number(value: Option<isize>) -> String {
    value.map_or_else(|| "null".to_string(), |value| value.to_string())
}

fn option_bool(value: Option<bool>) -> String {
    value.map_or_else(|| "null".to_string(), |value| value.to_string())
}

/// Provider initialization only: debug host timings and ClassAlloc system-boundary
/// held-byte snapshots (rounded live blocks plus caches), neither JS heap nor RSS.
#[test]
fn runtime_provider_initialization_profile() {
    struct Sample {
        profile: &'static str,
        warmup: bool,
        pair: usize,
        order: usize,
        init_us: u128,
        dom_us: u128,
        before: Option<usize>,
        initialized: Option<usize>,
        dom_installed: Option<usize>,
        dropped: Option<usize>,
        parent_before_spawn: Option<usize>,
        parent_after_join: Option<usize>,
    }
    fn sample(profile: &'static str, warmup: bool, pair: usize, order: usize) -> Sample {
        let parent_before_spawn = lumen::Engine::heap_bytes();
        let mut result = std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(move || {
                let before = lumen::Engine::heap_bytes();
                let started = Instant::now();
                let mut runtime = if profile == "browser" {
                    lumen_runtime::Runtime::new_browser()
                } else {
                    lumen_runtime::Runtime::new()
                };
                let init_us = started.elapsed().as_micros();
                let initialized = lumen::Engine::heap_bytes();
                let started = Instant::now();
                let realm = lumen_html_js::install(
                    runtime.engine().ctx(),
                    "<body><main id='provider-probe'>provider initialization</main></body>",
                    128,
                )
                .unwrap();
                let dom_us = started.elapsed().as_micros();
                let dom_installed = lumen::Engine::heap_bytes();
                // Assert actual profile semantics after the measured snapshots;
                // no wall-time or allocation threshold is used as correctness.
                let node_surface = if profile == "browser" {
                    "typeof process==='undefined' && typeof Buffer==='undefined' && typeof require==='undefined'"
                } else {
                    "typeof process==='object' && typeof Buffer==='function' && typeof require==='function'"
                };
                let source = format!(
                    "document.querySelector('#provider-probe').textContent==='provider initialization' && \
                     ({node_surface}) && \
                     typeof setTimeout==='function' && typeof fetch==='function' && typeof Worker==='function'"
                );
                match runtime.engine().eval(&source, false).unwrap() {
                    lumen::Completion::Value(value) => assert_eq!(value, "true"),
                    lumen::Completion::Throw { name, message } => panic!("{name}: {message}"),
                }
                drop(realm);
                drop(runtime);
                let dropped = lumen::Engine::heap_bytes();
                Sample {
                    profile,
                    warmup,
                    pair,
                    order,
                    init_us,
                    dom_us,
                    before,
                    initialized,
                    dom_installed,
                    dropped,
                    parent_before_spawn,
                    parent_after_join: None,
                }
            })
            .unwrap()
            .join()
            .unwrap();
        result.parent_after_join = lumen::Engine::heap_bytes();
        result
    }
    fn delta(after: Option<usize>, before: Option<usize>) -> Option<isize> {
        after.zip(before).map(|(after, before)| after as isize - before as isize)
    }
    fn summary(mut values: Vec<u128>) -> String {
        values.sort_unstable();
        format!(
            "{{\"min\":{},\"median\":{},\"max\":{}}}",
            values[0],
            values[values.len() / 2],
            values[values.len() - 1]
        )
    }
    let mut samples = Vec::with_capacity(16);
    samples.push(sample("node", true, 0, 0));
    samples.push(sample("browser", true, 0, 1));
    for pair in 0..7 {
        let profiles = if pair % 2 == 0 { ["node", "browser"] } else { ["browser", "node"] };
        for (order, profile) in profiles.into_iter().enumerate() {
            samples.push(sample(profile, false, pair, order));
        }
    }
    let rows = samples.iter().map(|sample| format!(
        "{{\"profile\":\"{}\",\"warmup\":{},\"pair\":{},\"order\":{},\"init_us\":{},\"dom_install_us\":{},\"process_held_bytes_before\":{},\"process_held_bytes_after_init\":{},\"process_held_bytes_after_dom\":{},\"process_held_bytes_after_drop\":{},\"init_held_bytes_delta\":{},\"dom_held_bytes_delta_from_baseline\":{},\"post_drop_held_bytes_delta\":{},\"parent_held_bytes_before_spawn\":{},\"parent_held_bytes_after_join\":{},\"post_thread_join_held_bytes_delta\":{}}}",
        sample.profile, sample.warmup, sample.pair, sample.order, sample.init_us, sample.dom_us,
        option_usize(sample.before), option_usize(sample.initialized), option_usize(sample.dom_installed), option_usize(sample.dropped),
        option_signed_number(delta(sample.initialized,sample.before)), option_signed_number(delta(sample.dom_installed,sample.before)), option_signed_number(delta(sample.dropped,sample.before)), option_usize(sample.parent_before_spawn), option_usize(sample.parent_after_join), option_signed_number(delta(sample.parent_after_join,sample.parent_before_spawn))
    )).collect::<Vec<_>>().join(",");
    let summaries = ["node", "browser"].into_iter().map(|profile| {
        let measured = samples.iter().filter(|sample| sample.profile==profile && !sample.warmup).collect::<Vec<_>>();
        let init = summary(measured.iter().map(|sample|sample.init_us).collect());
        let dom = summary(measured.iter().map(|sample|sample.dom_us).collect());
        let init_payload = measured.iter().filter_map(|sample|delta(sample.initialized,sample.before)).collect::<Vec<_>>();
        let dom_payload = measured.iter().filter_map(|sample|delta(sample.dom_installed,sample.before)).collect::<Vec<_>>();
        let residual = measured.iter().filter_map(|sample|delta(sample.dropped,sample.before)).collect::<Vec<_>>();
        let thread_residual = measured.iter().filter_map(|sample|delta(sample.parent_after_join,sample.parent_before_spawn)).collect::<Vec<_>>();
        let signed = |mut values: Vec<isize>| {
            if values.is_empty() { return "null".to_string(); }
            values.sort_unstable();
            format!("{{\"min\":{},\"median\":{},\"max\":{}}}",values[0],values[values.len()/2],values[values.len()-1])
        };
        format!("\"{profile}\":{{\"init_us\":{init},\"dom_install_us\":{dom},\"init_held_bytes_delta\":{},\"dom_held_bytes_delta_from_baseline\":{},\"post_drop_held_bytes_delta\":{},\"post_thread_join_held_bytes_delta\":{}}}",signed(init_payload),signed(dom_payload),signed(residual),signed(thread_residual))
    }).collect::<Vec<_>>().join(",");
    let metrics = format!(
        "{{\"scope\":\"debug host Runtime provider initialization and identical minimal DOM installation\",\"paired_runs\":7,\"warmup_per_profile\":1,\"thread_stack_bytes\":67108864,\"samples_sequential\":true,\"memory_scope\":\"process-wide ClassAlloc system-boundary held-byte counter: rounded live blocks plus cached free-list blocks, including Rust/native allocations; not requested payload, isolated live JS heap, RSS or native GUI/WPT throughput\",\"post_drop_scope\":\"inside-thread snapshot after genuine Runtime and DOM owner drop, before thread-local teardown; parent snapshot after joined thread is separately recorded. Residual attribution is not established; neither snapshot alone proves or excludes a leak\",\"performance_threshold_asserted\":false,\"samples\":[{rows}],\"summary\":{{{summaries}}}}}"
    );
    let output = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors().nth(4).unwrap()
        .join("target/html-application-profile/runtime-provider-init.json");
    std::fs::create_dir_all(output.parent().unwrap()).unwrap();
    std::fs::write(&output, &metrics).unwrap();
    eprintln!("runtime-provider-init {} {metrics}", output.display());
}


/// Isolate provider/DOM held-byte retention without changing allocator behavior.
#[test]
fn runtime_provider_retained_memory_isolation() {
    #[derive(Clone)]
    struct Sample {
        mode: &'static str,
        round: usize,
        before_held: Option<usize>,
        after_join_held: Option<usize>,
        thread_requested_before: isize,
        thread_requested_after_drop: isize,
        thread_held_after_drop: Option<usize>,
        thread_cached_after_drop: usize,
        thread_cache: lumen::fastalloc::ThreadCacheStats,
        process_cached_after_drop: Option<usize>,
        process_cached_before: Option<usize>,
        process_cached_after_join: Option<usize>,
        parent_cache_after_join: Option<lumen::fastalloc::ThreadCacheStats>,
        dom_alive_after_engine_drop: Option<bool>,
    }
    let modes = ["no-runtime", "engine-only", "engine-held-function", "engine-released-function", "engine-dom", "browser-only", "node-only", "browser-dom", "node-dom"];
    let mut samples = Vec::with_capacity(modes.len() * 4);
    for round in 0..4 {
        for index in 0..modes.len() {
            let mode = modes[if round % 2 == 0 {index} else {modes.len()-1-index}];
            let before_held = lumen::Engine::heap_bytes();
            let process_cached_before = lumen::memstats::allocator().map(|(_, _, cached, _)| cached);
            let mut sample = std::thread::Builder::new().stack_size(64*1024*1024).spawn(move || {
                let requested_before = lumen::fastalloc::thread_live_bytes();
                let mut engine = None;
                let mut runtime = None;
                if mode.starts_with("engine") { engine = Some(lumen::Engine::new()); }
                if mode.starts_with("browser") { runtime = Some(lumen_runtime::Runtime::new_browser()); }
                if mode.starts_with("node") { runtime = Some(lumen_runtime::Runtime::new()); }
                // Mimic Runtime's independently held JS callback fields, without a
                // Runtime or DOM, and compare releasing the handle before Engine.
                let mut held_function = if mode.contains("function") {
                    Some(engine.as_mut().unwrap().eval_value("(() => globalThis)").unwrap())
                } else { None };
                if mode == "engine-released-function" { drop(held_function.take()); }
                let mut weak = None;
                if mode.ends_with("dom") {
                    let ctx = match runtime.as_mut() { Some(runtime) => runtime.engine().ctx(), None => engine.as_mut().unwrap().ctx() };
                    let realm = lumen_html_js::install(ctx, "<body><main id='probe'>retention isolation</main></body>", 128).unwrap();
                    weak = Some(Rc::downgrade(&realm));
                    drop(realm);
                }
                drop(runtime);
                drop(engine);
                drop(held_function);
                let dom_alive = weak.as_ref().map(|weak| weak.strong_count()!=0);
                drop(weak);
                Sample { mode, round, before_held, after_join_held: None,
                    thread_requested_before: requested_before,
                    thread_requested_after_drop: lumen::fastalloc::thread_live_bytes(),
                    thread_held_after_drop: lumen::Engine::heap_bytes(),
                    thread_cached_after_drop: lumen::fastalloc::cached_bytes_for_test(),
                    thread_cache: lumen::fastalloc::thread_cache_stats(),
                    process_cached_after_drop: lumen::memstats::allocator().map(|(_, _, cached, _)| cached),
                    process_cached_before,
                    process_cached_after_join: None,
                    parent_cache_after_join: None,
                    dom_alive_after_engine_drop: dom_alive }
            }).unwrap().join().unwrap();
            sample.after_join_held = lumen::Engine::heap_bytes();
            sample.process_cached_after_join = lumen::memstats::allocator().map(|(_, _, cached, _)| cached);
            sample.parent_cache_after_join = Some(lumen::fastalloc::thread_cache_stats());
            samples.push(sample);
        }
    }
    let rows = samples.iter().map(|sample| {
        let held_delta = sample.after_join_held.zip(sample.before_held).map(|(after,before)|after as isize-before as isize);
        let alive = sample.dom_alive_after_engine_drop.map(|alive|alive.to_string()).unwrap_or_else(||"null".to_string());
        let held_split = |held: Option<usize>, cached: Option<usize>| match (held, cached) {
            (Some(held), Some(cached)) => format!("{{\"held_bytes\":{held},\"allocator_cached_bytes\":{cached},\"live_block_bytes\":{}}}", held as isize - cached as isize),
            (Some(held), None) => format!("{{\"held_bytes\":{held},\"allocator_cached_bytes\":null,\"live_block_bytes\":null}}"),
            _ => "null".to_string(),
        };
        let live_delta = match (sample.after_join_held, sample.process_cached_after_join, sample.before_held, sample.process_cached_before) {
            (Some(held_after), Some(cached_after), Some(held_before), Some(cached_before)) => Some((held_after as isize - cached_after as isize) - (held_before as isize - cached_before as isize)),
            _ => None,
        };
        let cached_delta = sample.process_cached_after_join.zip(sample.process_cached_before).map(|(after, before)| after as isize - before as isize);
        let classes = |stats: &lumen::fastalloc::ThreadCacheStats| {
            let mut classes = stats.classes.clone();
            classes.sort_by_key(|class| std::cmp::Reverse(class.block_bytes * class.blocks));
            classes.iter().take(8).map(|class| format!("{{\"block_bytes\":{},\"blocks\":{},\"bytes\":{}}}", class.block_bytes, class.blocks, class.block_bytes * class.blocks)).collect::<Vec<_>>().join(",")
        };
        let parent_cache = sample.parent_cache_after_join.as_ref().map(|stats| format!("{{\"cached_bytes\":{},\"cached_blocks\":{},\"top_classes\":[{}]}}", stats.cached_bytes, stats.cached_blocks, classes(stats))).unwrap_or_else(|| "null".to_string());
        let attribution = format!("{{\"before\":{},\"after_join\":{},\"live_block_bytes_delta\":{},\"allocator_cached_bytes_delta\":{},\"thread_cache_at_drop\":{{\"cached_bytes\":{},\"cached_blocks\":{},\"live_requested_bytes\":{},\"top_classes\":[{}]}},\"process_cached_at_drop\":{},\"parent_thread_cache_after_join\":{},\"counter_scope\":\"live_block_bytes is held minus the process-wide free-list total and needs lumen/mem-stats; parent-thread cache lists blocks a surviving thread parked\"}}",
            held_split(sample.before_held, sample.process_cached_before), held_split(sample.after_join_held, sample.process_cached_after_join), option_signed_number(live_delta), option_signed_number(cached_delta),
            sample.thread_cache.cached_bytes, sample.thread_cache.cached_blocks, sample.thread_cache.live_requested_bytes, classes(&sample.thread_cache), option_usize(sample.process_cached_after_drop), parent_cache);
        format!("{{\"mode\":\"{}\",\"round\":{},\"warmup\":{},\"attribution\":{attribution},\"parent_before_held_bytes\":{},\"parent_after_join_held_bytes\":{},\"post_join_held_bytes_delta\":{},\"thread_requested_bytes_before\":{},\"thread_requested_bytes_after_drop\":{},\"thread_requested_bytes_delta\":{},\"thread_held_bytes_after_drop\":{},\"thread_cached_bytes_after_drop\":{},\"dom_weak_alive_after_engine_drop\":{}}}", sample.mode,sample.round,sample.round==0,option_usize(sample.before_held),option_usize(sample.after_join_held),option_signed_number(held_delta),sample.thread_requested_before,sample.thread_requested_after_drop,sample.thread_requested_after_drop-sample.thread_requested_before,option_usize(sample.thread_held_after_drop),sample.thread_cached_after_drop,alive)
    }).collect::<Vec<_>>().join(",");
    let output = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(4).unwrap().join("target/html-application-profile/runtime-provider-retention-isolation.json");
    let metrics = format!("{{\"scope\":\"debug host sequential 64MiB threads, one warmup and three alternating-order samples per mode\",\"counter_scope\":\"held bytes count process-wide rounded live blocks plus free-list caches; requested-live delta counts allocations/frees on owning sample thread before thread teardown, excludes other-thread frees; neither is RSS nor isolated JS heap\",\"no_allocator_behavior_changes\":true,\"samples\":[{rows}]}}");
    std::fs::create_dir_all(output.parent().unwrap()).unwrap();
    std::fs::write(&output,&metrics).unwrap();
    eprintln!("runtime-provider-retention-isolation {} {metrics}",output.display());
}
