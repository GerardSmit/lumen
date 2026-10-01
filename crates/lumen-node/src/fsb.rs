//! Node's `internalBinding('fs')`: the primitives lib/fs.js, fs/promises and the fs streams are
//! written against (open/read/write/stat/readdir/...), as ops over `lumen_os::fs`. Each is a sync
//! op and an `...Async` op that runs on the runtime's worker pool, so the callback and promise
//! APIs never block the event loop. Errors carry libuv's code (`ENOENT`, `EPERM`, ...); fs.js
//! builds Node's `uvException` (errno, syscall, path, dest) from that code.
//!
//! fds 0-2 are the realm's standard streams (lumen-host routes them), handled by the sync ops
//! only.

use lumen::embed::{Ctx, OpDesc, OpError, SendError, Value};
use lumen_os::fs::{self as os, DirentKind, Stat, StatFs, Timespec};
use lumen_os::FsError;

// ---- errors -------------------------------------------------------------------------------------

/// A libuv error code; fs.js adds errno, description, syscall and paths.
pub struct UvErr(pub &'static str);

impl From<UvErr> for OpError {
    fn from(e: UvErr) -> OpError {
        OpError::new("Error", e.0).with_code(e.0)
    }
}

impl From<UvErr> for SendError {
    fn from(e: UvErr) -> SendError {
        SendError::new("Error", e.0.to_string()).with_code(e.0)
    }
}

impl From<std::io::Error> for UvErr {
    fn from(e: std::io::Error) -> UvErr {
        UvErr(uv_code(&e))
    }
}

type R<T> = Result<T, UvErr>;

pub use lumen_os::errno::uv_code;

trait Uv<T> {
    fn uv(self) -> R<T>;
}

impl<T> Uv<T> for Result<T, FsError> {
    #[inline]
    fn uv(self) -> R<T> {
        self.map_err(|e| UvErr(e.code()))
    }
}

// ---- JS-shaped values ---------------------------------------------------------------------------

/// A negative position reads and writes at the file's current offset.
fn position(pos: f64) -> Option<u64> {
    (pos >= 0.0).then_some(pos as u64)
}

fn ts(secs: f64) -> Timespec {
    Timespec::from_secs_f64(secs)
}

/// Node's stat array: dev, mode, nlink, uid, gid, rdev, blksize, ino, size, blocks, then
/// (seconds, nanoseconds) for atime, mtime, ctime, birthtime.
fn stat_array(s: &Stat) -> Vec<f64> {
    let t = |t: Timespec| [t.sec as f64, t.nsec as f64];
    let [as_, an] = t(s.atime);
    let [ms, mn] = t(s.mtime);
    let [cs, cn] = t(s.ctime);
    let [bs, bn] = t(s.birthtime);
    vec![
        s.dev as f64,
        s.mode as f64,
        s.nlink as f64,
        s.uid as f64,
        s.gid as f64,
        s.rdev as f64,
        s.blksize as f64,
        s.ino as f64,
        s.size as f64,
        s.blocks as f64,
        as_,
        an,
        ms,
        mn,
        cs,
        cn,
        bs,
        bn,
    ]
}

fn statfs_array(s: &StatFs) -> Vec<f64> {
    vec![0.0, s.bsize as f64, s.blocks as f64, s.bfree as f64, s.bavail as f64, s.files as f64, s.ffree as f64]
}

/// `[names, types]` with UV_DIRENT_* type values.
fn dirent_arrays(entries: Vec<(String, DirentKind)>) -> (Vec<String>, Vec<f64>) {
    entries.into_iter().map(|(n, k)| (n, k as u8 as f64)).unzip()
}

fn std_read(ctx: &mut Ctx, fd: i32, buf: &mut [u8]) -> R<usize> {
    if fd != 0 {
        return Err(UvErr("EBADF"));
    }
    match lumen_host::read_stdin_fd(ctx, buf) {
        Ok(n) => Ok(n),
        #[cfg(windows)]
        Err(e) if matches!(e.raw_os_error(), Some(38) | Some(109)) => Ok(0),
        Err(e) => Err(e.into()),
    }
}

fn std_write(ctx: &mut Ctx, fd: i32, data: &[u8]) -> R<usize> {
    if fd == 0 {
        return Err(UvErr("EBADF"));
    }
    lumen_host::write_std_fd(ctx, fd as u32, data)?;
    Ok(data.len())
}

// ---- ops ----------------------------------------------------------------------------------------
// Every op has a sync form (`open`) and a worker-pool form (`openAsync`).

#[lumen::op(name = "open")]
fn op_open(path: &str, flags: i32, mode: u32) -> Result<f64, OpError> {
    Ok(os::open(path, flags, mode).uv()? as f64)
}
#[lumen::op(async, name = "openAsync")]
fn op_open_async(path: String, flags: i32, mode: u32) -> Result<f64, SendError> {
    Ok(os::open(&path, flags, mode).uv()? as f64)
}

#[lumen::op(name = "close")]
fn op_close(fd: i32) -> Result<(), OpError> {
    if os::is_std(fd) && !os::is_open(fd) {
        return Ok(());
    }
    Ok(os::close(fd).uv()?)
}
#[lumen::op(async, name = "closeAsync")]
fn op_close_async(fd: i32) -> Result<(), SendError> {
    if os::is_std(fd) && !os::is_open(fd) {
        return Ok(());
    }
    Ok(os::close(fd).uv()?)
}

/// `read(fd, view, position)`: fills the view (JS passes the `[offset, offset+length)` window).
#[lumen::op(name = "read")]
fn op_read(ctx: &mut Ctx, fd: i32, buf: &mut [u8], pos: f64) -> Result<f64, OpError> {
    if os::is_std(fd) && !os::is_open(fd) {
        return Ok(std_read(ctx, fd, buf)? as f64);
    }
    Ok(os::read(fd, buf, position(pos)).uv()? as f64)
}
/// The bytes read (JS copies them into the caller's buffer).
#[lumen::op(async, name = "readAsync")]
fn op_read_async(fd: i32, len: u32, pos: f64) -> Result<Vec<u8>, SendError> {
    let mut buf = vec![0u8; len as usize];
    let n = os::read(fd, &mut buf, position(pos)).uv()?;
    buf.truncate(n);
    Ok(buf)
}

#[lumen::op(name = "write")]
fn op_write(ctx: &mut Ctx, fd: i32, data: &[u8], pos: f64) -> Result<f64, OpError> {
    if os::is_std(fd) && !os::is_open(fd) {
        return Ok(std_write(ctx, fd, data)? as f64);
    }
    Ok(os::write(fd, data, position(pos)).uv()? as f64)
}
#[lumen::op(async, name = "writeAsync")]
fn op_write_async(fd: i32, data: Vec<u8>, pos: f64) -> Result<f64, SendError> {
    Ok(os::write(fd, &data, position(pos)).uv()? as f64)
}

#[lumen::op(name = "fstat")]
fn op_fstat(fd: i32) -> Result<Vec<f64>, OpError> {
    Ok(stat_array(&os::fstat(fd).uv()?))
}
#[lumen::op(async, name = "fstatAsync")]
fn op_fstat_async(fd: i32) -> Result<Vec<f64>, SendError> {
    Ok(stat_array(&os::fstat(fd).uv()?))
}

#[lumen::op(name = "stat")]
fn op_stat(path: &str) -> Result<Vec<f64>, OpError> {
    Ok(stat_array(&os::stat(path, true).uv()?))
}
#[lumen::op(async, name = "statAsync")]
fn op_stat_async(path: String) -> Result<Vec<f64>, SendError> {
    Ok(stat_array(&os::stat(&path, true).uv()?))
}

#[lumen::op(name = "lstat")]
fn op_lstat(path: &str) -> Result<Vec<f64>, OpError> {
    Ok(stat_array(&os::stat(path, false).uv()?))
}
#[lumen::op(async, name = "lstatAsync")]
fn op_lstat_async(path: String) -> Result<Vec<f64>, SendError> {
    Ok(stat_array(&os::stat(&path, false).uv()?))
}

/// Like `stat` but a missing path is `undefined`, not an error (`throwIfNoEntry: false`).
#[lumen::op(name = "statMaybe")]
fn op_stat_maybe(path: &str, follow: bool) -> Result<Option<Vec<f64>>, OpError> {
    match os::stat(path, follow) {
        Ok(v) => Ok(Some(stat_array(&v))),
        Err(e) if matches!(e.code(), "ENOENT" | "ENOTDIR") => Ok(None),
        Err(e) => Err(UvErr(e.code()).into()),
    }
}

#[lumen::op(name = "statfs")]
fn op_statfs(path: &str) -> Result<Vec<f64>, OpError> {
    Ok(statfs_array(&os::statfs(path).uv()?))
}
#[lumen::op(async, name = "statfsAsync")]
fn op_statfs_async(path: String) -> Result<Vec<f64>, SendError> {
    Ok(statfs_array(&os::statfs(&path).uv()?))
}

#[lumen::op(name = "ftruncate")]
fn op_ftruncate(fd: i32, len: f64) -> Result<(), OpError> {
    Ok(os::ftruncate(fd, len.max(0.0) as u64).uv()?)
}
#[lumen::op(async, name = "ftruncateAsync")]
fn op_ftruncate_async(fd: i32, len: f64) -> Result<(), SendError> {
    Ok(os::ftruncate(fd, len.max(0.0) as u64).uv()?)
}

#[lumen::op(name = "fsync")]
fn op_fsync(fd: i32, data_only: bool) -> Result<(), OpError> {
    Ok(os::fsync(fd, data_only).uv()?)
}
#[lumen::op(async, name = "fsyncAsync")]
fn op_fsync_async(fd: i32, data_only: bool) -> Result<(), SendError> {
    Ok(os::fsync(fd, data_only).uv()?)
}

#[lumen::op(name = "fchmod")]
fn op_fchmod(fd: i32, mode: u32) -> Result<(), OpError> {
    Ok(os::fchmod(fd, mode).uv()?)
}
#[lumen::op(async, name = "fchmodAsync")]
fn op_fchmod_async(fd: i32, mode: u32) -> Result<(), SendError> {
    Ok(os::fchmod(fd, mode).uv()?)
}

#[lumen::op(name = "fchown")]
fn op_fchown(fd: i32, uid: u32, gid: u32) -> Result<(), OpError> {
    Ok(os::fchown(fd, uid, gid).uv()?)
}
#[lumen::op(async, name = "fchownAsync")]
fn op_fchown_async(fd: i32, uid: u32, gid: u32) -> Result<(), SendError> {
    Ok(os::fchown(fd, uid, gid).uv()?)
}

#[lumen::op(name = "futimes")]
fn op_futimes(fd: i32, atime: f64, mtime: f64) -> Result<(), OpError> {
    Ok(os::futimes(fd, ts(atime), ts(mtime)).uv()?)
}
#[lumen::op(async, name = "futimesAsync")]
fn op_futimes_async(fd: i32, atime: f64, mtime: f64) -> Result<(), SendError> {
    Ok(os::futimes(fd, ts(atime), ts(mtime)).uv()?)
}

#[lumen::op(name = "utimes")]
fn op_utimes(path: &str, atime: f64, mtime: f64, follow: bool) -> Result<(), OpError> {
    Ok(os::utimes(path, ts(atime), ts(mtime), follow).uv()?)
}
#[lumen::op(async, name = "utimesAsync")]
fn op_utimes_async(path: String, atime: f64, mtime: f64, follow: bool) -> Result<(), SendError> {
    Ok(os::utimes(&path, ts(atime), ts(mtime), follow).uv()?)
}

#[lumen::op(name = "access")]
fn op_access(path: &str, mode: u32) -> Result<(), OpError> {
    Ok(os::access(path, mode).uv()?)
}
#[lumen::op(async, name = "accessAsync")]
fn op_access_async(path: String, mode: u32) -> Result<(), SendError> {
    Ok(os::access(&path, mode).uv()?)
}

#[lumen::op(name = "exists")]
fn op_exists(path: &str) -> bool {
    os::exists(path)
}

#[lumen::op(name = "chmod")]
fn op_chmod(path: &str, mode: u32) -> Result<(), OpError> {
    Ok(os::chmod(path, mode).uv()?)
}
#[lumen::op(async, name = "chmodAsync")]
fn op_chmod_async(path: String, mode: u32) -> Result<(), SendError> {
    Ok(os::chmod(&path, mode).uv()?)
}

#[lumen::op(name = "chown")]
fn op_chown(path: &str, uid: u32, gid: u32, follow: bool) -> Result<(), OpError> {
    Ok(os::chown(path, uid, gid, follow).uv()?)
}
#[lumen::op(async, name = "chownAsync")]
fn op_chown_async(path: String, uid: u32, gid: u32, follow: bool) -> Result<(), SendError> {
    Ok(os::chown(&path, uid, gid, follow).uv()?)
}

#[lumen::op(name = "mkdir")]
fn op_mkdir(path: &str, mode: u32, recursive: bool) -> Result<Option<String>, OpError> {
    Ok(os::mkdir(path, mode, recursive).uv()?)
}
#[lumen::op(async, name = "mkdirAsync")]
fn op_mkdir_async(path: String, mode: u32, recursive: bool) -> Result<Option<String>, SendError> {
    Ok(os::mkdir(&path, mode, recursive).uv()?)
}

#[lumen::op(name = "mkdtemp")]
fn op_mkdtemp(prefix: &str) -> Result<String, OpError> {
    Ok(os::mkdtemp(prefix).uv()?)
}
#[lumen::op(async, name = "mkdtempAsync")]
fn op_mkdtemp_async(prefix: String) -> Result<String, SendError> {
    Ok(os::mkdtemp(&prefix).uv()?)
}

#[lumen::op(name = "rmdir")]
fn op_rmdir(path: &str) -> Result<(), OpError> {
    Ok(os::rmdir(path).uv()?)
}
#[lumen::op(async, name = "rmdirAsync")]
fn op_rmdir_async(path: String) -> Result<(), SendError> {
    Ok(os::rmdir(&path).uv()?)
}

#[lumen::op(name = "unlink")]
fn op_unlink(path: &str) -> Result<(), OpError> {
    Ok(os::unlink(path).uv()?)
}
#[lumen::op(async, name = "unlinkAsync")]
fn op_unlink_async(path: String) -> Result<(), SendError> {
    Ok(os::unlink(&path).uv()?)
}

#[lumen::op(name = "rename")]
fn op_rename(from: &str, to: &str) -> Result<(), OpError> {
    Ok(os::rename(from, to).uv()?)
}
#[lumen::op(async, name = "renameAsync")]
fn op_rename_async(from: String, to: String) -> Result<(), SendError> {
    Ok(os::rename(&from, &to).uv()?)
}

#[lumen::op(name = "link")]
fn op_link(existing: &str, path: &str) -> Result<(), OpError> {
    Ok(os::link(existing, path).uv()?)
}
#[lumen::op(async, name = "linkAsync")]
fn op_link_async(existing: String, path: String) -> Result<(), SendError> {
    Ok(os::link(&existing, &path).uv()?)
}

#[lumen::op(name = "symlink")]
fn op_symlink(target: &str, path: &str, flags: u32) -> Result<(), OpError> {
    Ok(os::symlink(target, path, flags).uv()?)
}
#[lumen::op(async, name = "symlinkAsync")]
fn op_symlink_async(target: String, path: String, flags: u32) -> Result<(), SendError> {
    Ok(os::symlink(&target, &path, flags).uv()?)
}

#[lumen::op(name = "readlink")]
fn op_readlink(path: &str) -> Result<String, OpError> {
    Ok(os::readlink(path).uv()?)
}
#[lumen::op(async, name = "readlinkAsync")]
fn op_readlink_async(path: String) -> Result<String, SendError> {
    Ok(os::readlink(&path).uv()?)
}

#[lumen::op(name = "realpath")]
fn op_realpath(path: &str) -> Result<String, OpError> {
    Ok(os::realpath(path).uv()?)
}
#[lumen::op(async, name = "realpathAsync")]
fn op_realpath_async(path: String) -> Result<String, SendError> {
    Ok(os::realpath(&path).uv()?)
}

#[lumen::op(name = "copyFile")]
fn op_copy_file(src: &str, dst: &str, mode: u32) -> Result<(), OpError> {
    Ok(os::copy_file(src, dst, mode).uv()?)
}
#[lumen::op(async, name = "copyFileAsync")]
fn op_copy_file_async(src: String, dst: String, mode: u32) -> Result<(), SendError> {
    Ok(os::copy_file(&src, &dst, mode).uv()?)
}

/// `[names, types]` (types are UV_DIRENT_* values).
#[lumen::op(name = "readdir")]
fn op_readdir(path: &str) -> Result<(Vec<String>, Vec<f64>), OpError> {
    Ok(dirent_arrays(os::readdir(path).uv()?))
}
#[lumen::op(async, name = "readdirAsync")]
fn op_readdir_async(path: String) -> Result<(Vec<String>, Vec<f64>), SendError> {
    Ok(dirent_arrays(os::readdir(&path).uv()?))
}

/// A whole file by path (readFileSync's fast path).
#[lumen::op(name = "readFile")]
fn op_read_file(path: &str, flags: i32) -> Result<Vec<u8>, OpError> {
    Ok(os::read_file(path, flags).uv()?)
}
#[lumen::op(async, name = "readFileAsync")]
fn op_read_file_async(path: String, flags: i32) -> Result<Vec<u8>, SendError> {
    Ok(os::read_file(&path, flags).uv()?)
}

/// A whole file decoded as UTF-8 (invalid sequences become U+FFFD), for `readFileSync(p, 'utf8')`.
#[lumen::op(name = "readFileUtf8")]
fn op_read_file_utf8(path: &str, flags: i32) -> Result<String, OpError> {
    let bytes = os::read_file(path, flags).uv()?;
    let s = match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    };
    Ok(crate::codec::canonical(s))
}

/// Whole-file write by path: `flags` are Node's numeric open flags.
#[lumen::op(name = "writeFile")]
fn op_write_file(path: &str, data: &[u8], flags: i32, mode: u32) -> Result<(), OpError> {
    Ok(os::write_file(path, data, flags, mode).uv()?)
}
#[lumen::op(async, name = "writeFileAsync")]
fn op_write_file_async(path: String, data: Vec<u8>, flags: i32, mode: u32) -> Result<(), SendError> {
    Ok(os::write_file(&path, &data, flags, mode).uv()?)
}

// ---- registration -------------------------------------------------------------------------------

const OPS: &[&OpDesc] = lumen::ops![
    op_open,
    op_open_async,
    op_close,
    op_close_async,
    op_read,
    op_read_async,
    op_write,
    op_write_async,
    op_fstat,
    op_fstat_async,
    op_stat,
    op_stat_async,
    op_lstat,
    op_lstat_async,
    op_stat_maybe,
    op_statfs,
    op_statfs_async,
    op_ftruncate,
    op_ftruncate_async,
    op_fsync,
    op_fsync_async,
    op_fchmod,
    op_fchmod_async,
    op_fchown,
    op_fchown_async,
    op_futimes,
    op_futimes_async,
    op_utimes,
    op_utimes_async,
    op_access,
    op_access_async,
    op_exists,
    op_chmod,
    op_chmod_async,
    op_chown,
    op_chown_async,
    op_mkdir,
    op_mkdir_async,
    op_mkdtemp,
    op_mkdtemp_async,
    op_rmdir,
    op_rmdir_async,
    op_unlink,
    op_unlink_async,
    op_rename,
    op_rename_async,
    op_link,
    op_link_async,
    op_symlink,
    op_symlink_async,
    op_readlink,
    op_readlink_async,
    op_realpath,
    op_realpath_async,
    op_copy_file,
    op_copy_file_async,
    op_readdir,
    op_readdir_async,
    op_read_file,
    op_read_file_async,
    op_read_file_utf8,
    op_write_file,
    op_write_file_async,
];

/// `__node.fsBinding()`: a fresh object holding every op above (called once by fs.js).
pub fn op_fs_binding(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    let ns = Value::Obj(ctx.new_object());
    for op in OPS {
        let f = ctx.op_function(op);
        let _ = ctx.set_member(&ns, op.name, f);
    }
    Ok(ns)
}


