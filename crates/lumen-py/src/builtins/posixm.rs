//! `posix`: the operating-system interface behind `os`, on the [`Platform`] layer (which maps to
//! `lumen_os` on a real host) and `lumen_os`'s constant tables.
//!
//! [`Platform`]: crate::platform::Platform

/// `PyObject_AsFileDescriptor`: an int, or the result of the object's `fileno()`.
pub fn as_file_descriptor(
    it: &mut crate::vm::Interp,
    v: &crate::object::Value,
) -> crate::object::R<i32> {
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
        return Err(it.value_error(&format!(
            "file descriptor cannot be a negative integer ({fd})"
        )));
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
    use crate::bind::{convert_path, fspath, wrap_path, PathArg, PathOrFd, Py, This};
    use crate::object::*;
    use crate::platform::{DirentKind, IoError, OsStat, ProcGroup, Timespec};
    use crate::pyint::BigInt;
    use crate::vm::{dict_del_str, dict_set_str, Interp};

    struct StatResult;
    struct TerminalSize;
    struct UnameResult;
    struct TimesResult;

    const STAT_FIELDS: [&str; 22] = [
        "st_mode",
        "st_ino",
        "st_dev",
        "st_nlink",
        "st_uid",
        "st_gid",
        "st_size",
        "",
        "",
        "",
        "st_atime",
        "st_mtime",
        "st_ctime",
        "st_atime_ns",
        "st_mtime_ns",
        "st_ctime_ns",
        "st_blksize",
        "st_blocks",
        "st_rdev",
        "st_flags",
        "st_gen",
        "st_birthtime",
    ];

    fn stat_result_type(it: &mut Interp) -> Obj {
        structseq_type::<StatResult>(it, "os", "stat_result", &STAT_FIELDS, 10)
    }

    fn terminal_size_type(it: &mut Interp) -> Obj {
        structseq_type::<TerminalSize>(it, "os", "terminal_size", &["columns", "lines"], 2)
    }

    fn uname_result_type(it: &mut Interp) -> Obj {
        structseq_type::<UnameResult>(
            it,
            "posix",
            "uname_result",
            &["sysname", "nodename", "release", "version", "machine"],
            5,
        )
    }

    fn times_result_type(it: &mut Interp) -> Obj {
        let f = [
            "user",
            "system",
            "children_user",
            "children_system",
            "elapsed",
        ];
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
        it.new_exc(
            &cls,
            vec![Value::string(format!(
                "{}: {} unavailable on this platform",
                fname, arg
            ))],
        )
    }

    /// `path` resolved against the directory open on `dir_fd`, when one is given.
    fn at_path<const FD: bool>(
        it: &mut Interp,
        dir_fd: Option<i32>,
        path: &crate::bind::FsPath<FD>,
    ) -> R<String> {
        match dir_fd {
            Some(fd) => {
                lumen_os::posix::at_path(fd, &path.path).map_err(|e| fs_path_err(it, e, path))
            }
            None => Ok(path.path.clone()),
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
        let rp = at_path(it, dir_fd, &path)?;
        let r = match path.fd {
            Some(fd) => it.platform.borrow_mut().fd_stat(fd),
            None => it.platform.borrow_mut().stat(&rp, follow_symlinks),
        };
        match r {
            Ok(s) => Ok(stat_value(it, &s)),
            Err(e) => Err(path_err(it, e, &path)),
        }
    }

    /// Perform a stat system call on the given path, without following symbolic links.
    #[op]
    fn lstat(it: &mut Interp, #[kw] path: PathArg, #[kwonly] dir_fd: Option<i32>) -> R<Value> {
        let rp = at_path(it, dir_fd, &path)?;
        let r = it.platform.borrow_mut().stat(&rp, false);
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
        let rp = at_path(it, dir_fd, &path)?;
        if effective_ids || !follow_symlinks {
            return Ok(lumen_os::posix::access_ex(
                &rp,
                mode as u32,
                effective_ids,
                follow_symlinks,
            )
            .is_ok());
        }
        Ok(it.platform.borrow_mut().access(&rp, mode as u32).is_ok())
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

    fn list_dir(
        it: &mut Interp,
        fname: &str,
        path: &Value,
    ) -> R<(PathOrFd, Vec<(String, DirentKind)>)> {
        let p: PathOrFd = convert_path(it, fname, "path", path, true, true)?;
        let dir = match p.fd {
            Some(fd) => lumen_os::posix::fd_path(fd).map_err(|e| fs_err(it, e))?,
            None => p.path.clone(),
        };
        let r = it.platform.borrow_mut().listdir(&dir);
        match r {
            Ok(v) => Ok((p, v)),
            Err(e) => Err(path_err(it, e, &p)),
        }
    }

    /// Return a list containing the names of the files in the directory.
    #[op]
    fn listdir(it: &mut Interp, #[kw] path: Option<&Value>) -> R<Value> {
        let (p, entries) = list_dir(it, "listdir", path.unwrap_or(&Value::None))?;
        Ok(Value::list(
            entries.into_iter().map(|(n, _)| p.wrap(n)).collect(),
        ))
    }

    /// Return an iterator of DirEntry objects for given path.
    #[op]
    fn scandir(it: &mut Interp, #[kw] path: Option<&Value>) -> R<Py<ScandirIterator>> {
        let (p, entries) = list_dir(it, "scandir", path.unwrap_or(&Value::None))?;
        let dir = match p.fd {
            Some(fd) => lumen_os::posix::fd_path(fd).map_err(|e| fs_err(it, e))?,
            None if matches!(p.obj, Value::None) => ".".to_string(),
            None => p.path.clone(),
        };
        let it_state = ScandirIterator {
            dir,
            bytes: p.bytes,
            by_fd: p.fd.is_some(),
            entries: entries.into_iter(),
            closed: false,
        };
        Ok(Py::new(it, it_state))
    }

    #[class(name = "ScandirIterator", skip(py))]
    pub struct ScandirIterator {
        dir: String,
        bytes: bool,
        by_fd: bool,
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
                if s.closed {
                    None
                } else {
                    s.entries
                        .next()
                        .map(|e| (e, s.dir.clone(), s.bytes, s.by_fd))
                }
            };
            let Some(((name, kind), dir, bytes, by_fd)) = next else {
                slf.0.borrow_mut(it)?.closed = true;
                return Ok(None);
            };
            let path = if dir.ends_with('/') {
                format!("{}{}", dir, name)
            } else {
                format!("{}/{}", dir, name)
            };
            let entry = DirEntry {
                name: wrap_path(bytes, name.clone()),
                path: wrap_path(bytes, if by_fd { name.clone() } else { path.clone() }),
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
                (
                    if follow {
                        s.stat.clone()
                    } else {
                        s.lstat.clone()
                    },
                    s.text.clone(),
                    follow,
                    s.path.clone(),
                )
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
            if is_link {
                s.stat = Some(st.clone())
            } else {
                s.lstat = Some(st.clone())
            }
            Ok(st)
        }

        fn mode(slf: &Py<Self>, it: &mut Interp, follow: bool) -> R<Option<u32>> {
            match Self::fetch(slf, it, follow) {
                Ok(st) => Ok(st
                    .tuple_items()
                    .and_then(|t| t[0].as_i64())
                    .map(|m| m as u32)),
                Err(e) if it.exc_is(&e, "FileNotFoundError") => Ok(None),
                Err(e) => Err(e),
            }
        }

        fn is_type(
            slf: &Py<Self>,
            it: &mut Interp,
            follow: bool,
            want: DirentKind,
            fmt: u32,
        ) -> R<bool> {
            let kind = slf.borrow(it)?.kind;
            if kind != DirentKind::Unknown && !(follow && kind == DirentKind::Link) {
                return Ok(kind == want);
            }
            Ok(Self::mode(slf, it, follow)?.is_some_and(|m| m & lumen_os::fs::S_IFMT == fmt))
        }
    }

    #[methods]
    impl DirEntry {
        /// the entry's base filename, relative to scandir() "path" argument
        #[getter]
        fn name(&self) -> Value {
            self.name.clone()
        }

        /// the entry's full path name; equivalent to os.path.join(scandir_path, entry.name)
        #[getter]
        fn path(&self) -> Value {
            self.path.clone()
        }

        /// Return True if the entry is a directory; cached per entry.
        fn is_dir(
            slf: This<Py<Self>>,
            it: &mut Interp,
            #[kwonly]
            #[default(true)]
            follow_symlinks: bool,
        ) -> R<bool> {
            Self::is_type(
                &slf.0,
                it,
                follow_symlinks,
                DirentKind::Dir,
                lumen_os::fs::S_IFDIR,
            )
        }

        /// Return True if the entry is a file; cached per entry.
        fn is_file(
            slf: This<Py<Self>>,
            it: &mut Interp,
            #[kwonly]
            #[default(true)]
            follow_symlinks: bool,
        ) -> R<bool> {
            Self::is_type(
                &slf.0,
                it,
                follow_symlinks,
                DirentKind::File,
                lumen_os::fs::S_IFREG,
            )
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
        fn stat(
            slf: This<Py<Self>>,
            it: &mut Interp,
            #[kwonly]
            #[default(true)]
            follow_symlinks: bool,
        ) -> R<Value> {
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
        let rp = at_path(it, dir_fd, &path)?;
        let r = it.platform.borrow_mut().mkdir(&rp, mode as u32);
        r.map_err(|e| path_err(it, e, &path))
    }

    /// Remove a directory.
    #[op]
    fn rmdir(it: &mut Interp, #[kw] path: PathArg, #[kwonly] dir_fd: Option<i32>) -> R<()> {
        let rp = at_path(it, dir_fd, &path)?;
        let r = it.platform.borrow_mut().rmdir(&rp);
        r.map_err(|e| path_err(it, e, &path))
    }

    /// Remove a file (same as remove()).
    #[op]
    fn unlink(it: &mut Interp, #[kw] path: PathArg, #[kwonly] dir_fd: Option<i32>) -> R<()> {
        let rp = at_path(it, dir_fd, &path)?;
        let r = it.platform.borrow_mut().unlink(&rp);
        r.map_err(|e| path_err(it, e, &path))
    }

    /// Remove a file (same as unlink()).
    #[op]
    fn remove(it: &mut Interp, #[kw] path: PathArg, #[kwonly] dir_fd: Option<i32>) -> R<()> {
        let rp = at_path(it, dir_fd, &path)?;
        let r = it.platform.borrow_mut().unlink(&rp);
        r.map_err(|e| path_err(it, e, &path))
    }

    fn do_rename(
        it: &mut Interp,
        fname: &str,
        src: &PathArg,
        dst: &PathArg,
        src_dir_fd: Option<i32>,
        dst_dir_fd: Option<i32>,
    ) -> R<()> {
        if src.bytes != dst.bytes {
            return Err(it.type_error(&format!("{}: src and dst must be the same type", fname)));
        }
        let rsrc = at_path(it, src_dir_fd, src)?;
        let rdst = at_path(it, dst_dir_fd, dst)?;
        let r = it.platform.borrow_mut().rename(&rsrc, &rdst);
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
        if src.bytes != dst.bytes {
            return Err(it.type_error("link: src and dst must be the same type"));
        }
        let rsrc = at_path(it, src_dir_fd, &src)?;
        let rdst = at_path(it, dst_dir_fd, &dst)?;
        let r = if follow_symlinks {
            it.platform.borrow_mut().link(&rsrc, &rdst)
        } else {
            lumen_os::posix::link_ex(&rsrc, &rdst, false).map_err(IoError::from)
        };
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
        if src.bytes != dst.bytes {
            return Err(it.type_error("symlink: src and dst must be the same type"));
        }
        let rdst = at_path(it, dir_fd, &dst)?;
        let r = it
            .platform
            .borrow_mut()
            .symlink(&src.path, &rdst, target_is_directory);
        r.map_err(|e| it.os_error_errno(e.errno, Some(&src.obj), Some(&dst.obj)))
    }

    /// Return a string representing the path to which the symbolic link points.
    #[op]
    fn readlink(it: &mut Interp, #[kw] path: PathArg, #[kwonly] dir_fd: Option<i32>) -> R<Value> {
        let rp = at_path(it, dir_fd, &path)?;
        let r = it.platform.borrow_mut().readlink(&rp);
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
        let rp = at_path(it, dir_fd, &path)?;
        if !follow_symlinks {
            if cfg!(target_vendor = "apple") && path.fd.is_none() {
                return lumen_os::posix::lchmod(&rp, mode as u32)
                    .map_err(|e| fs_path_err(it, e, &path));
            }
            return Err(unavailable(it, "chmod", "follow_symlinks"));
        }
        let r = match path.fd {
            Some(fd) => it.platform.borrow_mut().fd_chmod(fd, mode as u32),
            None => it.platform.borrow_mut().chmod(&rp, mode as u32),
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
        #[kw] path: PathOrFd,
        #[kw] uid: &Value,
        #[kw] gid: &Value,
        #[kwonly] dir_fd: Option<i32>,
        #[kwonly]
        #[default(true)]
        follow_symlinks: bool,
    ) -> R<()> {
        let (uid, gid) = (id_arg(it, uid, "uid")?, id_arg(it, gid, "gid")?);
        if let Some(fd) = path.fd {
            if dir_fd.is_some() {
                return Err(it.value_error("chown: can't specify both dir_fd and fd"));
            }
            if !follow_symlinks {
                return Err(it.value_error("chown: cannot use fd and follow_symlinks together"));
            }
            return lumen_os::posix::fchown(fd, uid, gid).map_err(|e| fs_path_err(it, e, &path));
        }
        let rp = at_path(it, dir_fd, &path)?;
        let r = it
            .platform
            .borrow_mut()
            .chown(&rp, uid, gid, follow_symlinks);
        r.map_err(|e| path_err(it, e, &path))
    }

    /// Change the owner and group id of path to the numeric uid and gid, without following links.
    #[op]
    fn lchown(it: &mut Interp, #[kw] path: PathArg, #[kw] uid: &Value, #[kw] gid: &Value) -> R<()> {
        let (uid, gid) = (id_arg(it, uid, "uid")?, id_arg(it, gid, "gid")?);
        let r = it.platform.borrow_mut().chown(&path.path, uid, gid, false);
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
                Ok(Timespec {
                    sec,
                    nsec: nsec as u32,
                })
            }
            _ => Ok(Timespec {
                sec: it.index_of(v)?,
                nsec: 0,
            }),
        }
    }

    fn ns_timespec(it: &mut Interp, v: &Value) -> R<Timespec> {
        let n = it.index_of(v)?;
        Ok(Timespec {
            sec: n.div_euclid(1_000_000_000),
            nsec: n.rem_euclid(1_000_000_000) as u32,
        })
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
        let rp = at_path(it, dir_fd, &path)?;
        let (atime, mtime) = match (times, ns) {
            (Some(_), Some(_)) => {
                return Err(
                    it.value_error("utime: you may specify either 'times' or 'ns' but not both")
                );
            }
            (Some(t), None) => match t.tuple_items() {
                Some([a, m]) if a.is_int_like() || matches!(a, Value::Float(_)) => {
                    let (a, m) = (a.clone(), m.clone());
                    (timespec_of(it, &a)?, timespec_of(it, &m)?)
                }
                _ => {
                    return Err(
                        it.type_error("utime: 'times' must be either a tuple of two ints or None")
                    );
                }
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
                let t = Timespec {
                    sec: now / 1_000_000_000,
                    nsec: (now % 1_000_000_000) as u32,
                };
                (t, t)
            }
        };
        let r = match path.fd {
            Some(fd) => it.platform.borrow_mut().fd_utimes(fd, atime, mtime),
            None => it
                .platform
                .borrow_mut()
                .utimes(&rp, atime, mtime, follow_symlinks),
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
        let rp = at_path(it, dir_fd, &path)?;
        let r = it.platform.borrow_mut().fd_open(&rp, flags, mode as u32);
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
    fn dup2(
        it: &mut Interp,
        #[kw] fd: i32,
        #[kw] fd2: i32,
        #[kw]
        #[default(true)]
        inheritable: bool,
    ) -> R<i32> {
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
        let r = it
            .platform
            .borrow_mut()
            .fd_read(fd, &mut buf, Some(offset as u64));
        let n = r.map_err(|e| os_err(it, e))?;
        buf.truncate(n);
        Ok(Value::bytes(buf))
    }

    /// Write a bytes object to a file descriptor.
    #[op]
    fn write(it: &mut Interp, fd: i32, data: &[u8]) -> R<usize> {
        let r = it.fd_write(fd, data)?;
        r.map_err(|e| os_err(it, e))
    }

    /// Write bytes to a file descriptor starting at a particular offset.
    #[op]
    fn pwrite(it: &mut Interp, fd: i32, buffer: &[u8], offset: i64) -> R<usize> {
        let r = it
            .platform
            .borrow_mut()
            .fd_write(fd, buffer, Some(offset as u64));
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
        let r = it
            .platform
            .borrow_mut()
            .fd_set_inheritable(fd, inheritable != 0);
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
        if it.platform.borrow_mut().fd_isatty(fd) {
            Value::str("UTF-8")
        } else {
            Value::None
        }
    }

    /// Return the size of the terminal window as (columns, lines).
    #[op]
    fn get_terminal_size(it: &mut Interp, #[default(1)] fd: i32) -> R<Value> {
        let r = it.platform.borrow_mut().terminal_size(fd);
        let (cols, lines) = r.map_err(|e| os_err(it, e))?;
        let ty = terminal_size_type(it);
        Ok(structseq_full(
            &ty,
            vec![Value::Int(cols as i64), Value::Int(lines as i64)],
        ))
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
        Ok(structseq_full(
            &ty,
            t.into_iter().map(Value::Float).collect(),
        ))
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
        Ok(Value::list(
            g.into_iter().map(|g| Value::Int(g as i64)).collect(),
        ))
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

    fn waitid_type(it: &mut Interp) -> Obj {
        structseq_type::<WaitidResult>(
            it,
            "posix",
            "waitid_result",
            &["si_pid", "si_uid", "si_signo", "si_status", "si_code"],
            5,
        )
    }

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
        let hooks: Vec<Value> = it
            .native_state::<ForkHooks>()
            .before
            .iter()
            .rev()
            .cloned()
            .collect();
        run_fork_hooks(it, hooks);
    }

    pub(crate) fn after_fork(it: &mut Interp, in_child: bool) {
        let state = it.native_state::<ForkHooks>();
        let hooks = if in_child {
            state.child.clone()
        } else {
            state.parent.clone()
        };
        run_fork_hooks(it, hooks);
    }

    /// Runs `spawn` (a fork) between the registered fork hooks; `pid_of` extracts the child
    /// indicator from its result.
    fn forked<T>(
        it: &mut Interp,
        spawn: impl FnOnce() -> Result<T, lumen_os::FsError>,
        pid_of: impl Fn(&T) -> i32,
    ) -> R<T> {
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
        let given = [
            ("before", before),
            ("after_in_child", after_in_child),
            ("after_in_parent", after_in_parent),
        ];
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
        let fd = super::as_file_descriptor(it, fd)?;
        lumen_os::tty::login_tty(fd).map_err(|e| fs_err(it, e))
    }

    fn rusage_result(
        it: &mut Interp,
        pid: i32,
        status: i32,
        usage: &lumen_os::rlimit::Rusage,
    ) -> (i32, i32, Value) {
        (
            pid,
            status,
            crate::builtins::resourcem::resource::rusage_value(it, usage),
        )
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
        let (pid, status, usage) =
            lumen_os::proc::wait4(pid, options).map_err(|e| fs_err(it, e))?;
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
        let Some(w) = info else {
            return Ok(Value::None);
        };
        let ty = waitid_type(it);
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

    // ---- argument helpers ------------------------------------------------------------------------

    fn fdv(it: &mut Interp, v: &Value) -> R<i32> {
        super::as_file_descriptor(it, v)
    }

    fn fs_path_err<const FD: bool>(
        it: &mut Interp,
        e: lumen_os::FsError,
        p: &crate::bind::FsPath<FD>,
    ) -> Obj {
        it.os_error_errno(e.errno(), Some(&p.obj), None)
    }

    /// `_Py_Uid_Converter` / `_Py_Gid_Converter`: an int, with `-1` meaning "unchanged".
    fn id_arg(it: &mut Interp, v: &Value, what: &str) -> R<u32> {
        let n = match it.index_of(v) {
            Ok(n) => n,
            Err(e) if it.exc_is(&e, "OverflowError") => {
                return Err(it.overflow_err(&format!("{what} is greater than maximum")));
            }
            Err(_) => {
                let t = it.type_name_of(v);
                return Err(it.type_error(&format!("{what} should be integer, not {t}")));
            }
        };
        match n {
            -1 => Ok(u32::MAX),
            n if (0..=u32::MAX as i64).contains(&n) => Ok(n as u32),
            _ => Err(it.overflow_err(&format!("{what} is less than minimum"))),
        }
    }

    fn is_sequence(v: &Value) -> bool {
        v.tuple_items().is_some() || list_of(v).is_some()
    }

    /// A configuration name (`sysconf`, `pathconf`, `confstr`): a string looked up in `names`, or an int.
    fn conf_name(it: &mut Interp, v: &Value, names: Vec<(&'static str, i64)>) -> R<i32> {
        if let Some(s) = v.as_str() {
            return match names.iter().find(|(k, _)| *k == s) {
                Some((_, n)) => Ok(*n as i32),
                None => Err(it.value_error("unrecognized configuration name")),
            };
        }
        if v.is_int_like() {
            return Ok(it.index_of(v)? as i32);
        }
        Err(it.type_error("configuration names must be strings or integers"))
    }

    fn names_dict(it: &mut Interp, names: Vec<(&'static str, i64)>) -> Value {
        let d = it.new_dict();
        for (k, v) in names {
            let _ = it.dict_set(&d, Value::str(k), Value::Int(v));
        }
        Value::Obj(d)
    }

    /// The `argv` of `exec*` / `posix_spawn*`: a non-empty tuple or list of paths.
    fn argv_list(
        it: &mut Interp,
        argv: &Value,
        what: &str,
        empty_msg: &str,
        type_msg: &str,
    ) -> R<Vec<Vec<u8>>> {
        let _ = what;
        if !is_sequence(argv) {
            return Err(it.type_error(type_msg));
        }
        let items = it.iterate_to_vec(argv)?;
        if items.is_empty() {
            return Err(it.value_error(empty_msg));
        }
        let mut out = Vec::with_capacity(items.len());
        for i in &items {
            out.push(crate::bind::path::fs_bytes(it, i)?);
        }
        Ok(out)
    }

    /// `parse_envlist`: the `KEY=value` entries of a mapping.
    fn env_list(it: &mut Interp, env: &Value) -> R<Vec<Vec<u8>>> {
        let keys = it.call_method(env, "keys", Vec::new())?;
        let vals = it.call_method(env, "values", Vec::new())?;
        let (keys, vals) = (it.iterate_to_vec(&keys)?, it.iterate_to_vec(&vals)?);
        let mut out = Vec::with_capacity(keys.len());
        for (k, v) in keys.iter().zip(&vals) {
            let (mut key, val) = (
                crate::bind::path::fs_bytes(it, k)?,
                crate::bind::path::fs_bytes(it, v)?,
            );
            if key.is_empty() || key[1..].contains(&b'=') {
                return Err(it.value_error("illegal environment variable name"));
            }
            key.push(b'=');
            key.extend(val);
            out.push(key);
        }
        Ok(out)
    }

    fn has_getitem(it: &mut Interp, v: &Value) -> bool {
        it.get_attr_str(v, "__getitem__").is_ok()
    }

    // ---- exec and posix_spawn ------------------------------------------------------------------

    /// Execute an executable path with arguments, replacing current process.
    ///
    ///   path
    ///     Path of executable file.
    ///   argv
    ///     Tuple or list of strings.
    #[op]
    fn execv(it: &mut Interp, path: PathArg, argv: &Value) -> R<Value> {
        let args = argv_list(
            it,
            argv,
            "execv",
            "execv() arg 2 must not be empty",
            "execv() arg 2 must be a tuple or list",
        )?;
        if args[0].is_empty() {
            return Err(it.value_error("execv() arg 2 first element cannot be empty"));
        }
        it.flush_out();
        let e = lumen_os::posix::exec(path.path.as_bytes(), None, &args, None);
        Err(it.os_error_errno(e.errno(), None, None))
    }

    /// Execute an executable path with arguments, replacing current process.
    ///
    ///   path
    ///     Path of executable file.
    ///   argv
    ///     Tuple or list of strings.
    ///   env
    ///     Dictionary of strings mapping to strings.
    #[op]
    fn execve(
        it: &mut Interp,
        #[kw] path: PathOrFd,
        #[kw] argv: &Value,
        #[kw] env: &Value,
    ) -> R<Value> {
        let args = argv_list(
            it,
            argv,
            "execve",
            "execve: argv must not be empty",
            "execve: argv must be a tuple or list",
        )?;
        if !has_getitem(it, env) {
            return Err(it.type_error("execve: environment must be a mapping object"));
        }
        if args[0].is_empty() {
            return Err(it.value_error("execve: argv first element cannot be empty"));
        }
        let envs = env_list(it, env)?;
        it.flush_out();
        let e = lumen_os::posix::exec(path.path.as_bytes(), path.fd, &args, Some(&envs));
        Err(fs_path_err(it, e, &path))
    }

    fn spawn_actions(it: &mut Interp, actions: &Value) -> R<Vec<lumen_os::posix::FileAction>> {
        use lumen_os::posix::{FileAction, SPAWN_CLOSE, SPAWN_CLOSEFROM, SPAWN_DUP2, SPAWN_OPEN};
        if !is_sequence(actions) {
            return Err(it.type_error("file_actions must be a sequence or None"));
        }
        let mut out = Vec::new();
        for action in it.iterate_to_vec(actions)? {
            let items = match action.tuple_items() {
                Some(t) if !t.is_empty() => t.to_vec(),
                _ => {
                    return Err(
                        it.type_error("Each file_actions element must be a non-empty tuple")
                    );
                }
            };
            let tag = it.index_of(&items[0])?;
            let fd_at = |it: &mut Interp, i: usize| -> R<i32> {
                let n = it.index_of(&items[i])?;
                i32::try_from(n)
                    .map_err(|_| it.overflow_err("Python int too large to convert to C int"))
            };
            match tag {
                SPAWN_OPEN => {
                    if items.len() != 5 {
                        return Err(it.type_error("A open file_action tuple must have 5 elements"));
                    }
                    let fd = fd_at(it, 1)?;
                    let path = crate::bind::path::fs_bytes(it, &items[2])?;
                    let flags = fd_at(it, 3)?;
                    let mode = it.index_of(&items[4])? as u32;
                    out.push(FileAction::Open {
                        fd,
                        path,
                        flags,
                        mode,
                    });
                }
                SPAWN_CLOSE => {
                    if items.len() != 2 {
                        return Err(it.type_error("A close file_action tuple must have 2 elements"));
                    }
                    out.push(FileAction::Close(fd_at(it, 1)?));
                }
                SPAWN_DUP2 => {
                    if items.len() != 3 {
                        return Err(it.type_error("A dup2 file_action tuple must have 3 elements"));
                    }
                    out.push(FileAction::Dup2(fd_at(it, 1)?, fd_at(it, 2)?));
                }
                SPAWN_CLOSEFROM if lumen_os::posix::spawn_has_closefrom() => {
                    if items.len() != 2 {
                        return Err(
                            it.type_error("A closefrom file_action tuple must have 2 elements")
                        );
                    }
                    out.push(FileAction::CloseFrom(fd_at(it, 1)?));
                }
                _ => return Err(it.type_error("Unknown file_actions identifier")),
            }
        }
        Ok(out)
    }

    #[allow(clippy::too_many_arguments)]
    fn posix_spawn_impl(
        it: &mut Interp,
        search: bool,
        path: &PathArg,
        argv: &Value,
        env: &Value,
        file_actions: Option<&Value>,
        setpgroup: Option<&Value>,
        resetids: bool,
        setsid: bool,
        setsigmask: Option<&Value>,
        setsigdef: Option<&Value>,
        scheduler: Option<&Value>,
    ) -> R<Value> {
        let fname = if search {
            "posix_spawnp"
        } else {
            "posix_spawn"
        };
        let args = argv_list(
            it,
            argv,
            fname,
            &format!("{fname}: argv must not be empty"),
            &format!("{fname}: argv must be a tuple or list"),
        )?;
        let env_given = !matches!(env, Value::None);
        if env_given && !has_getitem(it, env) {
            return Err(it.type_error(&format!(
                "{fname}: environment must be a mapping object or None"
            )));
        }
        if let Some(s) = scheduler {
            if !matches!(s, Value::None) && s.tuple_items().is_none() {
                return Err(it.type_error(&format!("{fname}: scheduler must be a tuple or None")));
            }
        }
        if args[0].is_empty() {
            return Err(it.value_error(&format!("{fname}: argv first element cannot be empty")));
        }
        let envs = if env_given {
            Some(env_list(it, env)?)
        } else {
            None
        };
        let actions = match file_actions {
            Some(a) if !matches!(a, Value::None) => spawn_actions(it, a)?,
            _ => Vec::new(),
        };
        let mut attrs = lumen_os::posix::SpawnAttrs::default();
        if let Some(pg) = setpgroup.filter(|v| !matches!(v, Value::None)) {
            let n = it.index_of(pg)?;
            attrs.setpgroup = Some(
                i32::try_from(n)
                    .map_err(|_| it.overflow_err("Python int too large to convert to C int"))?,
            );
        }
        attrs.resetids = resetids;
        if setsid {
            if !lumen_os::posix::spawn_has_setsid() {
                return Err(unavailable(it, fname, "setsid"));
            }
            attrs.setsid = true;
        }
        if let Some(s) = setsigmask {
            attrs.setsigmask = Some(super::super::signalm::_signal::signal_set(it, s)?);
        }
        if let Some(s) = setsigdef {
            attrs.setsigdef = Some(super::super::signalm::_signal::signal_set(it, s)?);
        }
        if let Some(s) = scheduler.filter(|v| !matches!(v, Value::None)) {
            if !lumen_os::posix::spawn_has_scheduler() {
                let cls = it.exc_type("NotImplementedError");
                return Err(it.new_exc(
                    &cls,
                    vec![Value::str(
                        "The scheduler option is not supported in this system.",
                    )],
                ));
            }
            let items = s.tuple_items().map(|t| t.to_vec()).unwrap_or_default();
            if items.len() != 2 {
                return Err(it.type_error("A scheduler tuple must have two elements"));
            }
            let priority = sched_priority(it, &items[1])?;
            let policy = match &items[0] {
                Value::None => None,
                p => Some(it.index_of(p)? as i32),
            };
            attrs.scheduler = Some((policy, priority));
        }
        let r = lumen_os::posix::posix_spawn(
            path.path.as_bytes(),
            search,
            &args,
            envs.as_deref(),
            &actions,
            &attrs,
        );
        match r {
            Ok(pid) => Ok(Value::Int(pid as i64)),
            Err(e) => Err(fs_path_err(it, e, path)),
        }
    }

    /// Execute the program specified by path in a new process.
    ///
    ///   path
    ///     Path of executable file.
    ///   argv
    ///     Tuple or list of strings.
    ///   env
    ///     Dictionary of strings mapping to strings.
    ///   file_actions
    ///     A sequence of file action tuples.
    ///   setpgroup
    ///     The pgroup to use with the POSIX_SPAWN_SETPGROUP flag.
    ///   resetids
    ///     If the value is `true` the POSIX_SPAWN_RESETIDS will be activated.
    ///   setsid
    ///     If the value is `true` the POSIX_SPAWN_SETSID or POSIX_SPAWN_SETSID_NP
    ///     will be activated.
    ///   setsigmask
    ///     The sigmask to use with the POSIX_SPAWN_SETSIGMASK flag.
    ///   setsigdef
    ///     The sigmask to use with the POSIX_SPAWN_SETSIGDEF flag.
    ///   scheduler
    ///     A tuple with the scheduler policy (optional) and parameters.
    #[op]
    fn posix_spawn(
        it: &mut Interp,
        path: PathArg,
        argv: &Value,
        env: &Value,
        #[kwonly] file_actions: Option<&Value>,
        #[kwonly] setpgroup: Option<&Value>,
        #[kwonly]
        #[default(false)]
        resetids: bool,
        #[kwonly]
        #[default(false)]
        setsid: bool,
        #[kwonly] setsigmask: Option<&Value>,
        #[kwonly] setsigdef: Option<&Value>,
        #[kwonly] scheduler: Option<&Value>,
    ) -> R<Value> {
        posix_spawn_impl(
            it,
            false,
            &path,
            argv,
            env,
            file_actions,
            setpgroup,
            resetids,
            setsid,
            setsigmask,
            setsigdef,
            scheduler,
        )
    }

    /// Execute the program specified by path in a new process.
    ///
    ///   path
    ///     Path of executable file.
    ///   argv
    ///     Tuple or list of strings.
    ///   env
    ///     Dictionary of strings mapping to strings.
    ///   file_actions
    ///     A sequence of file action tuples.
    ///   setpgroup
    ///     The pgroup to use with the POSIX_SPAWN_SETPGROUP flag.
    ///   resetids
    ///     If the value is `True` the POSIX_SPAWN_RESETIDS will be activated.
    ///   setsid
    ///     If the value is `True` the POSIX_SPAWN_SETSID or POSIX_SPAWN_SETSID_NP
    ///     will be activated.
    ///   setsigmask
    ///     The sigmask to use with the POSIX_SPAWN_SETSIGMASK flag.
    ///   setsigdef
    ///     The sigmask to use with the POSIX_SPAWN_SETSIGDEF flag.
    ///   scheduler
    ///     A tuple with the scheduler policy (optional) and parameters.
    #[op]
    fn posix_spawnp(
        it: &mut Interp,
        path: PathArg,
        argv: &Value,
        env: &Value,
        #[kwonly] file_actions: Option<&Value>,
        #[kwonly] setpgroup: Option<&Value>,
        #[kwonly]
        #[default(false)]
        resetids: bool,
        #[kwonly]
        #[default(false)]
        setsid: bool,
        #[kwonly] setsigmask: Option<&Value>,
        #[kwonly] setsigdef: Option<&Value>,
        #[kwonly] scheduler: Option<&Value>,
    ) -> R<Value> {
        posix_spawn_impl(
            it,
            true,
            &path,
            argv,
            env,
            file_actions,
            setpgroup,
            resetids,
            setsid,
            setsigmask,
            setsigdef,
            scheduler,
        )
    }

    // ---- paths and descriptors -----------------------------------------------------------------

    /// Change to the directory of the given file descriptor.
    ///
    /// fd must be opened on a directory, not a file.
    /// Equivalent to os.chdir(fd).
    #[op]
    fn fchdir(it: &mut Interp, #[kw] fd: &Value) -> R<()> {
        let fd = fdv(it, fd)?;
        lumen_os::posix::fchdir(fd).map_err(|e| fs_err(it, e))
    }

    /// Change the owner and group id of the file specified by file descriptor.
    ///
    /// Equivalent to os.chown(fd, uid, gid).
    #[op]
    fn fchown(it: &mut Interp, #[kw] fd: &Value, #[kw] uid: &Value, #[kw] gid: &Value) -> R<()> {
        let fd = fdv(it, fd)?;
        let (uid, gid) = (id_arg(it, uid, "uid")?, id_arg(it, gid, "gid")?);
        lumen_os::posix::fchown(fd, uid, gid).map_err(|e| fs_err(it, e))
    }

    /// Change root directory to path.
    #[op]
    fn chroot(it: &mut Interp, #[kw] path: PathArg) -> R<()> {
        lumen_os::posix::chroot(&path.path).map_err(|e| fs_path_err(it, e, &path))
    }

    /// Set file flags.
    ///
    /// If follow_symlinks is False, and the last element of the path is a symbolic
    ///   link, chflags will change flags on the symbolic link itself instead of the
    ///   file the link points to.
    /// follow_symlinks may not be implemented on your platform.  If it is
    /// unavailable, using it will raise a NotImplementedError.
    #[op]
    fn chflags(
        it: &mut Interp,
        #[kw] path: PathArg,
        #[kw] flags: u64,
        #[kw]
        #[default(true)]
        follow_symlinks: bool,
    ) -> R<()> {
        lumen_os::posix::chflags(&path.path, flags as u32, follow_symlinks)
            .map_err(|e| fs_path_err(it, e, &path))
    }

    /// Set file flags.
    ///
    /// This function will not follow symbolic links.
    /// Equivalent to chflags(path, flags, follow_symlinks=False).
    #[op]
    fn lchflags(it: &mut Interp, #[kw] path: PathArg, #[kw] flags: u64) -> R<()> {
        lumen_os::posix::chflags(&path.path, flags as u32, false)
            .map_err(|e| fs_path_err(it, e, &path))
    }

    /// Change the access permissions of a file, without following symbolic links.
    ///
    /// If path is a symlink, this affects the link itself rather than the target.
    /// Equivalent to chmod(path, mode, follow_symlinks=False)."
    #[op]
    fn lchmod(it: &mut Interp, #[kw] path: PathArg, #[kw] mode: i32) -> R<()> {
        lumen_os::posix::lchmod(&path.path, mode as u32).map_err(|e| fs_path_err(it, e, &path))
    }

    /// Create a "fifo" (a POSIX named pipe).
    ///
    /// If dir_fd is not None, it should be a file descriptor open to a directory,
    ///   and path should be relative; path will then be relative to that directory.
    /// dir_fd may not be implemented on your platform.
    ///   If it is unavailable, using it will raise a NotImplementedError.
    #[op]
    fn mkfifo(
        it: &mut Interp,
        #[kw] path: PathArg,
        #[kw]
        #[default(438)]
        mode: i32,
        #[kwonly] dir_fd: Option<i32>,
    ) -> R<()> {
        let rp = at_path(it, dir_fd, &path)?;
        lumen_os::posix::mkfifo(&rp, mode as u32).map_err(|e| fs_err(it, e))
    }

    /// Create a node in the file system.
    ///
    /// Create a node in the file system (file, device special file or named pipe)
    /// at path.  mode specifies both the permissions to use and the
    /// type of node to be created, being combined (bitwise OR) with one of
    /// S_IFREG, S_IFCHR, S_IFBLK, and S_IFIFO.  If S_IFCHR or S_IFBLK is set on mode,
    /// device defines the newly created device special file (probably using
    /// os.makedev()).  Otherwise device is ignored.
    ///
    /// If dir_fd is not None, it should be a file descriptor open to a directory,
    ///   and path should be relative; path will then be relative to that directory.
    /// dir_fd may not be implemented on your platform.
    ///   If it is unavailable, using it will raise a NotImplementedError.
    #[op]
    fn mknod(
        it: &mut Interp,
        #[kw] path: PathArg,
        #[kw]
        #[default(384)]
        mode: i32,
        #[kw]
        #[default(0)]
        device: u64,
        #[kwonly] dir_fd: Option<i32>,
    ) -> R<()> {
        let rp = at_path(it, dir_fd, &path)?;
        lumen_os::posix::mknod(&rp, mode as u32, device).map_err(|e| fs_err(it, e))
    }

    /// Extracts a device major number from a raw device number.
    #[op]
    fn major(device: u64) -> u32 {
        lumen_os::posix::major(device)
    }

    /// Extracts a device minor number from a raw device number.
    #[op]
    fn minor(device: u64) -> u32 {
        lumen_os::posix::minor(device)
    }

    /// Composes a raw device number from the major and minor device numbers.
    #[op]
    fn makedev(major: u32, minor: u32) -> u64 {
        lumen_os::posix::makedev(major, minor)
    }

    /// Force write of everything to disk.
    #[op]
    fn sync(it: &mut Interp) -> R<()> {
        lumen_os::posix::sync().map_err(|e| fs_err(it, e))
    }

    /// Apply, test or remove a POSIX lock on an open file descriptor.
    ///
    ///   fd
    ///     An open file descriptor.
    ///   command
    ///     One of F_LOCK, F_TLOCK, F_ULOCK or F_TEST.
    ///   length
    ///     The number of bytes to lock, starting at the current position.
    #[op]
    fn lockf(it: &mut Interp, fd: i32, command: i32, length: i64) -> R<()> {
        lumen_os::posix::lockf(fd, command, length).map_err(|e| fs_err(it, e))
    }

    /// Read into a buffer object from a file descriptor.
    ///
    /// The buffer should be mutable and bytes-like.  On success, returns the
    /// number of bytes read.  Less bytes may be read than the size of the
    /// buffer.  The underlying system call will be retried when interrupted by
    /// a signal, unless the signal handler raises an exception.  Other errors
    /// will not be retried and an error will be raised.
    ///
    /// Returns 0 if *fd* is at end of file or if the provided *buffer* has
    /// length 0 (which can be used to check for errors without reading data).
    /// Never returns negative.
    #[op]
    fn readinto(it: &mut Interp, fd: i32, buffer: &mut [u8]) -> R<usize> {
        it.wait_fd(fd, lumen_os::poll::POLLIN)?;
        let r = it.platform.borrow_mut().fd_read(fd, buffer, None);
        r.map_err(|e| os_err(it, e))
    }

    fn writable_lens(it: &mut Interp, fname: &str, buffers: &Value) -> R<Vec<(Value, usize)>> {
        if !is_sequence(buffers) {
            return Err(it.type_error(&format!("{fname}() arg 2 must be a sequence")));
        }
        let items = it.iterate_to_vec(buffers)?;
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            match crate::builtins::memview::with_writable(it, &item, |b| b.len())? {
                Some(n) => out.push((item, n)),
                None if it.is_buffer(&item) => {
                    return Err(it.new_exc_str("BufferError", "Object is not writable."));
                }
                None => {
                    let t = it.type_name_of(&item);
                    return Err(
                        it.type_error(&format!("a bytes-like object is required, not '{t}'"))
                    );
                }
            }
        }
        Ok(out)
    }

    /// Distributes the `n` bytes a vectored read produced over the buffers it filled, in order.
    fn scatter(
        it: &mut Interp,
        targets: &[(Value, usize)],
        filled: &[Vec<u8>],
        mut n: usize,
    ) -> R<()> {
        for ((item, _), data) in targets.iter().zip(filled) {
            if n == 0 {
                break;
            }
            let take = n.min(data.len());
            crate::builtins::memview::with_writable(it, item, |b| {
                b[..take].copy_from_slice(&data[..take])
            })?;
            n -= take;
        }
        Ok(())
    }

    fn gather(it: &mut Interp, fname: &str, buffers: &Value) -> R<Vec<Vec<u8>>> {
        if !is_sequence(buffers) {
            return Err(it.type_error(&format!("{fname}() arg 2 must be a sequence")));
        }
        let items = it.iterate_to_vec(buffers)?;
        items.iter().map(|i| it.buffer_bytes(i)).collect()
    }

    /// Read from a file descriptor fd into an iterable of buffers.
    ///
    /// The buffers should be mutable buffers accepting bytes.
    /// readv will transfer data into each buffer until it is full
    /// and then move on to the next buffer in the sequence to hold
    /// the rest of the data.
    ///
    /// readv returns the total number of bytes read,
    /// which may be less than the total capacity of all the buffers.
    #[op]
    fn readv(it: &mut Interp, fd: i32, buffers: &Value) -> R<usize> {
        let targets = writable_lens(it, "readv", buffers)?;
        let mut tmp: Vec<Vec<u8>> = targets.iter().map(|(_, n)| vec![0; *n]).collect();
        it.wait_fd(fd, lumen_os::poll::POLLIN)?;
        let n = lumen_os::posix::readv(fd, &mut tmp).map_err(|e| fs_err(it, e))?;
        scatter(it, &targets, &tmp, n)?;
        Ok(n)
    }

    /// Read from a file descriptor fd into a number of mutable bytes-like objects.
    ///
    /// Combines the functionality of readv() and pread(). As readv(), it will
    /// transfer data into each buffer until it is full and then move on to the next
    /// buffer in the sequence to hold the rest of the data. Its fourth argument,
    /// specifies the file offset at which the input operation is to be performed. It
    /// will return the total number of bytes read (which can be less than the total
    /// capacity of all the objects).
    ///
    /// The flags argument contains a bitwise OR of zero or more of the following flags:
    ///
    /// - RWF_HIPRI
    /// - RWF_NOWAIT
    ///
    /// Using non-zero flags requires Linux 4.6 or newer.
    #[op]
    fn preadv(
        it: &mut Interp,
        fd: i32,
        buffers: &Value,
        offset: i64,
        #[default(0)] flags: i32,
    ) -> R<usize> {
        if flags != 0 && !cfg!(any(target_os = "linux", target_os = "android")) {
            return Err(unavailable(it, "preadv2", "flags"));
        }
        let targets = writable_lens(it, "preadv", buffers)?;
        let mut tmp: Vec<Vec<u8>> = targets.iter().map(|(_, n)| vec![0; *n]).collect();
        let n = lumen_os::posix::preadv(fd, &mut tmp, offset, flags).map_err(|e| fs_err(it, e))?;
        scatter(it, &targets, &tmp, n)?;
        Ok(n)
    }

    /// Iterate over buffers, and write the contents of each to a file descriptor.
    ///
    /// Returns the total number of bytes written.
    /// buffers must be a sequence of bytes-like objects.
    #[op]
    fn writev(it: &mut Interp, fd: i32, buffers: &Value) -> R<usize> {
        let data = gather(it, "writev", buffers)?;
        let refs: Vec<&[u8]> = data.iter().map(|b| b.as_slice()).collect();
        lumen_os::posix::writev(fd, &refs).map_err(|e| fs_err(it, e))
    }

    /// Write bytes to a file descriptor starting at a particular offset.
    ///
    /// Combines the functionality of writev() and pwrite(). The flags argument
    /// contains a bitwise OR of zero or more of the following flags:
    ///
    /// - RWF_DSYNC
    /// - RWF_SYNC
    /// - RWF_APPEND
    ///
    /// Using non-zero flags requires Linux 4.7 or newer.
    #[op]
    fn pwritev(
        it: &mut Interp,
        fd: i32,
        buffers: &Value,
        offset: i64,
        #[default(0)] flags: i32,
    ) -> R<usize> {
        if flags != 0 && !cfg!(any(target_os = "linux", target_os = "android")) {
            return Err(unavailable(it, "pwritev2", "flags"));
        }
        let data = gather(it, "pwritev", buffers)?;
        let refs: Vec<&[u8]> = data.iter().map(|b| b.as_slice()).collect();
        lumen_os::posix::pwritev(fd, &refs, offset, flags).map_err(|e| fs_err(it, e))
    }

    /// Copy count bytes from file descriptor in_fd to file descriptor out_fd.
    #[op]
    fn sendfile(
        it: &mut Interp,
        #[kw] out_fd: i32,
        #[kw] in_fd: i32,
        #[kw] offset: &Value,
        #[kw] count: i64,
        #[kw] headers: Option<&Value>,
        #[kw] trailers: Option<&Value>,
        #[kw] flags: Option<i32>,
    ) -> R<Value> {
        if cfg!(target_vendor = "apple") {
            let off = it.index_of(offset)?;
            let mut parts = Vec::new();
            for (what, seq) in [("headers", headers), ("trailers", trailers)] {
                let bufs = match seq {
                    Some(s) if !matches!(s, Value::None) => {
                        if !is_sequence(s) {
                            return Err(
                                it.type_error(&format!("sendfile() {what} must be a sequence"))
                            );
                        }
                        it.iterate_to_vec(s)?
                            .iter()
                            .map(|i| it.buffer_bytes(i))
                            .collect::<R<Vec<_>>>()?
                    }
                    _ => Vec::new(),
                };
                parts.push(bufs);
            }
            let (h, t): (Vec<&[u8]>, Vec<&[u8]>) = (
                parts[0].iter().map(|b| b.as_slice()).collect(),
                parts[1].iter().map(|b| b.as_slice()).collect(),
            );
            let r = lumen_os::posix::sendfile_bsd(
                out_fd,
                in_fd,
                off,
                count,
                &h,
                &t,
                flags.unwrap_or(0),
            );
            return r.map(|n| Value::Int(n as i64)).map_err(|e| fs_err(it, e));
        }
        let off = match offset {
            Value::None => None,
            v => Some(it.index_of(v)?),
        };
        let r = lumen_os::posix::sendfile(out_fd, in_fd, off, count.max(0) as usize);
        r.map(|n| Value::Int(n as i64)).map_err(|e| fs_err(it, e))
    }

    /// Copy a file with the macOS `fcopyfile` call.
    #[op]
    fn _fcopyfile(it: &mut Interp, in_fd: i32, out_fd: i32, flags: i32) -> R<()> {
        lumen_os::posix::fcopyfile(in_fd, out_fd, flags as u32).map_err(|e| fs_err(it, e))
    }

    /// Run the input hook of the readline module.
    #[op]
    fn _inputhook() {}

    /// Report whether an input hook is installed.
    #[op]
    fn _is_inputhook_installed() -> bool {
        false
    }

    /// Create the environment dictionary.
    #[op]
    fn _create_environ(it: &mut Interp) -> Value {
        let env = it.new_dict();
        let vars = it.platform.borrow().environ();
        for (k, v) in vars {
            let _ = it.dict_set(&env, Value::bytes(k), Value::bytes(v));
        }
        Value::Obj(env)
    }

    // ---- lexical path helpers ------------------------------------------------------------------

    /// A `str`, `bytes` or `os.PathLike` as text (embedded NULs are fine: nothing is opened).
    fn nonstrict_path(it: &mut Interp, fname: &str, arg: &str, v: &Value) -> R<(String, bool)> {
        let is_text =
            v.as_str().is_some() || matches!(v, Value::Obj(o) if matches!(o.kind, Kind::Bytes(_)));
        if !is_text {
            let t = it.type_of(v);
            if it.lookup_mro(&t, "__fspath__").is_none() {
                let tn = it.type_name(&t);
                return Err(it.type_error(&format!(
                    "{fname}: {arg} should be string, bytes or os.PathLike, not {tn}"
                )));
            }
        }
        match fspath(it, v)? {
            Value::Obj(o) => match &o.kind {
                Kind::Str(s) => Ok((s.s.to_string(), false)),
                Kind::Bytes(b) => Ok((crate::bind::path::bytes_path(b), true)),
                _ => unreachable!("fspath returns str or bytes"),
            },
            _ => unreachable!("fspath returns str or bytes"),
        }
    }

    /// Normalize path, eliminating double slashes, etc.
    #[op]
    fn _path_normpath(it: &mut Interp, #[kw] path: &Value) -> R<Value> {
        let (p, bytes) = nonstrict_path(it, "_path_normpath", "path", path)?;
        Ok(wrap_path(bytes, lumen_common::pypath::normpath(&p)))
    }

    /// Split a pathname into drive, root and tail.
    ///
    /// The tail contains anything after the root.
    #[op]
    fn _path_splitroot_ex(it: &mut Interp, #[kw] p: &Value) -> R<Value> {
        let (text, bytes) = nonstrict_path(it, "_path_splitroot_ex", "p", p)?;
        let root = lumen_common::pypath::root_len(&text);
        Ok(Value::tuple(vec![
            wrap_path(bytes, String::new()),
            wrap_path(bytes, text[..root].to_string()),
            wrap_path(bytes, text[root..].to_string()),
        ]))
    }

    // ---- credentials and priorities ------------------------------------------------------------

    /// Set the current process's user id.
    #[op]
    fn setuid(it: &mut Interp, uid: &Value) -> R<()> {
        let uid = id_arg(it, uid, "uid")?;
        lumen_os::posix::setuid(uid).map_err(|e| fs_err(it, e))
    }

    /// Set the current process's effective user id.
    #[op]
    fn seteuid(it: &mut Interp, euid: &Value) -> R<()> {
        let id = id_arg(it, euid, "uid")?;
        lumen_os::posix::seteuid(id).map_err(|e| fs_err(it, e))
    }

    /// Set the current process's group id.
    #[op]
    fn setgid(it: &mut Interp, gid: &Value) -> R<()> {
        let id = id_arg(it, gid, "gid")?;
        lumen_os::posix::setgid(id).map_err(|e| fs_err(it, e))
    }

    /// Set the current process's effective group id.
    #[op]
    fn setegid(it: &mut Interp, egid: &Value) -> R<()> {
        let id = id_arg(it, egid, "gid")?;
        lumen_os::posix::setegid(id).map_err(|e| fs_err(it, e))
    }

    /// Set the current process's real and effective user ids.
    #[op]
    fn setreuid(it: &mut Interp, ruid: &Value, euid: &Value) -> R<()> {
        let (r, e) = (id_arg(it, ruid, "uid")?, id_arg(it, euid, "uid")?);
        lumen_os::posix::setreuid(r, e).map_err(|e| fs_err(it, e))
    }

    /// Set the current process's real and effective group ids.
    #[op]
    fn setregid(it: &mut Interp, rgid: &Value, egid: &Value) -> R<()> {
        let (r, e) = (id_arg(it, rgid, "gid")?, id_arg(it, egid, "gid")?);
        lumen_os::posix::setregid(r, e).map_err(|e| fs_err(it, e))
    }

    /// Set the current process's real, effective, and saved user ids.
    #[op]
    fn setresuid(it: &mut Interp, ruid: &Value, euid: &Value, suid: &Value) -> R<()> {
        let (r, e, s) = (
            id_arg(it, ruid, "uid")?,
            id_arg(it, euid, "uid")?,
            id_arg(it, suid, "uid")?,
        );
        lumen_os::posix::setresuid(r, e, s).map_err(|e| fs_err(it, e))
    }

    /// Set the current process's real, effective, and saved group ids.
    #[op]
    fn setresgid(it: &mut Interp, rgid: &Value, egid: &Value, sgid: &Value) -> R<()> {
        let (r, e, s) = (
            id_arg(it, rgid, "gid")?,
            id_arg(it, egid, "gid")?,
            id_arg(it, sgid, "gid")?,
        );
        lumen_os::posix::setresgid(r, e, s).map_err(|e| fs_err(it, e))
    }

    /// Return a tuple of the current process's real, effective, and saved user ids.
    #[op]
    fn getresuid(it: &mut Interp) -> R<Value> {
        let ids = lumen_os::posix::getresuid().map_err(|e| fs_err(it, e))?;
        Ok(Value::tuple(
            ids.iter().map(|&i| Value::Int(i as i64)).collect(),
        ))
    }

    /// Return a tuple of the current process's real, effective, and saved group ids.
    #[op]
    fn getresgid(it: &mut Interp) -> R<Value> {
        let ids = lumen_os::posix::getresgid().map_err(|e| fs_err(it, e))?;
        Ok(Value::tuple(
            ids.iter().map(|&i| Value::Int(i as i64)).collect(),
        ))
    }

    /// Set the groups of the current process to list.
    #[op]
    fn setgroups(it: &mut Interp, groups: &Value) -> R<()> {
        if !is_sequence(groups) {
            return Err(it.type_error("setgroups argument must be a sequence"));
        }
        let items = it.iterate_to_vec(groups)?;
        let max = lumen_os::posix::limits()
            .iter()
            .find(|(n, _)| *n == "NGROUPS_MAX")
            .map_or(65536, |(_, v)| *v) as usize;
        if items.len() > max {
            return Err(it.value_error("too many groups"));
        }
        let gids = items
            .iter()
            .map(|g| id_arg(it, g, "gid"))
            .collect::<R<Vec<_>>>()?;
        lumen_os::ident::setgroups(&gids).map_err(|e| fs_err(it, e))
    }

    /// Initialize the group access list.
    ///
    /// Call the system initgroups() to initialize the group access list with all of
    /// the groups of which the specified username is a member, plus the specified
    /// group id.
    #[op]
    fn initgroups(it: &mut Interp, username: &str, gid: &Value) -> R<()> {
        let gid = id_arg(it, gid, "gid")?;
        lumen_os::ident::initgroups(username, gid).map_err(|e| fs_err(it, e))
    }

    /// Returns a list of groups to which a user belongs.
    ///
    ///   user
    ///     username to lookup
    ///   group
    ///     base group id of the user
    #[op]
    fn getgrouplist(it: &mut Interp, user: &str, group: &Value) -> R<Value> {
        let group = id_arg(it, group, "gid")?;
        let groups = lumen_os::posix::getgrouplist(user, group).map_err(|e| fs_err(it, e))?;
        Ok(Value::list(
            groups.into_iter().map(|g| Value::Int(g as i64)).collect(),
        ))
    }

    /// Make the current process a session leader.
    #[op]
    fn setpgrp(it: &mut Interp) -> R<()> {
        lumen_os::posix::setpgrp().map_err(|e| fs_err(it, e))
    }

    /// Return program scheduling priority.
    #[op]
    fn getpriority(it: &mut Interp, #[kw] which: i32, #[kw] who: i64) -> R<i32> {
        lumen_os::posix::getpriority(which, who as u32).map_err(|e| fs_err(it, e))
    }

    /// Set program scheduling priority.
    #[op]
    fn setpriority(
        it: &mut Interp,
        #[kw] which: i32,
        #[kw] who: i64,
        #[kw] priority: i32,
    ) -> R<()> {
        lumen_os::posix::setpriority(which, who as u32, priority).map_err(|e| fs_err(it, e))
    }

    /// Add increment to the priority of process and return the new priority.
    #[op]
    fn nice(it: &mut Interp, increment: i32) -> R<i32> {
        lumen_os::posix::nice(increment).map_err(|e| fs_err(it, e))
    }

    /// Return average recent system load information.
    ///
    /// Return the number of processes in the system run queue averaged over
    /// the last 1, 5, and 15 minutes as a tuple of three floats.
    /// Raises OSError if the load average was unobtainable.
    #[op]
    fn getloadavg(it: &mut Interp) -> R<Value> {
        match lumen_os::posix::getloadavg() {
            Some(a) => Ok(Value::tuple(a.iter().map(|&f| Value::Float(f)).collect())),
            None => Err(it.new_exc_str("OSError", "Load averages are unobtainable")),
        }
    }

    // ---- configuration values ------------------------------------------------------------------

    /// Return a string-valued system configuration variable.
    #[op]
    fn confstr(it: &mut Interp, name: &Value) -> R<Value> {
        let n = conf_name(it, name, lumen_os::posix::confstr_names())?;
        match lumen_os::posix::confstr(n).map_err(|e| fs_err(it, e))? {
            Some(s) => Ok(Value::string(s)),
            None => Ok(Value::None),
        }
    }

    /// Return the configuration limit name for the file or directory path.
    ///
    /// If there is no limit, return -1.
    /// On some platforms, path may also be specified as an open file descriptor.
    ///   If this functionality is unavailable, using it raises an exception.
    #[op]
    fn pathconf(it: &mut Interp, #[kw] path: PathOrFd, #[kw] name: &Value) -> R<i64> {
        let n = conf_name(it, name, lumen_os::posix::pathconf_names())?;
        match path.fd {
            Some(fd) => lumen_os::posix::fpathconf(fd, n).map_err(|e| fs_err(it, e)),
            None => lumen_os::posix::pathconf(&path.path, n).map_err(|e| fs_path_err(it, e, &path)),
        }
    }

    /// Return the configuration limit name for the file descriptor fd.
    ///
    /// If there is no limit, return -1.
    #[op]
    fn fpathconf(it: &mut Interp, fd: &Value, name: &Value) -> R<i64> {
        let fd = fdv(it, fd)?;
        let n = conf_name(it, name, lumen_os::posix::pathconf_names())?;
        lumen_os::posix::fpathconf(fd, n).map_err(|e| fs_err(it, e))
    }

    // ---- statvfs -------------------------------------------------------------------------------

    struct StatvfsResult;

    const STATVFS_FIELDS: [&str; 11] = [
        "f_bsize",
        "f_frsize",
        "f_blocks",
        "f_bfree",
        "f_bavail",
        "f_files",
        "f_ffree",
        "f_favail",
        "f_flag",
        "f_namemax",
        "f_fsid",
    ];

    fn statvfs_type(it: &mut Interp) -> Obj {
        structseq_type::<StatvfsResult>(it, "os", "statvfs_result", &STATVFS_FIELDS, 10)
    }

    fn statvfs_value(it: &mut Interp, s: &lumen_os::posix::StatVfs) -> Value {
        let ty = statvfs_type(it);
        let vals = [
            s.bsize, s.frsize, s.blocks, s.bfree, s.bavail, s.files, s.ffree, s.favail, s.flag,
            s.namemax, s.fsid,
        ];
        structseq_full(&ty, vals.iter().map(|&n| uint(n)).collect())
    }

    /// Perform a statvfs system call on the given path.
    ///
    ///   path
    ///     Path to be examined; can be string, bytes, a path-like object or
    ///     open-file-descriptor int.
    ///
    /// statvfs() is an alias for os.statvfs().
    #[op]
    fn statvfs(it: &mut Interp, #[kw] path: PathOrFd) -> R<Value> {
        let r = match path.fd {
            Some(fd) => lumen_os::posix::fstatvfs(fd),
            None => lumen_os::posix::statvfs(&path.path),
        };
        match r {
            Ok(s) => Ok(statvfs_value(it, &s)),
            Err(e) => Err(fs_path_err(it, e, &path)),
        }
    }

    /// Perform an fstatvfs system call on the given fd.
    ///
    /// Equivalent to statvfs(fd).
    #[op]
    fn fstatvfs(it: &mut Interp, fd: &Value) -> R<Value> {
        let fd = fdv(it, fd)?;
        let s = lumen_os::posix::fstatvfs(fd).map_err(|e| fs_err(it, e))?;
        Ok(statvfs_value(it, &s))
    }

    // ---- terminals -----------------------------------------------------------------------------

    /// Return the process group associated with the terminal specified by fd.
    #[op]
    fn tcgetpgrp(it: &mut Interp, fd: i32) -> R<i32> {
        lumen_os::posix::tcgetpgrp(fd).map_err(|e| fs_err(it, e))
    }

    /// Set the process group associated with the terminal specified by fd.
    #[op]
    fn tcsetpgrp(it: &mut Interp, fd: i32, pgid: i32) -> R<()> {
        lumen_os::posix::tcsetpgrp(fd, pgid).map_err(|e| fs_err(it, e))
    }

    /// Return the name of the controlling terminal for this process.
    #[op]
    fn ctermid(it: &mut Interp) -> R<String> {
        lumen_os::posix::ctermid().map_err(|e| fs_err(it, e))
    }

    /// Return the name of the terminal device connected to 'fd'.
    ///
    ///   fd
    ///     Integer file descriptor handle.
    #[op]
    fn ttyname(it: &mut Interp, fd: i32) -> R<String> {
        lumen_os::posix::ttyname(fd).map_err(|e| fs_err(it, e))
    }

    /// Grant access to the slave pseudo-terminal device.
    ///
    ///   fd
    ///     File descriptor of a master pseudo-terminal device.
    ///
    /// Performs a grantpt() C function call.
    #[op]
    fn grantpt(it: &mut Interp, fd: &Value) -> R<()> {
        let fd = fdv(it, fd)?;
        lumen_os::posix::grantpt(fd).map_err(|e| fs_err(it, e))
    }

    /// Unlock a pseudo-terminal master/slave pair.
    ///
    ///   fd
    ///     File descriptor of a master pseudo-terminal device.
    ///
    /// Performs an unlockpt() C function call.
    #[op]
    fn unlockpt(it: &mut Interp, fd: &Value) -> R<()> {
        let fd = fdv(it, fd)?;
        lumen_os::posix::unlockpt(fd).map_err(|e| fs_err(it, e))
    }

    /// Open and return a file descriptor for a master pseudo-terminal device.
    ///
    ///   oflag
    ///     Bitwise OR of the following flags: O_RDWR, O_NOCTTY, O_CLOEXEC.
    ///
    /// Performs a posix_openpt() C function call. The oflag argument is used to
    /// set file status flags and file access modes as specified in the manual page
    /// of the C function.
    #[op]
    fn posix_openpt(it: &mut Interp, oflag: i32) -> R<i32> {
        lumen_os::posix::posix_openpt(oflag).map_err(|e| fs_err(it, e))
    }

    /// Return the name of the slave pseudo-terminal device.
    ///
    ///   fd
    ///     File descriptor of a master pseudo-terminal device.
    ///
    /// If the ptsname_r() C function is available, it is called;
    /// otherwise, performs a ptsname() C function call.
    #[op]
    fn ptsname(it: &mut Interp, fd: &Value) -> R<String> {
        let fd = fdv(it, fd)?;
        lumen_os::posix::ptsname(fd).map_err(|e| fs_err(it, e))
    }

    // ---- scheduling ----------------------------------------------------------------------------

    /// Voluntarily relinquish the CPU.
    #[op]
    fn sched_yield(it: &mut Interp) -> R<()> {
        lumen_os::posix::sched_yield().map_err(|e| fs_err(it, e))
    }

    /// Get the maximum scheduling priority for policy.
    #[op]
    fn sched_get_priority_max(it: &mut Interp, #[kw] policy: i32) -> R<i32> {
        lumen_os::posix::sched_get_priority_max(policy).map_err(|e| fs_err(it, e))
    }

    /// Get the minimum scheduling priority for policy.
    #[op]
    fn sched_get_priority_min(it: &mut Interp, #[kw] policy: i32) -> R<i32> {
        lumen_os::posix::sched_get_priority_min(policy).map_err(|e| fs_err(it, e))
    }

    struct SchedParam;

    fn sched_param_type(it: &mut Interp) -> Obj {
        structseq_type::<SchedParam>(it, "posix", "sched_param", &["sched_priority"], 1)
    }

    /// The priority of an `os.sched_param`.
    fn sched_priority(it: &mut Interp, v: &Value) -> R<i32> {
        let ty = sched_param_type(it);
        if !it.isinstance_value(v, &Value::Obj(ty))? {
            return Err(it.type_error("must have a sched_param object"));
        }
        let p = v
            .tuple_items()
            .and_then(|t| t.first().cloned())
            .unwrap_or(Value::Int(0));
        let n = it.index_of(&p)?;
        i32::try_from(n).map_err(|_| it.overflow_err("Python int too large to convert to C int"))
    }

    fn sched_param_value(it: &mut Interp, priority: i32) -> Value {
        let ty = sched_param_type(it);
        structseq_full(&ty, vec![Value::Int(priority as i64)])
    }

    /// Get the scheduling policy for the process identified by pid.
    ///
    /// Passing 0 for pid returns the scheduling policy for the calling process.
    #[op]
    fn sched_getscheduler(it: &mut Interp, pid: i32) -> R<i32> {
        lumen_os::posix::sched_getscheduler(pid).map_err(|e| fs_err(it, e))
    }

    /// Set the scheduling policy for the process identified by pid.
    ///
    /// If pid is 0, the calling process is changed.
    /// param is an instance of sched_param.
    #[op]
    fn sched_setscheduler(it: &mut Interp, pid: i32, policy: i32, param: &Value) -> R<()> {
        let prio = sched_priority(it, param)?;
        lumen_os::posix::sched_setscheduler(pid, policy, prio).map_err(|e| fs_err(it, e))
    }

    /// Returns scheduling parameters for the process identified by pid.
    ///
    /// If pid is 0, returns parameters for the calling process.
    /// Return value is an instance of sched_param.
    #[op]
    fn sched_getparam(it: &mut Interp, pid: i32) -> R<Value> {
        let prio = lumen_os::posix::sched_getparam(pid).map_err(|e| fs_err(it, e))?;
        Ok(sched_param_value(it, prio))
    }

    /// Set scheduling parameters for the process identified by pid.
    ///
    /// If pid is 0, sets parameters for the calling process.
    /// param should be an instance of sched_param.
    #[op]
    fn sched_setparam(it: &mut Interp, pid: i32, param: &Value) -> R<()> {
        let prio = sched_priority(it, param)?;
        lumen_os::posix::sched_setparam(pid, prio).map_err(|e| fs_err(it, e))
    }

    /// Return the round-robin quantum for the process identified by pid, in seconds.
    ///
    /// Value returned is a float.
    #[op]
    fn sched_rr_get_interval(it: &mut Interp, pid: i32) -> R<f64> {
        lumen_os::posix::sched_rr_get_interval(pid).map_err(|e| fs_err(it, e))
    }

    /// Return the affinity of the process identified by pid (or the current process if zero).
    ///
    /// The affinity is returned as a set of CPU identifiers.
    #[op]
    fn sched_getaffinity(it: &mut Interp, pid: i32) -> R<Value> {
        let cpus = lumen_os::posix::sched_getaffinity(pid).map_err(|e| fs_err(it, e))?;
        let set = Value::Obj(it.types.set.clone());
        let list = Value::list(cpus.into_iter().map(|c| Value::Int(c as i64)).collect());
        it.call(&set, vec![list], Vec::new())
    }

    /// Set the CPU affinity of the process identified by pid to mask.
    ///
    /// mask should be an iterable of integers identifying CPUs.
    #[op]
    fn sched_setaffinity(it: &mut Interp, pid: i32, mask: &Value) -> R<()> {
        let mut cpus = Vec::new();
        for c in it.iterate_to_vec(mask)? {
            let n = it.index_of(&c)?;
            if n < 0 {
                return Err(it.value_error("negative CPU number"));
            }
            cpus.push(n as usize);
        }
        lumen_os::posix::sched_setaffinity(pid, &cpus).map_err(|e| fs_err(it, e))
    }

    // ---- Linux-only calls ----------------------------------------------------------------------

    fn opt_off(it: &mut Interp, v: Option<&Value>) -> R<Option<i64>> {
        match v {
            None | Some(Value::None) => Ok(None),
            Some(v) => Ok(Some(it.index_of(v)?)),
        }
    }

    /// Force write of fd to disk without forcing update of metadata.
    #[op]
    fn fdatasync(it: &mut Interp, #[kw] fd: &Value) -> R<()> {
        let fd = fdv(it, fd)?;
        lumen_os::posix::fdatasync(fd).map_err(|e| fs_err(it, e))
    }

    /// Create a pipe with flags set atomically.
    ///
    /// Returns a tuple of two file descriptors:
    ///   (read_fd, write_fd)
    ///
    /// flags can be constructed by ORing together one or more of these values:
    /// O_NONBLOCK, O_CLOEXEC.
    #[op]
    fn pipe2(it: &mut Interp, flags: i32) -> R<(i32, i32)> {
        lumen_os::posix::pipe2(flags).map_err(|e| fs_err(it, e))
    }

    /// Ensure a file has allocated at least a particular number of bytes on disk.
    ///
    /// Ensure that the file specified by fd encompasses a range of bytes
    /// starting at offset bytes from the beginning and continuing for length
    /// bytes.
    #[op]
    fn posix_fallocate(it: &mut Interp, fd: i32, offset: i64, length: i64) -> R<()> {
        lumen_os::posix::posix_fallocate(fd, offset, length).map_err(|e| fs_err(it, e))
    }

    /// Announce an intention to access data in a specific pattern.
    ///
    /// Announce an intention to access data in a specific pattern, thus
    /// allowing the kernel to make optimizations.
    /// The advice applies to the region of the file specified by fd starting at
    /// offset and continuing for length bytes.
    /// advice is one of POSIX_FADV_NORMAL, POSIX_FADV_SEQUENTIAL,
    /// POSIX_FADV_RANDOM, POSIX_FADV_NOREUSE, POSIX_FADV_WILLNEED, or
    /// POSIX_FADV_DONTNEED.
    #[op]
    fn posix_fadvise(it: &mut Interp, fd: i32, offset: i64, length: i64, advice: i32) -> R<()> {
        lumen_os::posix::posix_fadvise(fd, offset, length, advice).map_err(|e| fs_err(it, e))
    }

    /// Obtain a series of random bytes.
    #[op]
    fn getrandom(it: &mut Interp, size: isize, #[default(0)] flags: u32) -> R<Value> {
        if size < 0 {
            return Err(it.value_error("negative argument not allowed"));
        }
        lumen_os::posix::getrandom(size as usize, flags)
            .map(Value::bytes)
            .map_err(|e| fs_err(it, e))
    }

    /// Create an anonymous file.
    #[op]
    fn memfd_create(
        it: &mut Interp,
        #[kw] name: &Value,
        #[kw]
        #[default(lumen_os::posix::MFD_CLOEXEC)]
        flags: u32,
    ) -> R<i32> {
        let n = crate::bind::path::fs_bytes(it, name)?;
        lumen_os::posix::memfd_create(&String::from_utf8_lossy(&n), flags)
            .map_err(|e| fs_err(it, e))
    }

    /// Creates and returns an event notification file descriptor.
    #[op]
    fn eventfd(
        it: &mut Interp,
        initval: u32,
        #[default(lumen_os::posix::EFD_CLOEXEC)] flags: i32,
    ) -> R<i32> {
        lumen_os::posix::eventfd(initval, flags).map_err(|e| fs_err(it, e))
    }

    /// Read eventfd value
    #[op]
    fn eventfd_read(it: &mut Interp, fd: &Value) -> R<Value> {
        let fd = fdv(it, fd)?;
        it.wait_fd(fd, lumen_os::poll::POLLIN)?;
        lumen_os::posix::eventfd_read(fd)
            .map(uint)
            .map_err(|e| fs_err(it, e))
    }

    /// Write eventfd value.
    #[op]
    fn eventfd_write(it: &mut Interp, fd: &Value, value: u64) -> R<()> {
        let fd = fdv(it, fd)?;
        lumen_os::posix::eventfd_write(fd, value).map_err(|e| fs_err(it, e))
    }

    /// Return a file descriptor referring to the process *pid*.
    ///
    /// The descriptor can be used to perform process management without races
    /// and signals.
    #[op]
    fn pidfd_open(
        it: &mut Interp,
        #[kw] pid: i32,
        #[kw]
        #[default(0)]
        flags: u32,
    ) -> R<i32> {
        lumen_os::posix::pidfd_open(pid, flags).map_err(|e| fs_err(it, e))
    }

    /// Disassociate parts of a process (or thread) execution context.
    ///
    ///   flags
    ///     Namespaces to be unshared.
    #[op]
    fn unshare(it: &mut Interp, #[kw] flags: i32) -> R<()> {
        lumen_os::posix::unshare(flags).map_err(|e| fs_err(it, e))
    }

    /// Move the calling thread into different namespaces.
    ///
    ///   fd
    ///     A file descriptor to a namespace.
    ///   nstype
    ///     Type of namespace.
    #[op]
    fn setns(
        it: &mut Interp,
        #[kw] fd: &Value,
        #[kw]
        #[default(0)]
        nstype: i32,
    ) -> R<()> {
        let fd = fdv(it, fd)?;
        lumen_os::posix::setns(fd, nstype).map_err(|e| fs_err(it, e))
    }

    /// Copy count bytes from one file descriptor to another.
    ///
    ///   src
    ///     Source file descriptor.
    ///   dst
    ///     Destination file descriptor.
    ///   count
    ///     Number of bytes to copy.
    ///   offset_src
    ///     Starting offset in src.
    ///   offset_dst
    ///     Starting offset in dst.
    ///
    /// If offset_src is None, then src is read from the current position;
    /// respectively for offset_dst.
    #[op]
    fn copy_file_range(
        it: &mut Interp,
        #[kw] src: i32,
        #[kw] dst: i32,
        #[kw] count: isize,
        #[kw] offset_src: Option<&Value>,
        #[kw] offset_dst: Option<&Value>,
    ) -> R<usize> {
        if count < 0 {
            return Err(it.value_error("negative value for 'count' not allowed"));
        }
        let (os, od) = (opt_off(it, offset_src)?, opt_off(it, offset_dst)?);
        lumen_os::posix::copy_file_range(src, dst, count as usize, os, od)
            .map_err(|e| fs_err(it, e))
    }

    /// Transfer count bytes from one pipe to a descriptor or vice versa.
    ///
    ///   src
    ///     Source file descriptor.
    ///   dst
    ///     Destination file descriptor.
    ///   count
    ///     Number of bytes to copy.
    ///   offset_src
    ///     Starting offset in src.
    ///   offset_dst
    ///     Starting offset in dst.
    ///   flags
    ///     Flags to modify the semantics of the call.
    ///
    /// If both descriptors are not pipes, OSError is raised.
    #[op]
    fn splice(
        it: &mut Interp,
        #[kw] src: i32,
        #[kw] dst: i32,
        #[kw] count: isize,
        #[kw] offset_src: Option<&Value>,
        #[kw] offset_dst: Option<&Value>,
        #[kw]
        #[default(0)]
        flags: u32,
    ) -> R<usize> {
        if count < 0 {
            return Err(it.value_error("negative value for 'count' not allowed"));
        }
        let (os, od) = (opt_off(it, offset_src)?, opt_off(it, offset_dst)?);
        lumen_os::posix::splice(src, dst, count as usize, os, od, flags).map_err(|e| fs_err(it, e))
    }

    /// Create and return a timer file descriptor.
    ///
    ///   clockid
    ///     A valid clock ID constant as timer file descriptor.
    ///   flags
    ///     0 or a bit mask of os.TFD_NONBLOCK or os.TFD_CLOEXEC.
    #[op]
    fn timerfd_create(
        it: &mut Interp,
        clockid: i32,
        #[kwonly]
        #[default(0)]
        flags: i32,
    ) -> R<i32> {
        lumen_os::posix::timerfd_create(clockid, flags | lumen_os::posix::TFD_CLOEXEC)
            .map_err(|e| fs_err(it, e))
    }

    fn ns_of_seconds(it: &mut Interp, secs: f64, what: &str) -> R<i64> {
        let ns = (secs * 1e9).floor();
        if !ns.is_finite() || ns.abs() >= 9.2e18 {
            return Err(it.overflow_err(&format!(
                "timestamp too large to convert to C _PyTime_t: {what}"
            )));
        }
        Ok(ns as i64)
    }

    fn timer_pair_secs(a: (i64, i64)) -> Value {
        Value::tuple(vec![
            Value::Float(a.0 as f64 * 1e-9),
            Value::Float(a.1 as f64 * 1e-9),
        ])
    }

    fn timer_pair_ns(a: (i64, i64)) -> Value {
        Value::tuple(vec![Value::Int(a.0), Value::Int(a.1)])
    }

    /// Alter a timer file descriptor's internal timer in seconds.
    ///
    ///   fd
    ///     A timer file descriptor.
    ///   flags
    ///     0 or a bit mask of TFD_TIMER_ABSTIME or TFD_TIMER_CANCEL_ON_SET.
    ///   initial
    ///     The initial expiration time, in seconds.
    ///   interval
    ///     The timer's interval, in seconds.
    #[op]
    fn timerfd_settime(
        it: &mut Interp,
        fd: &Value,
        #[kwonly]
        #[default(0)]
        flags: i32,
        #[kwonly]
        #[default(0.0)]
        initial: f64,
        #[kwonly]
        #[default(0.0)]
        interval: f64,
    ) -> R<Value> {
        let fd = fdv(it, fd)?;
        let (i, v) = (
            ns_of_seconds(it, initial, "initial")?,
            ns_of_seconds(it, interval, "interval")?,
        );
        let old = lumen_os::posix::timerfd_settime(fd, flags, i, v).map_err(|e| fs_err(it, e))?;
        Ok(timer_pair_secs(old))
    }

    /// Alter a timer file descriptor's internal timer in nanoseconds.
    ///
    ///   fd
    ///     A timer file descriptor.
    ///   flags
    ///     0 or a bit mask of TFD_TIMER_ABSTIME or TFD_TIMER_CANCEL_ON_SET.
    ///   initial
    ///     initial expiration timing in nanoseconds.
    ///   interval
    ///     interval for the timer in nanoseconds.
    #[op]
    fn timerfd_settime_ns(
        it: &mut Interp,
        fd: &Value,
        #[kwonly]
        #[default(0)]
        flags: i32,
        #[kwonly]
        #[default(0)]
        initial: i64,
        #[kwonly]
        #[default(0)]
        interval: i64,
    ) -> R<Value> {
        let fd = fdv(it, fd)?;
        let old = lumen_os::posix::timerfd_settime(fd, flags, initial, interval)
            .map_err(|e| fs_err(it, e))?;
        Ok(timer_pair_ns(old))
    }

    /// Return a tuple of a timer file descriptor's (next expiration, interval) in float seconds.
    ///
    ///   fd
    ///     A timer file descriptor.
    #[op]
    fn timerfd_gettime(it: &mut Interp, fd: &Value) -> R<Value> {
        let fd = fdv(it, fd)?;
        lumen_os::posix::timerfd_gettime(fd)
            .map(timer_pair_secs)
            .map_err(|e| fs_err(it, e))
    }

    /// Return a tuple of a timer file descriptor's (next expiration, interval) in nanoseconds.
    ///
    ///   fd
    ///     A timer file descriptor.
    #[op]
    fn timerfd_gettime_ns(it: &mut Interp, fd: &Value) -> R<Value> {
        let fd = fdv(it, fd)?;
        lumen_os::posix::timerfd_gettime(fd)
            .map(timer_pair_ns)
            .map_err(|e| fs_err(it, e))
    }

    /// Return the value of extended attribute attribute on path.
    ///
    /// path may be either a string, a path-like object, or an open file
    /// descriptor.
    /// If follow_symlinks is False, and the last element of the path is
    /// a symbolic link, getxattr will examine the symbolic link itself
    /// instead of the file the link points to.
    #[op]
    fn getxattr(
        it: &mut Interp,
        #[kw] path: PathArg,
        #[kw] attribute: PathArg,
        #[kwonly]
        #[default(true)]
        follow_symlinks: bool,
    ) -> R<Value> {
        let r = lumen_os::posix::getxattr(&path.path, &attribute.path, follow_symlinks);
        r.map(Value::bytes).map_err(|e| fs_path_err(it, e, &path))
    }

    /// Set extended attribute attribute on path to value.
    ///
    /// path may be either a string, a path-like object,  or an open file descriptor.
    /// If follow_symlinks is False, and the last element of the path is a symbolic
    ///   link, setxattr will modify the symbolic link itself instead of the file
    ///   the link points to.
    #[op]
    fn setxattr(
        it: &mut Interp,
        #[kw] path: PathArg,
        #[kw] attribute: PathArg,
        #[kw] value: &[u8],
        #[kw]
        #[default(0)]
        flags: i32,
        #[kwonly]
        #[default(true)]
        follow_symlinks: bool,
    ) -> R<()> {
        let r =
            lumen_os::posix::setxattr(&path.path, &attribute.path, value, flags, follow_symlinks);
        r.map_err(|e| fs_path_err(it, e, &path))
    }

    /// Remove extended attribute attribute on path.
    ///
    /// path may be either a string, a path-like object, or an open file descriptor.
    /// If follow_symlinks is False, and the last element of the path is a symbolic
    ///   link, removexattr will modify the symbolic link itself instead of the file
    ///   the link points to.
    #[op]
    fn removexattr(
        it: &mut Interp,
        #[kw] path: PathArg,
        #[kw] attribute: PathArg,
        #[kwonly]
        #[default(true)]
        follow_symlinks: bool,
    ) -> R<()> {
        let r = lumen_os::posix::removexattr(&path.path, &attribute.path, follow_symlinks);
        r.map_err(|e| fs_path_err(it, e, &path))
    }

    /// Return a list of extended attributes on path.
    ///
    /// path may be either None, a string, a path-like object, or a file descriptor.
    /// If path is None, listxattr will examine the current directory.
    /// If follow_symlinks is False, and the last element of the path is a symbolic
    ///   link, listxattr will examine the symbolic link itself instead of the file
    ///   the link points to.
    #[op]
    fn listxattr(
        it: &mut Interp,
        #[kw] path: Option<&Value>,
        #[kwonly]
        #[default(true)]
        follow_symlinks: bool,
    ) -> R<Value> {
        let p: PathArg = convert_path(
            it,
            "listxattr",
            "path",
            path.unwrap_or(&Value::None),
            false,
            true,
        )?;
        let names = lumen_os::posix::listxattr(&p.path, follow_symlinks)
            .map_err(|e| fs_path_err(it, e, &p))?;
        Ok(Value::list(names.into_iter().map(|n| p.wrap(n)).collect()))
    }

    const HAVE_FUNCTIONS: [&str; 26] = [
        "HAVE_FACCESSAT",
        "HAVE_FCHMODAT",
        "HAVE_FCHOWNAT",
        "HAVE_FDOPENDIR",
        "HAVE_FSTATAT",
        "HAVE_FUTIMENS",
        "HAVE_LINKAT",
        "HAVE_MKDIRAT",
        "HAVE_MKFIFOAT",
        "HAVE_MKNODAT",
        "HAVE_OPENAT",
        "HAVE_READLINKAT",
        "HAVE_RENAMEAT",
        "HAVE_SYMLINKAT",
        "HAVE_UNLINKAT",
        "HAVE_UTIMENSAT",
        "HAVE_FCHDIR",
        "HAVE_FCHMOD",
        "HAVE_FCHOWN",
        "HAVE_FPATHCONF",
        "HAVE_FSTATVFS",
        "HAVE_FTRUNCATE",
        "HAVE_FUTIMES",
        "HAVE_LCHOWN",
        "HAVE_LSTAT",
        "HAVE_LUTIMES",
    ];

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
        let mut have: Vec<&str> = HAVE_FUNCTIONS.to_vec();
        if cfg!(target_vendor = "apple") {
            have.extend(["HAVE_LCHFLAGS", "HAVE_LCHMOD"]);
        }
        dict_set_str(
            &d,
            "_have_functions",
            Value::list(have.iter().map(|n| Value::str(n)).collect()),
        );
        dict_set_str(&d, "waitid_result", Value::Obj(waitid_type(it)));
        dict_set_str(&d, "statvfs_result", Value::Obj(statvfs_type(it)));
        for (name, v) in lumen_os::posix::limits()
            .into_iter()
            .chain(lumen_os::posix::sched_policies())
            .chain(lumen_os::posix::statvfs_flags())
            .chain(lumen_os::posix::copyfile_flags())
            .chain(lumen_os::posix::rw_flags())
            .chain(lumen_os::posix::linux_constants())
        {
            dict_set_str(&d, name, Value::Int(v));
        }
        for (name, v) in [
            ("POSIX_SPAWN_OPEN", 0),
            ("POSIX_SPAWN_CLOSE", 1),
            ("POSIX_SPAWN_DUP2", 2),
        ] {
            dict_set_str(&d, name, Value::Int(v));
        }
        if lumen_os::posix::spawn_has_closefrom() {
            dict_set_str(
                &d,
                "POSIX_SPAWN_CLOSEFROM",
                Value::Int(lumen_os::posix::SPAWN_CLOSEFROM),
            );
        }
        dict_set_str(
            &d,
            "confstr_names",
            names_dict(it, lumen_os::posix::confstr_names()),
        );
        dict_set_str(
            &d,
            "pathconf_names",
            names_dict(it, lumen_os::posix::pathconf_names()),
        );
        if cfg!(any(target_os = "linux", target_os = "android")) {
            dict_set_str(&d, "sched_param", Value::Obj(sched_param_type(it)));
        } else {
            for name in lumen_os::posix::LINUX_ONLY {
                dict_del_str(&d, name);
            }
        }
        if !cfg!(target_vendor = "apple") {
            for name in [
                "chflags",
                "lchflags",
                "lchmod",
                "_fcopyfile",
                "_inputhook",
                "_is_inputhook_installed",
            ] {
                dict_del_str(&d, name);
            }
        }
    }
}
