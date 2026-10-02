//! Sender build, receiver adoption, structuredClone and real task latency.
use lumen::{
    Completion, Engine,
    embed::Value,
    parallel::{Limits, Parcel, ThreadHost, install},
};
use std::{
    hint::black_box,
    sync::Arc,
    time::{Duration, Instant},
};

fn eval(engine: &mut Engine, source: &str) -> String {
    match engine.eval(source, false).expect("benchmark parses") {
        Completion::Value(value) => value,
        Completion::Throw { name, message } => panic!("{name}: {message}"),
    }
}
fn root(engine: &mut Engine) -> Value {
    engine
        .eval_value("root")
        .expect("root parses")
        .unwrap_or_else(|_| panic!("root"))
}
fn install_clone_baseline(engine: &mut Engine) {
    // Reuse the web crate's actual in-realm copier. These workloads require no
    // encoding, network or file ops; their namespaces are never called.
    eval(
        engine,
        concat!(
            "(function(){\n",
            include_str!("../../lumen-web/src/js/preamble.js"),
            "\n",
            include_str!("../../lumen-web/src/js/events.js"),
            "\n",
            include_str!("../../lumen-web/src/js/blob.js"),
            "\n",
            include_str!("../../lumen-web/src/js/encoding.js"),
            "\n})();"
        ),
    );
}
fn main() {
    let samples = std::env::var("LUMEN_BENCH_SAMPLES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(5)
        .max(1);
    println!("graph,build_us,adopt_us,run_us,structured_clone_us,parcel_bytes,objects");
    for (name, source, transfer) in [
        ("small", "var root={a:1,b:'text',c:[2,3]};", false),
        (
            "tree_10k",
            "var root={v:0};var nodes=[root];for(let i=1;i<10000;i++){let node={v:i};nodes[(i-1)>>1][i%2?'left':'right']=node;nodes.push(node);}nodes=null;",
            false,
        ),
        (
            "array_100k",
            "var root=Array.from({length:100000},(_,i)=>i);",
            false,
        ),
        (
            "map_1m",
            "var root=new Map();for(let i=0;i<1000000;i++)root.set(i,i);",
            false,
        ),
        ("buffer_1k", "var root=new ArrayBuffer(1024);", true),
        ("buffer_1m", "var root=new ArrayBuffer(1024*1024);", true),
        (
            "buffer_64m",
            "var root=new ArrayBuffer(64*1024*1024);",
            true,
        ),
    ] {
        let mut totals = [Duration::ZERO; 4];
        let mut bytes = 0;
        let mut objects = 0;
        for _ in 0..samples {
            let mut sender = Engine::new();
            eval(&mut sender, source);
            let value = root(&mut sender);
            let mut receiver = Engine::new();
            let start = Instant::now();
            let parcel = if transfer {
                Parcel::build_with_transfer(
                    sender.ctx(),
                    &value,
                    std::slice::from_ref(&value),
                    Limits::default(),
                )
            } else {
                Parcel::build(sender.ctx(), &value, Limits::default())
            }
            .unwrap_or_else(|_| panic!("parcel build"));
            totals[0] += start.elapsed();
            bytes = parcel.bytes();
            objects = parcel.objects();
            let start = Instant::now();
            let adopted = receiver.ctx().adopt(parcel);
            totals[1] += start.elapsed();
            black_box(adopted);
            drop(receiver);
            drop(value);
            drop(sender);

            let mut sender = Engine::new();
            install(&mut sender, Arc::new(ThreadHost::default()));
            eval(&mut sender, source);
            let start = Instant::now();
            eval(
                &mut sender,
                if transfer {
                    "var done=false;Lumen.parallel.run(x=>x.byteLength,[root],{transfer:[root]}).then(()=>done=true,e=>{throw e;});"
                } else {
                    "var done=false;Lumen.parallel.run(x=>1,[root]).then(()=>done=true,e=>{throw e;});"
                },
            );
            loop {
                sender.ctx().poll_async();
                while sender.run_one_job() {}
                if eval(&mut sender, "done") == "true" {
                    break;
                }
                assert!(start.elapsed() < Duration::from_secs(120), "run timed out");
                std::thread::yield_now();
            }
            totals[2] += start.elapsed();
            drop(sender);

            let mut sender = Engine::new();
            install_clone_baseline(&mut sender);
            eval(&mut sender, source);
            let start = Instant::now();
            eval(
                &mut sender,
                if transfer {
                    "structuredClone(root,{transfer:[root]});undefined;"
                } else {
                    "structuredClone(root);undefined;"
                },
            );
            totals[3] += start.elapsed();
        }
        let us = totals.map(|v| v.as_secs_f64() * 1e6 / samples as f64);
        println!(
            "{name},{:.2},{:.2},{:.2},{:.2},{bytes},{objects}",
            us[0], us[1], us[2], us[3]
        );
    }
}
