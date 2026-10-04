//! libuv's error table (`uv_err_name` / `uv_strerror` / `UV_E*`): what Node reports as an error's
//! `code`, `errno` and description. On Unix a libuv errno is the negated OS errno for every name
//! the platform defines; names it lacks (and every name on Windows) use libuv's own portable
//! value (`-40xx`, `-30xx` for `EAI_*`).

use crate::errno::errno_of_code;

/// `(name, portable value, description)` in libuv's `UV_ERRNO_MAP` order.
static TABLE: &[(&str, i32, &str)] = &[
    ("E2BIG", -4093, "argument list too long"),
    ("EACCES", -4092, "permission denied"),
    ("EADDRINUSE", -4091, "address already in use"),
    ("EADDRNOTAVAIL", -4090, "address not available"),
    ("EAFNOSUPPORT", -4089, "address family not supported"),
    ("EAGAIN", -4088, "resource temporarily unavailable"),
    ("EAI_ADDRFAMILY", -3000, "address family not supported"),
    ("EAI_AGAIN", -3001, "temporary failure"),
    ("EAI_BADFLAGS", -3002, "bad ai_flags value"),
    ("EAI_BADHINTS", -3013, "invalid value for hints"),
    ("EAI_CANCELED", -3003, "request canceled"),
    ("EAI_FAIL", -3004, "permanent failure"),
    ("EAI_FAMILY", -3005, "ai_family not supported"),
    ("EAI_MEMORY", -3006, "out of memory"),
    ("EAI_NODATA", -3007, "no address"),
    ("EAI_NONAME", -3008, "unknown node or service"),
    ("EAI_OVERFLOW", -3009, "argument buffer overflow"),
    ("EAI_PROTOCOL", -3014, "resolved protocol is unknown"),
    (
        "EAI_SERVICE",
        -3010,
        "service not available for socket type",
    ),
    ("EAI_SOCKTYPE", -3011, "socket type not supported"),
    ("EALREADY", -4084, "connection already in progress"),
    ("EBADF", -4083, "bad file descriptor"),
    ("EBUSY", -4082, "resource busy or locked"),
    ("ECANCELED", -4081, "operation canceled"),
    ("ECHARSET", -4080, "invalid Unicode character"),
    ("ECONNABORTED", -4079, "software caused connection abort"),
    ("ECONNREFUSED", -4078, "connection refused"),
    ("ECONNRESET", -4077, "connection reset by peer"),
    ("EDESTADDRREQ", -4076, "destination address required"),
    ("EEXIST", -4075, "file already exists"),
    ("EFAULT", -4074, "bad address in system call argument"),
    ("EFBIG", -4036, "file too large"),
    ("EHOSTUNREACH", -4073, "host is unreachable"),
    ("EINTR", -4072, "interrupted system call"),
    ("EINVAL", -4071, "invalid argument"),
    ("EIO", -4070, "i/o error"),
    ("EISCONN", -4069, "socket is already connected"),
    ("EISDIR", -4068, "illegal operation on a directory"),
    ("ELOOP", -4067, "too many symbolic links encountered"),
    ("EMFILE", -4066, "too many open files"),
    ("EMSGSIZE", -4065, "message too long"),
    ("ENAMETOOLONG", -4064, "name too long"),
    ("ENETDOWN", -4063, "network is down"),
    ("ENETUNREACH", -4062, "network is unreachable"),
    ("ENFILE", -4061, "file table overflow"),
    ("ENOBUFS", -4060, "no buffer space available"),
    ("ENODEV", -4059, "no such device"),
    ("ENOENT", -4058, "no such file or directory"),
    ("ENOMEM", -4057, "not enough memory"),
    ("ENONET", -4056, "machine is not on the network"),
    ("ENOPROTOOPT", -4035, "protocol not available"),
    ("ENOSPC", -4055, "no space left on device"),
    ("ENOSYS", -4054, "function not implemented"),
    ("ENOTCONN", -4053, "socket is not connected"),
    ("ENOTDIR", -4052, "not a directory"),
    ("ENOTEMPTY", -4051, "directory not empty"),
    ("ENOTSOCK", -4050, "socket operation on non-socket"),
    ("ENOTSUP", -4049, "operation not supported on socket"),
    ("EOVERFLOW", -4026, "value too large for defined data type"),
    ("EPERM", -4048, "operation not permitted"),
    ("EPIPE", -4047, "broken pipe"),
    ("EPROTO", -4046, "protocol error"),
    ("EPROTONOSUPPORT", -4045, "protocol not supported"),
    ("EPROTOTYPE", -4044, "protocol wrong type for socket"),
    ("ERANGE", -4034, "result too large"),
    ("EROFS", -4043, "read-only file system"),
    (
        "ESHUTDOWN",
        -4042,
        "cannot send after transport endpoint shutdown",
    ),
    ("ESPIPE", -4041, "invalid seek"),
    ("ESRCH", -4040, "no such process"),
    ("ETIMEDOUT", -4039, "connection timed out"),
    ("ETXTBSY", -4038, "text file is busy"),
    ("EXDEV", -4037, "cross-device link not permitted"),
    ("UNKNOWN", -4094, "unknown error"),
    ("EOF", -4095, "end of file"),
    ("ENXIO", -4033, "no such device or address"),
    ("EMLINK", -4032, "too many links"),
    ("EHOSTDOWN", -4031, "host is down"),
    ("EREMOTEIO", -4030, "remote I/O error"),
    ("ENOTTY", -4029, "inappropriate ioctl for device"),
    ("EFTYPE", -4028, "inappropriate file type or format"),
    ("EILSEQ", -4027, "illegal byte sequence"),
    ("ESOCKTNOSUPPORT", -4025, "socket type not supported"),
    ("ENODATA", -4024, "no data available"),
    ("EUNATCH", -4023, "protocol driver not attached"),
];

fn platform_value(name: &str, portable: i32) -> i32 {
    if cfg!(unix) {
        if let Some(n) = errno_of_code(name) {
            return -n;
        }
    }
    portable
}

/// Every libuv error as `(name, errno on this platform, description)`, in libuv's order.
pub fn errors() -> impl Iterator<Item = (&'static str, i32, &'static str)> {
    TABLE
        .iter()
        .map(|&(name, portable, desc)| (name, platform_value(name, portable), desc))
}

/// The libuv errno of `name` on this platform (`errno("ENOENT")` is `-2` on Unix).
pub fn errno(name: &str) -> Option<i32> {
    TABLE
        .iter()
        .find(|e| e.0 == name)
        .map(|&(n, portable, _)| platform_value(n, portable))
}

/// The name of a libuv errno on this platform.
pub fn name(errno: i32) -> Option<&'static str> {
    errors().find(|e| e.1 == errno).map(|e| e.0)
}

/// libuv's description of `name` (`"no such file or directory"`).
pub fn message(name: &str) -> Option<&'static str> {
    TABLE.iter().find(|e| e.0 == name).map(|e| e.2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_numbers() {
        assert_eq!(message("EISDIR"), Some("illegal operation on a directory"));
        assert_eq!(errno("EOF"), Some(-4095));
        assert_eq!(name(-4095), Some("EOF"));
        assert_eq!(errno("EAI_NONAME"), Some(-3008));
        #[cfg(unix)]
        {
            assert_eq!(errno("ENOENT"), Some(-2));
            assert_eq!(name(-2), Some("ENOENT"));
        }
        #[cfg(windows)]
        assert_eq!(errno("ENOENT"), Some(-4058));
        assert_eq!(errors().count(), TABLE.len());
    }
}
