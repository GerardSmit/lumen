//! Stopping a running script from another thread: `InterruptHandle::interrupt` makes the
//! script unwind with `KeyboardInterrupt` within a short time wherever it is spending it.

use lumen_py::{Interp, InterruptHandle, Output};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[derive(Default)]
struct Captured {
    out: Vec<u8>,
    err: Vec<u8>,
}

struct Capture(Rc<RefCell<Captured>>);

impl Output for Capture {
    fn write_stdout(&mut self, bytes: &[u8]) {
        self.0.borrow_mut().out.extend_from_slice(bytes);
    }

    fn write_stderr(&mut self, bytes: &[u8]) {
        self.0.borrow_mut().err.extend_from_slice(bytes);
    }
}

struct Finished {
    code: i32,
    interrupted: bool,
    out: String,
    err: String,
}

const STOP_WITHIN: Duration = Duration::from_millis(1000);

/// Runs `src` on its own thread, interrupts it after `after`, and returns the outcome together
/// with how long the script took to stop once interrupted. The interpreter is dropped on its
/// thread, so the join also covers teardown.
fn interrupted_run(src: &str, after: Duration) -> (Finished, Duration) {
    let src = src.to_string();
    let (tx, rx) = mpsc::channel::<InterruptHandle>();
    let worker = std::thread::Builder::new()
        .stack_size(1 << 28)
        .spawn(move || {
            let cap = Rc::new(RefCell::new(Captured::default()));
            let mut it = Interp::new();
            it.set_output(Box::new(Capture(cap.clone())));
            tx.send(it.interrupt_handle()).unwrap();
            let code = it.run_source(&src, "<interrupt>");
            it.flush_out();
            let interrupted = it.was_interrupted();
            let c = cap.borrow();
            Finished {
                code,
                interrupted,
                out: String::from_utf8_lossy(&c.out).into_owned(),
                err: String::from_utf8_lossy(&c.err).into_owned(),
            }
        })
        .unwrap();
    let handle = rx.recv().unwrap();
    std::thread::sleep(after);
    let sent = Instant::now();
    handle.interrupt();
    let done = worker.join().unwrap();
    (done, sent.elapsed())
}

fn assert_stops(src: &str) {
    let (done, took) = interrupted_run(src, Duration::from_millis(150));
    assert!(took < STOP_WITHIN, "took {took:?} to stop {src:?}");
    assert!(done.interrupted, "{}", done.err);
    assert_eq!(done.code, 130, "{}", done.err);
    assert!(done.err.contains("KeyboardInterrupt"), "{}", done.err);
}

#[test]
fn handle_is_cloneable_and_send() {
    fn check<T: Clone + Send + Sync + 'static>() {}
    check::<InterruptHandle>();
}

#[test]
fn busy_loop_stops() {
    assert_stops("while True:\n    pass\n");
}

#[test]
fn loop_with_calls_stops() {
    assert_stops("def f(x):\n    return x + 1\nx = 0\nwhile True:\n    x = f(x)\n");
}

#[test]
fn deep_recursive_computation_stops() {
    assert_stops("def fib(n):\n    return n if n < 2 else fib(n - 1) + fib(n - 2)\nfib(60)\n");
}

#[test]
fn huge_sorted_stops() {
    assert_stops(
        "l = list(range(3 * 10**7, 0, -1))\nwhile True:\n    l = sorted(l)\n    l.reverse()\n",
    );
}

#[test]
fn huge_sort_with_key_stops() {
    assert_stops("l = list(range(10**7))\nl.sort(key=lambda v: -v)\n");
}

#[test]
fn huge_bigint_pow_stops() {
    assert_stops("x = 3 ** (250 * 10**6)\n");
}

#[test]
fn huge_bigint_multiplication_stops() {
    assert_stops("x = 7 ** (10 ** 8)\ny = x * x\n");
}

#[test]
fn huge_bigint_division_stops() {
    assert_stops("x = 3 ** (10 ** 8)\nq, r = divmod(x, 7 ** (10 ** 7))\n");
}

#[test]
fn huge_bigint_to_string_stops() {
    assert_stops(
        "import sys\nsys.set_int_max_str_digits(0)\nx = 3 ** (20 * 10 ** 6)\ns = str(x)\n",
    );
}

#[test]
fn huge_modular_pow_stops() {
    assert_stops("print(pow(3, 2 ** (2 ** 26), 10 ** 9 + 7))\n");
}

#[test]
fn native_loops_stop() {
    assert_stops("sum(range(10**13))\n");
    assert_stops("'x'.join(str(i) for i in range(10**13))\n");
    assert_stops("max(range(10**13))\n");
}

#[test]
fn generator_and_comprehension_loops_stop() {
    assert_stops("def gen():\n    while True:\n        yield 1\nfor _ in gen():\n    pass\n");
    assert_stops("[i for i in range(10**13)]\n");
}

#[test]
fn except_exception_does_not_catch_the_interrupt() {
    let (done, took) = interrupted_run("try:\n    while True:\n        pass\nexcept Exception:\n    print('caught')\nprint('after')\n", Duration::from_millis(100));
    assert!(took < STOP_WITHIN);
    assert!(done.interrupted);
    assert_eq!(done.out, "");
    assert!(done.err.contains("KeyboardInterrupt"));
}

#[test]
fn the_interrupt_cannot_be_swallowed() {
    let src = "while True:\n    try:\n        while True:\n            pass\n    except BaseException:\n        pass\n";
    let (done, took) = interrupted_run(src, Duration::from_millis(100));
    assert!(took < STOP_WITHIN, "took {took:?}");
    assert!(done.interrupted);
}

#[test]
fn keyboard_interrupt_is_a_base_exception_only() {
    let mut it = Interp::new();
    let code = it.run_source(
        "assert issubclass(KeyboardInterrupt, BaseException)\nassert not issubclass(KeyboardInterrupt, Exception)\n",
        "<ki>",
    );
    assert_eq!(code, 0);
}

#[test]
fn script_finishing_normally_is_not_reported_as_interrupted() {
    let mut it = Interp::new();
    assert_eq!(it.run_source("x = 1\n", "<ok>"), 0);
    assert!(!it.was_interrupted());
    assert_eq!(it.run_source("raise ValueError('x')\n", "<err>"), 1);
    assert!(!it.was_interrupted());
}

#[test]
fn interpreter_is_reusable_after_an_interrupt() {
    std::thread::Builder::new()
        .stack_size(1 << 26)
        .spawn(|| {
            let mut it = Interp::new();
            let handle = it.interrupt_handle();
            let stopper = std::thread::spawn({
                let h = handle.clone();
                move || {
                    std::thread::sleep(Duration::from_millis(100));
                    h.interrupt();
                }
            });
            assert_eq!(it.run_source("while True:\n    pass\n", "<first>"), 130);
            stopper.join().unwrap();
            assert!(it.was_interrupted());
            assert!(!handle.is_interrupted());
            assert_eq!(it.run_source("print(sum(range(10)))\n", "<second>"), 0);
            assert!(!it.was_interrupted());
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn interrupt_before_the_run_stops_it_at_the_first_safe_point() {
    std::thread::Builder::new()
        .stack_size(1 << 26)
        .spawn(|| {
            let mut it = Interp::new();
            it.interrupt_handle().interrupt();
            assert_eq!(it.run_source("while True:\n    pass\n", "<pre>"), 130);
            assert!(it.was_interrupted());
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn interrupt_during_a_pending_exception_still_reports_keyboard_interrupt() {
    let src = "def f():\n    x = 3 ** (250 * 10**6)\n    return [][x]\ntry:\n    f()\nexcept Exception:\n    print('wrong')\n";
    let (done, took) = interrupted_run(src, Duration::from_millis(100));
    assert!(took < STOP_WITHIN);
    assert!(done.interrupted);
    assert_eq!(done.out, "");
}

#[test]
fn polling_adds_no_output_or_state() {
    let mut it = Interp::new();
    assert_eq!(
        it.run_source("print(sum(i * 2 for i in range(100000)))\n", "<sum>"),
        0
    );
}

#[test]
fn sleep_is_interruptible() {
    let mut it = Interp::new();
    let h = it.interrupt_handle();
    let t = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(100));
        h.interrupt();
    });
    let start = std::time::Instant::now();
    let code = it.run_source("import time\ntime.sleep(60)\n", "<s>");
    t.join().unwrap();
    assert_eq!(code, 130);
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
}

#[test]
fn caught_interrupt_at_end_of_module_still_stops() {
    let mut it = Interp::new();
    let h = it.interrupt_handle();
    let t = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(100));
        h.interrupt();
    });
    let code = it.run_source(
        "try:\n    while True:\n        pass\nexcept KeyboardInterrupt:\n    pass\n",
        "<s>",
    );
    t.join().unwrap();
    assert_eq!(code, 130);
    assert!(it.was_interrupted());
}
