//! A realm blocked in the event loop (a timer, a socket, a promise nothing settles, a worker) must
//! still stop when another thread interrupts it, and the runtime must be droppable afterwards.
//! Every scenario runs on its own thread under a hard deadline, so a regression fails the test
//! instead of hanging the suite.

use std::net::TcpListener;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lumen_runtime::{Embedding, InterruptHandle, Runtime, SharedWriter};

const HARD_DEADLINE: Duration = Duration::from_secs(20);
const STOP_WITHIN: Duration = Duration::from_secs(1);
const DROP_WITHIN: Duration = Duration::from_secs(3);

enum Event {
    Handle(InterruptHandle),
    Returned(Instant),
    Dropped(Instant),
}

struct Outcome {
    stop: Duration,
    drop: Duration,
}

fn recv(rx: &mpsc::Receiver<Event>, what: &str) -> Event {
    match rx.recv_timeout(HARD_DEADLINE) {
        Ok(event) => event,
        Err(RecvTimeoutError::Timeout) => panic!("{what}: still blocked after the interrupt"),
        Err(RecvTimeoutError::Disconnected) => panic!("{what}: the runtime thread panicked"),
    }
}

/// Run `script` on a runtime from `build`, interrupt it from this thread after `after`, and time
/// how long it takes to return and to drop.
fn drive(
    build: impl FnOnce() -> Runtime + Send + 'static,
    script: String,
    after: Duration,
) -> Outcome {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024)
        .spawn(move || {
            let mut runtime = build();
            tx.send(Event::Handle(runtime.interrupt_handle())).unwrap();
            let _ = runtime.eval(&script);
            tx.send(Event::Returned(Instant::now())).unwrap();
            // Terminated, but still safe to call.
            let _ = runtime.eval("1 + 1");
            drop(runtime);
            tx.send(Event::Dropped(Instant::now())).unwrap();
        })
        .unwrap();
    let Event::Handle(handle) = recv(&rx, "startup") else {
        unreachable!()
    };
    std::thread::sleep(after);
    assert!(
        matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)),
        "the scenario returned on its own before the interrupt"
    );
    let interrupted_at = Instant::now();
    handle.interrupt();
    let Event::Returned(returned_at) = recv(&rx, "interrupt") else {
        unreachable!()
    };
    let Event::Dropped(dropped_at) = recv(&rx, "drop") else {
        unreachable!()
    };
    Outcome {
        stop: returned_at.saturating_duration_since(interrupted_at),
        drop: dropped_at.saturating_duration_since(returned_at),
    }
}

fn check(name: &str, script: &str) {
    check_after(name, script, Duration::from_millis(300));
}

fn check_after(name: &str, script: &str, after: Duration) {
    let outcome = drive(Runtime::new, script.to_string(), after);
    assert!(
        outcome.stop < STOP_WITHIN,
        "{name}: took {:?} to stop",
        outcome.stop
    );
    assert!(
        outcome.drop < DROP_WITHIN,
        "{name}: took {:?} to drop",
        outcome.drop
    );
}

#[test]
fn a_pending_timer_is_interrupted() {
    check("timer", "setTimeout(() => {}, 1e9);");
}

#[test]
fn an_interval_is_interrupted() {
    check("interval", "setInterval(() => {}, 10);");
}

#[test]
fn a_busy_script_is_interrupted() {
    check("busy", "for (;;) {}");
}

#[test]
fn an_unresolved_promise_with_a_listening_server_is_interrupted() {
    check(
        "listening server",
        "const net = require('net');
         const server = net.createServer(() => {});
         (async () => {
           await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
           await new Promise(() => {});
         })();",
    );
}

#[test]
fn an_http_server_is_interrupted() {
    check(
        "http server",
        "const server = require('http').createServer(() => {});
         server.listen(0, '127.0.0.1');",
    );
}

#[test]
fn a_bun_server_is_interrupted() {
    check(
        "Bun.serve",
        "Bun.serve({ port: 0, fetch() { return new Response('x'); } });",
    );
}

#[test]
fn a_socket_connected_to_a_silent_listener_is_interrupted() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    check(
        "silent socket",
        &format!(
            "const socket = require('net').connect({port}, '127.0.0.1');
             socket.on('connect', () => socket.write('hello'));
             socket.on('data', () => {{}});"
        ),
    );
    drop(listener);
}

#[test]
fn a_fetch_to_a_silent_listener_is_interrupted() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    check(
        "silent fetch",
        &format!("fetch('http://127.0.0.1:{port}/').then(() => {{}}, () => {{}});"),
    );
    drop(listener);
}

#[test]
fn a_server_accepting_and_a_client_waiting_are_interrupted() {
    check(
        "client and server",
        "const net = require('net');
         const server = net.createServer((socket) => socket.on('data', () => {}));
         server.listen(0, '127.0.0.1', () => {
           const client = net.connect(server.address().port, '127.0.0.1');
           client.on('connect', () => client.write('ping'));
           client.on('data', () => {});
         });",
    );
}

#[test]
fn a_synchronous_atomics_wait_is_interrupted() {
    check(
        "Atomics.wait",
        "Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0);",
    );
}

#[test]
fn a_synchronous_child_process_is_interrupted() {
    if cfg!(windows) {
        return;
    }
    check(
        "spawnSync",
        "require('child_process').spawnSync('sleep', ['60']);",
    );
}

struct Heartbeat(std::path::PathBuf);

impl Heartbeat {
    fn new(name: &str) -> Heartbeat {
        let path = std::env::temp_dir().join(format!(
            "lumen-interrupt-{}-{name}.beat",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        Heartbeat(path)
    }

    fn size(&self) -> u64 {
        std::fs::metadata(&self.0).map(|m| m.len()).unwrap_or(0)
    }

    fn path_literal(&self) -> String {
        format!("{:?}", self.0.to_string_lossy())
    }
}

impl Drop for Heartbeat {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A worker that appends to a file every few milliseconds while busy, so a test can tell whether
/// it is still alive after its parent was interrupted.
fn heartbeat_worker(beat: &Heartbeat, body: &str) -> String {
    format!(
        "const {{ Worker }} = require('worker_threads');
         const code = `
           const fs = require('fs');
           let last = 0;
           const beat = () => {{
             const now = Date.now();
             if (now - last > 10) {{ last = now; fs.appendFileSync(${{JSON.stringify({path})}}, 'x'); }}
           }};
           {body}
         `;
         new Worker(code, {{ eval: true }});",
        path = beat.path_literal(),
    )
}

fn assert_worker_stopped(beat: &Heartbeat) {
    assert!(beat.size() > 0, "the worker never ran");
    std::thread::sleep(Duration::from_millis(300));
    let settled = beat.size();
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(beat.size(), settled, "the worker is still running");
}

#[test]
fn a_busy_worker_is_interrupted() {
    let beat = Heartbeat::new("busy");
    check_after(
        "busy worker",
        &heartbeat_worker(&beat, "for (;;) beat();"),
        Duration::from_millis(600),
    );
    assert_worker_stopped(&beat);
}

#[test]
fn a_worker_waiting_on_a_timer_is_interrupted() {
    let beat = Heartbeat::new("timer");
    check_after(
        "idle worker",
        &heartbeat_worker(&beat, "beat(); setInterval(beat, 20);"),
        Duration::from_millis(600),
    );
    assert_worker_stopped(&beat);
}

#[test]
fn a_worker_blocked_in_atomics_wait_is_interrupted() {
    check_after(
        "worker Atomics.wait",
        "const { Worker } = require('worker_threads');
         new Worker(
           'Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0);',
           { eval: true },
         );",
        Duration::from_millis(600),
    );
}

#[test]
fn nested_workers_are_interrupted() {
    let beat = Heartbeat::new("nested");
    let inner = format!(
        "const fs = require('fs'); for (;;) {{ fs.appendFileSync({}, 'x'); }}",
        beat.path_literal()
    );
    let script = format!(
        "const {{ Worker }} = require('worker_threads');
         new Worker(
           `const {{ Worker }} = require('worker_threads');
            new Worker(${{JSON.stringify({inner:?})}}, {{ eval: true }});`,
           {{ eval: true }},
         );"
    );
    check_after("nested workers", &script, Duration::from_millis(800));
    assert_worker_stopped(&beat);
}

struct BlockedStdin(mpsc::Receiver<()>);

impl std::io::Read for BlockedStdin {
    fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        let _ = self.0.recv();
        Ok(0)
    }
}

#[test]
fn a_blocked_stdin_read_is_interrupted() {
    let (keep_open, closed_by) = mpsc::channel();
    let build = move || {
        Runtime::new_embedded(Embedding {
            argv: vec!["host".into()],
            env: Vec::new(),
            cwd: std::env::current_dir().unwrap(),
            stdin: Box::new(BlockedStdin(closed_by)),
            stdout: SharedWriter::new(std::io::sink()),
            stderr: SharedWriter::new(std::io::sink()),
            interrupt: Arc::new(AtomicBool::new(false)),
            live_object_limit: None,
            spawner: None,
        })
    };
    let outcome = drive(
        build,
        "process.stdin.on('data', () => {});".to_string(),
        Duration::from_millis(300),
    );
    drop(keep_open);
    assert!(outcome.stop < STOP_WITHIN, "{:?}", outcome.stop);
    assert!(outcome.drop < DROP_WITHIN, "{:?}", outcome.drop);
}

#[test]
fn a_deadline_interrupts_a_blocked_loop() {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024)
        .spawn(move || {
            let mut runtime = Runtime::new();
            runtime.set_deadline(Duration::from_millis(200));
            let started = Instant::now();
            let _ = runtime.eval("setInterval(() => {}, 10);");
            tx.send((started.elapsed(), runtime.is_interrupted())).unwrap();
        })
        .unwrap();
    let (elapsed, interrupted) = rx
        .recv_timeout(HARD_DEADLINE)
        .expect("the deadline never stopped the loop");
    assert!(interrupted);
    assert!(elapsed >= Duration::from_millis(150), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
}

#[test]
fn a_deadline_that_is_not_reached_does_not_delay_the_drop() {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024)
        .spawn(move || {
            let mut runtime = Runtime::new();
            runtime.set_deadline(Duration::from_secs(3600));
            let _ = runtime.eval("1 + 1");
            let started = Instant::now();
            drop(runtime);
            tx.send(started.elapsed()).unwrap();
        })
        .unwrap();
    let elapsed = rx.recv_timeout(HARD_DEADLINE).expect("drop hung");
    assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
}

#[test]
fn an_interrupt_before_the_script_runs_stops_it_at_once() {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024)
        .spawn(move || {
            let mut runtime = Runtime::new();
            runtime.interrupt_handle().interrupt();
            let started = Instant::now();
            let _ = runtime.eval("setInterval(() => {}, 10);");
            tx.send(started.elapsed()).unwrap();
        })
        .unwrap();
    let elapsed = rx.recv_timeout(HARD_DEADLINE).expect("the loop never stopped");
    assert!(elapsed < STOP_WITHIN, "{elapsed:?}");
}
