//! `Interp::set_heap_limit`: exceeding the budget raises `MemoryError`. This binary installs the
//! size-class allocator, which makes the accounting exact; without it only single requests larger
//! than the budget are refused (see `heap_limit_without_the_counting_allocator` in limits.rs).

use lumen_common::fastalloc::{thread_live_bytes, ClassAlloc};
use lumen_py::{Interp, Output};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

#[global_allocator]
static ALLOC: ClassAlloc = ClassAlloc;

const MB: usize = 1 << 20;

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

fn run_limited(limit: usize, src: &str) -> (i32, String, String) {
    let src = src.to_string();
    std::thread::Builder::new()
        .stack_size(1 << 26)
        .spawn(move || {
            let cap = Rc::new(RefCell::new(Captured::default()));
            let mut it = Interp::new();
            it.set_output(Box::new(Capture(cap.clone())));
            it.set_heap_limit(limit);
            let code = it.run_source(&src, "<heap>");
            it.flush_out();
            let c = cap.borrow();
            (code, String::from_utf8_lossy(&c.out).into_owned(), String::from_utf8_lossy(&c.err).into_owned())
        })
        .unwrap()
        .join()
        .unwrap()
}

#[test]
fn growth_in_a_loop_raises_memory_error() {
    let started = Instant::now();
    let (code, out, err) = run_limited(
        64 * MB,
        "l = []\ntry:\n    while True:\n        l.append(bytearray(100000))\nexcept MemoryError:\n    n = len(l)\n    l = None\n    print('MemoryError', n > 100)\n",
    );
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "MemoryError True\n");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn many_small_objects_count_too() {
    let (code, out, err) = run_limited(
        32 * MB,
        "d = {}\ntry:\n    i = 0\n    while True:\n        d[i] = str(i) * 4\n        i += 1\nexcept MemoryError:\n    d = None\n    print('MemoryError', i > 1000)\n",
    );
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "MemoryError True\n");
}

#[test]
fn uncaught_memory_error_exits_with_a_traceback() {
    let (code, _, err) = run_limited(16 * MB, "x = []\nwhile True:\n    x.append([0] * 1000)\n");
    assert_eq!(code, 1);
    assert!(err.contains("MemoryError"), "{err}");
}

#[test]
fn single_large_allocation_is_refused_before_it_happens() {
    let (code, out, err) = run_limited(
        8 * MB,
        "for f in (lambda: 'a' * 10**8, lambda: [0] * 10**7, lambda: bytes(10**8), lambda: 2 ** (2 ** 28), lambda: ('x' * 10).ljust(10**8)):\n    try:\n        f()\n        print('allocated')\n    except MemoryError:\n        print('MemoryError')\n",
    );
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "MemoryError\n".repeat(5));
}

#[test]
fn memory_is_reusable_after_the_error_is_handled() {
    let src = "
try:
    big = [0] * 10**6
    print('allocated')
except MemoryError:
    print('MemoryError')
big = None
small = [0] * 1000
print(len(small))
for _ in range(3):
    try:
        junk = bytearray(10 * 2**20)
        print('ok')
        junk = None
    except MemoryError:
        print('MemoryError')
";
    let (code, out, err) = run_limited(40 * MB, src);
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "allocated\n1000\nok\nok\nok\n");
}

#[test]
fn memory_error_is_an_ordinary_exception() {
    let (code, out, err) = run_limited(8 * MB, "try:\n    [0] * 10**8\nexcept Exception as e:\n    print(type(e).__name__)\n");
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "MemoryError\n");
}

#[test]
fn scripts_within_the_budget_are_unaffected() {
    let (code, out, err) = run_limited(256 * MB, "l = [i * 2 for i in range(200000)]\nprint(len(l), sum(l))\ns = ''.join(str(i) for i in range(10000))\nprint(len(s))\n");
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "200000 39999800000\n38890\n");
}

#[test]
fn limit_can_be_removed() {
    std::thread::Builder::new()
        .stack_size(1 << 26)
        .spawn(|| {
            let mut it = Interp::new();
            it.set_heap_limit(4 * MB);
            assert_eq!(it.run_source("x = bytearray(10**7)\n", "<a>"), 1);
            it.set_heap_limit(0);
            assert_eq!(it.run_source("x = bytearray(10**7)\n", "<b>"), 0);
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn counting_allocator_tracks_live_bytes() {
    let before = thread_live_bytes();
    let v = vec![0u8; 5 * MB];
    assert!(thread_live_bytes() - before >= (5 * MB) as isize);
    drop(v);
    assert!(thread_live_bytes() - before < MB as isize);
}
