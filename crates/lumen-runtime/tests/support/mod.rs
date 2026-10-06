//! Guards for integration tests that start servers or child processes: nothing here may block
//! forever, so a test that goes wrong fails instead of hanging the suite.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read};
use std::process::{Child, ChildStdout, Command, ExitStatus, Output};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use lumen_runtime::Runtime;

/// How long a test's runtime may run before it is interrupted.
pub const RUNTIME_DEADLINE: Duration = Duration::from_secs(30);

/// How long a test waits for a child process it expects to finish.
pub const CHILD_DEADLINE: Duration = Duration::from_secs(30);

/// Interrupt `runtime` after [`RUNTIME_DEADLINE`], whatever it is blocked on.
pub fn arm(runtime: &mut Runtime) {
    runtime.set_deadline(RUNTIME_DEADLINE);
}

/// Fail the test when the deadline stopped the runtime instead of the script finishing.
pub fn assert_in_time(runtime: &Runtime) {
    assert!(
        !runtime.is_interrupted(),
        "the script did not finish within {RUNTIME_DEADLINE:?}"
    );
}

/// A spawned child that is killed and reaped when it goes out of scope, so a failing test never
/// leaves it running or waits on it.
pub struct ChildGuard(Child);

impl ChildGuard {
    pub fn new(child: Child) -> ChildGuard {
        ChildGuard(child)
    }

    pub fn take_stdout(&mut self) -> ChildStdout {
        self.0.stdout.take().expect("piped stdout")
    }

    /// Wait for the child to exit, failing the test if it has not within `limit`.
    pub fn wait(&mut self, limit: Duration) -> ExitStatus {
        lumen_os::child::wait_timeout(&mut self.0, limit, Duration::from_millis(10))
            .expect("poll child")
            .unwrap_or_else(|| panic!("the child process did not exit within {limit:?}"))
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Read one line from `reader`, failing the test if none arrives within `limit`.
pub fn read_line(reader: impl Read + Send + 'static, limit: Duration) -> String {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = BufReader::new(reader).read_line(&mut line);
        let _ = tx.send(line);
    });
    rx.recv_timeout(limit)
        .unwrap_or_else(|_| panic!("no output line within {limit:?}"))
}

/// `child.wait_with_output()`, with the child killed and the test failed after `limit`.
pub fn output_within(child: Child, limit: Duration) -> Output {
    let pid = child.id();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(limit) {
        Ok(output) => output.expect("wait for child"),
        Err(_) => {
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
            panic!("the child process did not exit within {limit:?}");
        }
    }
}

/// Runs `body` on the stack real engine hosts use: the lazily evaluated node glue nests deeply
/// enough in a debug build to overflow a default 2 MiB test thread, which surfaces as a
/// RangeError thrown inside a loop callback.
pub fn on_engine_stack(body: fn()) {
    std::thread::Builder::new()
        .stack_size(lumen::THREAD_STACK_SIZE)
        .spawn(body)
        .unwrap()
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
}
