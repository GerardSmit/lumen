use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Condvar, Mutex};

const LONG: Duration = Duration::from_secs(30);

#[test]
fn unparks_coalesce_into_one_token() {
    let sched = OsScheduler::new();
    let park = sched.parker().unwrap();
    park.unpark();
    park.unpark();
    park.unpark();
    assert_eq!(park.park(None), Woke::Unparked);
    let soon = Instant::now() + Duration::from_millis(20);
    assert_eq!(park.park(Some(soon)), Woke::TimedOut);
}

#[test]
fn park_without_deadline_returns_at_once_after_unpark() {
    let park = OsScheduler::new().parker().unwrap();
    park.unpark();
    let start = Instant::now();
    assert_eq!(park.park(None), Woke::Unparked);
    assert!(start.elapsed() < LONG);
}

#[test]
fn timed_park_times_out() {
    let park = OsScheduler::new().parker().unwrap();
    let start = Instant::now();
    let wait = Duration::from_millis(30);
    assert_eq!(park.park(Some(start + wait)), Woke::TimedOut);
    assert!(start.elapsed() >= wait);
}

#[test]
fn past_deadline_still_consumes_a_pending_token() {
    let park = OsScheduler::new().parker().unwrap();
    park.unpark();
    assert_eq!(park.park(Some(Instant::now())), Woke::Unparked);
    assert_eq!(park.park(Some(Instant::now())), Woke::TimedOut);
}

#[test]
fn unpark_from_another_thread_wakes_a_blocked_park() {
    let park = OsScheduler::new().parker().unwrap();
    let remote = park.clone().unparker();
    let thread = std::thread::spawn(move || remote.unpark());
    assert_eq!(park.park(Some(Instant::now() + LONG)), Woke::Unparked);
    thread.join().unwrap();
}

#[test]
fn spawn_thread_runs_and_joins() {
    let sched = OsScheduler::new();
    let mut spec = ThreadSpec::new("sched-test", Purpose::Service);
    spec.stack_bytes = 1 << 20;
    let (tx, rx) = mpsc::channel();
    let handle = sched
        .spawn_thread(
            spec,
            Box::new(move |start| {
                tx.send((start.stack_bytes, std::thread::current().name().map(str::to_owned)))
                    .unwrap();
            }),
        )
        .unwrap();
    handle.join().unwrap();
    let (stack, name) = rx.recv().unwrap();
    assert_eq!(stack, 1 << 20);
    assert_eq!(name.as_deref(), Some("sched-test"));
}

#[test]
fn joining_a_panicked_thread_reports_it() {
    let sched = OsScheduler::new();
    let spec = ThreadSpec::new("sched-panic", Purpose::Service);
    let handle = sched.spawn_thread(spec, Box::new(|_| panic!("expected test panic"))).unwrap();
    assert!(handle.join().is_err());
}

#[test]
fn spawn_blocking_runs_every_job() {
    let sched = OsScheduler::new();
    let done = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = mpsc::channel();
    for _ in 0..32 {
        let (done, tx) = (done.clone(), tx.clone());
        let job: Job = Box::new(move || {
            done.fetch_add(1, Ordering::SeqCst);
            tx.send(()).unwrap();
        });
        assert!(sched.spawn_blocking(job).is_ok());
    }
    for _ in 0..32 {
        rx.recv_timeout(LONG).unwrap();
    }
    assert_eq!(done.load(Ordering::SeqCst), 32);
    assert!(sched.blocking_workers() <= sched.available_parallelism().get().max(4));
}

#[test]
fn spawn_blocking_reuses_an_idle_worker() {
    let sched = OsScheduler::new();
    assert_eq!(sched.blocking_workers(), 0);
    let (tx, rx) = mpsc::channel();
    let first_tx = tx.clone();
    assert!(sched.spawn_blocking(Box::new(move || first_tx.send(()).unwrap())).is_ok());
    rx.recv_timeout(LONG).unwrap();
    while sched.blocking_idle() < 1 {
        std::thread::yield_now();
    }
    assert_eq!(sched.blocking_workers(), 1);
    assert!(sched.spawn_blocking(Box::new(move || tx.send(()).unwrap())).is_ok());
    rx.recv_timeout(LONG).unwrap();
    assert_eq!(sched.blocking_workers(), 1);
}

#[test]
fn spawn_blocking_grows_when_all_workers_are_busy() {
    let sched = OsScheduler::new();
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let (started_tx, started_rx) = mpsc::channel();
    for _ in 0..2 {
        let (gate, started) = (gate.clone(), started_tx.clone());
        let job: Job = Box::new(move || {
            started.send(()).unwrap();
            let (open, cv) = &*gate;
            let mut open = open.lock().unwrap();
            while !*open {
                open = cv.wait(open).unwrap();
            }
        });
        assert!(sched.spawn_blocking(job).is_ok());
    }
    started_rx.recv_timeout(LONG).unwrap();
    started_rx.recv_timeout(LONG).unwrap();
    assert_eq!(sched.blocking_workers(), 2);
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
}

#[test]
fn unavailable_and_no_threads_fail_without_blocking() {
    for sched in [&Unavailable as &dyn Scheduler, &NoThreads] {
        assert!(sched.parker().is_err());
        assert!(sched.after(Duration::ZERO, Box::new(|| {})).is_err());
        assert!(sched.spawn_blocking(Box::new(|| {})).is_err());
        assert!(sched
            .spawn_thread(ThreadSpec::new("x", Purpose::Service), Box::new(|_| {}))
            .is_err());
    }
}

#[test]
fn sleep_waits_at_least_the_duration() {
    let start = Instant::now();
    sleep(Duration::from_millis(25));
    assert!(start.elapsed() >= Duration::from_millis(25));
}

#[test]
fn dropping_a_timer_cancels_and_detach_does_not() {
    struct Flag(AtomicUsize);
    impl TimerCancel for Flag {
        fn cancel(&self) -> bool {
            self.0.fetch_add(1, Ordering::SeqCst) == 0
        }
    }
    let flag = Arc::new(Flag(AtomicUsize::new(0)));
    drop(Timer::new(flag.clone()));
    assert_eq!(flag.0.load(Ordering::SeqCst), 1);
    Timer::new(flag.clone()).detach();
    assert_eq!(flag.0.load(Ordering::SeqCst), 1);
}

#[test]
fn current_defaults_to_the_host_cpu_count() {
    assert_eq!(current().available_parallelism(), host_parallelism());
}
