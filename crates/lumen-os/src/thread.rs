//! Identity of the calling OS thread, as the Python `_thread` module reports it.

/// An identifier unique among the live threads of the process (`pthread_self`).
pub fn ident() -> u64 {
    #[cfg(unix)]
    {
        // SAFETY: pthread_self takes no arguments and cannot fail.
        unsafe { libc::pthread_self() as u64 }
    }
    #[cfg(not(unix))]
    {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        thread_local! {
            static ID: u64 = NEXT.fetch_add(1, Ordering::Relaxed);
        }
        ID.with(|id| *id)
    }
}

/// The kernel's identifier of the calling thread.
pub fn native_id() -> u64 {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // SAFETY: gettid takes no arguments and cannot fail.
        unsafe { libc::syscall(libc::SYS_gettid) as u64 }
    }
    #[cfg(target_vendor = "apple")]
    {
        let mut id = 0u64;
        // SAFETY: a zero thread means the calling thread; `id` is a live out-pointer.
        unsafe { libc::pthread_threadid_np(0, &mut id) };
        id
    }
    #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
    {
        ident()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_differ_between_live_threads() {
        let (a, b) = (ident(), std::thread::spawn(ident).join().unwrap());
        assert_ne!(a, b);
        assert_ne!(native_id(), 0);
    }
}
