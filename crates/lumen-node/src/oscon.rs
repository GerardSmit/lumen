//! `globalThis.__oscon`: the platform's signal and errno tables and libuv's error map, served from
//! `lumen_os` so no JS glue hard-codes one platform's numbers.

pub(crate) use bindings::Module;

#[lumen_bind::module(name = "__oscon")]
pub(crate) mod bindings {
    /// The errno names Node's `os.constants.errno` lists (node_constants.cc), in its order.
    const NODE_ERRNO: &[&str] = &[
        "E2BIG", "EACCES", "EADDRINUSE", "EADDRNOTAVAIL", "EAFNOSUPPORT", "EAGAIN", "EALREADY", "EBADF",
        "EBADMSG", "EBUSY", "ECANCELED", "ECHILD", "ECONNABORTED", "ECONNREFUSED", "ECONNRESET", "EDEADLK",
        "EDESTADDRREQ", "EDOM", "EDQUOT", "EEXIST", "EFAULT", "EFBIG", "EHOSTUNREACH", "EIDRM", "EILSEQ",
        "EINPROGRESS", "EINTR", "EINVAL", "EIO", "EISCONN", "EISDIR", "ELOOP", "EMFILE", "EMLINK", "EMSGSIZE",
        "EMULTIHOP", "ENAMETOOLONG", "ENETDOWN", "ENETRESET", "ENETUNREACH", "ENFILE", "ENOBUFS", "ENODATA",
        "ENODEV", "ENOENT", "ENOEXEC", "ENOLCK", "ENOLINK", "ENOMEM", "ENOMSG", "ENOPROTOOPT", "ENOSPC",
        "ENOSR", "ENOSTR", "ENOSYS", "ENOTCONN", "ENOTDIR", "ENOTEMPTY", "ENOTSOCK", "ENOTSUP", "ENOTTY",
        "ENXIO", "EOPNOTSUPP", "EOVERFLOW", "EPERM", "EPIPE", "EPROTO", "EPROTONOSUPPORT", "EPROTOTYPE",
        "ERANGE", "EROFS", "ESPIPE", "ESRCH", "ESTALE", "ETIME", "ETIMEDOUT", "ETXTBSY", "EWOULDBLOCK", "EXDEV",
    ];

    /// `[[name, number], ...]`: `os.constants.signals` of this platform.
    #[op]
    pub fn signals() -> Vec<(String, i32)> {
        lumen_os::signal::names().iter().map(|&(name, n)| (name.to_string(), n)).collect()
    }

    /// `[[name, number], ...]`: `os.constants.errno` of this platform.
    #[op]
    pub fn errno() -> Vec<(String, i32)> {
        NODE_ERRNO
            .iter()
            .filter_map(|&name| lumen_os::errno::errno_of_code(name).map(|n| (name.to_string(), n)))
            .collect()
    }

    /// `[[name, errno, description], ...]`: libuv's error map on this platform.
    #[op(name = "uvErrors")]
    pub fn uv_errors() -> Vec<(String, i32, String)> {
        lumen_os::uv::errors().map(|(name, n, desc)| (name.to_string(), n, desc.to_string())).collect()
    }

    /// `os.tmpdir()` from the realm's `TMPDIR`, `TMP`, `TEMP` and `SystemRoot` (or `windir`).
    #[op]
    pub fn tmpdir(tmpdir: Option<String>, tmp: Option<String>, temp: Option<String>, system_root: Option<String>) -> String {
        lumen_os::sysinfo::tmpdir_from(|name| match name {
            "TMPDIR" => tmpdir.clone(),
            "TMP" => tmp.clone(),
            "TEMP" => temp.clone(),
            "SystemRoot" => system_root.clone(),
            _ => None,
        })
    }
}
