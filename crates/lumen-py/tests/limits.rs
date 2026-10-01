//! Size limits: operations that would build an enormous sequence, string, bytes object or
//! integer fail with the exception CPython raises, before allocating, and the integer/string
//! digit limit follows CPython's `sys.set_int_max_str_digits`.

use lumen_py::{Interp, Output};
use std::cell::RefCell;
use std::rc::Rc;
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

fn run(src: &str) -> (i32, String, String) {
    let src = src.to_string();
    std::thread::Builder::new()
        .stack_size(1 << 26)
        .spawn(move || {
            let cap = Rc::new(RefCell::new(Captured::default()));
            let mut it = Interp::new();
            it.set_output(Box::new(Capture(cap.clone())));
            let code = it.run_source(&src, "<limits>");
            it.flush_out();
            let c = cap.borrow();
            (code, String::from_utf8_lossy(&c.out).into_owned(), String::from_utf8_lossy(&c.err).into_owned())
        })
        .unwrap()
        .join()
        .unwrap()
}

/// Evaluates each expression in turn and reports `Name: message` or `ok` per line.
fn outcomes(exprs: &[&str]) -> Vec<String> {
    let mut src = String::from("N = 10**12\nHUGE = 10**19\n");
    for e in exprs {
        src.push_str(&format!(
            "try:\n    r = {e}\n    print('ok')\nexcept BaseException as e:\n    print(type(e).__name__ + ': ' + str(e))\n"
        ));
    }
    let started = Instant::now();
    let (code, out, err) = run(&src);
    assert_eq!(code, 0, "{err}");
    assert!(started.elapsed() < Duration::from_secs(5), "size checks must fail fast");
    let lines: Vec<String> = out.lines().map(str::to_string).collect();
    assert_eq!(lines.len(), exprs.len(), "{out}");
    lines
}

fn expect(cases: &[(&str, &str)]) {
    let exprs: Vec<&str> = cases.iter().map(|c| c.0).collect();
    for ((expr, want), got) in cases.iter().zip(outcomes(&exprs)) {
        assert_eq!(&got, want, "{expr}");
    }
}

#[test]
fn repeat_raises_memory_error_before_allocating() {
    let mem = "MemoryError: ";
    expect(&[
        ("[0] * N", mem),
        ("N * [0]", mem),
        ("(0,) * N", mem),
        ("N * (0,)", mem),
        ("'ab' * N", mem),
        ("N * 'ab'", mem),
        ("b'ab' * N", mem),
        ("N * b'ab'", mem),
        ("bytearray(b'ab') * N", mem),
        ("N * bytearray(b'ab')", mem),
        ("[0, 1].__mul__(N)", mem),
        ("[0] * (1 << 40)", mem),
    ]);
}

#[test]
fn repeat_in_place_raises_memory_error() {
    let src = "
def imul(x):
    x *= N
    return x
for v in ([0, 1], bytearray(b'ab')):
    try:
        imul(v)
    except MemoryError:
        print('MemoryError', len(v))
";
    let (code, out, err) = run(&format!("N = 10**12\n{src}"));
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "MemoryError 2\nMemoryError 2\n");
}

#[test]
fn repeat_with_empty_operand_or_zero_count_is_cheap() {
    expect(&[("len([] * N)", "ok"), ("len('' * N)", "ok"), ("len(b'' * N)", "ok"), ("len([0] * 0)", "ok"), ("len('ab' * -N)", "ok")]);
}

#[test]
fn repeat_count_past_ssize_t_is_overflow_error() {
    let msg = "OverflowError: cannot fit 'int' into an index-sized integer";
    expect(&[("[0] * HUGE", msg), ("'a' * HUGE", msg), ("HUGE * b'a'", msg), ("bytes(HUGE)", msg), ("bytearray(HUGE)", msg)]);
}

#[test]
fn materialising_a_huge_range_raises_memory_error() {
    let mem = "MemoryError: ";
    expect(&[
        ("list(range(N))", mem),
        ("tuple(range(N))", mem),
        ("sorted(range(N))", mem),
        ("[*range(N)]", mem),
        ("bytes(range(N))", mem),
        ("list(range(10**30, 10**30 + 5))", "ok"),
        ("len(range(N))", "ok"),
        ("list(range(10**30))", "OverflowError: Python int too large to convert to C ssize_t"),
    ]);
}

#[test]
fn padding_methods_raise_memory_error() {
    let mem = "MemoryError: ";
    expect(&[
        ("'a'.ljust(N)", mem),
        ("'a'.rjust(N)", mem),
        ("'a'.center(N)", mem),
        ("'a'.zfill(N)", mem),
        ("'a'.ljust(N, '\\u20ac')", mem),
        ("b'a'.ljust(N)", mem),
        ("b'a'.rjust(N)", mem),
        ("b'a'.center(N)", mem),
        ("b'a'.zfill(N)", mem),
        ("bytearray(b'a').ljust(N)", mem),
        ("'a'.ljust(5)", "ok"),
    ]);
}

#[test]
fn expandtabs_checks_tabsize_and_result_size() {
    let c = "OverflowError: Python int too large to convert to C int";
    expect(&[
        ("'\\t'.expandtabs(N)", c),
        ("b'\\t'.expandtabs(N)", c),
        ("('\\t' * 4).expandtabs(2**30)", "MemoryError: "),
        ("('\\t' * 4).expandtabs(2**30).__len__()", "MemoryError: "),
        ("'a\\tb'.expandtabs(4)", "ok"),
    ]);
}

#[test]
fn bytes_constructors_raise_memory_error() {
    expect(&[("bytes(N)", "MemoryError: "), ("bytearray(N)", "MemoryError: "), ("bytes(5)", "ok"), ("bytes(-1)", "ValueError: negative count")]);
}

#[test]
fn format_width_and_precision_are_limited() {
    expect(&[
        ("f'{1:{N}}'", "MemoryError: "),
        ("format(1, '1000000000000')", "MemoryError: "),
        ("format('a', '>1000000000000')", "MemoryError: "),
        ("format(1.5, '1000000000000.2f')", "MemoryError: "),
        ("'{:{}}'.format(1, N)", "MemoryError: "),
        ("format(1, '1' * 30)", "ValueError: Too many decimal digits in format string"),
        ("format(1.5, '.1000000000000f')", "ValueError: precision too big"),
        ("format('a', '.1000000000000')", "ok"),
        ("'%1000000000000d' % 1", "MemoryError: "),
        ("'%*d' % (N, 1)", "MemoryError: "),
        ("'%-*s' % (N, 'a')", "MemoryError: "),
        ("'%.1000000000000f' % 1.0", "ValueError: precision too big"),
        ("'%99999999999999999999d' % 1", "ValueError: width too big"),
        ("b'%1000000000000d' % 1", "MemoryError: "),
        ("'%5d' % 1", "ok"),
    ]);
}

#[test]
fn replace_and_join_check_the_result_size_up_front() {
    let mem = "MemoryError: ";
    expect(&[
        ("('a' * 1000).replace('', 'b' * 1000)", "ok"),
        ("('a' * 10**6).replace('', 'b' * 10**6)", mem),
        ("('a' * 10**6).replace('a', 'b' * 10**6)", mem),
        ("(b'a' * 10**6).replace(b'a', b'b' * 10**6)", mem),
        ("(b'a' * 10**6).replace(b'', b'b' * 10**6)", mem),
        ("('a' * 10**6).replace('a', 'b', 3).__len__()", "ok"),
        ("('x' * 10**6).join(['a'] * (3 * 10**6))", mem),
        ("(b'x' * 10**6).join([b'a'] * (3 * 10**6))", mem),
        ("','.join(['a'] * 1000).__len__()", "ok"),
    ]);
}

#[test]
fn int_to_bytes_and_big_int_results_are_limited() {
    expect(&[
        ("(1).to_bytes(N, 'big')", "MemoryError: "),
        ("(1).to_bytes(HUGE, 'big')", "OverflowError: Python int too large to convert to C ssize_t"),
        ("1 << N", "MemoryError: "),
        ("1 << HUGE", "OverflowError: too many digits in integer"),
        ("2 ** N", "MemoryError: "),
        ("3 ** HUGE", "MemoryError: "),
        ("(-1) ** HUGE", "ok"),
        ("1 ** HUGE", "ok"),
        ("(2 ** (2 ** 29)) * (2 ** (2 ** 29))", "MemoryError: "),
    ]);
}

#[test]
fn caps_sit_at_the_documented_sizes() {
    let mem = "MemoryError: ";
    expect(&[
        ("'a' * (2**30 + 1)", mem),
        ("b'a' * (2**30 + 1)", mem),
        ("[0] * (2**26 + 1)", mem),
        ("(0,) * (2**26 + 1)", mem),
        ("'a' * 2**20 + 'b'", "ok"),
    ]);
}

#[test]
fn math_functions_refuse_results_that_cannot_fit() {
    let mem = "MemoryError: ";
    expect(&[
        ("__import__('math').factorial(10**9)", mem),
        ("__import__('math').perm(10**9, 10**9)", mem),
        ("__import__('math').comb(10**18, 10**9)", mem),
        ("__import__('math').factorial(20)", "ok"),
    ]);
}

#[test]
fn int_digit_limit_defaults_and_validation() {
    let src = "
import sys
print(sys.get_int_max_str_digits(), sys.flags.int_max_str_digits)
sys.set_int_max_str_digits(0)
print(sys.get_int_max_str_digits(), sys.flags.int_max_str_digits)
sys.set_int_max_str_digits(5000)
print(len(str(7 ** 5900)))
try:
    str(7 ** 6000)
except ValueError as e:
    print(e)
try:
    int('9' * 6000)
except ValueError as e:
    print(e)
print(len(hex(7 ** 6000)), len(bin(7 ** 6000)), int('1' * 6000, 16) > 0)
";
    let (code, out, err) = run(src);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        out,
        "4300 4300\n0 0\n4987\n\
         Exceeds the limit (5000 digits) for integer string conversion; use sys.set_int_max_str_digits() to increase the limit\n\
         Exceeds the limit (5000 digits) for integer string conversion: value has 6000 digits; use sys.set_int_max_str_digits() to increase the limit\n\
         4214 16847 True\n"
    );
}

#[test]
fn embedder_can_set_the_digit_limit() {
    std::thread::Builder::new()
        .stack_size(1 << 26)
        .spawn(|| {
            let mut it = Interp::new();
            assert_eq!(it.int_max_str_digits(), 4300);
            assert!(!it.set_int_max_str_digits(100));
            assert!(it.set_int_max_str_digits(640));
            assert_eq!(it.int_max_str_digits(), 640);
            let code = it.run_source("try:\n    str(10 ** 640)\nexcept ValueError:\n    raise SystemExit(7)\n", "<limit>");
            assert_eq!(code, 7);
            assert!(it.set_int_max_str_digits(0));
            assert_eq!(it.run_source("assert len(str(10 ** 640)) == 641\n", "<limit>"), 0);
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn digit_limit_error_for_a_huge_value_is_immediate() {
    let started = Instant::now();
    let (code, out, err) = run("try:\n    str(7 ** 2000000)\nexcept ValueError as e:\n    print('ValueError')\n");
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "ValueError\n");
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[test]
fn heap_limit_without_the_counting_allocator() {
    std::thread::Builder::new()
        .stack_size(1 << 26)
        .spawn(|| {
            let mut it = Interp::new();
            it.set_heap_limit(1 << 20);
            let code = it.run_source(
                "try:\n    bytes(10**7)\nexcept MemoryError:\n    pass\nelse:\n    raise SystemExit(3)\nx = bytes(1000)\n",
                "<heap>",
            );
            assert_eq!(code, 0);
        })
        .unwrap()
        .join()
        .unwrap();
}
