//! The `lumen-py` binary: `--timeout`, `--max-memory`, `-X int_max_str_digits`,
//! `PYTHONINTMAXSTRDIGITS` and Ctrl-C.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

struct Run {
    code: Option<i32>,
    out: String,
    err: String,
    elapsed: Duration,
}

fn script(name: &str, src: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("lumen-py-cli-{}-{}.py", std::process::id(), name));
    std::fs::write(&path, src).unwrap();
    path
}

fn command(args: &[&str], path: &PathBuf) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_lumen-py"));
    c.args(args).arg(path).env_remove("PYTHONINTMAXSTRDIGITS").env_remove("LUMEN_TIMEOUT_MS");
    c
}

fn finish(mut child: std::process::Child, limit: Duration) -> Run {
    let started = Instant::now();
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let out_h = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = out.read_to_string(&mut s);
        s
    });
    let err_h = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err.read_to_string(&mut s);
        s
    });
    let code = loop {
        match child.try_wait().unwrap() {
            Some(status) => break status.code(),
            None if started.elapsed() > limit => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("process did not exit within {limit:?}");
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    Run { code, out: out_h.join().unwrap(), err: err_h.join().unwrap(), elapsed: started.elapsed() }
}

fn run(args: &[&str], name: &str, src: &str) -> Run {
    let path = script(name, src);
    let child = command(args, &path).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let r = finish(child, Duration::from_secs(20));
    let _ = std::fs::remove_file(path);
    r
}

#[test]
fn timeout_stops_a_busy_loop_with_exit_124() {
    let r = run(&["--timeout=300"], "busy", "while True:\n    pass\n");
    assert_eq!(r.code, Some(124), "{}", r.err);
    assert!(r.err.contains("timed out after 300 ms"), "{}", r.err);
    assert!(r.elapsed < Duration::from_secs(3), "{:?}", r.elapsed);
}

#[test]
fn timeout_stops_a_huge_integer_computation() {
    let r = run(&["--timeout=300"], "bigpow", "x = 3 ** (250 * 10**6)\n");
    assert_eq!(r.code, Some(124), "{}", r.err);
    assert!(r.elapsed < Duration::from_secs(3), "{:?}", r.elapsed);
}

#[test]
fn timeout_does_not_fire_for_a_script_that_finishes() {
    let r = run(&["--timeout=5000"], "quick", "print('done')\n");
    assert_eq!(r.code, Some(0));
    assert_eq!(r.out, "done\n");
}

#[test]
fn timeout_can_come_from_the_environment() {
    let path = script("envtimeout", "while True:\n    pass\n");
    let mut c = command(&[], &path);
    c.env("LUMEN_TIMEOUT_MS", "200");
    let child = c.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let r = finish(child, Duration::from_secs(20));
    let _ = std::fs::remove_file(path);
    assert_eq!(r.code, Some(124), "{}", r.err);
}

#[test]
fn invalid_timeout_is_a_usage_error() {
    let r = run(&["--timeout=soon"], "badtimeout", "print(1)\n");
    assert_eq!(r.code, Some(2));
    assert!(r.err.contains("--timeout=soon"), "{}", r.err);
}

#[test]
fn max_memory_raises_memory_error() {
    let r = run(&["--max-memory=64"], "maxmem", "l = []\nwhile True:\n    l.append(bytearray(100000))\n");
    assert_eq!(r.code, Some(1), "{}", r.err);
    assert!(r.err.contains("MemoryError"), "{}", r.err);
}

#[test]
fn max_memory_leaves_ordinary_scripts_alone() {
    let r = run(&["--max-memory=64"], "smallmem", "print(len([0] * 100000))\n");
    assert_eq!(r.code, Some(0), "{}", r.err);
    assert_eq!(r.out, "100000\n");
}

const DIGITS: &str = "import sys\nprint(sys.get_int_max_str_digits(), sys.flags.int_max_str_digits)\ntry:\n    str(10 ** 1000)\nexcept ValueError:\n    print('limited')\nelse:\n    print('unlimited')\n";

#[test]
fn int_max_str_digits_option_and_environment() {
    assert_eq!(run(&[], "d0", DIGITS).out, "4300 4300\nunlimited\n");
    assert_eq!(run(&["-X", "int_max_str_digits=700"], "d1", DIGITS).out, "700 700\nlimited\n");
    assert_eq!(run(&["-Xint_max_str_digits=0"], "d2", DIGITS).out, "0 0\nunlimited\n");

    let path = script("d3", DIGITS);
    let child = command(&[], &path).env("PYTHONINTMAXSTRDIGITS", "640").stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    assert_eq!(finish(child, Duration::from_secs(20)).out, "640 640\nlimited\n");

    let child = command(&["-X", "int_max_str_digits=5000"], &path).env("PYTHONINTMAXSTRDIGITS", "640").stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    assert_eq!(finish(child, Duration::from_secs(20)).out, "5000 5000\nunlimited\n");
    let _ = std::fs::remove_file(path);
}

#[test]
fn invalid_digit_limits_are_fatal_like_cpython() {
    for bad in ["5", "-1", "abc", "639"] {
        let r = run(&["-X", &format!("int_max_str_digits={bad}")], "dbad", "print(1)\n");
        assert_eq!(r.code, Some(1), "{bad}");
        assert!(r.err.contains("-X int_max_str_digits: invalid limit; must be >= 640 or 0 for unlimited."), "{}", r.err);
        assert_eq!(r.out, "");
    }
    let path = script("dbadenv", "print(1)\n");
    let child = command(&[], &path).env("PYTHONINTMAXSTRDIGITS", "12").stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let r = finish(child, Duration::from_secs(20));
    let _ = std::fs::remove_file(path);
    assert_eq!(r.code, Some(1));
    assert!(r.err.contains("PYTHONINTMAXSTRDIGITS: invalid limit; must be >= 640 or 0 for unlimited."), "{}", r.err);
}

#[cfg(unix)]
#[test]
fn ctrl_c_becomes_keyboard_interrupt() {
    let path = script("sigint", "import sys\nprint('ready')\nsys.stdout.flush()\nwhile True:\n    pass\n");
    let mut child = command(&[], &path).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let mut ready = [0u8; 6];
    child.stdout.as_mut().unwrap().read_exact(&mut ready).unwrap();
    assert_eq!(&ready, b"ready\n");
    let status = Command::new("kill").args(["-INT", &child.id().to_string()]).status().unwrap();
    assert!(status.success());
    let r = finish(child, Duration::from_secs(10));
    let _ = std::fs::remove_file(path);
    assert_eq!(r.code, Some(130), "{}", r.err);
    assert!(r.err.contains("KeyboardInterrupt"), "{}", r.err);
}
