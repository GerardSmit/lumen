//! OS error mapping: libuv's symbolic names (what Node reports) and numeric errnos (what
//! Python's `OSError.errno` and `errno` module carry), both derived from an `io::Error`.
//!
//! The tables list every errno name Python's `errno` module exposes on Linux and macOS. Values
//! come from `libc` on Unix (the Linux number is the fallback elsewhere); messages are glibc's
//! (or Apple's for the Apple-only names), and [`strerror`] asks the C library on Unix.

use std::fmt;
use std::io;

macro_rules! errno_table {
    ($($name:ident = $fallback:expr, $msg:expr;)*) => {
        #[cfg(unix)]
        pub(super) static TABLE: &[(&str, i32, &str)] = &[$((stringify!($name), libc::$name, $msg),)*];
        #[cfg(not(unix))]
        pub(super) static TABLE: &[(&str, i32, &str)] = &[$((stringify!($name), $fallback, $msg),)*];
    };
}

// Where two names share a number the later one is canonical (`code_of_errno`, Python's
// `errno.errorcode`), as in CPython's errnomodule.c.
mod common {
    errno_table! {
        ENODEV = 19, "No such device";
        EHOSTUNREACH = 113, "No route to host";
        ENOMSG = 42, "No message of desired type";
        ENODATA = 61, "No data available";
        ENOTBLK = 15, "Block device required";
        ENOSYS = 38, "Function not implemented";
        EPIPE = 32, "Broken pipe";
        EINVAL = 22, "Invalid argument";
        EOVERFLOW = 75, "Value too large for defined data type";
        EINTR = 4, "Interrupted system call";
        EUSERS = 87, "Too many users";
        ENOTEMPTY = 39, "Directory not empty";
        ENOBUFS = 105, "No buffer space available";
        EPROTO = 71, "Protocol error";
        EREMOTE = 66, "Object is remote";
        ECHILD = 10, "No child processes";
        ELOOP = 40, "Too many levels of symbolic links";
        EXDEV = 18, "Invalid cross-device link";
        E2BIG = 7, "Argument list too long";
        ESRCH = 3, "No such process";
        EMSGSIZE = 90, "Message too long";
        EAFNOSUPPORT = 97, "Address family not supported by protocol";
        EHOSTDOWN = 112, "Host is down";
        EPFNOSUPPORT = 96, "Protocol family not supported";
        ENOPROTOOPT = 92, "Protocol not available";
        EBUSY = 16, "Device or resource busy";
        EWOULDBLOCK = 11, "Resource temporarily unavailable";
        EISCONN = 106, "Transport endpoint is already connected";
        ESHUTDOWN = 108, "Cannot send after transport endpoint shutdown";
        EBADF = 9, "Bad file descriptor";
        EMULTIHOP = 72, "Multihop attempted";
        EIO = 5, "Input/output error";
        EPROTOTYPE = 91, "Protocol wrong type for socket";
        ENOSPC = 28, "No space left on device";
        ENOEXEC = 8, "Exec format error";
        EALREADY = 114, "Operation already in progress";
        ENETDOWN = 100, "Network is down";
        EACCES = 13, "Permission denied";
        EILSEQ = 84, "Invalid or incomplete multibyte or wide character";
        ENOTDIR = 20, "Not a directory";
        EPERM = 1, "Operation not permitted";
        EDOM = 33, "Numerical argument out of domain";
        ECONNREFUSED = 111, "Connection refused";
        EISDIR = 21, "Is a directory";
        EPROTONOSUPPORT = 93, "Protocol not supported";
        EROFS = 30, "Read-only file system";
        EADDRNOTAVAIL = 99, "Cannot assign requested address";
        EIDRM = 43, "Identifier removed";
        EBADMSG = 74, "Bad message";
        ENFILE = 23, "Too many open files in system";
        ESPIPE = 29, "Illegal seek";
        ENOLINK = 67, "Link has been severed";
        ENETRESET = 102, "Network dropped connection on reset";
        ETIMEDOUT = 110, "Connection timed out";
        ENOENT = 2, "No such file or directory";
        EEXIST = 17, "File exists";
        EDQUOT = 122, "Disk quota exceeded";
        ENOSTR = 60, "Device not a stream";
        EFAULT = 14, "Bad address";
        EFBIG = 27, "File too large";
        ENOTCONN = 107, "Transport endpoint is not connected";
        EDESTADDRREQ = 89, "Destination address required";
        ENOLCK = 37, "No locks available";
        ECONNABORTED = 103, "Software caused connection abort";
        ENETUNREACH = 101, "Network is unreachable";
        ESTALE = 116, "Stale file handle";
        ENOSR = 63, "Out of streams resources";
        ENOMEM = 12, "Cannot allocate memory";
        ENOTSOCK = 88, "Socket operation on non-socket";
        EMLINK = 31, "Too many links";
        ERANGE = 34, "Numerical result out of range";
        ECONNRESET = 104, "Connection reset by peer";
        EADDRINUSE = 98, "Address already in use";
        EOPNOTSUPP = 95, "Operation not supported";
        EAGAIN = 11, "Resource temporarily unavailable";
        ENAMETOOLONG = 36, "File name too long";
        ENOTTY = 25, "Inappropriate ioctl for device";
        ESOCKTNOSUPPORT = 94, "Socket type not supported";
        ETIME = 62, "Timer expired";
        ETOOMANYREFS = 109, "Too many references: cannot splice";
        EMFILE = 24, "Too many open files";
        ETXTBSY = 26, "Text file busy";
        EINPROGRESS = 115, "Operation now in progress";
        ENXIO = 6, "No such device or address";
        ENOTSUP = 95, "Operation not supported";
        EDEADLK = 35, "Resource deadlock avoided";
        ECANCELED = 125, "Operation canceled";
        EOWNERDEAD = 130, "Owner died";
        ENOTRECOVERABLE = 131, "State not recoverable";
    }
}

#[cfg(any(target_os = "linux", target_os = "android", not(unix)))]
mod system {
    errno_table! {
        ECHRNG = 44, "Channel number out of range";
        EL2NSYNC = 45, "Level 2 not synchronized";
        EL3HLT = 46, "Level 3 halted";
        EL3RST = 47, "Level 3 reset";
        ELNRNG = 48, "Link number out of range";
        EUNATCH = 49, "Protocol driver not attached";
        ENOCSI = 50, "No CSI structure available";
        EL2HLT = 51, "Level 2 halted";
        EBADE = 52, "Invalid exchange";
        EBADR = 53, "Invalid request descriptor";
        EXFULL = 54, "Exchange full";
        ENOANO = 55, "No anode";
        EBADRQC = 56, "Invalid request code";
        EBADSLT = 57, "Invalid slot";
        EDEADLOCK = 35, "Resource deadlock avoided";
        EBFONT = 59, "Bad font file format";
        ENONET = 64, "Machine is not on the network";
        ENOPKG = 65, "Package not installed";
        EADV = 68, "Advertise error";
        ESRMNT = 69, "Srmount error";
        ECOMM = 70, "Communication error on send";
        EDOTDOT = 73, "RFS specific error";
        ENOTUNIQ = 76, "Name not unique on network";
        EBADFD = 77, "File descriptor in bad state";
        EREMCHG = 78, "Remote address changed";
        ELIBACC = 79, "Can not access a needed shared library";
        ELIBBAD = 80, "Accessing a corrupted shared library";
        ELIBSCN = 81, ".lib section in a.out corrupted";
        ELIBMAX = 82, "Attempting to link in too many shared libraries";
        ELIBEXEC = 83, "Cannot exec a shared library directly";
        ERESTART = 85, "Interrupted system call should be restarted";
        ESTRPIPE = 86, "Streams pipe error";
        EUCLEAN = 117, "Structure needs cleaning";
        ENOTNAM = 118, "Not a XENIX named type file";
        ENAVAIL = 119, "No XENIX semaphores available";
        EISNAM = 120, "Is a named type file";
        EREMOTEIO = 121, "Remote I/O error";
        ENOMEDIUM = 123, "No medium found";
        EMEDIUMTYPE = 124, "Wrong medium type";
        ENOKEY = 126, "Required key not available";
        EKEYEXPIRED = 127, "Key has expired";
        EKEYREVOKED = 128, "Key has been revoked";
        EKEYREJECTED = 129, "Key was rejected by service";
        ERFKILL = 132, "Operation not possible due to RF-kill";
        EHWPOISON = 133, "Memory page has hardware error";
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod system {
    errno_table! {
        EPROCLIM = 67, "Too many processes";
        EBADRPC = 72, "RPC struct is bad";
        ERPCMISMATCH = 73, "RPC version wrong";
        EPROGUNAVAIL = 74, "RPC prog. not avail";
        EPROGMISMATCH = 75, "Program version wrong";
        EPROCUNAVAIL = 76, "Bad procedure for program";
        EFTYPE = 79, "Inappropriate file type or format";
        EAUTH = 80, "Authentication error";
        ENEEDAUTH = 81, "Need authenticator";
        EPWROFF = 82, "Device power is off";
        EDEVERR = 83, "Device error";
        EBADEXEC = 85, "Bad executable (or shared library)";
        EBADARCH = 86, "Bad CPU type in executable";
        ESHLIBVERS = 87, "Shared library version mismatch";
        EBADMACHO = 88, "Malformed Mach-o file";
        ENOATTR = 93, "Attribute not found";
        ENOPOLICY = 103, "Policy not found";
        EQFULL = 106, "Interface output queue is full";
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos", target_os = "ios", not(unix))))]
mod system {
    pub(super) static TABLE: &[(&str, i32, &str)] = &[];
}

fn entries() -> impl DoubleEndedIterator<Item = &'static (&'static str, i32, &'static str)> {
    common::TABLE.iter().chain(system::TABLE.iter())
}

/// Every errno name of this platform with its number, in Python's `errno` module order.
pub fn names() -> impl Iterator<Item = (&'static str, i32)> {
    entries().map(|e| (e.0, e.1))
}

/// The numeric errno behind a libuv name (`None` for names with no errno, such as `EOF`).
pub fn errno_of_code(code: &str) -> Option<i32> {
    entries().find(|e| e.0 == code).map(|e| e.1)
}

/// The canonical name for a numeric errno of this platform.
pub fn code_of_errno(n: i32) -> Option<&'static str> {
    entries().rev().find(|e| e.1 == n).map(|e| e.0)
}

/// `strerror` text for a libuv name.
pub fn message(code: &str) -> &'static str {
    entries().find(|e| e.0 == code).map_or("Unknown error", |e| e.2)
}

/// The C library's `strerror` for a numeric errno (the table's text off Unix).
pub fn strerror(n: i32) -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        // SAFETY: the buffer is writable for its full length and strerror_r NUL-terminates it.
        let rc = unsafe { libc::strerror_r(n, buf.as_mut_ptr().cast(), buf.len()) };
        if rc == 0 {
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            return String::from_utf8_lossy(&buf[..end]).into_owned();
        }
    }
    match code_of_errno(n) {
        Some(code) => message(code).to_string(),
        None => format!("Unknown error {}", n),
    }
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
        K::WouldBlock => "EAGAIN",
        K::Unsupported => "ENOSYS",
        K::OutOfMemory => "ENOMEM",
        K::ConnectionRefused => "ECONNREFUSED",
        K::ConnectionReset => "ECONNRESET",
        K::ConnectionAborted => "ECONNABORTED",
        K::NotConnected => "ENOTCONN",
        K::AddrInUse => "EADDRINUSE",
        K::AddrNotAvailable => "EADDRNOTAVAIL",
        K::TimedOut => "ETIMEDOUT",
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
        10022 => "EINVAL", // WSAEINVAL
        10038 => "ENOTSOCK",
        10040 => "EMSGSIZE",
        10047 => "EAFNOSUPPORT",
        10048 => "EADDRINUSE",
        10049 => "EADDRNOTAVAIL",
        10051 => "ENETUNREACH",
        10053 => "ECONNABORTED",
        10054 => "ECONNRESET",
        10057 => "ENOTCONN",
        10060 => "ETIMEDOUT",
        10061 => "ECONNREFUSED",
        10065 => "EHOSTUNREACH",
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
