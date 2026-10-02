use super::{Limits, Parcel, parcel::HeapGuard};
use crate::{
    Completion, Engine,
    value::{Value, live_objects, set_data},
};

fn value(engine: &mut Engine, name: &str) -> Value {
    let global = Value::Obj(engine.interp.global.clone());
    engine
        .interp
        .get_member(&global, name)
        .unwrap_or_else(|_| panic!("global lookup"))
}
fn evaluate(engine: &mut Engine, source: &str) -> String {
    match engine.eval(source, false).unwrap() {
        Completion::Value(value) => value,
        Completion::Throw { name, message } => panic!("{name}: {message}; source: {source}"),
    }
}

fn await_global(engine: &mut Engine, name: &str) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        engine.ctx().poll_async();
        for _ in 0..128 {
            if !engine.run_one_job() {
                break;
            }
        }
        let result = evaluate(engine, name);
        if result != "undefined" {
            return result;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "parallel completion timed out"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

#[test]
fn shared_wait_notify_across_parallel_realms() {
    let mut engine = Engine::new();
    super::install(
        &mut engine,
        std::sync::Arc::new(super::ThreadHost::default()),
    );
    evaluate(
        &mut engine,
        "var shared=new SharedArrayBuffer(16);var words=new Int32Array(shared,4,1);var ready;var answer;var task=Lumen.parallel.spawn((port,words)=>{port.postMessage('ready');return {wait:Atomics.wait(words,0,0,3000),value:Atomics.load(words,0)};},[words]);task.receive().then(x=>ready=x);task.result.then(x=>answer=JSON.stringify(x),e=>answer=String(e));",
    );
    assert_eq!(await_global(&mut engine, "ready"), "ready");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while evaluate(&mut engine, "Atomics.notify(words,0,1)") != "1" {
        assert!(std::time::Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert_eq!(
        await_global(&mut engine, "answer"),
        "{\"wait\":\"ok\",\"value\":0}"
    );
    evaluate(
        &mut engine,
        "var modes;Lumen.parallel.run(words=>[Atomics.wait(words,0,1,0),Atomics.wait(words,0,0,1)],[words]).then(x=>modes=JSON.stringify(x));",
    );
    assert_eq!(
        await_global(&mut engine, "modes"),
        "[\"not-equal\",\"timed-out\"]"
    );
}

#[test]
fn shared_bigint_wait_count_and_cancellation() {
    let mut engine = Engine::new();
    super::install(
        &mut engine,
        std::sync::Arc::new(super::ThreadHost::default()),
    );
    evaluate(
        &mut engine,
        "var words=new BigInt64Array(new SharedArrayBuffer(16),8,1);var ready=0;var tasks=[];var replies=0;for(let n=0;n<2;n++){let t=Lumen.parallel.spawn((port,words)=>{port.postMessage(true);return Atomics.wait(words,0,0n,3000);},[words]);t.receive().then(()=>ready++);t.result.then(x=>{if(x==='ok')replies++;});tasks.push(t);}",
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while evaluate(&mut engine, "ready") != "2" {
        engine.ctx().poll_async();
        while engine.run_one_job() {}
        assert!(std::time::Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert_eq!(evaluate(&mut engine, "Atomics.notify(words,0,0)"), "0");
    let mut woken = 0;
    while woken < 2 {
        woken += evaluate(&mut engine, "Atomics.notify(words,0,1)")
            .parse::<usize>()
            .unwrap();
        assert!(std::time::Instant::now() < deadline);
        std::thread::yield_now();
    }
    while evaluate(&mut engine, "replies") != "2" {
        engine.ctx().poll_async();
        while engine.run_one_job() {}
        assert!(std::time::Instant::now() < deadline);
    }
    evaluate(
        &mut engine,
        "var entered;var cancelled;var blocked=Lumen.parallel.spawn((port,words)=>{port.postMessage(true);Atomics.wait(words,0,0n);},[words]);blocked.receive().then(x=>entered=x);blocked.result.catch(x=>cancelled=x);",
    );
    assert_eq!(await_global(&mut engine, "entered"), "true");
    evaluate(&mut engine, "blocked.terminate('stop');");
    assert_eq!(await_global(&mut engine, "cancelled"), "stop");
}

#[test]
fn checkpoint_preserves_task_state_messages_and_transfers() {
    let mut engine = Engine::new();
    super::install(
        &mut engine,
        std::sync::Arc::new(super::ThreadHost::default()),
    );
    evaluate(
        &mut engine,
        "var done;var state={bytes:new Uint8Array([40,2])};state.self=state;var task=Lumen.parallel.spawn(async(port,state)=>{await port.receive();return port.migrate(async(port,state)=>{const value=await port.receive();return {sum:state.bytes[0]+state.bytes[1]+value,cycle:state.self===state};},[state],{transfer:[state.bytes.buffer]});},[state]);task.postMessage(0);task.postMessage(1);task.result.then(x=>done=JSON.stringify(x),e=>done=String(e));",
    );
    assert_eq!(
        await_global(&mut engine, "done"),
        "{\"sum\":43,\"cycle\":true}"
    );
    assert_eq!(
        evaluate(
            &mut engine,
            "task.placement.core>0 && state.bytes.byteLength===2"
        ),
        "true"
    );
}

#[test]
fn checkpoint_rejects_captures_and_live_child_tasks() {
    let mut engine = Engine::new();
    super::install(
        &mut engine,
        std::sync::Arc::new(super::ThreadHost::default()),
    );
    evaluate(
        &mut engine,
        "var errors;Lumen.parallel.spawn(port=>{let local=1;let capture=false;try{port.migrate(()=>local);}catch(e){capture=e instanceof TypeError;}let child=Lumen.parallel.spawn(()=>new Promise(()=>{}));child.result.catch(()=>{});let children=false;try{port.migrate(()=>42);}catch(e){children=e instanceof TypeError;}child.terminate();return [capture,children];}).result.then(x=>errors=JSON.stringify(x));",
    );
    assert_eq!(await_global(&mut engine, "errors"), "[true,true]");
}

#[test]
fn checkpoint_launch_failure_rejects_and_releases_capacity() {
    struct Host;
    impl super::ParallelHost for Host {
        fn spawn(
            &self,
            cpu: super::CpuHint,
            job: super::Job,
        ) -> Result<(super::Placement, super::TaskHandle), super::SpawnError> {
            if job.previous_core().is_some() {
                return Err(super::SpawnError("offline".into()));
            }
            super::ParallelHost::spawn(&super::ThreadHost::default(), cpu, job)
        }
        fn interrupt_after(&self, task: super::TaskHandle, grace: std::time::Duration) {
            super::ParallelHost::interrupt_after(&super::ThreadHost::default(), task, grace);
        }
    }
    let mut engine = Engine::new();
    super::install_with_limits(
        &mut engine,
        std::sync::Arc::new(Host),
        super::Limits {
            active_tasks: 1,
            ..Default::default()
        },
    );
    evaluate(
        &mut engine,
        "var failed;Lumen.parallel.spawn(port=>port.migrate(()=>42)).result.catch(e=>failed=e.code);",
    );
    assert_eq!(await_global(&mut engine, "failed"), "ERR_TASK_CANCELLED");
    evaluate(
        &mut engine,
        "var recovered;Lumen.parallel.run(()=>42).then(x=>recovered=x);",
    );
    assert_eq!(await_global(&mut engine, "recovered"), "42");
}

#[test]
fn checkpoint_reserves_before_reentrant_getters() {
    let mut engine = Engine::new();
    super::install(
        &mut engine,
        std::sync::Arc::new(super::ThreadHost::default()),
    );
    evaluate(
        &mut engine,
        "var result;Lumen.parallel.spawn(port=>port.migrate((port,state)=>state.value,[{get value(){try{__parallelMigrate(()=>1,[],'any',[],[]);return 'nested checkpoint accepted';}catch(e){return e instanceof TypeError?42:String(e);}}}])).result.then(x=>result=x,e=>result=String(e));",
    );
    assert_eq!(await_global(&mut engine, "result"), "42");
}

#[test]
fn signals_close_iteration_argument_shapes_and_error_classes() {
    let mut engine = Engine::new();
    super::install(
        &mut engine,
        std::sync::Arc::new(super::ThreadHost::default()),
    );
    assert_eq!(
        evaluate(
            &mut engine,
            "(()=>{try{Lumen.parallel.run(()=>1,{});return false;}catch(e){return e instanceof TypeError;}})()"
        ),
        "true"
    );
    evaluate(
        &mut engine,
        "var empty;Lumen.parallel.run(()=>42,undefined,{cpu:'performance'}).then(x=>empty=x);",
    );
    assert_eq!(await_global(&mut engine, "empty"), "42");
    evaluate(
        &mut engine,
        "var stopped;var buffer=new ArrayBuffer(8);var signal=AbortSignal.abort('already');Lumen.parallel.run(()=>1,[buffer],{signal,transfer:[buffer]}).catch(x=>stopped=x);",
    );
    assert_eq!(await_global(&mut engine, "stopped"), "already");
    assert_eq!(evaluate(&mut engine, "buffer.byteLength"), "8");
    evaluate(
        &mut engine,
        "var failure;Lumen.parallel.run(()=>{throw new RangeError('wrong');}).catch(e=>failure=e instanceof RangeError && e.message==='wrong');",
    );
    assert_eq!(await_global(&mut engine, "failure"), "true");
    evaluate(
        &mut engine,
        "var sum;var task=Lumen.parallel.spawn(async port=>{let sum=0;for await(const value of port)sum+=value;return sum;});task.result.then(x=>sum=x);task.postMessage(40);task.postMessage(2);task.close();",
    );
    assert_eq!(await_global(&mut engine, "sum"), "42");
    evaluate(
        &mut engine,
        "var ready,flushed,reason;var controller=new AbortController();var cooperative=Lumen.parallel.spawn(port=>new Promise(resolve=>{Lumen.parallel.signal.addEventListener('abort',()=>{port.postMessage(String(port.signal.reason));resolve();});port.postMessage('ready');}),[],{signal:controller.signal,grace:1000});cooperative.result.catch(e=>reason=e);cooperative.receive().then(x=>ready=x);",
    );
    assert_eq!(await_global(&mut engine, "ready"), "ready");
    evaluate(
        &mut engine,
        "cooperative.receive().then(x=>flushed=x);controller.abort(Symbol('stop'));",
    );
    assert_eq!(
        await_global(&mut engine, "flushed"),
        "AbortError: unclonable abort reason"
    );
    assert_eq!(
        evaluate(&mut engine, "controller.signal._listeners.length"),
        "0"
    );
    assert_eq!(evaluate(&mut engine, "typeof reason"), "symbol");
}

#[test]
fn repeated_tasks_release_parcels_and_return_to_a_steady_heap() {
    let mut engine = Engine::new();
    super::install(
        &mut engine,
        std::sync::Arc::new(super::ThreadHost::default()),
    );
    evaluate(
        &mut engine,
        "var answer;Lumen.parallel.run(x=>x+1,[41]).then(x=>answer=x);",
    );
    assert_eq!(await_global(&mut engine, "answer"), "42");
    engine.collect_garbage();
    let baseline = live_objects();
    for _ in 0..1000 {
        evaluate(
            &mut engine,
            "answer=undefined;Lumen.parallel.run(x=>x+1,[41]).then(x=>answer=x);",
        );
        assert_eq!(await_global(&mut engine, "answer"), "42");
    }
    engine.collect_garbage();
    assert!(
        live_objects() <= baseline + 16,
        "parallel task roots accumulated: {} -> {}",
        baseline,
        live_objects()
    );
}

#[test]
fn closing_a_port_from_a_getter_preserves_transfer_ownership() {
    let mut engine = Engine::new();
    super::install(
        &mut engine,
        std::sync::Arc::new(super::ThreadHost::default()),
    );
    evaluate(
        &mut engine,
        "var ready;var task=Lumen.parallel.spawn(port=>{port.postMessage(true);return new Promise(()=>{});});task.result.catch(()=>{});task.receive().then(x=>ready=x);",
    );
    assert_eq!(await_global(&mut engine, "ready"), "true");
    assert_eq!(
        evaluate(
            &mut engine,
            "(()=>{const buffer=new ArrayBuffer(8);const message={get closing(){task.close();return 1;},buffer};try{task.postMessage(message,{transfer:[buffer]});return false;}catch(e){return e instanceof TypeError&&buffer.byteLength===8;}})()"
        ),
        "true"
    );
    evaluate(&mut engine, "task.terminate();");
}

#[test]
fn installed_web_abort_globals_keep_their_identity() {
    let mut engine = Engine::new();
    evaluate(&mut engine, "globalThis.performance={now:()=>Date.now()};");
    evaluate(
        &mut engine,
        concat!(
            "(function(){\n",
            include_str!("../../../lumen-web/src/js/preamble.js"),
            "\n",
            include_str!("../../../lumen-web/src/js/events.js"),
            "\n})();"
        ),
    );
    evaluate(
        &mut engine,
        "var originalController=AbortController,originalSignal=AbortSignal;",
    );
    super::install(
        &mut engine,
        std::sync::Arc::new(super::ThreadHost::default()),
    );
    assert_eq!(
        evaluate(
            &mut engine,
            "AbortController===originalController&&AbortSignal===originalSignal&&new AbortController().signal instanceof EventTarget"
        ),
        "true"
    );
    evaluate(
        &mut engine,
        "var cancelled;var controller=new AbortController();var task=Lumen.parallel.spawn(()=>new Promise(()=>{}),[],{signal:controller.signal});task.result.catch(x=>cancelled=x);controller.abort('web abort');",
    );
    assert_eq!(await_global(&mut engine, "cancelled"), "web abort");
}

#[test]
fn host_cancellation_delivers_a_stable_code_and_worker_signal() {
    let mut engine = Engine::new();
    super::install(
        &mut engine,
        std::sync::Arc::new(super::ThreadHost::default()),
    );
    evaluate(
        &mut engine,
        "var ready,code,flushed;var task=Lumen.parallel.spawn(port=>new Promise(resolve=>{Lumen.signal.addEventListener('abort',()=>{port.postMessage(port.signal.reason.code);resolve();});port.postMessage(true);}));task.receive().then(x=>ready=x);task.result.catch(e=>code=e.name+':'+e.code);",
    );
    assert_eq!(await_global(&mut engine, "ready"), "true");
    evaluate(&mut engine, "task.receive().then(x=>flushed=x);");
    let handle = engine
        .interp
        .host_mut::<super::api::Realm>()
        .unwrap()
        .first_task();
    handle.cancel(
        super::CancelReason::Host("core offline"),
        std::time::Duration::from_secs(1),
    );
    assert_eq!(
        await_global(&mut engine, "code"),
        "AbortError:ERR_TASK_CANCELLED"
    );
    assert_eq!(await_global(&mut engine, "flushed"), "ERR_TASK_CANCELLED");
}

#[test]
fn disposed_parcels_release_empty_chunks_on_a_persistent_owner() {
    let mut sender = Engine::new();
    evaluate(&mut sender, "var root=[x=>x.value+1,{value:41}];");
    let root = value(&mut sender, "root");
    let (send, receive) = std::sync::mpsc::channel::<Parcel>();
    let receiver = std::thread::spawn(move || {
        let mut baseline = None;
        for parcel in receive {
            {
                let mut engine = Engine::new();
                let root = engine.interp.adopt(parcel);
                set_data(&engine.interp.global, "root", root);
                assert_eq!(evaluate(&mut engine, "root[0](root[1])"), "42");
            }
            crate::collect_quiescent_realms();
            let chunks = crate::value::gc_heap_chunks();
            let warm = *baseline.get_or_insert(chunks);
            assert!(
                chunks <= warm,
                "empty parcel chunks retained: {warm} -> {chunks}"
            );
        }
    });
    for _ in 0..128 {
        send.send(
            Parcel::build(&mut sender.interp, &root, Limits::default())
                .unwrap_or_else(|_| panic!("build")),
        )
        .unwrap_or_else(|_| panic!("receiver exited"));
    }
    drop(send);
    receiver.join().unwrap();
}

#[test]
fn tasks_execute_copy_arguments_and_exchange_messages() {
    let mut engine = Engine::new();
    super::install(
        &mut engine,
        std::sync::Arc::new(super::ThreadHost::default()),
    );
    evaluate(
        &mut engine,
        "var answer; var input={v:21}; Lumen.parallel.run(x=>x.v*2,[input]).then(x=>answer=x,e=>answer=e.name+':'+e.message); input.v=9;",
    );
    assert_eq!(await_global(&mut engine, "answer"), "42");
    evaluate(
        &mut engine,
        "var reply;var result;var task=Lumen.parallel.spawn(async(port,k)=>{const x=await port.receive();port.postMessage(x*k);return 7;},[6]);task.receive().then(x=>reply=x);task.result.then(x=>result=x,e=>result=e.name+':'+e.message);task.postMessage(7);",
    );
    assert_eq!(await_global(&mut engine, "reply"), "42");
    assert_eq!(await_global(&mut engine, "result"), "7");
}

#[test]
fn tasks_cancel_busy_code_and_reject_outer_bindings() {
    let mut engine = Engine::new();
    super::install(
        &mut engine,
        std::sync::Arc::new(super::ThreadHost::default()),
    );
    assert_eq!(
        evaluate(
            &mut engine,
            "(()=>{const captured=3;try{Lumen.parallel.run(()=>captured);return false;}catch(e){return e instanceof TypeError&&e.message.includes('captured');}})()"
        ),
        "true"
    );
    evaluate(
        &mut engine,
        "var entered;var cancelled;var task=Lumen.parallel.spawn(port=>{port.postMessage('entered');while(true){}},[],{grace:10});task.receive().then(x=>entered=x);task.result.catch(e=>cancelled=e);",
    );
    assert_eq!(await_global(&mut engine, "entered"), "entered");
    evaluate(&mut engine, "task.terminate('stop');");
    assert_eq!(await_global(&mut engine, "cancelled"), "stop");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while engine
        .interp
        .host_mut::<super::api::Realm>()
        .unwrap()
        .task_count()
        != 0
    {
        engine.ctx().poll_async();
        while engine.run_one_job() {}
        assert!(
            std::time::Instant::now() < deadline,
            "worker failed to stop"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

#[test]
fn module_exports_and_realm_shutdown() {
    assert_eq!(
        crate::compile_snapshot(include_str!("glue.js")).unwrap(),
        include_bytes!("glue.bin")
    );
    let mut unavailable = Engine::new();
    match unavailable
        .eval_module(
            "import {run} from 'lumen:parallel';",
            "unavailable",
            |_, _| Some(("spoofed".into(), "export const run=()=>0;".into())),
        )
        .unwrap()
    {
        Completion::Throw { name, message } => {
            assert_eq!(name, "TypeError");
            assert!(message.contains("parallel host"));
        }
        _ => panic!("host loader spoofed a reserved module without a parallel host"),
    }
    drop(unavailable);
    let mut engine = Engine::new();
    super::install(
        &mut engine,
        std::sync::Arc::new(super::ThreadHost::default()),
    );
    match engine.eval_module("import {run,spawn} from 'lumen:parallel';globalThis.exportsMatch=run===Lumen.parallel.run&&spawn===Lumen.parallel.spawn;", "main", |_, _| None).unwrap() {
        Completion::Value(_) => {},
        Completion::Throw {name,message} => panic!("{name}: {message}"),
    }
    assert_eq!(evaluate(&mut engine, "exportsMatch"), "true");
    evaluate(
        &mut engine,
        "var stopped;var task=Lumen.parallel.spawn(async port=>{await port.receive();});task.result.catch(e=>stopped=e.name+':'+e.code);",
    );
    super::shutdown(&mut engine, std::time::Duration::ZERO);
    assert_eq!(
        await_global(&mut engine, "stopped"),
        "AbortError:ERR_SHUTDOWN"
    );
    assert_eq!(evaluate(&mut engine, "Lumen.signal.aborted"), "true");
}

#[test]
fn task_limits_precede_transfer_and_arguments_are_read_once() {
    let mut engine = Engine::new();
    super::install_with_limits(
        &mut engine,
        std::sync::Arc::new(super::ThreadHost::default()),
        Limits {
            queue_depth: 1,
            active_tasks: 1,
            ..Limits::default()
        },
    );
    evaluate(
        &mut engine,
        "var answer;var reads=0;var args=[];Object.defineProperty(args,'0',{get(){reads++;return 42;},enumerable:true});Lumen.parallel.run(x=>x,args).then(x=>answer=x);",
    );
    assert_eq!(await_global(&mut engine, "answer"), "42");
    assert_eq!(evaluate(&mut engine, "reads"), "1");
    evaluate(
        &mut engine,
        "var ready;var task=Lumen.parallel.spawn(async port=>{port.postMessage('ready');await new Promise(()=>{});});task.receive().then(x=>ready=x);task.result.catch(()=>{});",
    );
    assert_eq!(await_global(&mut engine, "ready"), "ready");
    assert_eq!(
        evaluate(
            &mut engine,
            "(()=>{const b=new ArrayBuffer(8);try{Lumen.parallel.run(x=>x,[b],{transfer:[b]});return false;}catch(e){return e instanceof RangeError&&b.byteLength===8;}})()"
        ),
        "true"
    );
    evaluate(&mut engine, "task.postMessage(1);");
    assert_eq!(
        evaluate(
            &mut engine,
            "(()=>{const b=new ArrayBuffer(8);try{task.postMessage(b,{transfer:[b]});return false;}catch(e){return e instanceof RangeError&&b.byteLength===8;}})()"
        ),
        "true"
    );
    super::shutdown(&mut engine, std::time::Duration::ZERO);
}

#[test]
fn parent_drop_interrupts_a_child_already_in_a_grace_period() {
    let mut engine = Engine::new();
    super::install(
        &mut engine,
        std::sync::Arc::new(super::ThreadHost::default()),
    );
    evaluate(
        &mut engine,
        "var ready;var task=Lumen.parallel.spawn(port=>{port.postMessage('ready');while(true){}},[],{grace:60000});task.receive().then(x=>ready=x);task.result.catch(()=>{});",
    );
    assert_eq!(await_global(&mut engine, "ready"), "ready");
    let handle = engine
        .interp
        .host_mut::<super::api::Realm>()
        .unwrap()
        .first_task();
    evaluate(&mut engine, "task.terminate();");
    drop(engine);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !handle.is_finished() {
        assert!(
            std::time::Instant::now() < deadline,
            "orphan worker failed to stop"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

#[test]
fn tasks_copy_helpers_and_resolve_worker_builtins() {
    let mut engine = Engine::new();
    super::install(
        &mut engine,
        std::sync::Arc::new(super::ThreadHost::default()),
    );
    evaluate(
        &mut engine,
        "var answer;Lumen.parallel.run((even,odd)=>JSON.stringify([even(8,odd),Math.max(1,42),Lumen.parallel.signal.aborted]),[function even(n,odd){return n===0?true:odd(n-1,even);},function odd(n,even){return n===0?false:even(n-1,odd);}]).then(x=>answer=x,e=>answer=e.name+':'+e.message);",
    );
    assert_eq!(await_global(&mut engine, "answer"), "[true,42,false]");
    evaluate(
        &mut engine,
        "var returned;Lumen.parallel.run(()=>()=>42).then(fn=>returned=fn(),e=>returned=e.name+':'+e.message);",
    );
    assert_eq!(await_global(&mut engine, "returned"), "42");
}

#[test]
fn classes_copy_code_fields_methods_and_evaluated_statics() {
    let mut sender = Engine::new();
    evaluate(
        &mut sender,
        "var root=class Counter {static count=41;value=3;constructor(n){this.value+=n;}get total(){return this.value;}add(n){this.value+=n;return this;}static answer(){return this.count+1;}};root.count=42;",
    );
    let root = value(&mut sender, "root");
    let parcel =
        Parcel::build(&mut sender.interp, &root, Limits::default()).unwrap_or_else(|error| {
            panic!(
                "class clone: {}",
                sender
                    .interp
                    .coerce_string(&error)
                    .map(|s| s.to_string())
                    .unwrap_or_default()
            )
        });
    drop(sender);
    std::thread::spawn(move || {
        let mut receiver = Engine::new();
        let root = receiver.interp.adopt(parcel);
        set_data(&receiver.interp.global, "Copied", root);
        assert_eq!(evaluate(&mut receiver, "var instance=new Copied(4);JSON.stringify([instance.add(5).total,Copied.answer(),instance instanceof Copied,Object.getOwnPropertyDescriptor(Copied.prototype,'total').enumerable]);"), "[12,43,true,false]");
        receiver.collect_garbage();
        assert_eq!(evaluate(&mut receiver, "new Copied(7).total"), "10");
    }).join().unwrap();
}

#[test]
fn classes_preserve_inheritance_private_brands_and_reject_captures() {
    let mut sender = Engine::new();
    evaluate(
        &mut sender,
        "var root={Private:class Private{#value=40;static #n=2;get(){return this.#value+Private.#n;}},Derived:(()=>{const Base=class{constructor(n){this.n=n;}get(){return this.n;}};return class extends Base{extra=2;get(){return super.get()+this.extra;}};})(),Array:class extends Array{sum(){return this.reduce((a,b)=>a+b,0);}}};",
    );
    let root = value(&mut sender, "root");
    let parcel =
        Parcel::build(&mut sender.interp, &root, Limits::default()).unwrap_or_else(|error| {
            panic!(
                "class clone: {}",
                sender
                    .interp
                    .coerce_string(&error)
                    .map(|s| s.to_string())
                    .unwrap_or_default()
            )
        });
    std::thread::spawn(move || {
        let mut receiver = Engine::new();
        evaluate(&mut receiver, "var Original=class Private{#value=7;static #n=1;get(){return this.#value+Private.#n;}};");
        let root = receiver.interp.adopt(parcel);
        set_data(&receiver.interp.global, "root", root);
        assert_eq!(evaluate(&mut receiver, "new root.Private().get()"), "42");
        assert_eq!(evaluate(&mut receiver, "new root.Derived(40).get()"), "42");
        assert_eq!(evaluate(&mut receiver, "new root.Array(20,22).sum()"), "42");
        assert_eq!(evaluate(&mut receiver, "(()=>{try{root.Private.prototype.get.call(new Original());return false;}catch(e){return e instanceof TypeError;}})()"), "true");
        receiver.collect_garbage();
        assert_eq!(evaluate(&mut receiver, "new root.Private().get()"), "42");
    }).join().unwrap();
    for source in [
        "(()=>{const captured=42;return class{field=captured;};})()",
        "(()=>{const captured=42;return class{get(){return captured;}};})()",
    ] {
        evaluate(&mut sender, &format!("var bad={source};"));
        let bad = value(&mut sender, "bad");
        let error = Parcel::build(&mut sender.interp, &bad, Limits::default())
            .err()
            .expect("capture error");
        set_data(&sender.interp.global, "error", error);
        assert_eq!(
            evaluate(
                &mut sender,
                "error instanceof TypeError && error.message.includes('captured') && error.message.includes('args')"
            ),
            "true"
        );
    }
}

#[test]
fn tasks_copy_class_arguments_and_partial_class_drops_release_the_heap() {
    let mut engine = Engine::new();
    super::install(
        &mut engine,
        std::sync::Arc::new(super::ThreadHost::default()),
    );
    evaluate(
        &mut engine,
        "var answer;Lumen.parallel.run(C=>new C(40).get(),[class{value=2;constructor(n){this.value+=n;}get(){return this.value;}}]).then(x=>answer=x,e=>answer=e.name+':'+e.message);",
    );
    assert_eq!(await_global(&mut engine, "answer"), "42");
    evaluate(&mut engine, "var root=class{#v=42;get(){return this.#v;}};");
    let root = value(&mut engine, "root");
    let baseline = live_objects();
    for _ in 0..100 {
        let parcel = Parcel::build(&mut engine.interp, &root, Limits::default())
            .unwrap_or_else(|_| panic!("class build"));
        let state = parcel.heap.clone();
        std::thread::spawn(move || {
            drop(parcel);
            let _entered = HeapGuard::enter(&state);
            assert_eq!(live_objects(), 0);
        })
        .join()
        .unwrap();
        assert_eq!(live_objects(), baseline);
    }
}

#[test]
fn classes_preserve_auto_accessors() {
    let mut sender = Engine::new();
    evaluate(
        &mut sender,
        "var root=class {accessor value=40;static accessor extra=2;sum(){return this.value+this.constructor.extra;}};root.extra=3;",
    );
    let root = value(&mut sender, "root");
    let parcel =
        Parcel::build(&mut sender.interp, &root, Limits::default()).unwrap_or_else(|error| {
            panic!(
                "accessor clone: {}",
                sender
                    .interp
                    .coerce_string(&error)
                    .map(|s| s.to_string())
                    .unwrap_or_default()
            )
        });
    std::thread::spawn(move || {
        let mut receiver = Engine::new();
        let root = receiver.interp.adopt(parcel);
        set_data(&receiver.interp.global, "Copied", root);
        assert_eq!(evaluate(&mut receiver, "var object=new Copied();object.value=39;object.sum()"), "42");
        assert_eq!(evaluate(&mut receiver, "(()=>{try{Object.getOwnPropertyDescriptor(Copied.prototype,'value').get.call({});return false;}catch(e){return e instanceof TypeError;}})()"), "true");
    }).join().unwrap();
}

#[test]
fn classes_preserve_well_known_symbol_methods_and_fields() {
    let mut sender = Engine::new();
    evaluate(
        &mut sender,
        "var root=class Values { [Symbol.toStringTag]='Values'; *[Symbol.iterator](){yield 40;yield 2;} }; ",
    );
    let root = value(&mut sender, "root");
    let parcel = Parcel::build(&mut sender.interp, &root, Limits::default())
        .unwrap_or_else(|_| panic!("symbol class clone"));
    std::thread::spawn(move || {
        let mut receiver=Engine::new();
        let root=receiver.interp.adopt(parcel);
        set_data(&receiver.interp.global,"Copied",root);
        assert_eq!(evaluate(&mut receiver,"var object=new Copied();JSON.stringify([[...object],Object.prototype.toString.call(object)])"),"[[40,2],\"[object Values]\"]");
        receiver.collect_garbage();
        assert_eq!(evaluate(&mut receiver,"[...object].reduce((a,b)=>a+b,0)"),"42");
    }).join().unwrap();
}

#[test]
fn graph_copy_preserves_identity_holes_keys_and_snapshot_time() {
    let mut sender = Engine::new();
    evaluate(
        &mut sender,
        "var shared={v:42}; var root={a:shared,b:shared,array:[,shared,,]}; root.self=root; root.array.extra='yes'; root.text='a'.repeat(1024).slice(200,700); root.big=123456789012345678901234567890n;",
    );
    let root = value(&mut sender, "root");
    let parcel = Parcel::build(&mut sender.interp, &root, Limits::default())
        .unwrap_or_else(|_| panic!("build"));
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
    evaluate(
        &mut sender,
        "var root={};root.self=root;root.text='x'.repeat(2000);",
    );
    let root = value(&mut sender, "root");
    let parcel = Parcel::build(&mut sender.interp, &root, Limits::default())
        .unwrap_or_else(|_| panic!("build"));
    let heap = parcel.heap.clone();
    std::thread::spawn(move || {
        drop(parcel);
        let _entered = HeapGuard::enter(&heap);
        assert_eq!(live_objects(), 0);
    })
    .join()
    .unwrap();
}

#[test]
fn clone_limits_and_getter_failure_restore_sender_state() {
    let mut sender = Engine::new();
    evaluate(
        &mut sender,
        "var root={a:{}};Object.defineProperty(root,'fail',{enumerable:true,get:function(){throw Error('getter failed');}});",
    );
    let root = value(&mut sender, "root");
    let heap = crate::value::gc_state_handle();
    let error = Parcel::build(&mut sender.interp, &root, Limits::default())
        .err()
        .expect("throw");
    assert!(sender.interp.is_error_value(&error));
    assert!(std::sync::Arc::ptr_eq(
        &heap,
        &crate::value::gc_state_handle()
    ));
    let limit = Limits {
        objects: 0,
        ..Limits::default()
    };
    assert!(Parcel::build(&mut sender.interp, &root, limit).is_err());
    evaluate(&mut sender, "var symbol=Symbol('x');");
    let symbol = value(&mut sender, "symbol");
    assert!(Parcel::build(&mut sender.interp, &symbol, Limits::default()).is_err());
}

#[test]
fn builtin_slots_and_views_survive_adoption_and_gc() {
    let mut sender = Engine::new();
    evaluate(
        &mut sender,
        "var key={v:42};var buffer=new ArrayBuffer(32);var typed=new Uint16Array(buffer,4,3);typed[1]=1234;var view=new DataView(buffer,4,6);var root={key:key,map:new Map([[key,key]]),set:new Set([key]),date:new Date(123456),regex:/a+/gi,num:new Number(7),str:new String('ok'),bool:new Boolean(true),big:Object(123n),error:new TypeError('bad',{cause:key}),buffer:buffer,typed:typed,view:view};root.regex.lastIndex=2;",
    );
    let root = value(&mut sender, "root");
    let parcel = Parcel::build(&mut sender.interp, &root, Limits::default())
        .unwrap_or_else(|_| panic!("build builtins"));
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
    evaluate(
        &mut sender,
        "var buffer=new ArrayBuffer(65536);var root={buffer:buffer,view:new Uint8Array(buffer)};root.view[0]=42;",
    );
    let root = value(&mut sender, "root");
    let buffer = value(&mut sender, "buffer");
    let pointer = crate::value::Gc::as_ptr(buffer.as_obj().unwrap()) as usize;
    let backing = sender.interp.array_buffers[&pointer].as_ptr() as usize;
    evaluate(&mut sender, "var unsupported=Symbol('x');");
    let unsupported = value(&mut sender, "unsupported");
    assert!(
        Parcel::build_with_transfer(
            &mut sender.interp,
            &unsupported,
            std::slice::from_ref(&buffer),
            Limits::default()
        )
        .is_err()
    );
    assert!(sender.interp.is_transferable_array_buffer(&buffer));
    let parcel = Parcel::build_with_transfer(
        &mut sender.interp,
        &root,
        std::slice::from_ref(&buffer),
        Limits::default(),
    )
    .unwrap_or_else(|_| panic!("transfer"));
    assert!(!sender.interp.is_transferable_array_buffer(&buffer));
    std::thread::spawn(move || {
        let mut receiver = Engine::new();
        let root = receiver.interp.adopt(parcel);
        set_data(&receiver.interp.global, "root", root);
        let buffer = value(&mut receiver, "root");
        let buffer = receiver
            .interp
            .get_member(&buffer, "buffer")
            .unwrap_or_else(|_| panic!("buffer"));
        let pointer = crate::value::Gc::as_ptr(buffer.as_obj().unwrap()) as usize;
        assert_eq!(
            receiver.interp.array_buffers[&pointer].as_ptr() as usize,
            backing
        );
        assert_eq!(evaluate(&mut receiver, "root.view[0]"), "42");
    })
    .join()
    .unwrap();
}

#[test]
fn shared_buffer_aliases_backing_without_sharing_js_objects() {
    let mut sender = Engine::new();
    evaluate(
        &mut sender,
        "var shared=new SharedArrayBuffer(1048576);var root={buffer:shared,view:new Int32Array(shared)};root.view[0]=7;",
    );
    let root = value(&mut sender, "root");
    let parcel = Parcel::build(
        &mut sender.interp,
        &root,
        Limits {
            bytes: 4096,
            ..Limits::default()
        },
    )
    .unwrap_or_else(|_| panic!("shared build"));
    std::thread::spawn(move || {
        let mut receiver = Engine::new();
        let root = receiver.interp.adopt(parcel);
        set_data(&receiver.interp.global, "root", root);
        assert_eq!(evaluate(&mut receiver, "root.view[0]"), "7");
        evaluate(&mut receiver, "Atomics.store(root.view,0,42);");
        receiver.collect_garbage();
        assert_eq!(
            evaluate(
                &mut receiver,
                "root.buffer instanceof SharedArrayBuffer && root.view.buffer===root.buffer"
            ),
            "true"
        );
    })
    .join()
    .unwrap();
    assert_eq!(evaluate(&mut sender, "root.view[0]"), "42");
}

#[test]
fn unsupported_objects_are_rejected_and_repeated_drop_is_steady() {
    let mut sender = Engine::new();
    for source in [
        "Symbol('x')",
        "new Proxy({}, {})",
        "Promise.resolve(1)",
        "new WeakMap()",
        "new WeakSet()",
        "new WeakRef({})",
        "(function*(){yield 1})()",
        "Math.sin",
        "(function(){}).bind(null)",
        "new (class { #secret=1; })()",
    ] {
        evaluate(&mut sender, &format!("var root={source};"));
        let root = value(&mut sender, "root");
        assert!(
            Parcel::build(&mut sender.interp, &root, Limits::default()).is_err(),
            "accepted {source}"
        );
    }
    evaluate(&mut sender, "var root={child:{text:'abc'}};root.self=root;");
    let root = value(&mut sender, "root");
    sender.collect_garbage();
    let before = live_objects();
    for _ in 0..1000 {
        let parcel = Parcel::build(&mut sender.interp, &root, Limits::default())
            .unwrap_or_else(|_| panic!("build"));
        drop(parcel);
    }
    sender.collect_garbage();
    assert_eq!(live_objects(), before);
}

#[test]
fn functions_use_receiver_globals_and_fresh_code() {
    let mut sender = Engine::new();
    evaluate(
        &mut sender,
        "var root={arrow:(x)=>Math.max(x,7),async:async function(x){return x+1;},generator:function*(x){yield x;},strict:function(x){'use strict';return this===undefined?x:0;},recursive:function factorial(n){return n<2?1:n*factorial(n-1);},shadow:(x)=>{let outer=9;return {outer:x}.outer+outer;}};",
    );
    let root = value(&mut sender, "root");
    let parcel = Parcel::build(&mut sender.interp, &root, Limits::default())
        .unwrap_or_else(|_| panic!("functions"));
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
        (
            "(()=>{const config=7;return (x=config)=>{let config=9;return x;};})()",
            "config",
        ),
        ("(()=>{const config=7;return ({x=config})=>x;})()", "config"),
        ("(()=>this)", "this"),
        ("(function(){return ()=>arguments;})()", "arguments"),
        ("(function(){return ()=>new.target;})()", "new.target"),
    ] {
        evaluate(&mut sender, &format!("var root={source};"));
        let root = value(&mut sender, "root");
        let error = Parcel::build(&mut sender.interp, &root, Limits::default())
            .err()
            .expect("capture error");
        set_data(&sender.interp.global, "error", error);
        assert_eq!(
            evaluate(
                &mut sender,
                &format!(
                    "error instanceof TypeError && error.message.includes('{name}') && error.message.includes('args')"
                )
            ),
            "true"
        );
    }
    evaluate(&mut sender, "const userGlobal=7;var root=()=>userGlobal;");
    let root = value(&mut sender, "root");
    assert!(Parcel::build(&mut sender.interp, &root, Limits::default()).is_err());
    evaluate(
        &mut sender,
        "var outer=7;var root=(config)=>{const outer=3;return config.outer+outer;};",
    );
    let root = value(&mut sender, "root");
    assert!(Parcel::build(&mut sender.interp, &root, Limits::default()).is_ok());
}

#[test]
fn buffer_flags_and_all_typed_array_kinds_are_preserved() {
    let mut sender = Engine::new();
    evaluate(
        &mut sender,
        "var names=['Int8Array','Uint8Array','Uint8ClampedArray','Int16Array','Uint16Array','Int32Array','Uint32Array','Float16Array','Float32Array','Float64Array','BigInt64Array','BigUint64Array'];var root={arrays:names.map(n=>new globalThis[n](2)),resizable:new ArrayBuffer(8,{maxByteLength:32})};root.arrays.forEach((a,i)=>a[0]=i>9?7n:7);root.track=new Uint8Array(root.resizable);root.track[0]=42;Object.setPrototypeOf(root.resizable,null);",
    );
    let root = value(&mut sender, "root");
    let parcel = Parcel::build(&mut sender.interp, &root, Limits::default())
        .unwrap_or_else(|_| panic!("buffer flags"));
    std::thread::spawn(move || {
        let mut receiver = Engine::new();
        let root = receiver.interp.adopt(parcel);
        set_data(&receiver.interp.global, "root", root);
        assert_eq!(evaluate(&mut receiver, "root.arrays.map(a=>String(a[0])).join(',')"), "7,7,7,7,7,7,7,7,7,7,7,7");
        assert_eq!(evaluate(&mut receiver, "root.arrays.map(a=>a.constructor.name).join(',')"), "Int8Array,Uint8Array,Uint8ClampedArray,Int16Array,Uint16Array,Int32Array,Uint32Array,Float16Array,Float32Array,Float64Array,BigInt64Array,BigUint64Array");
        assert_eq!(evaluate(&mut receiver, "root.resizable.resizable && root.resizable.maxByteLength===32"), "true");
        evaluate(&mut receiver, "root.resizable.resize(16);");
        assert_eq!(evaluate(&mut receiver, "root.track.length===16 && root.track[0]===42"), "true");
    }).join().unwrap();
}
