//! OS error mapping: libuv's symbolic names (what Node reports) and numeric errnos (what
//! Python's `OSError.errno` carries), both derived from an `io::Error`.

use std::fmt;
use std::io;

macro_rules! errno_table {
    ($($name:ident = $linux:expr, $msg:expr;)*) => {
        #[cfg(unix)]
        mod num { $(pub const $name: i32 = libc::$name;)* }
        #[cfg(not(unix))]
        mod num { $(pub const $name: i32 = $linux;)* }

        /// `(libuv name, errno, strerror text)` for every code the file-system layer produces.
        static TABLE: &[(&str, i32, &str)] = &[$((stringify!($name), num::$name, $msg),)*];
    };
}

errno_table! {
    EPERM = 1, "Operation not permitted";
    ENOENT = 2, "No such file or directory";
    ESRCH = 3, "No such process";
    EINTR = 4, "Interrupted system call";
    EIO = 5, "Input/output error";
    ENXIO = 6, "No such device or address";
    E2BIG = 7, "Argument list too long";
    EBADF = 9, "Bad file descriptor";
    EAGAIN = 11, "Resource temporarily unavailable";
    ENOMEM = 12, "Cannot allocate memory";
    EACCES = 13, "Permission denied";
    EFAULT = 14, "Bad address";
    EBUSY = 16, "Device or resource busy";
    EEXIST = 17, "File exists";
    EXDEV = 18, "Invalid cross-device link";
    ENODEV = 19, "No such device";
    ENOTDIR = 20, "Not a directory";
    EISDIR = 21, "Is a directory";
    EINVAL = 22, "Invalid argument";
    ENFILE = 23, "Too many open files in system";
    EMFILE = 24, "Too many open files";
    ENOTTY = 25, "Inappropriate ioctl for device";
    ETXTBSY = 26, "Text file busy";
    EFBIG = 27, "File too large";
    ENOSPC = 28, "No space left on device";
    ESPIPE = 29, "Illegal seek";
    EROFS = 30, "Read-only file system";
    EMLINK = 31, "Too many links";
    EPIPE = 32, "Broken pipe";
    ERANGE = 34, "Numerical result out of range";
    ENAMETOOLONG = 36, "File name too long";
    ENOSYS = 38, "Function not implemented";
    ENOTEMPTY = 39, "Directory not empty";
    ELOOP = 40, "Too many levels of symbolic links";
    EOVERFLOW = 75, "Value too large for defined data type";
    ENOTSUP = 95, "Operation not supported";
}

/// The numeric errno behind a libuv name (`None` for names with no errno, such as `EOF`).
pub fn errno_of_code(code: &str) -> Option<i32> {
    TABLE.iter().find(|e| e.0 == code).map(|e| e.1)
}

/// The libuv name for a numeric errno of this platform.
pub fn code_of_errno(n: i32) -> Option<&'static str> {
    TABLE.iter().find(|e| e.1 == n).map(|e| e.0)
}

/// `strerror` text for a libuv name.
pub fn message(code: &str) -> &'static str {
    TABLE.iter().find(|e| e.0 == code).map_or("Unknown error", |e| e.2)
}

/// libuv's name for an OS error (`uv_translate_sys_error` on Windows, errno names on Unix).
pub fn uv_code(e: &io::Error) -> &'static str {
    if let Some(code) = e.raw_os_error().and_then(os_code) {
        return code;
    }
    use io::ErrorKind as K;
    match e.kind() {
        K::NotFound => "ENOENT",
        K::PermissionDenied => "EACCES",
        K::AlreadyExists => "EEXIST",
        K::InvalidInput => "EINVAL",
        K::IsADirectory => "EISDIR",
        K::NotADirectory => "ENOTDIR",
        K::DirectoryNotEmpty => "ENOTEMPTY",
        K::BrokenPipe => "EPIPE",
        K::Unsupported => "ENOSYS",
        K::OutOfMemory => "ENOMEM",
        _ => "EIO",
    }
}

/// The numeric errno an OS error reports: the raw errno on Unix, the mapped libuv code's
/// equivalent elsewhere.
pub fn errno(e: &io::Error) -> i32 {
    #[cfg(unix)]
    if let Some(n) = e.raw_os_error() {
        return n;
    }
    errno_of_code(uv_code(e)).unwrap_or(errno_of_code("EIO").unwrap_or(5))
}

#[cfg(unix)]
fn os_code(n: i32) -> Option<&'static str> {
    code_of_errno(n)
}

#[cfg(windows)]
fn os_code(n: i32) -> Option<&'static str> {
    Some(match n {
        1 => "EISDIR",     // ERROR_INVALID_FUNCTION (a read on a directory handle)
        2 | 3 => "ENOENT", // FILE_NOT_FOUND, PATH_NOT_FOUND
        4 => "EMFILE",
        5 => "EPERM", // ACCESS_DENIED
        6 => "EBADF",
        8 | 14 => "ENOMEM",
        15 => "ENOENT", // INVALID_DRIVE
        17 => "EXDEV",  // NOT_SAME_DEVICE
        19 => "EROFS",  // WRITE_PROTECT
        31 => "EIO",
        32 | 33 => "EBUSY", // SHARING_VIOLATION, LOCK_VIOLATION
        38 => "EOF",
        39 | 112 => "ENOSPC",
        50 => "ENOTSUP",
        80 | 183 => "EEXIST",
        87 => "EINVAL",
        109 => "EOF",                // BROKEN_PIPE
        123 | 126 | 161 => "ENOENT", // INVALID_NAME, MOD_NOT_FOUND, BAD_PATHNAME
        131 => "EINVAL",             // NEGATIVE_SEEK
        145 => "ENOTEMPTY",
        206 => "ENAMETOOLONG",
        230 => "EPIPE",
        231 => "EBUSY",
        232 => "EPIPE",
        267 => "ENOENT", // DIRECTORY (libuv maps "invalid directory name" to ENOENT)
        740 | 998 | 1920 => "EACCES",
        1314 => "EPERM", // PRIVILEGE_NOT_HELD
        1921 => "ELOOP",
        4390 => "EINVAL", // NOT_A_REPARSE_POINT
        4393 => "EINVAL", // INVALID_REPARSE_DATA
        _ => return None,
    })
}

#[cfg(not(any(unix, windows)))]
fn os_code(_: i32) -> Option<&'static str> {
    None
}

/// A file-system error: the libuv code (`ENOENT`, `EPERM`, ...) it maps to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FsError(pub &'static str);

impl FsError {
    #[inline]
    pub fn code(self) -> &'static str {
        self.0
    }

    pub fn errno(self) -> i32 {
        errno_of_code(self.0).unwrap_or(5)
    }

    pub fn message(self) -> &'static str {
        message(self.0)
    }
}

impl From<io::Error> for FsError {
    #[inline]
    fn from(e: io::Error) -> FsError {
        FsError(uv_code(&e))
    }
}

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.0, self.message())
    }
}

impl std::error::Error for FsError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_both_ways() {
        let e = io::Error::from_raw_os_error(errno_of_code("ENOENT").unwrap());
        assert_eq!(uv_code(&e), "ENOENT");
        assert_eq!(FsError::from(e).errno(), errno_of_code("ENOENT").unwrap());
        assert_eq!(code_of_errno(errno_of_code("EEXIST").unwrap()), Some("EEXIST"));
        assert_eq!(message("ENOTDIR"), "Not a directory");
        assert_eq!(errno_of_code("EOF"), None);
        let kind_only = io::Error::from(io::ErrorKind::PermissionDenied);
        assert_eq!(uv_code(&kind_only), "EACCES");
        assert_eq!(errno(&kind_only), errno_of_code("EACCES").unwrap());
    }
}
