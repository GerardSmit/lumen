//! `node:net` on the loop's readiness reactor: idle sockets own no threads, and a read that hit
//! `WouldBlock` is re-armed for the next chunk.
#![cfg(unix)]

use std::cell::RefCell;
use std::io::{Read, Write};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use lumen_runtime::{Completion, ConsoleOut, Runtime};

/// Thread counts are process-wide: the tests of this file must not overlap.
static SERIAL: Mutex<()> = Mutex::new(());

#[derive(Clone, Default)]
struct Captured(Rc<RefCell<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Captured {
    fn lines(&self) -> Vec<String> {
        String::from_utf8(self.0.borrow().clone())
            .expect("utf8 console output")
            .lines()
            .map(str::to_string)
            .collect()
    }
}

/// Runs `body` on the stack real engine hosts use: the lazily evaluated node glue nests deeply
/// enough in a debug build to overflow a default 2 MiB test thread.
fn on_engine_stack(body: fn()) {
    std::thread::Builder::new()
        .stack_size(lumen::THREAD_STACK_SIZE)
        .spawn(body)
        .unwrap()
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
}

fn test_runtime() -> (Runtime, Captured) {
    let mut rt = Runtime::new();
    let out = Captured::default();
    rt.engine().ctx().op_state().put(ConsoleOut {
        out: Box::new(out.clone()),
        err: Box::new(Captured::default()),
    });
    (rt, out)
}

fn eval_ok(rt: &mut Runtime, src: &str) {
    match rt.eval(src).expect("parses") {
        Completion::Value(_) => {}
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
}

/// Threads in this process.
fn thread_count() -> usize {
    if let Ok(tasks) = std::fs::read_dir("/proc/self/task") {
        return tasks.count();
    }
    let listing = std::process::Command::new("ps")
        .args(["-M", "-p", &std::process::id().to_string()])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&listing.stdout).lines().count().saturating_sub(1)
}

#[test]
fn many_idle_sockets_create_no_threads() {
    on_engine_stack(many_idle_sockets_create_no_threads_body);
}

fn many_idle_sockets_create_no_threads_body() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    const PAIRS: usize = 60;
    let signal = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let signal_port = signal.local_addr().unwrap().port();
    let measured = std::sync::Arc::new(AtomicUsize::new(0));
    let observer = {
        let measured = measured.clone();
        std::thread::spawn(move || {
            let (mut s, _) = signal.accept().expect("accept");
            measured.store(thread_count(), Ordering::SeqCst);
            s.write_all(b"x").expect("release");
            let mut rest = Vec::new();
            let _ = s.read_to_end(&mut rest);
        })
    };
    let (mut rt, out) = test_runtime();
    // Pool, driver and observer threads exist by the time of the baseline.
    let baseline = thread_count();
    eval_ok(
        &mut rt,
        &format!(
            r#"
            const net = require("node:net");
            const PAIRS = {PAIRS};
            const socks = [];
            let established = 0;
            const up = () => {{
                if (++established < 2 * PAIRS) return;
                const s = net.connect({signal_port}, "127.0.0.1");
                s.on("data", () => {{
                    socks.forEach((x) => x.destroy());
                    s.destroy();
                    server.close(() => console.log("released", socks.length));
                }});
            }};
            const server = net.createServer((sock) => {{
                socks.push(sock);
                sock.on("data", () => {{}});
                sock.on("error", () => {{}});
                up();
            }});
            server.listen(0, "127.0.0.1", () => {{
                for (let i = 0; i < PAIRS; i++) {{
                    const c = net.connect(server.address().port, "127.0.0.1", () => {{
                        socks.push(c);
                        c.on("data", () => {{}});
                        up();
                    }});
                    c.on("error", () => {{}});
                }}
            }});
            "#
        ),
    );
    observer.join().expect("observer thread");
    assert_eq!(out.lines(), [format!("released {}", 2 * PAIRS)]);
    let during = measured.load(Ordering::SeqCst);
    assert!(
        during <= baseline + 8,
        "{} idle sockets held {} threads over the baseline of {baseline}",
        2 * PAIRS,
        during.saturating_sub(baseline)
    );
}

#[test]
fn read_after_would_block_rearms_for_the_next_chunk() {
    on_engine_stack(read_after_would_block_rearms_for_the_next_chunk_body);
}

fn read_after_would_block_rearms_for_the_next_chunk_body() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let peer = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().expect("accept");
        for chunk in [&b"one"[..], b"two", b"three"] {
            std::thread::sleep(std::time::Duration::from_millis(60));
            s.write_all(chunk).expect("write");
        }
    });
    let (mut rt, out) = test_runtime();
    eval_ok(
        &mut rt,
        &format!(
            r#"
            const net = require("node:net");
            const c = net.connect({port}, "127.0.0.1");
            c.on("data", (d) => console.log("chunk", d.toString()));
            c.on("end", () => console.log("end"));
            "#
        ),
    );
    peer.join().expect("peer thread");
    assert_eq!(out.lines(), ["chunk one", "chunk two", "chunk three", "end"]);
}
