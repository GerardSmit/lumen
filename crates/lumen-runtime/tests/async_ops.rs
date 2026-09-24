//! `#[op(async)]` inside the runtime: the op body runs on a worker thread (not the loop thread),
//! several run concurrently, the event loop keeps firing timers meanwhile, and the promise settles
//! on the loop thread — resolved with the converted result, or rejected with a `SendError`.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use lumen::embed::SendError;
use lumen_runtime::Runtime;

static REPORT: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn thread_name() -> String {
    format!("{:?}", std::thread::current().id())
}

/// Sleeps on whatever thread runs it and returns that thread's id.
#[lumen::op(async, name = "slowThreadId")]
fn slow_thread_id(ms: u32) -> String {
    std::thread::sleep(Duration::from_millis(ms as u64));
    thread_name()
}

#[lumen::op(async, name = "failAsync")]
fn fail_async(code: String) -> Result<u32, SendError> {
    Err(SendError::new("RangeError", "no luck").with_code(code))
}

#[lumen::op(name = "loopThreadId")]
fn loop_thread_id() -> String {
    thread_name()
}

#[lumen::op]
fn report(line: String) {
    REPORT.lock().unwrap().push(line);
}

#[test]
fn async_ops_run_off_the_loop_thread() {
    let mut rt = Runtime::new();
    rt.engine()
        .define_ops("t", lumen::ops![slow_thread_id, fail_async, loop_thread_id, report]);
    let started = Instant::now();
    rt.eval(
        r#"
        let ticks = 0;
        const timer = setInterval(() => ticks++, 10);
        Promise.all([t.slowThreadId(200), t.slowThreadId(200), t.slowThreadId(200)]).then((ids) => {
          clearInterval(timer);
          const main = t.loopThreadId();
          t.report(`off-loop ${ids.every((id) => id !== main)}`);
          t.report(`ticks ${ticks >= 5}`);
        });
        t.failAsync("E_NOPE").then(
          () => t.report("resolved?"),
          (e) => t.report(`rejected ${e instanceof RangeError} ${e.code} ${e.message}`),
        );
        "#,
    )
    .expect("parses");
    let elapsed = started.elapsed();
    let report = REPORT.lock().unwrap().clone();
    assert!(report.contains(&"off-loop true".to_string()), "{report:?}");
    assert!(report.contains(&"ticks true".to_string()), "{report:?}");
    assert!(report.contains(&"rejected true E_NOPE no luck".to_string()), "{report:?}");
    // Three 200 ms bodies on a 4-thread pool overlap.
    assert!(elapsed < Duration::from_millis(550), "took {elapsed:?}");
}
