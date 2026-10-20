//! The Performance Timeline for HTML realms: the shared `performance.mark`/`measure`/entry
//! facade of `lumen-host`, plus the legacy `performance.timing` and `performance.navigation`.
//!
//! An embedder installs [`performance_extension`] after the HTML realm (and after
//! `lumen_host::performance::install_globals`); realms that already install `lumen-web`'s
//! extension already have the facade and must not install this one.
use super::*;
use lumen::embed::JsFunction;
use lumen_host::Extension;
use std::cell::Cell;

const TIMING_SLOT: &str = "#lumen_window\u{1}performance.timing";
const NAVIGATION_SLOT: &str = "#lumen_window\u{1}performance.navigation";

const GLUE: &str = lumen_host::performance_timeline::SOURCE;
const GLUE_AOT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/html_performance_timeline.aot"));

/// Extension that gives a document's `performance` the User Timing, Performance Observer and
/// legacy navigation timing APIs.
pub fn performance_extension() -> Extension {
    Extension {
        name: "html-performance",
        modules: &[
            lumen_host::namespace::<lumen_host::performance_timeline::bindings::Module>,
            install_legacy_members,
        ],
        state_init: None,
        js_init: Some(GLUE),
        js_init_snapshot: (!GLUE_AOT.is_empty()).then_some(GLUE_AOT),
        lazy_globals: &[],
    }
}

fn epoch_now() -> f64 {
    (lumen_host::perf::time_origin_ms() + lumen_host::perf::now_ms()).floor()
}

/// Wall-clock milestones of one document, in Unix milliseconds; zero until reached.
pub(crate) struct NavigationTimes {
    start: Cell<f64>,
    interactive: Cell<f64>,
    complete: Cell<f64>,
    load_start: Cell<f64>,
    load_end: Cell<f64>,
}

impl Default for NavigationTimes {
    fn default() -> Self {
        Self {
            start: Cell::new(epoch_now()),
            interactive: Cell::new(0.0),
            complete: Cell::new(0.0),
            load_start: Cell::new(0.0),
            load_end: Cell::new(0.0),
        }
    }
}

impl NavigationTimes {
    pub(crate) fn interactive(&self) {
        if self.interactive.get() == 0.0 {
            self.interactive.set(epoch_now());
        }
    }

    pub(crate) fn complete(&self) {
        if self.complete.get() == 0.0 {
            self.complete.set(epoch_now());
        }
    }

    pub(crate) fn load_started(&self) {
        self.load_start.set(epoch_now());
    }

    pub(crate) fn load_finished(&self) {
        self.load_end.set(epoch_now());
    }
}

#[lumen_bind::class(name = "PerformanceTiming", hint(js(webidl)))]
pub(crate) struct DomPerformanceTiming {
    realm: std::rc::Weak<DomRealm>,
}

impl DomPerformanceTiming {
    fn read(&self, pick: impl Fn(&NavigationTimes) -> f64) -> f64 {
        self.realm.upgrade().map_or(0.0, |realm| pick(&realm.lifecycle.times))
    }

    fn start(&self) -> f64 {
        self.read(|times| times.start.get())
    }

}

macro_rules! timing_getters {
    ($($rust:ident $js:literal => $value:expr;)*) => {
        #[lumen_bind::methods]
        impl DomPerformanceTiming {
            $(
                #[getter(name = $js)]
                fn $rust(&self) -> f64 {
                    let this = self;
                    $value(this)
                }
            )*

            #[method(name = "toJSON")]
            fn to_json(&self, ctx: &mut Ctx) -> OpResult<Value> {
                let object = Value::Obj(ctx.new_object());
                $(ctx.member_set(&object, $js, Value::Num(self.$rust())).map_err(OpError::thrown)?;)*
                Ok(object)
            }
        }
    };
}

timing_getters! {
    navigation_start "navigationStart" => |t: &DomPerformanceTiming| t.start();
    unload_event_start "unloadEventStart" => |_: &DomPerformanceTiming| 0.0;
    unload_event_end "unloadEventEnd" => |_: &DomPerformanceTiming| 0.0;
    redirect_start "redirectStart" => |_: &DomPerformanceTiming| 0.0;
    redirect_end "redirectEnd" => |_: &DomPerformanceTiming| 0.0;
    fetch_start "fetchStart" => |t: &DomPerformanceTiming| t.start();
    domain_lookup_start "domainLookupStart" => |t: &DomPerformanceTiming| t.start();
    domain_lookup_end "domainLookupEnd" => |t: &DomPerformanceTiming| t.start();
    connect_start "connectStart" => |t: &DomPerformanceTiming| t.start();
    connect_end "connectEnd" => |t: &DomPerformanceTiming| t.start();
    secure_connection_start "secureConnectionStart" => |_: &DomPerformanceTiming| 0.0;
    request_start "requestStart" => |t: &DomPerformanceTiming| t.start();
    response_start "responseStart" => |t: &DomPerformanceTiming| t.start();
    response_end "responseEnd" => |t: &DomPerformanceTiming| t.start();
    dom_loading "domLoading" => |t: &DomPerformanceTiming| t.start();
    dom_interactive "domInteractive" => |t: &DomPerformanceTiming| t.read(|times| times.interactive.get());
    dom_content_loaded_event_start "domContentLoadedEventStart" => |t: &DomPerformanceTiming| t.read(|times| times.interactive.get());
    dom_content_loaded_event_end "domContentLoadedEventEnd" => |t: &DomPerformanceTiming| t.read(|times| times.interactive.get());
    dom_complete "domComplete" => |t: &DomPerformanceTiming| t.read(|times| times.complete.get());
    load_event_start "loadEventStart" => |t: &DomPerformanceTiming| t.read(|times| times.load_start.get());
    load_event_end "loadEventEnd" => |t: &DomPerformanceTiming| t.read(|times| times.load_end.get());
}

#[lumen_bind::class(name = "PerformanceNavigation", hint(js(webidl)))]
pub(crate) struct DomPerformanceNavigation {}

#[lumen_bind::methods]
impl DomPerformanceNavigation {
    #[constant(name = "TYPE_NAVIGATE")]
    const TYPE_NAVIGATE: u16 = 0;
    #[constant(name = "TYPE_RELOAD")]
    const TYPE_RELOAD: u16 = 1;
    #[constant(name = "TYPE_BACK_FORWARD")]
    const TYPE_BACK_FORWARD: u16 = 2;
    #[constant(name = "TYPE_RESERVED")]
    const TYPE_RESERVED: u16 = 255;

    #[getter(name = "type")]
    fn kind(&self) -> u16 {
        0
    }

    #[getter(name = "redirectCount")]
    fn redirect_count(&self) -> u16 {
        0
    }

    #[method(name = "toJSON")]
    fn to_json(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let object = Value::Obj(ctx.new_object());
        ctx.member_set(&object, "type", Value::Num(0.0)).map_err(OpError::thrown)?;
        ctx.member_set(&object, "redirectCount", Value::Num(0.0)).map_err(OpError::thrown)?;
        Ok(object)
    }
}

#[lumen_bind::op(name = "timing")]
fn performance_timing(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
    in_receiver_realm(ctx, &this.0, |ctx| {
        cached(ctx, TIMING_SLOT, |ctx, realm| ctx.new_instance(DomPerformanceTiming { realm: Rc::downgrade(realm) }))
    })
}

#[lumen_bind::op(name = "navigation")]
fn performance_navigation(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
    in_receiver_realm(ctx, &this.0, |ctx| cached(ctx, NAVIGATION_SLOT, |ctx, _| ctx.new_instance(DomPerformanceNavigation {})))
}

/// Runs `body` in the realm the `Performance` receiver belongs to, so a getter borrowed from
/// another realm still reports the receiver's own document.
fn in_receiver_realm(ctx: &mut Ctx, receiver: &Value, body: impl FnOnce(&mut Ctx) -> OpResult<Value>) -> OpResult<Value> {
    if ctx.instance_data::<lumen_host::performance::bindings::Performance>(receiver).is_none() {
        return Err(OpError::type_error("Illegal invocation"));
    }
    let constructor = ctx.member_get(receiver, "constructor").map_err(OpError::thrown)?;
    let handle = JsFunction::from_value(constructor).and_then(|function| ctx.function_host_realm(&function).ok());
    match handle {
        Some(handle) => ctx
            .with_host_realm(&handle, body)
            .map_err(|_| OpError::new("InvalidStateError", "performance realm is unavailable"))?,
        None => body(ctx),
    }
}

fn cached(ctx: &mut Ctx, slot: &str, create: impl FnOnce(&mut Ctx, &Rc<DomRealm>) -> Value) -> OpResult<Value> {
    let global = ctx.global_object();
    if let Some(existing) = ctx.native_private_value_slot(&global, slot) {
        return Ok(existing);
    }
    let realm = window_globals::current_dom_realm(ctx)
        .ok_or_else(|| OpError::new("InvalidStateError", "performance has no document"))?;
    let created = create(ctx, &realm);
    ctx.set_native_internal_value_slot(&global, slot, created.clone()).map_err(OpError::thrown)?;
    Ok(created)
}

fn install_legacy_members(ctx: &mut Ctx) -> Result<(), Value> {
    let global = ctx.global_object();
    let timing = ctx.class_constructor::<DomPerformanceTiming>();
    crate::install_interface(ctx, &global, "PerformanceTiming", timing)?;
    let navigation = ctx.class_constructor::<DomPerformanceNavigation>();
    crate::install_interface(ctx, &global, "PerformanceNavigation", navigation)?;
    let constructor = ctx.member_get(&global, "Performance")?;
    if !constructor.is_callable() {
        return Ok(());
    }
    let prototype = ctx.member_get(&constructor, "prototype")?;
    for (name, getter) in [
        ("timing", ctx.op_function::<performance_timing::Op>()),
        ("navigation", ctx.op_function::<performance_navigation::Op>()),
    ] {
        let descriptor = ctx.new_object_with_proto(&Value::Null);
        ctx.member_set(&descriptor, "get", getter)?;
        ctx.member_set(&descriptor, "enumerable", Value::Bool(true))?;
        ctx.member_set(&descriptor, "configurable", Value::Bool(true))?;
        ctx.define_property_value(&prototype, Value::str(name), &descriptor)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen::Engine;

    fn eval(engine: &mut Engine, source: &str) -> Value {
        match engine.eval_value(source).unwrap() {
            Ok(value) => value,
            Err(error) => {
                let stack = engine.ctx().get_member(&error, "stack").ok().and_then(|stack| engine.ctx().coerce_string(&stack).ok());
                panic!("{stack:?}\nSource: {source}");
            }
        }
    }

    fn engine() -> Engine {
        let mut engine = Engine::new();
        lumen_host::install(&mut engine, &[
            Extension { modules: &[lumen_host::performance::install_globals], ..Extension::new("performance") },
        ]);
        crate::install(engine.ctx(), "<body></body>", 64).unwrap();
        lumen_host::install(&mut engine, &[performance_extension()]);
        engine
    }

    #[test]
    fn documents_get_user_timing_observers_and_legacy_navigation_timing() {
        let mut engine = engine();
        let script = r#"(() => {
            const mark = performance.mark('a', { detail: { n: 1 } });
            if (!(mark instanceof PerformanceMark) || mark.name !== 'a' || mark.entryType !== 'mark' || mark.detail.n !== 1) return 'mark';
            performance.mark('b');
            const measure = performance.measure('a-to-b', 'a', 'b');
            if (!(measure instanceof PerformanceMeasure) || measure.entryType !== 'measure' || measure.duration < 0) return 'measure';
            if (performance.getEntriesByName('a').length !== 1 || performance.getEntriesByType('mark').length !== 2) return 'lookup';
            if (performance.getEntries().length !== 3) return 'entries';
            performance.clearMarks('a');
            if (performance.getEntriesByType('mark').length !== 1) return 'clearMarks';
            performance.clearMeasures();
            if (performance.getEntriesByType('measure').length !== 0) return 'clearMeasures';
            let rejected = false;
            try { performance.mark('navigationStart'); } catch (e) { rejected = e.name === 'SyntaxError'; }
            if (!rejected) return 'reserved';
            const timing = performance.timing;
            if (!(timing instanceof PerformanceTiming) || timing !== performance.timing) return 'timing identity';
            const origin = Math.floor(performance.timeOrigin);
            if (timing.navigationStart < origin || timing.fetchStart !== timing.navigationStart) return 'timing start';
            if (timing.loadEventEnd !== 0 && timing.loadEventEnd < timing.navigationStart) return 'timing order';
            if (typeof timing.toJSON().domComplete !== 'number') return 'timing json';
            const navigation = performance.navigation;
            if (!(navigation instanceof PerformanceNavigation) || navigation.type !== 0 || navigation.redirectCount !== 0) return 'navigation';
            if (PerformanceNavigation.TYPE_RELOAD !== 1) return 'constants';
            if (typeof PerformanceObserver !== 'function') return 'observer';
            const observer = new PerformanceObserver(() => {});
            observer.observe({ entryTypes: ['mark'] });
            performance.mark('observed');
            if (observer.takeRecords().map(entry => entry.name).join() !== 'observed') return 'records';
            observer.disconnect();
            return 'ok';
        })()"#;
        let outcome = eval(&mut engine, script);
        let Value::Str(text) = outcome else { panic!("script did not return a status") };
        assert_eq!(text.as_str(), "ok");
    }

    #[test]
    fn legacy_getters_reject_foreign_receivers() {
        let mut engine = engine();
        assert!(matches!(
            eval(&mut engine, r#"(() => {
                const get = Object.getOwnPropertyDescriptor(Performance.prototype, 'timing').get;
                try { get.call({}); return false; } catch (e) { if (!(e instanceof TypeError)) return false; }
                return get.call(performance) === performance.timing;
            })()"#),
            Value::Bool(true)
        ));
    }

    #[test]
    fn navigation_milestones_follow_the_ready_state() {
        let mut engine = engine();
        assert!(matches!(
            eval(&mut engine, "performance.timing.domInteractive === 0 && performance.timing.domComplete === 0 && performance.timing.loadEventEnd === 0"),
            Value::Bool(true)
        ));
        let realm = crate::window_globals::current_dom_realm(engine.ctx()).expect("document realm");
        realm.lifecycle.times.interactive();
        realm.lifecycle.times.complete();
        assert!(matches!(
            eval(&mut engine, "performance.timing.domInteractive >= performance.timing.navigationStart && performance.timing.domComplete >= performance.timing.domInteractive"),
            Value::Bool(true)
        ));
    }
}
