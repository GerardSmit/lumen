//! The system logger (`openlog(3)`, `syslog(3)`, `closelog(3)`, `setlogmask(3)`) behind the
//! Python `syslog` module. Off Unix the calls do nothing.

#[cfg(unix)]
use std::sync::Mutex;

#[cfg(unix)]
static IDENT: Mutex<Option<std::ffi::CString>> = Mutex::new(None);

/// Opens the log. `openlog` keeps the ident pointer, so the text is kept alive until
/// [`closelog`] or the next open.
pub fn openlog(ident: Option<&str>, option: i32, facility: i32) {
    #[cfg(unix)]
    {
        let text = ident.map(|s| std::ffi::CString::new(s.replace('\0', "")).unwrap_or_default());
        let mut slot = IDENT.lock().unwrap_or_else(|p| p.into_inner());
        let ptr = text.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());
        // SAFETY: the ident string is stored in `IDENT` right below and outlives the log.
        unsafe { libc::openlog(ptr, option, facility) };
        *slot = text;
    }
    #[cfg(not(unix))]
    let _ = (ident, option, facility);
}

/// Sends `message` at `priority`; the text is never interpreted as a format string.
pub fn syslog(priority: i32, message: &str) {
    #[cfg(unix)]
    {
        let text = std::ffi::CString::new(message.replace('\0', "")).unwrap_or_default();
        // SAFETY: both strings are NUL-terminated and "%s" consumes exactly one argument.
        unsafe { libc::syslog(priority, c"%s".as_ptr(), text.as_ptr()) };
    }
    #[cfg(not(unix))]
    let _ = (priority, message);
}

pub fn closelog() {
    #[cfg(unix)]
    {
        // SAFETY: closelog takes no arguments.
        unsafe { libc::closelog() };
        *IDENT.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }
}

/// Sets the priority mask and returns the previous one.
pub fn setlogmask(mask: i32) -> i32 {
    #[cfg(unix)]
    {
        // SAFETY: plain integer argument.
        unsafe { libc::setlogmask(mask) }
    }
    #[cfg(not(unix))]
    {
        let _ = mask;
        0
    }
}

/// The `syslog` module's `LOG_*` constants.
pub fn constants() -> Vec<(&'static str, i64)> {
    let base: Vec<(&'static str, i64)> = vec![
        ("LOG_EMERG", 0),
        ("LOG_ALERT", 1),
        ("LOG_CRIT", 2),
        ("LOG_ERR", 3),
        ("LOG_WARNING", 4),
        ("LOG_NOTICE", 5),
        ("LOG_INFO", 6),
        ("LOG_DEBUG", 7),
        ("LOG_PID", 0x01),
        ("LOG_CONS", 0x02),
        ("LOG_NDELAY", 0x08),
        ("LOG_ODELAY", 0x04),
        ("LOG_NOWAIT", 0x10),
        ("LOG_PERROR", 0x20),
        ("LOG_KERN", 0),
        ("LOG_USER", 8),
        ("LOG_MAIL", 16),
        ("LOG_DAEMON", 24),
        ("LOG_AUTH", 32),
        ("LOG_SYSLOG", 40),
        ("LOG_LPR", 48),
        ("LOG_NEWS", 56),
        ("LOG_UUCP", 64),
        ("LOG_CRON", 72),
        ("LOG_AUTHPRIV", 80),
        ("LOG_LOCAL0", 128),
        ("LOG_LOCAL1", 136),
        ("LOG_LOCAL2", 144),
        ("LOG_LOCAL3", 152),
        ("LOG_LOCAL4", 160),
        ("LOG_LOCAL5", 168),
        ("LOG_LOCAL6", 176),
        ("LOG_LOCAL7", 184),
    ];
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos", target_os = "ios"))]
    let mut v = base;
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos", target_os = "ios")))]
    let v = base;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    v.push(("LOG_FTP", 88));
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    v.extend([("LOG_FTP", 88), ("LOG_NETINFO", 96), ("LOG_REMOTEAUTH", 104), ("LOG_INSTALL", 112), ("LOG_RAS", 120), ("LOG_LAUNCHD", 192)]);
    v
}
