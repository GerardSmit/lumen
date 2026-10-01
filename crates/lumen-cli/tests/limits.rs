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

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn heap_limit_throws_a_catchable_range_error() {
    let (out, _) = lumen(&[
        "--max-old-space-size=256",
        "-e",
        "try { Array.from({length: 1e9}); console.log('no error'); } \
         catch (e) { console.log(e instanceof RangeError, e.message); } \
         console.log([1, 2, 3].map(x => x * 2).join());",
    ]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(stdout(&out), "true JavaScript heap out of memory\n2,4,6\n");
}

#[test]
fn heap_limit_stops_script_level_growth() {
    let (out, _) = lumen(&[
        "--max-old-space-size=128",
        "-e",
        "const keep = []; \
         try { for (;;) keep.push({ a: [1, 2, 3], s: 'x'.repeat(64) + keep.length }); } \
         catch (e) { keep.length = 0; console.log(e.name, e.message); }",
    ]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(stdout(&out), "RangeError JavaScript heap out of memory\n");
}

#[test]
fn timeout_stops_a_native_loop() {
    let (out, elapsed) = lumen(&[
        "--timeout=200",
        "-e",
        "Array.prototype.indexOf.call({length: 2**53 - 1}, 1)",
    ]);
    assert_eq!(out.status.code(), Some(124), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("timed out after 200 ms"));
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
}

#[test]
fn timeout_stops_a_script_loop_and_a_pending_timer() {
    let (out, _) = lumen(&["--timeout=200", "-e", "for (;;) {}"]);
    assert_eq!(out.status.code(), Some(124), "{out:?}");
    let (out, elapsed) = lumen(&["--timeout=200", "-e", "setTimeout(() => {}, 1e8)"]);
    assert_eq!(out.status.code(), Some(124), "{out:?}");
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
}

#[test]
fn zlib_max_output_length_stops_a_decompression_bomb() {
    let (out, _) = lumen(&[
        "-e",
        r#"const zlib = require('zlib');
        const bomb = Buffer.alloc(64 << 20);
        const cases = [
          ['inflateSync', zlib.deflateSync(bomb)],
          ['inflateRawSync', zlib.deflateRawSync(bomb)],
          ['gunzipSync', zlib.gzipSync(bomb)],
          ['unzipSync', zlib.gzipSync(bomb)],
          ['brotliDecompressSync', zlib.brotliCompressSync(bomb)],
        ];
        for (const [fn, packed] of cases) {
          try { zlib[fn](packed, { maxOutputLength: 2 ** 20 }); console.log(fn, 'no error'); }
          catch (e) { console.log(fn, e.name, e.code, e.message); }
        }
        console.log(zlib.inflateSync(zlib.deflateSync(Buffer.from('ok')), { maxOutputLength: 2 }).toString());
        zlib.inflate(cases[0][1], { maxOutputLength: 1024 }, (e) => console.log('async', e && e.code));"#,
    ]);
    assert!(out.status.success(), "{out:?}");
    let msg = "RangeError ERR_BUFFER_TOO_LARGE Cannot create a Buffer larger than 1048576 bytes";
    assert_eq!(
        stdout(&out),
        format!(
            "inflateSync {msg}\ninflateRawSync {msg}\ngunzipSync {msg}\nunzipSync {msg}\n\
             brotliDecompressSync {msg}\nok\nasync ERR_BUFFER_TOO_LARGE\n"
        )
    );
}

#[test]
fn timeout_stops_catastrophic_regex_backtracking() {
    let (out, elapsed) = lumen(&[
        "--timeout=300",
        "-e",
        "/(x+x+)+y/.test('x'.repeat(1e6))",
    ]);
    assert_eq!(out.status.code(), Some(124), "{out:?}");
    assert!(elapsed < Duration::from_millis(1500), "{elapsed:?}");
}

#[test]
fn heap_limit_stops_a_huge_regex_backtrack_stack() {
    let (out, elapsed) = lumen(&[
        "--max-old-space-size=64",
        "-e",
        "try { /(?:(x)|y)*z/.test('x'.repeat(3e6)); console.log('no error'); } \
         catch (e) { console.log(e instanceof RangeError, e.message); }",
    ]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(stdout(&out), "true JavaScript heap out of memory\n");
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
}
