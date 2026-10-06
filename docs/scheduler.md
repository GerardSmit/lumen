# Scheduler and reactor

`lumen_os::sched` is the process scheduler: dedicated threads, a shared pool for bounded blocking
work, parking and one-shot timers behind one trait. `lumen_os::reactor` is the I/O readiness
reactor. Both are engine-neutral and OS-facing, so they live in `lumen-os`.

## `lumen_os::sched`

`Scheduler` has `spawn_thread`, `spawn_blocking`, `parker`, `after` and `reactor`.
`sched::current()` returns the scheduler a host installed with `sched::install`, else the platform
default:

| Platform | Default | Behavior |
| --- | --- | --- |
| Operating systems | `OsScheduler` | std threads, shared blocking pool, driver thread |
| wasm32 | `NoThreads` | nothing spawns, parks or times; callers run work inline |
| `target_os = "none"` | `Unavailable` | every call fails at once until a host installs a scheduler |

`OsScheduler` creates everything on first use; an idle process holds no thread for it.

- **Blocking pool**: workers are created only when a job arrives and every worker is busy, up to
  `max(4, parallelism)`. Idle workers wait on a condition variable with no timeout. Work that blocks
  for an unbounded time uses `spawn_thread(Purpose::Blocking)` instead.
- **Park**: `OsPark` is a token-semantics park slot; any number of unparks before a park end it once.
  `park(None)` arms no timer.
- **Timers and the driver thread**: `after(delay, job)` queues a job on one `lumen-driver` thread,
  started by the first `after` or the first reactor registration. It owns a `DeadlineQueue` and a
  reactor `Poller`, and each pass runs `Poller::turn(next_deadline)` and then fires due jobs; with
  nothing queued it blocks with no timeout. Jobs and readiness wakes therefore run on the driver
  thread and must be short and must not block. `sched::Deadline` is the drop-to-cancel wrapper
  over `after`. Where no `Poller` can be created (wasm32) the driver waits on a condition variable
  instead and timers behave the same.

## `lumen_os::reactor`

Hand-rolled over raw syscalls: no crate, no mio.

| Backend | Platforms | Wait | Wake | Registrations |
| --- | --- | --- | --- | --- |
| `epoll` | Linux, Android | `epoll_wait` into a reused buffer, timeout rounded up to ms | `eventfd` | `EPOLLONESHOT`, token in `data.u64` |
| `kqueue` | macOS, iOS, FreeBSD (64-bit) | `kevent` into a reused buffer | `EVFILT_USER` + `NOTE_TRIGGER` | `EV_ONESHOT` read and write filters, token in `udata` |
| `pollfd` | other Unix | `poll(2)` over the armed set, rebuilt only when it changes | self-pipe | level-triggered, one-shot emulated |
| `hosted` | embedders | none: the host delivers readiness | none | `Source::Host(u64)` through `HostHooks` |
| `iocp` | Windows | `GetQueuedCompletionStatusEx` on an I/O completion port | `PostQueuedCompletionStatus` with a reserved key | `Source::Socket(usize)`: `ProcessSocketNotifications` one-shot level-triggered registrations (Windows 10 build 20348+, resolved from `ws2_32` with `GetProcAddress` and probed once). Otherwise one `lumen-wsapoll` helper thread blocks in `WSAPoll` over the armed sockets and a loopback UDP wake socket and posts packets to the port. Only documented APIs. |

wasm32 has no backend: `Poller::new()` returns `SchedError::Unsupported`.

Windows notes: the completion key of each registration is a fresh counter value, never reused, so late
packets of a dropped registration are ignored. Timeouts have the usual ~15.6 ms granularity. The `WSAPoll`
fallback inherits `WSAPoll`'s old-Windows gap (failed non-blocking connects are not reported), so connect
should stay a blocking offload there. `poll::poll` is also implemented on Windows with `WSAPoll`; `select`
is still `ENOSYS` there.

Concepts:

- `Reactor::register(source, interest, wake)` returns a `Registration`. Registrations are one-shot:
  after the wake the source is disarmed until `Registration::rearm`, which the consumer calls after
  it sees `WouldBlock`. `take_ready` returns and clears the recorded `Ready` bits (read, write,
  error, hangup).
- Dropping a `Registration` deregisters it. Drop it before closing the descriptor; the reactor
  borrows the fd. Every kernel token is a slot index plus a generation, so a late event for a
  dropped registration, or for a reused slot, is ignored.
- `Poller` is owned by one loop thread that calls `turn(timeout)`; wakes run inline there.
  Registering, rearming and dropping work from any thread, including while the loop is blocked.
- `LoopWaker` is the cross-thread wake of a `Poller`. An atomic flag coalesces wakes: only the first
  after the loop consumed the previous wake performs the syscall, so a burst costs one.
- `OsScheduler::reactor()` registers on the driver's `Poller`.
- `event.rs` holds the allocation-free primitives the backends use (`epoll_ctl_data`,
  `epoll_wait_into`, `eventfd`, `kevent_into`, `EVFILT_USER`, `NOTE_TRIGGER`); the Python-facing
  `epoll_ctl`, `epoll_wait` and `kevent` are thin wrappers over them.

## What has run

Compile-checked for macOS (`aarch64-apple-darwin`), wasm32, Linux and Windows (`x86_64`). The reactor
and driver tests are written for the kqueue and `poll` backends (macOS host) and epoll (Linux host);
they were compiled but not run when this was added, and the Windows backend (`iocp`, both the `ProcessSocketNotifications` path and the forced `WSAPoll` fallback) and
its tests are compiled for `x86_64-pc-windows-msvc` but have not been run on Windows. No other Lumen crate registers with the
reactor yet: sockets, child pipes, the `lumen-runtime` loop and the web server still use their own
threads and blocking calls.
