//! POSIX inter-process primitives: named shared memory (`shm_open`, `shm_unlink`) and named
//! semaphores (`sem_open`, `sem_wait`, `sem_post`, ...). Python's `_posixshmem` and
//! `_multiprocessing.SemLock` run on these. Android has no named shared memory; off Unix every
//! call fails with `ENOSYS`.

use crate::errno::FsError;

pub type R<T> = Result<T, FsError>;

#[cfg(unix)]
fn cname(name: &str) -> R<std::ffi::CString> {
    std::ffi::CString::new(name).map_err(|_| FsError("EINVAL"))
}

#[cfg(unix)]
fn os_err() -> FsError {
    std::io::Error::last_os_error().into()
}

/// Whether named shared memory exists here.
pub const HAVE_SHM: bool = cfg!(all(unix, not(target_os = "android")));

/// `shm_open(3)`: a raw descriptor.
pub fn shm_open(name: &str, flags: i32, mode: u32) -> R<i32> {
    #[cfg(all(unix, not(target_os = "android")))]
    {
        let name = cname(name)?;
        loop {
            // SAFETY: `name` is a NUL-terminated string.
            let fd = unsafe { libc::shm_open(name.as_ptr(), flags, mode as libc::c_uint) };
            if fd >= 0 {
                return Ok(fd);
            }
            let e = os_err();
            if e.errno() != libc::EINTR {
                return Err(e);
            }
        }
    }
    #[cfg(not(all(unix, not(target_os = "android"))))]
    {
        let _ = (name, flags, mode);
        Err(FsError("ENOSYS"))
    }
}

/// `shm_unlink(3)`.
pub fn shm_unlink(name: &str) -> R<()> {
    #[cfg(all(unix, not(target_os = "android")))]
    {
        let name = cname(name)?;
        // SAFETY: `name` is a NUL-terminated string.
        if unsafe { libc::shm_unlink(name.as_ptr()) } != 0 {
            return Err(os_err());
        }
        Ok(())
    }
    #[cfg(not(all(unix, not(target_os = "android"))))]
    {
        let _ = name;
        Err(FsError("ENOSYS"))
    }
}

/// An open named semaphore (`sem_t *`).
pub struct Semaphore {
    #[cfg(unix)]
    handle: *mut libc::sem_t,
    owned: bool,
}

/// What `sem_getvalue` reports, where it exists (macOS has none).
pub const HAVE_SEM_GETVALUE: bool = cfg!(all(unix, not(any(target_os = "macos", target_os = "ios"))));

/// `SEM_VALUE_MAX`.
pub fn sem_value_max() -> i32 {
    #[cfg(unix)]
    {
        // SAFETY: sysconf takes a plain integer.
        let n = unsafe { libc::sysconf(libc::_SC_SEM_VALUE_MAX) };
        if n <= 0 || n > i32::MAX as libc::c_long {
            i32::MAX
        } else {
            n as i32
        }
    }
    #[cfg(not(unix))]
    i32::MAX
}

/// `sem_unlink(3)`.
pub fn sem_unlink(name: &str) -> R<()> {
    #[cfg(unix)]
    {
        let name = cname(name)?;
        // SAFETY: `name` is a NUL-terminated string.
        if unsafe { libc::sem_unlink(name.as_ptr()) } != 0 {
            return Err(os_err());
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = name;
        Err(FsError("ENOSYS"))
    }
}

impl Semaphore {
    /// `sem_open(name, O_CREAT | O_EXCL, 0600, value)`.
    pub fn create(name: &str, value: u32) -> R<Semaphore> {
        #[cfg(unix)]
        {
            let name = cname(name)?;
            // SAFETY: `name` is a NUL-terminated string; mode and value are the variadic ints.
            let h = unsafe { libc::sem_open(name.as_ptr(), libc::O_CREAT | libc::O_EXCL, 0o600 as libc::c_uint, value as libc::c_uint) };
            if h == libc::SEM_FAILED {
                return Err(os_err());
            }
            Ok(Semaphore { handle: h, owned: true })
        }
        #[cfg(not(unix))]
        {
            let _ = (name, value);
            Err(FsError("ENOSYS"))
        }
    }

    /// `sem_open(name, 0)`: an existing semaphore.
    pub fn open(name: &str) -> R<Semaphore> {
        #[cfg(unix)]
        {
            let name = cname(name)?;
            // SAFETY: `name` is a NUL-terminated string.
            let h = unsafe { libc::sem_open(name.as_ptr(), 0) };
            if h == libc::SEM_FAILED {
                return Err(os_err());
            }
            Ok(Semaphore { handle: h, owned: true })
        }
        #[cfg(not(unix))]
        {
            let _ = name;
            Err(FsError("ENOSYS"))
        }
    }

    /// The semaphore behind a handle number from [`Semaphore::raw`] (inherited across `fork`);
    /// the result does not close it.
    pub fn from_raw(raw: usize) -> Semaphore {
        #[cfg(not(unix))]
        let _ = raw;
        Semaphore {
            #[cfg(unix)]
            handle: raw as *mut libc::sem_t,
            owned: false,
        }
    }

    /// The handle as a number.
    pub fn raw(&self) -> usize {
        #[cfg(unix)]
        {
            self.handle as usize
        }
        #[cfg(not(unix))]
        0
    }

    /// `sem_trywait`: `Ok(false)` when the value is zero; `EINTR` is an error.
    pub fn try_wait(&self) -> R<bool> {
        #[cfg(unix)]
        {
            // SAFETY: `handle` is an open semaphore.
            if unsafe { libc::sem_trywait(self.handle) } == 0 {
                return Ok(true);
            }
            let e = os_err();
            if e.errno() == libc::EAGAIN {
                return Ok(false);
            }
            Err(e)
        }
        #[cfg(not(unix))]
        Err(FsError("ENOSYS"))
    }

    /// Waits up to `ms` milliseconds for the value to become positive and takes it: `Ok(true)`
    /// when taken, `Ok(false)` on timeout, `EINTR` as an error. Without `sem_timedwait`
    /// (macOS) it polls with a growing delay.
    pub fn wait_ms(&self, ms: u64) -> R<bool> {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            let mut now: libc::timespec = unsafe { std::mem::zeroed() };
            // SAFETY: `now` is a live timespec.
            unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut now) };
            let total = now.tv_nsec as u64 + (ms % 1000) * 1_000_000;
            let deadline = libc::timespec {
                tv_sec: now.tv_sec + (ms / 1000) as libc::time_t + (total / 1_000_000_000) as libc::time_t,
                tv_nsec: (total % 1_000_000_000) as _,
            };
            // SAFETY: `handle` is an open semaphore and `deadline` a live timespec.
            if unsafe { libc::sem_timedwait(self.handle, &deadline) } == 0 {
                return Ok(true);
            }
            let e = os_err();
            if e.errno() == libc::ETIMEDOUT {
                return Ok(false);
            }
            Err(e)
        }
        #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
        {
            let start = std::time::Instant::now();
            let limit = std::time::Duration::from_millis(ms);
            let mut delay = std::time::Duration::from_micros(500);
            loop {
                if self.try_wait()? {
                    return Ok(true);
                }
                let spent = start.elapsed();
                if spent >= limit {
                    return Ok(false);
                }
                std::thread::sleep(delay.min(limit - spent));
                delay = (delay * 2).min(std::time::Duration::from_millis(20));
            }
        }
        #[cfg(not(unix))]
        {
            let _ = ms;
            Err(FsError("ENOSYS"))
        }
    }

    /// `sem_post`.
    pub fn post(&self) -> R<()> {
        #[cfg(unix)]
        {
            // SAFETY: `handle` is an open semaphore.
            if unsafe { libc::sem_post(self.handle) } != 0 {
                return Err(os_err());
            }
            Ok(())
        }
        #[cfg(not(unix))]
        Err(FsError("ENOSYS"))
    }

    /// `sem_getvalue`, `None` where the platform has none.
    pub fn value(&self) -> R<Option<i32>> {
        #[cfg(all(unix, not(any(target_os = "macos", target_os = "ios"))))]
        {
            let mut v: libc::c_int = 0;
            // SAFETY: `handle` is an open semaphore and `v` a live int.
            if unsafe { libc::sem_getvalue(self.handle, &mut v) } != 0 {
                return Err(os_err());
            }
            Ok(Some(v))
        }
        #[cfg(not(all(unix, not(any(target_os = "macos", target_os = "ios")))))]
        Ok(None)
    }
}

impl Drop for Semaphore {
    fn drop(&mut self) {
        #[cfg(unix)]
        if self.owned {
            // SAFETY: `handle` was opened by this value and is closed once.
            unsafe { libc::sem_close(self.handle) };
        }
        #[cfg(not(unix))]
        let _ = self.owned;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn semaphore_counts() {
        let name = format!("/lumen-os-test-{}", std::process::id());
        let s = Semaphore::create(&name, 1).unwrap();
        sem_unlink(&name).unwrap();
        assert!(s.try_wait().unwrap());
        assert!(!s.try_wait().unwrap());
        assert!(!s.wait_ms(5).unwrap());
        s.post().unwrap();
        assert!(s.wait_ms(5).unwrap());
    }
}
