//! `_posixsubprocess` on `lumen_os::spawn`: argument conversion only; the child side is shared.

/// A POSIX helper for the subprocess module.
#[lumen_bind::module(name = "_posixsubprocess")]
pub mod _posixsubprocess {
    use crate::bind::path::fs_bytes;
    use crate::object::*;
    use crate::vm::Interp;
    use lumen_os::spawn::ForkExec;

    fn tuple_items(v: &Value) -> Option<&[Value]> {
        match v {
            Value::Obj(o) => match &o.kind {
                Kind::Tuple(items) => Some(items),
                _ => None,
            },
            _ => None,
        }
    }

    fn byte_list(it: &mut Interp, v: &Value) -> R<Vec<Vec<u8>>> {
        let items = it.iterate_to_vec(v)?;
        items.iter().map(|i| fs_bytes(it, i)).collect()
    }

    fn id_arg(it: &mut Interp, v: &Value, what: &str) -> R<Option<u32>> {
        if matches!(v, Value::None) {
            return Ok(None);
        }
        let n = it.index_of(v)?;
        if n == -1 {
            return Ok(Some(u32::MAX));
        }
        u32::try_from(n)
            .map(Some)
            .map_err(|_| it.overflow_err(&format!("{what} is greater than maximum")))
    }

    /// Spawn a fresh new child process.
    ///
    /// Fork a child process, close parent file descriptors as appropriate in the
    /// child and duplicate the few that are needed before calling exec() in the
    /// child process.
    ///
    /// If close_fds is True, close file descriptors 3 and higher, except those listed
    /// in the sorted tuple pass_fds.
    ///
    /// The preexec_fn, if supplied, will be called immediately before closing file
    /// descriptors and exec.
    ///
    /// WARNING: preexec_fn is NOT SAFE if your application uses threads.
    ///          It may trigger infrequent, difficult to debug deadlocks.
    ///
    /// If an error occurs in the child process before the exec, it is
    /// serialized and written to the errpipe_write fd per subprocess.py.
    ///
    /// Returns: the child process's PID.
    ///
    /// Raises: Only on an error in the parent process.
    #[op]
    #[allow(clippy::too_many_arguments)]
    fn fork_exec(
        it: &mut Interp,
        args: &Value,
        executable_list: &Value,
        close_fds: bool,
        pass_fds: &Value,
        cwd: &Value,
        env: &Value,
        p2cread: i32,
        p2cwrite: i32,
        c2pread: i32,
        c2pwrite: i32,
        errread: i32,
        errwrite: i32,
        errpipe_read: i32,
        errpipe_write: i32,
        restore_signals: bool,
        call_setsid: bool,
        pgid_to_set: i32,
        gid: &Value,
        extra_groups: &Value,
        uid: &Value,
        child_umask: i32,
        preexec_fn: &Value,
    ) -> R<i32> {
        let Some(keep) = tuple_items(pass_fds) else {
            let n = it.type_name_of(pass_fds);
            return Err(it.type_error(&format!("fork_exec() argument 4 must be tuple, not {n}")));
        };
        if close_fds && errpipe_write < 3 {
            return Err(it.value_error("errpipe_write must be >= 3"));
        }
        let mut fds_to_keep = Vec::with_capacity(keep.len());
        for v in keep {
            let fd = match v {
                Value::Int(n)
                    if (0..=i32::MAX as i64).contains(n)
                        && fds_to_keep.last().is_none_or(|&p| (p as i64) < *n) =>
                {
                    *n as i32
                }
                _ => return Err(it.value_error("bad value(s) in fds_to_keep")),
            };
            fds_to_keep.push(fd);
        }
        let executables = byte_list(it, executable_list)?;
        let args = match args {
            Value::None => None,
            v => Some(byte_list(it, v)?),
        };
        let env = match env {
            Value::None => None,
            v => Some(byte_list(it, v)?),
        };
        let cwd = match cwd {
            Value::None => None,
            v => Some(fs_bytes(it, v)?),
        };
        let extra_groups = match extra_groups {
            Value::None => None,
            Value::Obj(o) if matches!(o.kind, Kind::List(_)) => {
                let items = it.iterate_to_vec(extra_groups)?;
                let mut out = Vec::with_capacity(items.len());
                for g in &items {
                    if !g.is_int_like() || matches!(g, Value::Bool(_)) {
                        return Err(it.type_error("extra_groups must be integers"));
                    }
                    match id_arg(it, g, "gid") {
                        Ok(Some(g)) => out.push(g),
                        _ => return Err(it.value_error("invalid group id")),
                    }
                }
                Some(out)
            }
            _ => return Err(it.type_error("setgroups argument must be a list")),
        };
        let gid = id_arg(it, gid, "gid")?;
        let uid = id_arg(it, uid, "uid")?;
        let cfg = ForkExec {
            args,
            executables,
            env,
            cwd,
            close_fds,
            fds_to_keep,
            p2cread,
            p2cwrite,
            c2pread,
            c2pwrite,
            errread,
            errwrite,
            errpipe_read,
            errpipe_write,
            restore_signals,
            call_setsid,
            pgid: pgid_to_set,
            gid,
            extra_groups,
            uid,
            umask: child_umask,
        };
        let r = if matches!(preexec_fn, Value::None) {
            lumen_os::spawn::fork_exec(&cfg, None)
        } else {
            let f = preexec_fn.clone();
            let mut call = || it.call(&f, Vec::new(), Vec::new()).is_ok();
            lumen_os::spawn::fork_exec(&cfg, Some(&mut call))
        };
        r.map_err(|e| {
            if e.errno() == 22 {
                it.value_error("embedded null byte")
            } else {
                it.os_error_errno(e.errno(), None, None)
            }
        })
    }
}
