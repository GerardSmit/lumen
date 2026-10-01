use std::process::{Command, Output};
use std::time::{Duration, Instant};

fn lumen(args: &[&str]) -> (Output, Duration) {
    let start = Instant::now();
    let out = Command::new(env!("CARGO_BIN_EXE_lumen-cli"))
        .args(args)
        .output()
        .expect("spawn lumen-cli");
    (out, start.elapsed())
}

#[test]
fn timeout_stops_long_bigint_operations() {
    let cases = [
        "(3n ** 40000000n).toString()",
        "const a = 7n ** 20000000n; a * a",
        "const a = 7n ** 12000000n; (a * a) / (a + 1n)",
        "BigInt('9'.repeat(30000000))",
    ];
    for src in cases {
        let (out, elapsed) = lumen(&["--timeout=300", "-e", src]);
        assert_eq!(out.status.code(), Some(124), "{src}: {out:?}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("timed out after 300 ms"), "{src}");
        assert!(elapsed < Duration::from_millis(1500), "{src}: {elapsed:?}");
    }
}
