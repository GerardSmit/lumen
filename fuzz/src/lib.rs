//! Shared plumbing for the fuzz targets. The engine recurses on the native stack and is `!Send`,
//! so every target runs its work on one long-lived worker thread with a large stack (libFuzzer
//! calls the target on the process main thread, which only has 8 MiB).

use std::cell::RefCell;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Mutex, OnceLock};

use lumen::Engine;
#[cfg(feature = "runtime")]
use lumen_runtime::Runtime;

const STACK_BYTES: usize = 256 * 1024 * 1024;

type Job = Box<dyn FnOnce() + Send>;

fn worker() -> &'static Mutex<Sender<Job>> {
    static WORKER: OnceLock<Mutex<Sender<Job>>> = OnceLock::new();
    WORKER.get_or_init(|| {
        let (tx, rx) = channel::<Job>();
        std::thread::Builder::new()
            .name("lumen-fuzz".into())
            .stack_size(STACK_BYTES)
            .spawn(move || {
                for job in rx {
                    job();
                }
            })
            .expect("spawn fuzz worker");
        Mutex::new(tx)
    })
}

/// Run `f` on the big-stack worker and wait for it. A panic in `f` kills the worker and is
/// re-raised here so libFuzzer records the crash.
pub fn on_worker<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    let (tx, rx) = channel();
    worker()
        .lock()
        .unwrap()
        .send(Box::new(move || {
            let _ = tx.send(f());
        }))
        .expect("fuzz worker alive");
    rx.recv().expect("fuzz worker panicked")
}

thread_local! {
    static ENGINE: RefCell<Option<Engine>> = const { RefCell::new(None) };
    #[cfg(feature = "runtime")]
    static RUNTIME: RefCell<Option<Runtime>> = const { RefCell::new(None) };
}

/// Evaluate `src` in the worker's engine, which is recreated whenever `fresh` is set.
pub fn eval(src: String, fresh: bool) {
    on_worker(move || {
        ENGINE.with(|slot| {
            let mut slot = slot.borrow_mut();
            if fresh || slot.is_none() {
                *slot = Some(Engine::new());
                lumen::collect_disposed_realms();
            }
            let _ = slot.as_mut().unwrap().eval(&src, false);
        });
    });
}

/// Evaluate `src` in the worker's full runtime (web extensions installed: `WebAssembly`, ...).
#[cfg(feature = "runtime")]
pub fn eval_runtime(src: String) {
    on_worker(move || {
        RUNTIME.with(|slot| {
            let mut slot = slot.borrow_mut();
            let rt = slot.get_or_insert_with(Runtime::new);
            let _ = rt.eval(&src);
        });
    });
}

/// A JS string literal that evaluates to exactly the UTF-16 code units of `s`.
pub fn js_string_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2 + 2);
    out.push('"');
    for unit in s.encode_utf16() {
        out.push_str(&format!("\\u{unit:04x}"));
    }
    out.push('"');
    out
}
