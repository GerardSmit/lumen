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

/// The longest thread name, in bytes without the NUL, the OS keeps (`_thread._NAME_MAXLEN`).
pub const NAME_MAXLEN: usize = if cfg!(target_vendor = "apple") { 63 } else { 15 };

/// Sets the name of the calling thread, cut at [`NAME_MAXLEN`] bytes on a character boundary.
/// A no-op where the OS has no per-thread names.
pub fn set_name(name: &str) {
    let mut end = name.len().min(NAME_MAXLEN);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    let Ok(c) = std::ffi::CString::new(&name.as_bytes()[..end]) else { return };
    #[cfg(target_vendor = "apple")]
    {
        // SAFETY: `c` is a NUL-terminated name; the call names the calling thread.
        unsafe { libc::pthread_setname_np(c.as_ptr()) };
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // SAFETY: `c` is a NUL-terminated name no longer than 15 bytes; the thread is the caller.
        unsafe { libc::pthread_setname_np(libc::pthread_self(), c.as_ptr()) };
    }
    #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android")))]
    let _ = c;
}

/// The name of the calling thread, when the OS reports one.
pub fn get_name() -> Option<String> {
    #[cfg(unix)]
    {
        let mut buf = [0 as libc::c_char; 64];
        // SAFETY: `buf` is writable for its stated length; the thread is the caller.
        let rc = unsafe { libc::pthread_getname_np(libc::pthread_self(), buf.as_mut_ptr(), buf.len()) };
        if rc != 0 {
            return None;
        }
        // SAFETY: on success the buffer holds a NUL-terminated string.
        let name = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) };
        Some(name.to_string_lossy().into_owned())
    }
    #[cfg(not(unix))]
    {
        None
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
