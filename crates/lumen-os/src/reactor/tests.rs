use super::*;
use crate::sched::{OsScheduler, Scheduler};
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::mpsc;
use std::time::Instant;

const LONG: Duration = Duration::from_secs(30);
const SHORT: Duration = Duration::from_millis(60);

fn pipe() -> (i32, i32) {
    let mut fds = [0i32; 2];
    // SAFETY: a two-int out array.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    for fd in fds {
        // SAFETY: fcntl on descriptors just created.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
    (fds[0], fds[1])
}

fn socketpair() -> (i32, i32) {
    let mut fds = [0i32; 2];
    // SAFETY: a two-int out array.
    assert_eq!(unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) }, 0);
    (fds[0], fds[1])
}

fn write_byte(fd: i32) {
    // SAFETY: writes one byte from a live buffer.
    assert_eq!(unsafe { libc::write(fd, b"x".as_ptr().cast(), 1) }, 1);
}

fn read_all(fd: i32) {
    let mut buf = [0u8; 64];
    // SAFETY: reads into a live buffer.
    unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
}

fn close(fd: i32) {
    // SAFETY: closing a descriptor opened by the test.
    unsafe { libc::close(fd) };
}

fn counter() -> (Arc<AtomicUsize>, Arc<dyn Wake>) {
    let count = Arc::new(AtomicUsize::new(0));
    let c = count.clone();
    (count, Arc::new(move || {
        c.fetch_add(1, SeqCst);
    }))
}

fn native() -> Poller {
    Poller::new().unwrap()
}

#[cfg(unix)]
fn polling() -> Poller {
    Poller::with_backend(Box::new(pollfd::PollBackend::new().unwrap()))
}

macro_rules! backend_tests {
    ($name:ident, $make:path) => {
        mod $name {
            use super::*;

            #[test]
            fn pipe_read_readiness() {
                let poller = $make();
                let (r, w) = pipe();
                let (count, wake) = counter();
                let reg = poller.register(Source::Fd(r), Interest::READ, wake).unwrap();
                assert_eq!(poller.turn(Some(Duration::ZERO)).unwrap(), 0);
                assert_eq!(count.load(SeqCst), 0);
                write_byte(w);
                assert_eq!(poller.turn(Some(LONG)).unwrap(), 1);
                assert_eq!(count.load(SeqCst), 1);
                assert!(reg.take_ready().contains(Ready::READ));
                assert!(reg.take_ready().is_empty());
                drop(reg);
                close(r);
                close(w);
            }

            #[test]
            fn one_shot_until_rearmed() {
                let poller = $make();
                let (r, w) = pipe();
                let (count, wake) = counter();
                let reg = poller.register(Source::Fd(r), Interest::READ, wake).unwrap();
                write_byte(w);
                assert_eq!(poller.turn(Some(LONG)).unwrap(), 1);
                write_byte(w);
                assert_eq!(poller.turn(Some(SHORT)).unwrap(), 0);
                assert_eq!(count.load(SeqCst), 1);
                reg.rearm(Interest::READ).unwrap();
                assert_eq!(poller.turn(Some(LONG)).unwrap(), 1);
                assert_eq!(count.load(SeqCst), 2);
                read_all(r);
                reg.rearm(Interest::READ).unwrap();
                assert_eq!(poller.turn(Some(SHORT)).unwrap(), 0);
                assert_eq!(count.load(SeqCst), 2);
                drop(reg);
                close(r);
                close(w);
            }

            #[test]
            fn write_interest_fires_on_a_writable_pipe() {
                let poller = $make();
                let (r, w) = pipe();
                let (count, wake) = counter();
                let reg = poller.register(Source::Fd(w), Interest::WRITE, wake).unwrap();
                assert_eq!(poller.turn(Some(LONG)).unwrap(), 1);
                assert!(reg.take_ready().contains(Ready::WRITE));
                assert_eq!(count.load(SeqCst), 1);
                drop(reg);
                close(r);
                close(w);
            }

            #[test]
            fn both_interests_disarm_together() {
                let poller = $make();
                let (a, b) = socketpair();
                let (count, wake) = counter();
                let reg = poller.register(Source::Fd(a), Interest::BOTH, wake).unwrap();
                assert_eq!(poller.turn(Some(LONG)).unwrap(), 1);
                assert!(reg.take_ready().contains(Ready::WRITE));
                write_byte(b);
                assert_eq!(poller.turn(Some(SHORT)).unwrap(), 0);
                assert_eq!(count.load(SeqCst), 1);
                reg.rearm(Interest::READ).unwrap();
                assert_eq!(poller.turn(Some(LONG)).unwrap(), 1);
                assert!(reg.take_ready().contains(Ready::READ));
                drop(reg);
                close(a);
                close(b);
            }

            #[test]
            fn hangup_wakes_the_reader() {
                let poller = $make();
                let (r, w) = pipe();
                let (count, wake) = counter();
                let reg = poller.register(Source::Fd(r), Interest::READ, wake).unwrap();
                close(w);
                assert_eq!(poller.turn(Some(LONG)).unwrap(), 1);
                assert!(reg.take_ready().contains(Ready::HUP));
                assert_eq!(count.load(SeqCst), 1);
                drop(reg);
                close(r);
            }

            #[test]
            fn dropping_the_registration_deregisters() {
                let poller = $make();
                let (r, w) = pipe();
                let (count, wake) = counter();
                let reg = poller.register(Source::Fd(r), Interest::READ, wake).unwrap();
                drop(reg);
                write_byte(w);
                assert_eq!(poller.turn(Some(SHORT)).unwrap(), 0);
                assert_eq!(count.load(SeqCst), 0);
                close(r);
                close(w);
            }

            #[test]
            fn a_stale_generation_is_ignored() {
                let poller = $make();
                let (r, w) = pipe();
                let (stale_count, stale_wake) = counter();
                let first = poller.register(Source::Fd(r), Interest::READ, stale_wake).unwrap();
                drop(first);
                let (count, wake) = counter();
                let second = poller.register(Source::Fd(r), Interest::READ, wake).unwrap();
                assert!(!poller.dispatch_for_test(token_of(0, 1), Ready::READ));
                assert_eq!(count.load(SeqCst), 0);
                assert!(poller.dispatch_for_test(token_of(0, 2), Ready::READ));
                assert_eq!(count.load(SeqCst), 1);
                assert_eq!(stale_count.load(SeqCst), 0);
                drop(second);
                close(r);
                close(w);
            }

            #[test]
            fn a_wake_after_drop_does_not_run_the_wake() {
                let poller = $make();
                let (r, w) = pipe();
                let (count, wake) = counter();
                let reg = poller.register(Source::Fd(r), Interest::READ, wake).unwrap();
                write_byte(w);
                drop(reg);
                assert_eq!(poller.turn(Some(SHORT)).unwrap(), 0);
                assert_eq!(count.load(SeqCst), 0);
                close(r);
                close(w);
            }

            #[test]
            fn many_wakes_cost_one_trigger() {
                let poller = $make();
                let waker = poller.waker();
                for _ in 0..100 {
                    waker.wake();
                }
                assert_eq!(poller.trigger_count(), 1);
                let start = Instant::now();
                assert_eq!(poller.turn(Some(LONG)).unwrap(), 0);
                assert!(start.elapsed() < LONG);
                waker.wake();
                waker.wake();
                assert_eq!(poller.trigger_count(), 2);
                assert_eq!(poller.turn(Some(LONG)).unwrap(), 0);
            }

            #[test]
            fn a_wake_from_another_thread_ends_a_blocking_turn() {
                let poller = $make();
                let waker = poller.waker();
                let thread = std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(30));
                    waker.wake();
                });
                let start = Instant::now();
                poller.turn(None).unwrap();
                assert!(start.elapsed() < LONG);
                thread.join().unwrap();
            }

            #[test]
            fn a_turn_times_out() {
                let poller = $make();
                let start = Instant::now();
                assert_eq!(poller.turn(Some(Duration::from_millis(40))).unwrap(), 0);
                assert!(start.elapsed() >= Duration::from_millis(30));
            }

            #[test]
            fn registering_from_another_thread_while_blocked_is_seen() {
                let poller = Arc::new($make());
                let (r, w) = pipe();
                let (tx, rx) = mpsc::channel();
                let looper = poller.clone();
                let thread = std::thread::spawn(move || {
                    while rx.try_recv().is_err() {
                        looper.turn(Some(Duration::from_millis(500))).unwrap();
                    }
                });
                std::thread::sleep(Duration::from_millis(30));
                let (count, wake) = counter();
                let reg = poller.register(Source::Fd(r), Interest::READ, wake).unwrap();
                write_byte(w);
                let start = Instant::now();
                while count.load(SeqCst) == 0 && start.elapsed() < LONG {
                    std::thread::sleep(Duration::from_millis(5));
                }
                assert_eq!(count.load(SeqCst), 1);
                tx.send(()).unwrap();
                poller.waker().wake();
                thread.join().unwrap();
                drop(reg);
                close(r);
                close(w);
            }
        }
    };
}

backend_tests!(native_backend, native);
backend_tests!(poll_backend, polling);

#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn epoll_into_returns_the_data_it_was_given() {
    let ep = crate::event::epoll_create().unwrap();
    let efd = crate::event::eventfd().unwrap();
    crate::event::epoll_ctl_data(ep, libc::EPOLL_CTL_ADD, efd, libc::EPOLLIN as u32, 0xfeed_beef_u64).unwrap();
    let mut buf = [crate::event::EpollEvent { events: 0, u64: 0 }; 4];
    assert_eq!(crate::event::epoll_wait_into(ep, &mut buf, 0).unwrap(), 0);
    let one = 1u64;
    // SAFETY: writes eight bytes from a live u64.
    unsafe { libc::write(efd, (&one as *const u64).cast(), 8) };
    assert_eq!(crate::event::epoll_wait_into(ep, &mut buf, 1000).unwrap(), 1);
    let data = buf[0].u64;
    assert_eq!(data, 0xfeed_beef);
    close(ep);
    close(efd);
}

#[cfg(all(
    any(target_os = "macos", target_os = "ios", target_os = "freebsd"),
    target_pointer_width = "64"
))]
#[test]
fn kqueue_user_event_triggers() {
    use crate::event::{self, Kevent};
    let kq = event::kqueue().unwrap();
    let add = Kevent { ident: 7, filter: event::EVFILT_USER, flags: (libc::EV_ADD | libc::EV_CLEAR) as u16, ..Kevent::default() };
    let mut out = [Kevent::default().to_raw(); 4];
    assert_eq!(event::kevent_into(kq, &[add.to_raw()], &mut out, Some((0, 0))).unwrap(), 0);
    let fire = Kevent { ident: 7, filter: event::EVFILT_USER, fflags: event::NOTE_TRIGGER, udata: 42, ..Kevent::default() };
    assert_eq!(event::kevent_into(kq, &[fire.to_raw()], &mut out, Some((1, 0))).unwrap(), 1);
    assert_eq!(Kevent::from_raw(&out[0]).ident, 7);
    assert_eq!(event::kevent_into(kq, &[], &mut out, Some((0, 0))).unwrap(), 0);
    close(kq);
}

#[test]
fn timers_and_registrations_share_the_driver_thread() {
    let sched = OsScheduler::new();
    assert!(!sched.driver_started());
    let reactor = sched.reactor().expect("a reactor on unix");
    assert!(!sched.driver_started());

    let (r, w) = pipe();
    let (tx, rx) = mpsc::channel();
    let io_tx = tx.clone();
    let reg = reactor
        .register(
            Source::Fd(r),
            Interest::READ,
            Arc::new(move || {
                io_tx.send(("io", std::thread::current().name().map(str::to_owned))).unwrap();
            }),
        )
        .unwrap();
    assert!(sched.driver_started());
    sched
        .after(Duration::from_millis(20), Box::new(move || {
            tx.send(("timer", std::thread::current().name().map(str::to_owned))).unwrap();
        }))
        .unwrap()
        .detach();
    write_byte(w);

    let mut seen = vec![rx.recv_timeout(LONG).unwrap(), rx.recv_timeout(LONG).unwrap()];
    seen.sort();
    assert_eq!(seen[0], ("io", Some("lumen-driver".to_owned())));
    assert_eq!(seen[1], ("timer", Some("lumen-driver".to_owned())));
    drop(reg);
    close(r);
    close(w);
}

#[test]
fn a_registration_made_while_the_driver_waits_on_a_far_timer_is_served() {
    let sched = OsScheduler::new();
    let _far = sched.after(LONG, Box::new(|| {})).unwrap();
    std::thread::sleep(Duration::from_millis(30));
    let (r, w) = pipe();
    let (tx, rx) = mpsc::channel();
    let reg = sched
        .reactor()
        .unwrap()
        .register(Source::Fd(r), Interest::READ, Arc::new(move || tx.send(()).unwrap()))
        .unwrap();
    write_byte(w);
    rx.recv_timeout(Duration::from_secs(10)).unwrap();
    drop(reg);
    close(r);
    close(w);
}

mod hosted_hooks {
    use super::*;
    use std::sync::Mutex;

    static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());
    static WAKES: Mutex<Vec<Arc<dyn Wake>>> = Mutex::new(Vec::new());

    fn register(token: u64, interest: u8, wake: Arc<dyn Wake>) -> Result<u64, i32> {
        LOG.lock().unwrap().push(format!("register {token} {interest}"));
        WAKES.lock().unwrap().push(wake);
        Ok(token + 100)
    }
    fn rearm(id: u64, interest: u8) -> Result<(), i32> {
        LOG.lock().unwrap().push(format!("rearm {id} {interest}"));
        Ok(())
    }
    fn deregister(id: u64) {
        LOG.lock().unwrap().push(format!("deregister {id}"));
    }

    #[test]
    fn host_sources_go_through_the_hooks() {
        let reactor = HostedReactor::new(HostHooks { register, rearm, deregister });
        let (count, wake) = counter();
        assert!(reactor.register(Source::Fd(0), Interest::READ, wake.clone()).is_err());
        let reg = reactor.register(Source::Host(5), Interest::READ, wake).unwrap();
        WAKES.lock().unwrap()[0].wake();
        assert_eq!(count.load(SeqCst), 1);
        assert!(reg.take_ready().contains(Ready::READ));
        reg.rearm(Interest::WRITE).unwrap();
        WAKES.lock().unwrap()[0].wake();
        assert!(reg.take_ready().contains(Ready::WRITE));
        drop(reg);
        WAKES.lock().unwrap()[0].wake();
        assert_eq!(count.load(SeqCst), 2);
        assert_eq!(
            *LOG.lock().unwrap(),
            ["register 5 1", "rearm 105 2", "deregister 105"]
        );
    }
}
