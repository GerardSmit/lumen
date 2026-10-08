use lumen_runtime::{Completion, Runtime};

fn evaluate(runtime: &mut Runtime, source: &str) {
    match runtime.eval(source).expect("Performance guard parses") {
        Completion::Value(_) => (),
        Completion::Throw { name, message } => panic!("{name}: {message}"),
    }
}

#[test]
fn browser_worker_reserved_navigation_names_require_window_before_mark_lookup() {
    let mut runtime = Runtime::new_browser();
    evaluate(&mut runtime, r#"
        if (typeof document !== 'undefined') throw new Error('Expected worker-like browser realm');
        for (const marked of [false, true]) {
            if (marked) performance.mark('navigationStart');
            let rejected = false;
            try { performance.measure('reserved', 'navigationStart', 'navigationStart'); }
            catch (error) { rejected = error instanceof TypeError; }
            if (!rejected) throw new Error('Reserved name must require Window before mark lookup');
        }
        performance.mark('ordinary', {startTime: 2});
        if (performance.measure('ordinary', 'ordinary', 'ordinary').duration !== 0)
            throw new Error('Ordinary worker marks still resolve');
    "#);
}

#[test]
fn browser_user_timing_clones_details_and_keeps_observer_records_after_clear() {
    let mut runtime = Runtime::new_browser();
    runtime.expose_gc();
    evaluate(&mut runtime, r#"
        function check(ok) { if (!ok) throw new Error('Performance contract'); }
        check(typeof process === 'undefined' && typeof require === 'undefined' && typeof Buffer === 'undefined');
        check(typeof ReadableStream === 'function');
        const detail = { nested: { value: 1 } };
        detail.self = detail;
        const first = performance.mark('same', {startTime: 10, detail});
        detail.nested.value = 2;
        check(first.detail !== detail && first.detail.self === first.detail && first.detail.nested.value === 1);
        const publicClone = globalThis.structuredClone;
        globalThis.structuredClone = () => { throw new Error('User replacement was invoked'); };
        const independent = performance.mark('intrinsic-clone', {detail});
        const independentMeasure = performance.measure('intrinsic-measure', {start:0, end:1, detail});
        globalThis.structuredClone = publicClone;
        detail.nested.value = 3;
        gc();
        check(independent.detail.nested.value === 2 && independent.detail.self === independent.detail);
        check(independentMeasure.detail.nested.value === 2 && independentMeasure.detail.self === independentMeasure.detail);
        performance.clearMarks('intrinsic-clone');
        performance.clearMeasures('intrinsic-measure');
        new PerformanceMark('constructed', {startTime: 1});
        check(performance.getEntriesByName('constructed').length === 0);
        globalThis.delivery = [];
        const observer = new PerformanceObserver(function(list, self, options) {
            check(this === observer && self === observer && list instanceof PerformanceObserverEntryList);
            check(options.droppedEntriesCount === 0);
            delivery = list.getEntries();
            self.disconnect();
        });
        observer.observe({type: 'mark', buffered: true});
        performance.mark('same', {startTime: 2});
        const measure = performance.measure('between', {start:'same', end:9});
        check(measure.startTime === 2 && measure.duration === 7 && measure.detail === null);
        performance.clearMarks('same');
        check(performance.getEntriesByType('mark').length === 0 && delivery.length === 0);
        let syntax = false;
        try { performance.measure('missing', 'same'); } catch (e) { syntax = e.name === 'SyntaxError'; }
        check(syntax);
        Promise.resolve().then(() => check(delivery.length === 0));
    "#);
    runtime.run_until_idle();
    evaluate(&mut runtime, "check(delivery.length === 2 && delivery[0].startTime === 2 && delivery[1].startTime === 10);");
}

#[test]
fn browser_observer_modes_records_and_reentrant_tasks() {
    let mut runtime = Runtime::new_browser();
    evaluate(&mut runtime, r#"
        function check(ok) { if (!ok) throw new Error('Observer contract'); }
        const observer = new PerformanceObserver(() => { throw new Error('drained observer fired'); });
        observer.observe({type: 'mark'});
        performance.mark('drained');
        check(observer.takeRecords()[0].name === 'drained' && observer.takeRecords().length === 0);
        observer.disconnect();
        let mode = false;
        try { observer.observe({entryTypes: ['mark']}); } catch(e) { mode = e.name === 'InvalidModificationError'; }
        check(mode);
        let sequence = false;
        try { new PerformanceObserver(() => {}).observe({entryTypes: 'mark'}); } catch(e) { sequence = e instanceof TypeError; }
        check(sequence);
        let rejected = false;
        try { performance.mark('invalid', {startTime: -1}); } catch(e) { rejected = e instanceof TypeError; }
        check(rejected && performance.getEntriesByName('invalid').length === 0);
        globalThis.calls = [];
        const reentrant = new PerformanceObserver(list => {
            calls.push(list.getEntries()[0].name);
            if (calls.length === 1) performance.mark('second');
            else reentrant.disconnect();
        });
        reentrant.observe({entryTypes:['mark']});
        performance.mark('first');
    "#);
    runtime.run_until_idle();
    evaluate(&mut runtime, "check(calls.join() === 'first,second');");
}

#[test]
fn node_diagnostics_extend_the_shared_provider() {
    let mut runtime = Runtime::new();
    evaluate(&mut runtime, r#"
        const mark = performance.mark('node');
        const hooks = require('node:perf_hooks');
        if (!(mark instanceof hooks.PerformanceMark) || hooks.performance !== performance)
            throw new Error('Node and global timeline differ');
        if (typeof performance.timerify !== 'function' || typeof hooks.createHistogram !== 'function')
            throw new Error('Node diagnostics were not installed');
        const histogram = hooks.createHistogram();
        const publicNow = performance.now;
        performance.now = () => 0;
        const timed = performance.timerify(() => 7, {histogram});
        for (let i = 0; i < 12; i++) {
            if (timed() !== 7) throw new Error('Timerify return value lost');
        }
        performance.now = publicNow;
        if (histogram.count !== 12 || histogram.min <= 0) throw new Error('Node histogram source lost');
        performance.clearMarks();
        if (performance.getEntriesByType('mark').length !== 0) throw new Error('Node clear failed');
    "#);
}

#[test]
fn browser_foreign_realm_calls_clone_into_the_provider_realm() {
    let mut runtime = Runtime::new_browser();
    runtime.expose_gc();
    let other = runtime.engine().ctx().create_host_realm();
    runtime.install_browser_realm(&other).expect("install independent browser provider");
    let ctx = runtime.engine().ctx();
    let global = ctx.global_object();
    ctx.member_set(&global, "otherWindow", other.global()).ok().expect("publish foreign realm handle");
    evaluate(&mut runtime, r#"
        const foreign = otherWindow.performance.mark('foreign', {detail: {nested: [1,2]}});
        if (!(foreign instanceof otherWindow.PerformanceMark)
            || !(foreign.detail instanceof otherWindow.Object)
            || !(foreign.detail.nested instanceof otherWindow.Array)
            || performance.getEntriesByName('foreign').length !== 0)
            throw new Error('Performance provider crossed realm ownership');
        gc();
        if (otherWindow.performance.getEntriesByName('foreign')[0] !== foreign
            || foreign.detail.nested[1] !== 2)
            throw new Error('Traced foreign entry was lost');
        otherWindow.performance.clearMarks();
        gc();
        if (otherWindow.performance.getEntries().length !== 0 || foreign.detail.nested[0] !== 1)
            throw new Error('Clear incorrectly released an externally owned detail');
    "#);
}
