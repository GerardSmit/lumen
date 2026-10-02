//! `posix`: the operating-system interface behind `os`, on the [`Platform`] layer (which maps to
//! `lumen_os` on a real host) and `lumen_os`'s constant tables.
//!
//! [`Platform`]: crate::platform::Platform

/// `PyObject_AsFileDescriptor`: an int, or the result of the object's `fileno()`.
pub fn as_file_descriptor(it: &mut crate::vm::Interp, v: &crate::object::Value) -> crate::object::R<i32> {
    let fd = if v.is_int_like() {
        it.index_of(v)?
    } else if it.get_attr_str(v, "fileno").is_ok() {
        let f = it.call_method(v, "fileno", Vec::new())?;
        if !f.is_int_like() {
            return Err(it.type_error("fileno() returned a non-integer"));
        }
        it.index_of(&f)?
    } else {
        return Err(it.type_error("argument must be an int, or have a fileno() method."));
    };
    if fd < 0 {
        return Err(it.value_error(&format!("file descriptor cannot be a negative integer ({fd})")));
    }
    i32::try_from(fd).map_err(|_| it.overflow_err("Python int too large to convert to C int"))
}

/// This module provides access to operating system functionality that is
/// standardized by the C Standard and the POSIX standard (a thinly
/// disguised Unix interface).  Refer to the library manual and
/// corresponding Unix manual entries for more information on calls.
#[lumen_bind::module(name = "posix")]
pub mod posix {
    use super::super::sysextra::{structseq_full, structseq_type};
    use crate::bind::{convert_path, fspath, wrap_path, Py, PathArg, PathOrFd, This};
    use crate::object::*;
    use crate::platform::{DirentKind, IoError, OsStat, ProcGroup, Timespec};
    use crate::pyint::BigInt;
    use crate::vm::{dict_set_str, Interp};

    struct StatResult;
    struct TerminalSize;
    struct UnameResult;
    struct TimesResult;

    const STAT_FIELDS: [&str; 22] = [
        "st_mode", "st_ino", "st_dev", "st_nlink", "st_uid", "st_gid", "st_size", "", "", "", "st_atime", "st_mtime",
        "st_ctime", "st_atime_ns", "st_mtime_ns", "st_ctime_ns", "st_blksize", "st_blocks", "st_rdev", "st_flags",
        "st_gen", "st_birthtime",
    ];

    fn stat_result_type(it: &mut Interp) -> Obj {
        structseq_type::<StatResult>(it, "os", "stat_result", &STAT_FIELDS, 10)
    }

    fn terminal_size_type(it: &mut Interp) -> Obj {
        structseq_type::<TerminalSize>(it, "os", "terminal_size", &["columns", "lines"], 2)
    }

    fn uname_result_type(it: &mut Interp) -> Obj {
        structseq_type::<UnameResult>(it, "posix", "uname_result", &["sysname", "nodename", "release", "version", "machine"], 5)
    }

    fn times_result_type(it: &mut Interp) -> Obj {
        let f = ["user", "system", "children_user", "children_system", "elapsed"];
        structseq_type::<TimesResult>(it, "posix", "times_result", &f, 5)
    }

    const O_WRONLY: i32 = 1;

    fn einval() -> i32 {
        lumen_os::errno::errno_of_code("EINVAL").unwrap_or(22)
    }

    fn uint(n: u64) -> Value {
        Value::big(BigInt::from(n))
    }

    fn int128(n: i128) -> Value {
        Value::big(BigInt::from(n))
    }

    fn secs(t: Timespec) -> f64 {
        t.sec as f64 + t.nsec as f64 * 1e-9
    }

    fn nanos(t: Timespec) -> Value {
        int128(t.sec as i128 * 1_000_000_000 + t.nsec as i128)
    }

    fn stat_value(it: &mut Interp, s: &OsStat) -> Value {
        let ty = stat_result_type(it);
        let vals = vec![
            Value::Int(s.mode as i64),
            uint(s.ino),
            uint(s.dev),
            uint(s.nlink),
            Value::Int(s.uid as i64),
            Value::Int(s.gid as i64),
            uint(s.size),
            Value::Int(s.atime.sec),
            Value::Int(s.mtime.sec),
            Value::Int(s.ctime.sec),
            Value::Float(secs(s.atime)),
            Value::Float(secs(s.mtime)),
            Value::Float(secs(s.ctime)),
            nanos(s.atime),
            nanos(s.mtime),
            nanos(s.ctime),
            uint(s.blksize),
            uint(s.blocks),
            uint(s.rdev),
            Value::Int(0),
            Value::Int(0),
            Value::Float(secs(s.birthtime)),
        ];
        structseq_full(&ty, vals)
    }

    fn os_err(it: &mut Interp, e: IoError) -> Obj {
        it.os_error_io(&e, None)
    }

    fn path_err<const FD: bool>(it: &mut Interp, e: IoError, p: &crate::bind::FsPath<FD>) -> Obj {
        it.os_error_io(&e, Some(&p.obj))
    }

    fn unavailable(it: &mut Interp, fname: &str, arg: &str) -> Obj {
        let cls = it.exc_type("NotImplementedError");
        it.new_exc(&cls, vec![Value::string(format!("{}: {} unavailable on this platform", fname, arg))])
    }

    fn no_dir_fd(it: &mut Interp, fname: &str, dir_fd: Option<i32>) -> R<()> {
        match dir_fd {
            Some(_) => Err(unavailable(it, fname, "dir_fd")),
            None => Ok(()),
        }
    }

    /// `PyUnicode_FSConverter`: a `str`, `bytes` or `os.PathLike` as text.
    fn fs_text(it: &mut Interp, v: &Value) -> R<String> {
        let r = fspath(it, v)?;
        let s = match &r {
            Value::Obj(o) => match &o.kind {
                Kind::Str(s) => s.s.to_string(),
                Kind::Bytes(b) => crate::bind::path::bytes_path(b),
                _ => unreachable!("fspath returns str or bytes"),
            },
            _ => unreachable!("fspath returns str or bytes"),
        };
        if s.contains('\0') {
            return Err(it.value_error("embedded null byte"));
        }
        Ok(s)
    }

    // ---- stat ----------------------------------------------------------------------------------

    /// Perform a stat system call on the given path.
    #[op]
    fn stat(
        it: &mut Interp,
        #[kw] path: PathOrFd,
        #[kwonly] dir_fd: Option<i32>,
        #[kwonly]
        #[default(true)]
        follow_symlinks: bool,
    ) -> R<Value> {
        no_dir_fd(it, "stat", dir_fd)?;
        let r = match path.fd {
            Some(fd) => it.platform.borrow_mut().fd_stat(fd),
            None => it.platform.borrow_mut().stat(&path.path, follow_symlinks),
        };
        match r {
            Ok(s) => Ok(stat_value(it, &s)),
            Err(e) => Err(path_err(it, e, &path)),
        }
    }

    /// Perform a stat system call on the given path, without following symbolic links.
    #[op]
    fn lstat(it: &mut Interp, #[kw] path: PathArg, #[kwonly] dir_fd: Option<i32>) -> R<Value> {
        no_dir_fd(it, "lstat", dir_fd)?;
        let r = it.platform.borrow_mut().stat(&path.path, false);
        match r {
            Ok(s) => Ok(stat_value(it, &s)),
            Err(e) => Err(path_err(it, e, &path)),
        }
    }

    /// Perform a stat system call on the given file descriptor.
    #[op]
    fn fstat(it: &mut Interp, #[kw] fd: i32) -> R<Value> {
        let r = it.platform.borrow_mut().fd_stat(fd);
        match r {
            Ok(s) => Ok(stat_value(it, &s)),
            Err(e) => Err(os_err(it, e)),
        }
    }

    /// Use the real uid/gid to test for access to a path.
    #[op]
    fn access(
        it: &mut Interp,
        #[kw] path: PathArg,
        #[kw] mode: i32,
        #[kwonly] dir_fd: Option<i32>,
        #[kwonly]
        #[default(false)]
        effective_ids: bool,
        #[kwonly]
        #[default(true)]
        follow_symlinks: bool,
    ) -> R<bool> {
        no_dir_fd(it, "access", dir_fd)?;
        if effective_ids {
            return Err(unavailable(it, "access", "effective_ids"));
        }
        if !follow_symlinks {
            return Err(unavailable(it, "access", "follow_symlinks"));
        }
        Ok(it.platform.borrow_mut().access(&path.path, mode as u32).is_ok())
    }

    // ---- directories ---------------------------------------------------------------------------

    /// Change the current working directory to the specified path.
    #[op]
    fn chdir(it: &mut Interp, #[kw] path: PathArg) -> R<()> {
        let r = it.platform.borrow_mut().chdir(&path.path);
        r.map_err(|e| path_err(it, e, &path))
    }

    /// Return a unicode string representing the current working directory.
    #[op]
    fn getcwd(it: &mut Interp) -> R<Value> {
        let r = it.platform.borrow_mut().getcwd();
        r.map(Value::string).map_err(|e| os_err(it, e))
    }

    /// Return a bytes string representing the current working directory.
    #[op]
    fn getcwdb(it: &mut Interp) -> R<Value> {
        let r = it.platform.borrow_mut().getcwd();
        r.map(|s| wrap_path(true, s)).map_err(|e| os_err(it, e))
    }

    fn list_dir(it: &mut Interp, fname: &str, path: &Value) -> R<(PathOrFd, Vec<(String, DirentKind)>)> {
        let p: PathOrFd = convert_path(it, fname, "path", path, true, true)?;
        if p.fd.is_some() {
            return Err(unavailable(it, fname, "path should not be an integer;"));
        }
        let r = it.platform.borrow_mut().listdir(&p.path);
        match r {
            Ok(v) => Ok((p, v)),
            Err(e) => Err(path_err(it, e, &p)),
        }
    }

    /// Return a list containing the names of the files in the directory.
    #[op]
    fn listdir(it: &mut Interp, #[kw] path: Option<&Value>) -> R<Value> {
        let (p, entries) = list_dir(it, "listdir", path.unwrap_or(&Value::None))?;
        Ok(Value::list(entries.into_iter().map(|(n, _)| p.wrap(n)).collect()))
    }

    /// Return an iterator of DirEntry objects for given path.
    #[op]
    fn scandir(it: &mut Interp, #[kw] path: Option<&Value>) -> R<Py<ScandirIterator>> {
        let (p, entries) = list_dir(it, "scandir", path.unwrap_or(&Value::None))?;
        let dir = if matches!(p.obj, Value::None) { ".".to_string() } else { p.path.clone() };
        let it_state = ScandirIterator { dir, bytes: p.bytes, entries: entries.into_iter(), closed: false };
        Ok(Py::new(it, it_state))
    }

    #[class(name = "ScandirIterator", skip(py))]
    pub struct ScandirIterator {
        dir: String,
        bytes: bool,
        entries: std::vec::IntoIter<(String, DirentKind)>,
        closed: bool,
    }

    #[methods]
    impl ScandirIterator {
        #[proto(iter)]
        fn __iter__(slf: This<Py<Self>>) -> Py<Self> {
            slf.0
        }

        #[proto(next)]
        fn __next__(slf: This<Py<Self>>, it: &mut Interp) -> R<Option<Value>> {
            let next = {
                let mut s = slf.0.borrow_mut(it)?;
                if s.closed { None } else { s.entries.next().map(|e| (e, s.dir.clone(), s.bytes)) }
            };
            let Some(((name, kind), dir, bytes)) = next else {
                slf.0.borrow_mut(it)?.closed = true;
                return Ok(None);
            };
            let path = if dir.ends_with('/') { format!("{}{}", dir, name) } else { format!("{}/{}", dir, name) };
            let entry = DirEntry {
                name: wrap_path(bytes, name),
                path: wrap_path(bytes, path.clone()),
                text: path,
                kind,
                stat: None,
                lstat: None,
            };
            Ok(Some(Py::new(it, entry).into_value()))
        }

        fn close(&mut self) {
            self.closed = true;
            self.entries = Vec::new().into_iter();
        }

        #[proto(enter)]
        fn __enter__(slf: This<Py<Self>>) -> Py<Self> {
            slf.0
        }

        #[proto(exit)]
        fn __exit__(&mut self, #[varargs] _args: &[Value]) -> bool {
            self.close();
            false
        }
    }

    #[class(name = "DirEntry", generic)]
    pub struct DirEntry {
        name: Value,
        path: Value,
        text: String,
        kind: DirentKind,
        stat: Option<Value>,
        lstat: Option<Value>,
    }

    impl DirEntry {
        fn fetch(slf: &Py<Self>, it: &mut Interp, follow: bool) -> R<Value> {
            let (cached, text, is_link, path) = {
                let s = slf.borrow(it)?;
                let follow = follow && matches!(s.kind, DirentKind::Link | DirentKind::Unknown);
                (if follow { s.stat.clone() } else { s.lstat.clone() }, s.text.clone(), follow, s.path.clone())
            };
            if let Some(v) = cached {
                return Ok(v);
            }
            let r = it.platform.borrow_mut().stat(&text, is_link);
            let st = match r {
                Ok(st) => stat_value(it, &st),
                Err(e) => return Err(it.os_error_io(&e, Some(&path))),
            };
            let mut s = slf.borrow_mut(it)?;
            if is_link { s.stat = Some(st.clone()) } else { s.lstat = Some(st.clone()) }
            Ok(st)
        }

        fn mode(slf: &Py<Self>, it: &mut Interp, follow: bool) -> R<Option<u32>> {
            match Self::fetch(slf, it, follow) {
                Ok(st) => Ok(st.tuple_items().and_then(|t| t[0].as_i64()).map(|m| m as u32)),
                Err(e) if it.exc_is(&e, "FileNotFoundError") => Ok(None),
                Err(e) => Err(e),
            }
        }

        fn is_type(slf: &Py<Self>, it: &mut Interp, follow: bool, want: DirentKind, fmt: u32) -> R<bool> {
            let kind = slf.borrow(it)?.kind;
            if kind != DirentKind::Unknown && !(follow && kind == DirentKind::Link) {
                return Ok(kind == want);
            }
            Ok(Self::mode(slf, it, follow)?.is_some_and(|m| m & lumen_os::fs::S_IFMT == fmt))
        }
    }

    #[methods]
    impl DirEntry {
        #[getter]
        fn name(&self) -> Value {
            self.name.clone()
        }

        #[getter]
        fn path(&self) -> Value {
            self.path.clone()
        }

        /// Return True if the entry is a directory; cached per entry.
        fn is_dir(slf: This<Py<Self>>, it: &mut Interp, #[kwonly] #[default(true)] follow_symlinks: bool) -> R<bool> {
            Self::is_type(&slf.0, it, follow_symlinks, DirentKind::Dir, lumen_os::fs::S_IFDIR)
        }

        /// Return True if the entry is a file; cached per entry.
        fn is_file(slf: This<Py<Self>>, it: &mut Interp, #[kwonly] #[default(true)] follow_symlinks: bool) -> R<bool> {
            Self::is_type(&slf.0, it, follow_symlinks, DirentKind::File, lumen_os::fs::S_IFREG)
        }

        /// Return True if the entry is a symbolic link; cached per entry.
        fn is_symlink(slf: This<Py<Self>>, it: &mut Interp) -> R<bool> {
            Self::is_type(&slf.0, it, false, DirentKind::Link, lumen_os::fs::S_IFLNK)
        }

        /// Return True if the entry is a junction; cached per entry.
        fn is_junction(&self) -> bool {
            false
        }

        /// Return stat_result object for the entry; cached per entry.
        fn stat(slf: This<Py<Self>>, it: &mut Interp, #[kwonly] #[default(true)] follow_symlinks: bool) -> R<Value> {
            Self::fetch(&slf.0, it, follow_symlinks)
        }

        /// Return inode of the entry; cached per entry.
        fn inode(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
            let st = Self::fetch(&slf.0, it, false)?;
            Ok(st.tuple_items().map_or(Value::Int(0), |t| t[1].clone()))
        }

        /// Returns the path for the entry.
        fn __fspath__(&self) -> Value {
            self.path.clone()
        }

        #[proto(repr)]
        fn __repr__(slf: This<Py<Self>>, it: &mut Interp) -> R<String> {
            let name = slf.0.borrow(it)?.name.clone();
            Ok(format!("<DirEntry {}>", it.repr_of(&name)?))
        }
    }

    /// Create a directory.
    #[op]
    fn mkdir(
        it: &mut Interp,
        #[kw] path: PathArg,
        #[kw]
        #[default(511)]
        mode: i32,
        #[kwonly] dir_fd: Option<i32>,
    ) -> R<()> {
        no_dir_fd(it, "mkdir", dir_fd)?;
        let r = it.platform.borrow_mut().mkdir(&path.path, mode as u32);
        r.map_err(|e| path_err(it, e, &path))
    }

    /// Remove a directory.
    #[op]
    fn rmdir(it: &mut Interp, #[kw] path: PathArg, #[kwonly] dir_fd: Option<i32>) -> R<()> {
        no_dir_fd(it, "rmdir", dir_fd)?;
        let r = it.platform.borrow_mut().rmdir(&path.path);
        r.map_err(|e| path_err(it, e, &path))
    }

    /// Remove a file (same as remove()).
    #[op]
    fn unlink(it: &mut Interp, #[kw] path: PathArg, #[kwonly] dir_fd: Option<i32>) -> R<()> {
        no_dir_fd(it, "unlink", dir_fd)?;
        let r = it.platform.borrow_mut().unlink(&path.path);
        r.map_err(|e| path_err(it, e, &path))
    }

    /// Remove a file (same as unlink()).
    #[op]
    fn remove(it: &mut Interp, #[kw] path: PathArg, #[kwonly] dir_fd: Option<i32>) -> R<()> {
        no_dir_fd(it, "remove", dir_fd)?;
        let r = it.platform.borrow_mut().unlink(&path.path);
        r.map_err(|e| path_err(it, e, &path))
    }

    fn do_rename(it: &mut Interp, fname: &str, src: &PathArg, dst: &PathArg, src_dir_fd: Option<i32>, dst_dir_fd: Option<i32>) -> R<()> {
        if src_dir_fd.is_some() {
            return Err(unavailable(it, fname, "src_dir_fd"));
        }
        if dst_dir_fd.is_some() {
            return Err(unavailable(it, fname, "dst_dir_fd"));
        }
        if src.bytes != dst.bytes {
            return Err(it.type_error(&format!("{}: src and dst must be the same type", fname)));
        }
        let r = it.platform.borrow_mut().rename(&src.path, &dst.path);
        r.map_err(|e| it.os_error_errno(e.errno, Some(&src.obj), Some(&dst.obj)))
    }

    /// Rename a file or directory.
    #[op]
    fn rename(
        it: &mut Interp,
        #[kw] src: PathArg,
        #[kw] dst: PathArg,
        #[kwonly] src_dir_fd: Option<i32>,
        #[kwonly] dst_dir_fd: Option<i32>,
    ) -> R<()> {
        do_rename(it, "rename", &src, &dst, src_dir_fd, dst_dir_fd)
    }

    /// Rename a file or directory, overwriting the destination.
    #[op]
    fn replace(
        it: &mut Interp,
        #[kw] src: PathArg,
        #[kw] dst: PathArg,
        #[kwonly] src_dir_fd: Option<i32>,
        #[kwonly] dst_dir_fd: Option<i32>,
    ) -> R<()> {
        do_rename(it, "replace", &src, &dst, src_dir_fd, dst_dir_fd)
    }

    /// Create a hard link to a file.
    #[op]
    fn link(
        it: &mut Interp,
        #[kw] src: PathArg,
        #[kw] dst: PathArg,
        #[kwonly] src_dir_fd: Option<i32>,
        #[kwonly] dst_dir_fd: Option<i32>,
        #[kwonly]
        #[default(true)]
        follow_symlinks: bool,
    ) -> R<()> {
        if src_dir_fd.is_some() || dst_dir_fd.is_some() {
            return Err(unavailable(it, "link", "src_dir_fd and dst_dir_fd"));
        }
        let _ = follow_symlinks;
        if src.bytes != dst.bytes {
            return Err(it.type_error("link: src and dst must be the same type"));
        }
        let r = it.platform.borrow_mut().link(&src.path, &dst.path);
        r.map_err(|e| it.os_error_errno(e.errno, Some(&src.obj), Some(&dst.obj)))
    }

    /// Create a symbolic link pointing to src named dst.
    #[op]
    fn symlink(
        it: &mut Interp,
        #[kw] src: PathArg,
        #[kw] dst: PathArg,
        #[kw]
        #[default(false)]
        target_is_directory: bool,
        #[kwonly] dir_fd: Option<i32>,
    ) -> R<()> {
        no_dir_fd(it, "symlink", dir_fd)?;
        if src.bytes != dst.bytes {
            return Err(it.type_error("symlink: src and dst must be the same type"));
        }
        let r = it.platform.borrow_mut().symlink(&src.path, &dst.path, target_is_directory);
        r.map_err(|e| it.os_error_errno(e.errno, Some(&src.obj), Some(&dst.obj)))
    }

    /// Return a string representing the path to which the symbolic link points.
    #[op]
    fn readlink(it: &mut Interp, #[kw] path: PathArg, #[kwonly] dir_fd: Option<i32>) -> R<Value> {
        no_dir_fd(it, "readlink", dir_fd)?;
        let r = it.platform.borrow_mut().readlink(&path.path);
        r.map(|s| path.wrap(s)).map_err(|e| path_err(it, e, &path))
    }

    /// Change the access permissions of a file.
    #[op]
    fn chmod(
        it: &mut Interp,
        #[kw] path: PathOrFd,
        #[kw] mode: i32,
        #[kwonly] dir_fd: Option<i32>,
        #[kwonly]
        #[default(true)]
        follow_symlinks: bool,
    ) -> R<()> {
        no_dir_fd(it, "chmod", dir_fd)?;
        if !follow_symlinks {
            return Err(unavailable(it, "chmod", "follow_symlinks"));
        }
        let r = match path.fd {
            Some(fd) => it.platform.borrow_mut().fd_chmod(fd, mode as u32),
            None => it.platform.borrow_mut().chmod(&path.path, mode as u32),
        };
        r.map_err(|e| path_err(it, e, &path))
    }

    /// Change the access permissions of a file.
    #[op]
    fn fchmod(it: &mut Interp, #[kw] fd: i32, #[kw] mode: i32) -> R<()> {
        let r = it.platform.borrow_mut().fd_chmod(fd, mode as u32);
        r.map_err(|e| os_err(it, e))
    }

    /// Change the owner and group id of path to the numeric uid and gid.
    #[op]
    fn chown(
        it: &mut Interp,
        #[kw] path: PathArg,
        #[kw] uid: i64,
        #[kw] gid: i64,
        #[kwonly] dir_fd: Option<i32>,
        #[kwonly]
        #[default(true)]
        follow_symlinks: bool,
    ) -> R<()> {
        no_dir_fd(it, "chown", dir_fd)?;
        let r = it.platform.borrow_mut().chown(&path.path, uid as u32, gid as u32, follow_symlinks);
        r.map_err(|e| path_err(it, e, &path))
    }

    /// Change the owner and group id of path to the numeric uid and gid, without following links.
    #[op]
    fn lchown(it: &mut Interp, #[kw] path: PathArg, #[kw] uid: i64, #[kw] gid: i64) -> R<()> {
        let r = it.platform.borrow_mut().chown(&path.path, uid as u32, gid as u32, false);
        r.map_err(|e| path_err(it, e, &path))
    }

    fn timespec_of(it: &mut Interp, v: &Value) -> R<Timespec> {
        match v {
            Value::Float(f) => {
                if !f.is_finite() {
                    return Err(it.value_error("Invalid value NaN (not a number)"));
                }
                let sec = f.floor();
                let mut nsec = ((f - sec) * 1e9).floor();
                let mut sec = sec as i64;
                if nsec >= 1e9 {
                    sec += 1;
                    nsec -= 1e9;
                }
                Ok(Timespec { sec, nsec: nsec as u32 })
            }
            _ => Ok(Timespec { sec: it.index_of(v)?, nsec: 0 }),
        }
    }

    fn ns_timespec(it: &mut Interp, v: &Value) -> R<Timespec> {
        let n = it.index_of(v)?;
        Ok(Timespec { sec: n.div_euclid(1_000_000_000), nsec: n.rem_euclid(1_000_000_000) as u32 })
    }

    /// Set the access and modified time of path.
    #[op]
    fn utime(
        it: &mut Interp,
        #[kw] path: PathOrFd,
        #[kw] times: Option<&Value>,
        #[kwonly] ns: Option<&Value>,
        #[kwonly] dir_fd: Option<i32>,
        #[kwonly]
        #[default(true)]
        follow_symlinks: bool,
    ) -> R<()> {
        no_dir_fd(it, "utime", dir_fd)?;
        let (atime, mtime) = match (times, ns) {
            (Some(_), Some(_)) => return Err(it.value_error("utime: you may specify either 'times' or 'ns' but not both")),
            (Some(t), None) => match t.tuple_items() {
                Some([a, m]) if a.is_int_like() || matches!(a, Value::Float(_)) => {
                    let (a, m) = (a.clone(), m.clone());
                    (timespec_of(it, &a)?, timespec_of(it, &m)?)
                }
                _ => return Err(it.type_error("utime: 'times' must be either a tuple of two ints or None")),
            },
            (None, Some(n)) => match n.tuple_items() {
                Some([a, m]) => {
                    let (a, m) = (a.clone(), m.clone());
                    (ns_timespec(it, &a)?, ns_timespec(it, &m)?)
                }
                _ => return Err(it.type_error("utime: 'ns' must be a tuple of two ints")),
            },
            (None, None) => {
                let now = it.platform.borrow().wall_time_ns() as i64;
                let t = Timespec { sec: now / 1_000_000_000, nsec: (now % 1_000_000_000) as u32 };
                (t, t)
            }
        };
        let r = match path.fd {
            Some(fd) => it.platform.borrow_mut().fd_utimes(fd, atime, mtime),
            None => it.platform.borrow_mut().utimes(&path.path, atime, mtime, follow_symlinks),
        };
        r.map_err(|e| path_err(it, e, &path))
    }

    /// Truncate a file, specified by path, to a specific length.
    #[op]
    fn truncate(it: &mut Interp, #[kw] path: PathOrFd, #[kw] length: i64) -> R<()> {
        let r = match path.fd {
            Some(fd) => it.platform.borrow_mut().fd_truncate(fd, length as u64),
            None => {
                let mut p = it.platform.borrow_mut();
                p.fd_open(&path.path, O_WRONLY, 0).and_then(|fd| {
                    let r = p.fd_truncate(fd, length as u64);
                    let _ = p.fd_close(fd);
                    r
                })
            }
        };
        r.map_err(|e| path_err(it, e, &path))
    }

    // ---- file descriptors ----------------------------------------------------------------------

    /// Open a file for low level IO.  Returns a file descriptor (integer).
    #[op]
    fn open(
        it: &mut Interp,
        #[kw] path: PathArg,
        #[kw] flags: i32,
        #[kw]
        #[default(511)]
        mode: i32,
        #[kwonly] dir_fd: Option<i32>,
    ) -> R<i32> {
        no_dir_fd(it, "open", dir_fd)?;
        let r = it.platform.borrow_mut().fd_open(&path.path, flags, mode as u32);
        r.map_err(|e| path_err(it, e, &path))
    }

    /// Close a file descriptor.
    #[op]
    fn close(it: &mut Interp, #[kw] fd: i32) -> R<()> {
        let r = it.platform.borrow_mut().fd_close(fd);
        r.map_err(|e| os_err(it, e))
    }

    /// Closes all file descriptors in [fd_low, fd_high), ignoring errors.
    #[op]
    fn closerange(it: &mut Interp, fd_low: i32, fd_high: i32) {
        let mut p = it.platform.borrow_mut();
        for fd in fd_low.max(0)..fd_high.min(65536) {
            let _ = p.fd_close(fd);
        }
    }

    /// Return a duplicate of a file descriptor.
    #[op]
    fn dup(it: &mut Interp, fd: i32) -> R<i32> {
        let r = it.platform.borrow_mut().fd_dup(fd);
        r.map_err(|e| os_err(it, e))
    }

    /// Duplicate file descriptor.
    #[op]
    fn dup2(it: &mut Interp, #[kw] fd: i32, #[kw] fd2: i32, #[kw] #[default(true)] inheritable: bool) -> R<i32> {
        let r = it.platform.borrow_mut().fd_dup2(fd, fd2, inheritable);
        r.map_err(|e| os_err(it, e))
    }

    /// Read from a file descriptor.  Returns a bytes object.
    #[op]
    fn read(it: &mut Interp, fd: i32, length: isize) -> R<Value> {
        if length < 0 {
            return Err(it.os_error_errno(einval(), None, None));
        }
        it.wait_fd(fd, lumen_os::poll::POLLIN)?;
        let mut buf = vec![0u8; length as usize];
        let r = it.platform.borrow_mut().fd_read(fd, &mut buf, None);
        let n = r.map_err(|e| os_err(it, e))?;
        buf.truncate(n);
        Ok(Value::bytes(buf))
    }

    /// Read a number of bytes from a file descriptor starting at a particular offset.
    #[op]
    fn pread(it: &mut Interp, fd: i32, length: isize, offset: i64) -> R<Value> {
        if length < 0 {
            return Err(it.os_error_errno(einval(), None, None));
        }
        let mut buf = vec![0u8; length as usize];
        let r = it.platform.borrow_mut().fd_read(fd, &mut buf, Some(offset as u64));
        let n = r.map_err(|e| os_err(it, e))?;
        buf.truncate(n);
        Ok(Value::bytes(buf))
    }

    /// Write a bytes object to a file descriptor.
    #[op]
    fn write(it: &mut Interp, fd: i32, data: &[u8]) -> R<usize> {
        let r = it.fd_write(fd, data);
        r.map_err(|e| os_err(it, e))
    }

    /// Write bytes to a file descriptor starting at a particular offset.
    #[op]
    fn pwrite(it: &mut Interp, fd: i32, buffer: &[u8], offset: i64) -> R<usize> {
        let r = it.platform.borrow_mut().fd_write(fd, buffer, Some(offset as u64));
        r.map_err(|e| os_err(it, e))
    }

    /// Set the position of a file descriptor.  Return the new position.
    #[op]
    fn lseek(it: &mut Interp, fd: i32, position: i64, whence: i32) -> R<u64> {
        let r = it.platform.borrow_mut().fd_seek(fd, position, whence);
        r.map_err(|e| os_err(it, e))
    }

    /// Force write of fd to disk.
    #[op]
    fn fsync(it: &mut Interp, #[kw] fd: &Value) -> R<()> {
        let fd = super::as_file_descriptor(it, fd)?;
        let r = it.platform.borrow_mut().fd_sync(fd, false);
        r.map_err(|e| os_err(it, e))
    }

    /// Truncate a file, specified by file descriptor, to a specific length.
    #[op]
    fn ftruncate(it: &mut Interp, fd: i32, length: i64) -> R<()> {
        let r = it.platform.borrow_mut().fd_truncate(fd, length as u64);
        r.map_err(|e| os_err(it, e))
    }

    /// Create a pipe.
    #[op]
    fn pipe(it: &mut Interp) -> R<(i32, i32)> {
        let r = it.platform.borrow_mut().fd_pipe();
        r.map_err(|e| os_err(it, e))
    }

    /// Return True if the fd is connected to a terminal.
    #[op]
    fn isatty(it: &mut Interp, fd: i32) -> bool {
        it.platform.borrow_mut().fd_isatty(fd)
    }

    /// Get the close-on-exe flag of the specified file descriptor.
    #[op]
    fn get_inheritable(it: &mut Interp, fd: i32) -> R<bool> {
        let r = it.platform.borrow_mut().fd_get_inheritable(fd);
        r.map_err(|e| os_err(it, e))
    }

    /// Set the inheritable flag of the specified file descriptor.
    #[op]
    fn set_inheritable(it: &mut Interp, fd: i32, inheritable: i32) -> R<()> {
        let r = it.platform.borrow_mut().fd_set_inheritable(fd, inheritable != 0);
        r.map_err(|e| os_err(it, e))
    }

    /// Get the blocking mode of the file descriptor.
    #[op]
    fn get_blocking(it: &mut Interp, fd: i32) -> R<bool> {
        let r = it.platform.borrow_mut().fd_get_blocking(fd);
        r.map_err(|e| os_err(it, e))
    }

    /// Set the blocking mode of the specified file descriptor.
    #[op]
    fn set_blocking(it: &mut Interp, fd: i32, blocking: bool) -> R<()> {
        let r = it.platform.borrow_mut().fd_set_blocking(fd, blocking);
        r.map_err(|e| os_err(it, e))
    }

    /// Return a string describing the encoding of a terminal's file descriptor.
    #[op]
    fn device_encoding(it: &mut Interp, #[kw] fd: i32) -> Value {
        if it.platform.borrow_mut().fd_isatty(fd) { Value::str("UTF-8") } else { Value::None }
    }

    /// Return the size of the terminal window as (columns, lines).
    #[op]
    fn get_terminal_size(it: &mut Interp, #[default(1)] fd: i32) -> R<Value> {
        let r = it.platform.borrow_mut().terminal_size(fd);
        let (cols, lines) = r.map_err(|e| os_err(it, e))?;
        let ty = terminal_size_type(it);
        Ok(structseq_full(&ty, vec![Value::Int(cols as i64), Value::Int(lines as i64)]))
    }

    // ---- process -------------------------------------------------------------------------------

    /// Set the current numeric umask and return the previous umask.
    #[op]
    fn umask(it: &mut Interp, mask: i32) -> i64 {
        it.platform.borrow_mut().umask(mask as u32) as i64
    }

    /// Return an object identifying the current operating system.
    #[op]
    fn uname(it: &mut Interp) -> Value {
        let u = it.platform.borrow().uname();
        let ty = uname_result_type(it);
        structseq_full(&ty, u.into_iter().map(Value::string).collect())
    }

    /// Return a collection containing process timing information.
    #[op]
    fn times(it: &mut Interp) -> R<Value> {
        let r = it.platform.borrow().process_times();
        let t = r.map_err(|e| os_err(it, e))?;
        let ty = times_result_type(it);
        Ok(structseq_full(&ty, t.into_iter().map(Value::Float).collect()))
    }

    /// Return a bytes object containing random bytes suitable for cryptographic use.
    #[op]
    fn urandom(it: &mut Interp, size: isize) -> R<Value> {
        if size < 0 {
            return Err(it.value_error("negative argument not allowed"));
        }
        let mut buf = vec![0u8; size as usize];
        it.platform.borrow_mut().entropy(&mut buf);
        Ok(Value::bytes(buf))
    }

    /// Return the current process id.
    #[op]
    fn getpid(it: &mut Interp) -> u32 {
        it.platform.borrow().process_id()
    }

    /// Return the parent's process id.
    #[op]
    fn getppid(it: &mut Interp) -> u32 {
        it.platform.borrow().parent_process_id()
    }

    /// Return the current process's user id.
    #[op]
    fn getuid(it: &mut Interp) -> u32 {
        it.platform.borrow().user_ids()[0]
    }

    /// Return the current process's effective user id.
    #[op]
    fn geteuid(it: &mut Interp) -> u32 {
        it.platform.borrow().user_ids()[1]
    }

    /// Return the current process's group id.
    #[op]
    fn getgid(it: &mut Interp) -> u32 {
        it.platform.borrow().user_ids()[2]
    }

    /// Return the current process's effective group id.
    #[op]
    fn getegid(it: &mut Interp) -> u32 {
        it.platform.borrow().user_ids()[3]
    }

    /// Return the actual login name.
    #[op]
    fn getlogin(it: &mut Interp) -> R<String> {
        let r = it.platform.borrow_mut().getlogin();
        r.map_err(|e| os_err(it, e))
    }

    /// Return list of supplemental group IDs for the process.
    #[op]
    fn getgroups(it: &mut Interp) -> R<Value> {
        let r = it.platform.borrow_mut().getgroups();
        let g = r.map_err(|e| os_err(it, e))?;
        Ok(Value::list(g.into_iter().map(|g| Value::Int(g as i64)).collect()))
    }

    fn group(it: &mut Interp, call: ProcGroup) -> R<i32> {
        let r = it.platform.borrow_mut().process_group(call);
        r.map_err(|e| os_err(it, e))
    }

    /// Return the current process group id.
    #[op]
    fn getpgrp(it: &mut Interp) -> R<i32> {
        group(it, ProcGroup::GetPgrp)
    }

    /// Call the system call getpgid(), and return the result.
    #[op]
    fn getpgid(it: &mut Interp, #[kw] pid: i32) -> R<i32> {
        group(it, ProcGroup::GetPgid(pid))
    }

    /// Call the system call getsid(pid) and return the result.
    #[op]
    fn getsid(it: &mut Interp, pid: i32) -> R<i32> {
        group(it, ProcGroup::GetSid(pid))
    }

    /// Call the system call setsid().
    #[op]
    fn setsid(it: &mut Interp) -> R<()> {
        group(it, ProcGroup::SetSid).map(|_| ())
    }

    /// Call the system call setpgid(pid, pgrp).
    #[op]
    fn setpgid(it: &mut Interp, pid: i32, pgrp: i32) -> R<()> {
        group(it, ProcGroup::SetPgid(pid, pgrp)).map(|_| ())
    }

    /// Kill a process with a signal.
    #[op]
    fn kill(it: &mut Interp, pid: i32, signal: i32) -> R<()> {
        let r = it.platform.borrow_mut().kill(pid, signal);
        r.map_err(|e| os_err(it, e))?;
        crate::builtins::signalm::check(it)
    }

    /// Kill a process group with a signal.
    #[op]
    fn killpg(it: &mut Interp, pgid: i32, signal: i32) -> R<()> {
        let r = it.platform.borrow_mut().kill(-pgid, signal);
        r.map_err(|e| os_err(it, e))
    }

    /// Translate an error code to a message string.
    #[op]
    fn strerror(code: i32) -> String {
        lumen_os::errno::strerror(code)
    }

    /// Return the number of logical CPUs in the system.
    #[op]
    fn cpu_count(it: &mut Interp) -> Option<usize> {
        let n = it.platform.borrow().cpu_count();
        (n > 0).then_some(n)
    }

    /// Return an integer-valued system configuration variable.
    #[op]
    fn sysconf(it: &mut Interp, name: &Value) -> R<i64> {
        let n = if let Some(s) = name.as_str() {
            match lumen_os::consts::sysconf_names().find(|(k, _)| *k == s) {
                Some((_, v)) => v as i32,
                None => return Err(it.value_error("unrecognized configuration name")),
            }
        } else if name.is_int_like() {
            it.index_of(name)? as i32
        } else {
            return Err(it.type_error("configuration names must be strings or integers"));
        };
        let r = it.platform.borrow().sysconf(n);
        r.map_err(|e| os_err(it, e))
    }

    fn env_name(it: &mut Interp, v: &Value) -> R<String> {
        let s = fs_text(it, v)?;
        if s.is_empty() || s.contains('=') {
            return Err(it.value_error("illegal environment variable name"));
        }
        Ok(s)
    }

    /// Change or add an environment variable.
    #[op]
    fn putenv(it: &mut Interp, name: &Value, value: &Value) -> R<()> {
        let name = env_name(it, name)?;
        let value = fs_text(it, value)?;
        let r = it.platform.borrow_mut().setenv(&name, &value);
        r.map_err(|e| os_err(it, e))
    }

    /// Delete an environment variable.
    #[op]
    fn unsetenv(it: &mut Interp, name: &Value) -> R<()> {
        let name = env_name(it, name)?;
        let r = it.platform.borrow_mut().unsetenv(&name);
        r.map_err(|e| os_err(it, e))
    }

    /// Wait for completion of a given child process.
    #[op]
    fn waitpid(it: &mut Interp, pid: i32, options: i32) -> R<(i32, i32)> {
        let r = it.waitpid_blocking(pid, options)?;
        r.map_err(|e| os_err(it, e))
    }

    /// Wait for completion of a child process.
    #[op]
    fn wait(it: &mut Interp) -> R<(i32, i32)> {
        let r = it.waitpid_blocking(-1, 0)?;
        r.map_err(|e| os_err(it, e))
    }

    struct WaitidResult;

    fn fs_err(it: &mut Interp, e: lumen_os::FsError) -> Obj {
        it.os_error_errno(e.errno(), None, None)
    }

    #[derive(Default)]
    pub struct ForkHooks {
        before: Vec<Value>,
        parent: Vec<Value>,
        child: Vec<Value>,
    }

    fn run_fork_hooks(it: &mut Interp, hooks: Vec<Value>) {
        for hook in hooks {
            if let Err(e) = it.call(&hook, Vec::new(), Vec::new()) {
                it.flush_out();
                let repr = it.repr_of(&hook).unwrap_or_default();
                it.write_stderr(&format!("Exception ignored in: {repr}\n"));
                let text = it.format_exception(&e);
                it.write_stderr(&text);
            }
        }
    }

    pub(crate) fn before_fork(it: &mut Interp) {
        it.flush_out();
        let hooks: Vec<Value> = it.native_state::<ForkHooks>().before.iter().rev().cloned().collect();
        run_fork_hooks(it, hooks);
    }

    pub(crate) fn after_fork(it: &mut Interp, in_child: bool) {
        let state = it.native_state::<ForkHooks>();
        let hooks = if in_child { state.child.clone() } else { state.parent.clone() };
        run_fork_hooks(it, hooks);
    }

    /// Runs `spawn` (a fork) between the registered fork hooks; `pid_of` extracts the child
    /// indicator from its result.
    fn forked<T>(it: &mut Interp, spawn: impl FnOnce() -> Result<T, lumen_os::FsError>, pid_of: impl Fn(&T) -> i32) -> R<T> {
        before_fork(it);
        match spawn() {
            Ok(r) => {
                after_fork(it, pid_of(&r) == 0);
                Ok(r)
            }
            Err(e) => {
                after_fork(it, false);
                Err(fs_err(it, e))
            }
        }
    }

    /// Fork a child process.
    ///
    /// Return 0 to child process and PID of child to parent process.
    #[op]
    fn fork(it: &mut Interp) -> R<i32> {
        forked(it, lumen_os::proc::fork, |&pid| pid)
    }

    /// Register callables to be called when forking a new process.
    ///
    ///   before
    ///     A callable to be called in the parent before the fork() syscall.
    ///   after_in_child
    ///     A callable to be called in the child after fork().
    ///   after_in_parent
    ///     A callable to be called in the parent after fork().
    ///
    /// 'before' callbacks are called in reverse order.
    /// 'after_in_child' and 'after_in_parent' callbacks are called in order.
    #[op]
    fn register_at_fork(
        it: &mut Interp,
        #[kwonly] before: Option<&Value>,
        #[kwonly] after_in_child: Option<&Value>,
        #[kwonly] after_in_parent: Option<&Value>,
    ) -> R<()> {
        let given = [("before", before), ("after_in_child", after_in_child), ("after_in_parent", after_in_parent)];
        if given.iter().all(|(_, v)| v.is_none()) {
            return Err(it.type_error("At least one argument is required."));
        }
        for (name, v) in &given {
            if let Some(v) = v {
                if !it.is_callable(v) {
                    let t = it.type_name_of(v);
                    return Err(it.type_error(&format!("'{name}' must be callable, not {t}")));
                }
            }
        }
        let state = it.native_state::<ForkHooks>();
        if let Some(v) = before {
            state.before.push(v.clone());
        }
        if let Some(v) = after_in_child {
            state.child.push(v.clone());
        }
        if let Some(v) = after_in_parent {
            state.parent.push(v.clone());
        }
        Ok(())
    }

    /// Open a pseudo-terminal.
    ///
    /// Return a tuple of (master_fd, slave_fd) containing open file descriptors
    /// for both the master and slave ends.
    #[op]
    fn openpty(it: &mut Interp) -> R<(i32, i32)> {
        lumen_os::tty::openpty().map_err(|e| fs_err(it, e))
    }

    /// Fork a new process with a new pseudo-terminal as controlling tty.
    ///
    /// Returns a tuple of (pid, master_fd).
    /// Like fork(), return pid of 0 to the child process,
    /// and pid of child to the parent process.
    /// To both, return fd of newly opened pseudo-terminal.
    #[op]
    fn forkpty(it: &mut Interp) -> R<(i32, i32)> {
        forked(it, lumen_os::tty::forkpty, |&(pid, _)| pid)
    }

    /// Prepare the tty of which fd is a file descriptor for a new login session.
    ///
    /// Make the calling process a session leader; make the tty the
    /// controlling tty, the stdin, the stdout, and the stderr of the
    /// calling process; close fd.
    #[op]
    fn login_tty(it: &mut Interp, fd: &Value) -> R<()> {
        let fd = as_file_descriptor(it, fd)?;
        lumen_os::tty::login_tty(fd).map_err(|e| fs_err(it, e))
    }

    fn rusage_result(it: &mut Interp, pid: i32, status: i32, usage: &lumen_os::rlimit::Rusage) -> (i32, i32, Value) {
        (pid, status, crate::builtins::resourcem::resource::rusage_value(it, usage))
    }

    /// Wait for completion of a child process.
    ///
    /// Returns a tuple of information about the child process:
    ///   (pid, status, rusage)
    #[op]
    fn wait3(it: &mut Interp, options: i32) -> R<(i32, i32, Value)> {
        it.flush_out();
        let (pid, status, usage) = lumen_os::proc::wait4(-1, options).map_err(|e| fs_err(it, e))?;
        it.poll()?;
        Ok(rusage_result(it, pid, status, &usage))
    }

    /// Wait for completion of a specific child process.
    ///
    /// Returns a tuple of information about the child process:
    ///   (pid, status, rusage)
    #[op]
    fn wait4(it: &mut Interp, pid: i32, options: i32) -> R<(i32, i32, Value)> {
        it.flush_out();
        let (pid, status, usage) = lumen_os::proc::wait4(pid, options).map_err(|e| fs_err(it, e))?;
        it.poll()?;
        Ok(rusage_result(it, pid, status, &usage))
    }

    /// Returns the result of waiting for a process or processes.
    ///
    ///   idtype
    ///     Must be one of be P_PID, P_PGID or P_ALL.
    ///   id
    ///     The id to wait on.
    ///   options
    ///     Constructed from the ORing of one or more of WEXITED, WSTOPPED
    ///     or WCONTINUED and additionally may be ORed with WNOHANG or WNOWAIT.
    ///
    /// Returns either waitid_result or None if WNOHANG is specified and there are
    /// no children in a waitable state.
    #[op]
    fn waitid(it: &mut Interp, idtype: i32, id: &Value, options: i32) -> R<Value> {
        let id = match it.index_of(id)? {
            n if n >= 0 && n <= u32::MAX as i64 => n as u32,
            n if n == -1 => u32::MAX,
            _ => return Err(it.overflow_err("Python int too large to convert to C unsigned int")),
        };
        it.flush_out();
        let info = lumen_os::proc::waitid(idtype, id, options).map_err(|e| fs_err(it, e))?;
        it.poll()?;
        let Some(w) = info else { return Ok(Value::None) };
        let ty = structseq_type::<WaitidResult>(
            it,
            "posix",
            "waitid_result",
            &["si_pid", "si_uid", "si_signo", "si_status", "si_code"],
            5,
        );
        Ok(structseq_full(
            &ty,
            vec![
                Value::Int(w.pid as i64),
                Value::Int(w.uid as i64),
                Value::Int(w.signo as i64),
                Value::Int(w.status as i64),
                Value::Int(w.code as i64),
            ],
        ))
    }

    /// Return True if the process returning status exited via the exit() system call.
    #[op(name = "WIFEXITED")]
    fn wifexited(#[kw] status: i32) -> bool {
        lumen_os::proc::wait::if_exited(status)
    }

    /// Return the process return code from status.
    #[op(name = "WEXITSTATUS")]
    fn wexitstatus(#[kw] status: i32) -> i32 {
        lumen_os::proc::wait::exit_status(status)
    }

    /// Return True if the process returning status was terminated by a signal.
    #[op(name = "WIFSIGNALED")]
    fn wifsignaled(#[kw] status: i32) -> bool {
        lumen_os::proc::wait::if_signaled(status)
    }

    /// Return the signal that terminated the process that provided the status value.
    #[op(name = "WTERMSIG")]
    fn wtermsig(#[kw] status: i32) -> i32 {
        lumen_os::proc::wait::term_sig(status)
    }

    /// Return True if the process returning status was stopped.
    #[op(name = "WIFSTOPPED")]
    fn wifstopped(#[kw] status: i32) -> bool {
        lumen_os::proc::wait::if_stopped(status)
    }

    /// Return the signal that stopped the process that provided the status value.
    #[op(name = "WSTOPSIG")]
    fn wstopsig(#[kw] status: i32) -> i32 {
        lumen_os::proc::wait::stop_sig(status)
    }

    /// Return True if the process returning status was dumped to a core file.
    #[op(name = "WCOREDUMP")]
    fn wcoredump(status: i32) -> bool {
        lumen_os::proc::wait::core_dump(status)
    }

    /// Return True if a particular process was continued from a job control stop.
    #[op(name = "WIFCONTINUED")]
    fn wifcontinued(#[kw] status: i32) -> bool {
        lumen_os::proc::wait::if_continued(status)
    }

    /// Convert a wait status to an exit code.
    #[op]
    fn waitstatus_to_exitcode(it: &mut Interp, #[kw] status: i32) -> R<i32> {
        use lumen_os::proc::wait;
        if wait::if_exited(status) {
            Ok(wait::exit_status(status))
        } else if wait::if_signaled(status) {
            Ok(-wait::term_sig(status))
        } else {
            Err(it.value_error(&format!("invalid wait status: {}", status)))
        }
    }

    /// Exit to the system with specified status, without normal exit processing.
    #[op]
    fn _exit(it: &mut Interp, #[kw] status: i32) {
        it.platform.borrow_mut().exit_process(status)
    }

    /// Abort the interpreter immediately.
    #[op]
    fn abort(it: &mut Interp) {
        it.platform.borrow_mut().abort_process()
    }

    /// Execute the command in a subshell.
    #[op]
    fn system(it: &mut Interp, #[kw] command: &Value) -> R<i32> {
        let cmd = fs_text(it, command)?;
        let r = it.platform.borrow_mut().system(&cmd);
        r.map_err(|e| os_err(it, e))
    }

    /// Return the file system path representation of the object.
    #[op(name = "fspath")]
    fn fspath_op(it: &mut Interp, #[kw] path: &Value) -> R<Value> {
        fspath(it, path)
    }

    const HAVE_FUNCTIONS: [&str; 6] = ["HAVE_FCHMOD", "HAVE_FTRUNCATE", "HAVE_FUTIMES", "HAVE_LCHOWN", "HAVE_LSTAT", "HAVE_LUTIMES"];

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        for (name, ty) in [
            ("stat_result", stat_result_type(it)),
            ("terminal_size", terminal_size_type(it)),
            ("uname_result", uname_result_type(it)),
            ("times_result", times_result_type(it)),
        ] {
            dict_set_str(&d, name, Value::Obj(ty));
        }
        dict_set_str(&d, "error", Value::Obj(it.exc_type("OSError")));
        for (name, v) in [("F_OK", 0), ("R_OK", 4), ("W_OK", 2), ("X_OK", 1)] {
            dict_set_str(&d, name, Value::Int(v));
        }
        for (name, v) in lumen_os::consts::open_flags().chain(lumen_os::consts::misc()) {
            dict_set_str(&d, name, Value::Int(v));
        }
        let names = it.new_dict();
        for (name, v) in lumen_os::consts::sysconf_names() {
            let _ = it.dict_set(&names, Value::str(name), Value::Int(v));
        }
        dict_set_str(&d, "sysconf_names", Value::Obj(names));
        let env = it.new_dict();
        let vars = it.platform.borrow().environ();
        for (k, v) in vars {
            let _ = it.dict_set(&env, Value::bytes(k), Value::bytes(v));
        }
        dict_set_str(&d, "environ", Value::Obj(env));
        for (name, v) in lumen_os::proc::CLD_CONSTANTS {
            dict_set_str(&d, name, Value::Int(v));
        }
        dict_set_str(&d, "_have_functions", Value::list(HAVE_FUNCTIONS.iter().map(|n| Value::str(n)).collect()));
    }
}
