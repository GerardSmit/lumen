//! Waiting on a spawned child process within a budget.

use std::io;
use std::process::{Child, ExitStatus};
use std::time::{Duration, Instant};

/// Polls `child` every `poll` until it exits, or until `give_up` (checked between polls: a
/// deadline, a progress or memory check) returns true; the child is then killed and reaped and
/// the result is `Ok(None)`.
pub fn wait_or_kill(
    child: &mut Child,
    poll: Duration,
    mut give_up: impl FnMut() -> bool,
) -> io::Result<Option<ExitStatus>> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if give_up() {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(poll);
    }
}

/// [`wait_or_kill`] with a wall-clock `limit` from now.
pub fn wait_timeout(child: &mut Child, limit: Duration, poll: Duration) -> io::Result<Option<ExitStatus>> {
    let deadline = Instant::now() + limit;
    wait_or_kill(child, poll, || Instant::now() >= deadline)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn exits_or_is_killed() {
        let mut quick = Command::new("true").spawn().unwrap();
        let status = wait_timeout(&mut quick, Duration::from_secs(10), Duration::from_millis(5)).unwrap();
        assert!(status.is_some_and(|s| s.success()));
        let mut slow = Command::new("sleep").arg("10").spawn().unwrap();
        let started = Instant::now();
        let status = wait_timeout(&mut slow, Duration::from_millis(50), Duration::from_millis(5)).unwrap();
        assert!(status.is_none());
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
