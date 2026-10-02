//! Waiting on a spawned child process within a budget.

use std::io;
use std::process::{Child, Command, ExitStatus};
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

/// Makes the command's child the leader of a new process group, so that it and everything it
/// spawns can be killed together with [`kill_group`]. A no-op off Unix.
pub fn new_group(cmd: &mut Command) -> &mut Command {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd
}

/// Sends SIGKILL to every process in the group led by `child` (spawned with [`new_group`]).
/// Members outlive the leader, so this also works after the leader has been reaped. Off Unix it
/// kills the child only.
pub fn kill_group(child: &mut Child) {
    #[cfg(unix)]
    if let Ok(pid) = libc::pid_t::try_from(child.id()) {
        // SAFETY: killpg only signals; a stale group id yields ESRCH, which is ignored.
        unsafe { libc::killpg(pid, libc::SIGKILL) };
    }
    #[cfg(not(unix))]
    let _ = child.kill();
}

/// [`wait_or_kill`] for a child spawned with [`new_group`]: whether the leader exits or is given
/// up on, the whole group is killed afterwards, so no grandchild outlives the run.
pub fn wait_or_kill_group(
    child: &mut Child,
    poll: Duration,
    give_up: impl FnMut() -> bool,
) -> io::Result<Option<ExitStatus>> {
    let waited = wait_or_kill(child, poll, give_up);
    kill_group(child);
    waited
}

/// [`wait_or_kill_group`] with a wall-clock `limit` from now.
pub fn wait_timeout_group(child: &mut Child, limit: Duration, poll: Duration) -> io::Result<Option<ExitStatus>> {
    let deadline = Instant::now() + limit;
    wait_or_kill_group(child, poll, || Instant::now() >= deadline)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

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

    fn alive(pid: &str) -> bool {
        let pid: libc::pid_t = pid.trim().parse().unwrap();
        // SAFETY: signal 0 only checks for existence.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[test]
    fn group_kill_reaches_grandchildren() {
        let dir = std::env::temp_dir().join(format!("lumen-os-child-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("grandchild.pid");
        let script = format!("sleep 30 & echo $! > {}; exit 0", pidfile.display());
        let mut child = new_group(Command::new("sh").args(["-c", &script])).spawn().unwrap();
        let status = wait_timeout_group(&mut child, Duration::from_secs(10), Duration::from_millis(5)).unwrap();
        assert!(status.is_some_and(|s| s.success()));
        let pid = std::fs::read_to_string(&pidfile).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while alive(&pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!alive(&pid), "grandchild survived the group kill");
        std::fs::remove_file(&pidfile).unwrap();
        std::fs::remove_dir(&dir).unwrap();
    }
}
