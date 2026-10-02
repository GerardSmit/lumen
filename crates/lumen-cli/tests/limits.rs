use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Every run is killed after this long, so a hung child fails the test instead of the suite.
const HARD_DEADLINE: Duration = Duration::from_secs(30);

fn lumen(args: &[&str]) -> (Output, Duration) {
    lumen_with_stdin(args, Stdio::null(), |_| {})
}

/// Run the CLI with `stdin`; `hold` receives the child's stdin pipe (when piped) and keeps it open
/// until the child has exited.
fn lumen_with_stdin<T>(
    args: &[&str],
    stdin: Stdio,
    hold: impl FnOnce(Option<std::process::ChildStdin>) -> T,
) -> (Output, Duration) {
    let start = Instant::now();
    let mut child = Command::new(env!("CARGO_BIN_EXE_lumen-cli"))
        .args(args)
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn lumen-cli");
    let pid = child.id();
    let held = hold(child.stdin.take());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    let out = match rx.recv_timeout(HARD_DEADLINE) {
        Ok(out) => out.expect("wait for lumen-cli"),
        Err(_) => {
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
            panic!("lumen-cli {args:?} did not exit within {HARD_DEADLINE:?}");
        }
    };
    drop(held);
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
    let (out, elapsed) = lumen(&["--timeout=300", "-e", "/(x+x+)+y/.test('x'.repeat(1e6))"]);
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

/// `--timeout` must end the run through the CLI's normal exit path, not the watchdog's last-resort
/// exit seconds later: exit 124, the message, and the output printed so far.
fn assert_times_out(out: &Output, elapsed: Duration, limit_ms: u64) {
    assert_eq!(out.status.code(), Some(124), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&format!("timed out after {limit_ms} ms")),
        "{stderr}"
    );
    assert!(
        elapsed < Duration::from_millis(limit_ms + 2_500),
        "{elapsed:?}"
    );
}

#[test]
fn timeout_stops_a_run_blocked_in_the_event_loop() {
    let cases = [
        "setInterval(() => {}, 10)",
        "new Promise(() => {}).then(() => {}); setTimeout(() => {}, 1e9)",
        "const net = require('net'); \
         const server = net.createServer(() => {}); \
         server.listen(0, '127.0.0.1', () => { \
           net.connect(server.address().port, '127.0.0.1').on('data', () => {}); \
         })",
        "const { Worker } = require('worker_threads'); \
         new Worker('for (;;) {}', { eval: true })",
        "Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0)",
        "require('child_process').spawnSync('sleep', ['60'])",
        "Bun.serve({ port: 0, fetch() { return new Response('x') } })",
    ];
    for src in cases {
        let script = format!("console.log('started'); {src}");
        let (out, elapsed) = lumen(&["--timeout=300", "-e", &script]);
        assert_times_out(&out, elapsed, 300);
        assert_eq!(stdout(&out), "started\n", "{src}");
    }
}

#[test]
fn timeout_stops_a_run_waiting_for_stdin() {
    let sync_read = "console.log('started'); require('fs').readSync(0, Buffer.alloc(16))";
    let stream_read = "console.log('started'); process.stdin.on('data', () => {})";
    for src in [sync_read, stream_read] {
        let (out, elapsed) =
            lumen_with_stdin(&["--timeout=300", "-e", src], Stdio::piped(), |stdin| stdin);
        assert_times_out(&out, elapsed, 300);
        assert_eq!(stdout(&out), "started\n", "{src}");
    }
}

#[test]
fn timeout_stops_an_awaited_promise_while_a_server_listens() {
    let dir = std::env::temp_dir().join(format!("lumen-limits-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("hang.mjs");
    std::fs::write(
        &file,
        "import net from 'node:net';
         const server = net.createServer(() => {});
         await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
         console.log('listening');
         await new Promise(() => {});",
    )
    .unwrap();
    let (out, elapsed) = lumen(&["--timeout=300", file.to_str().unwrap()]);
    let _ = std::fs::remove_dir_all(&dir);
    assert_times_out(&out, elapsed, 300);
    assert_eq!(stdout(&out), "listening\n");
}
