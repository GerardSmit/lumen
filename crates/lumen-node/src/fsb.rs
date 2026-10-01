//! Node's `internalBinding('fs')`: the primitives lib/fs.js, fs/promises and the fs streams are
//! written against (open/read/write/stat/readdir/...), as ops over `lumen_os::fs`. Each is a sync
//! op and an `...Async` op that runs on the runtime's worker pool, so the callback and promise
//! APIs never block the event loop. Errors carry libuv's code (`ENOENT`, `EPERM`, ...); fs.js
//! builds Node's `uvException` (errno, syscall, path, dest) from that code.
//!
//! fds 0-2 are the realm's standard streams (lumen-host routes them), handled by the sync ops
//! only.

use lumen::embed::{Ctx, OpError, SendError, Value};
use lumen_os::fs::{self as os, DirentKind, Stat, StatFs, Timespec};
use lumen_os::FsError;

pub(crate) use bindings::*;

#[lumen_bind::module(name = "fs")]
pub(crate) mod bindings {
use super::*;

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

#[op(name = "open")]
fn op_open(path: &str, flags: i32, mode: u32) -> Result<f64, OpError> {
    Ok(os::open(path, flags, mode).uv()? as f64)
}
#[op(async, name = "openAsync")]
fn op_open_async(path: String, flags: i32, mode: u32) -> Result<f64, SendError> {
    Ok(os::open(&path, flags, mode).uv()? as f64)
}

#[op(name = "close")]
fn op_close(fd: i32) -> Result<(), OpError> {
    if os::is_std(fd) && !os::is_open(fd) {
        return Ok(());
    }
    Ok(os::close(fd).uv()?)
}
#[op(async, name = "closeAsync")]
fn op_close_async(fd: i32) -> Result<(), SendError> {
    if os::is_std(fd) && !os::is_open(fd) {
        return Ok(());
    }
    Ok(os::close(fd).uv()?)
}

/// `read(fd, view, position)`: fills the view (JS passes the `[offset, offset+length)` window).
#[op(name = "read")]
fn op_read(ctx: &mut Ctx, fd: i32, buf: &mut [u8], pos: f64) -> Result<f64, OpError> {
    if os::is_std(fd) && !os::is_open(fd) {
        return Ok(std_read(ctx, fd, buf)? as f64);
    }
    Ok(os::read(fd, buf, position(pos)).uv()? as f64)
}
/// The bytes read (JS copies them into the caller's buffer).
#[op(async, name = "readAsync")]
fn op_read_async(fd: i32, len: u32, pos: f64) -> Result<Vec<u8>, SendError> {
    let mut buf = vec![0u8; len as usize];
    let n = os::read(fd, &mut buf, position(pos)).uv()?;
    buf.truncate(n);
    Ok(buf)
}

#[op(name = "write")]
fn op_write(ctx: &mut Ctx, fd: i32, data: &[u8], pos: f64) -> Result<f64, OpError> {
    if os::is_std(fd) && !os::is_open(fd) {
        return Ok(std_write(ctx, fd, data)? as f64);
    }
    Ok(os::write(fd, data, position(pos)).uv()? as f64)
}
#[op(async, name = "writeAsync")]
fn op_write_async(fd: i32, data: Vec<u8>, pos: f64) -> Result<f64, SendError> {
    Ok(os::write(fd, &data, position(pos)).uv()? as f64)
}

#[op(name = "fstat")]
fn op_fstat(fd: i32) -> Result<Vec<f64>, OpError> {
    Ok(stat_array(&os::fstat(fd).uv()?))
}
#[op(async, name = "fstatAsync")]
fn op_fstat_async(fd: i32) -> Result<Vec<f64>, SendError> {
    Ok(stat_array(&os::fstat(fd).uv()?))
}

#[op(name = "stat")]
fn op_stat(path: &str) -> Result<Vec<f64>, OpError> {
    Ok(stat_array(&os::stat(path, true).uv()?))
}
#[op(async, name = "statAsync")]
fn op_stat_async(path: String) -> Result<Vec<f64>, SendError> {
    Ok(stat_array(&os::stat(&path, true).uv()?))
}

#[op(name = "lstat")]
fn op_lstat(path: &str) -> Result<Vec<f64>, OpError> {
    Ok(stat_array(&os::stat(path, false).uv()?))
}
#[op(async, name = "lstatAsync")]
fn op_lstat_async(path: String) -> Result<Vec<f64>, SendError> {
    Ok(stat_array(&os::stat(&path, false).uv()?))
}

/// Like `stat` but a missing path is `undefined`, not an error (`throwIfNoEntry: false`).
#[op(name = "statMaybe")]
fn op_stat_maybe(path: &str, follow: bool) -> Result<Option<Vec<f64>>, OpError> {
    match os::stat(path, follow) {
        Ok(v) => Ok(Some(stat_array(&v))),
        Err(e) if matches!(e.code(), "ENOENT" | "ENOTDIR") => Ok(None),
        Err(e) => Err(UvErr(e.code()).into()),
    }
}

#[op(name = "statfs")]
fn op_statfs(path: &str) -> Result<Vec<f64>, OpError> {
    Ok(statfs_array(&os::statfs(path).uv()?))
}
#[op(async, name = "statfsAsync")]
fn op_statfs_async(path: String) -> Result<Vec<f64>, SendError> {
    Ok(statfs_array(&os::statfs(&path).uv()?))
}

#[op(name = "ftruncate")]
fn op_ftruncate(fd: i32, len: f64) -> Result<(), OpError> {
    Ok(os::ftruncate(fd, len.max(0.0) as u64).uv()?)
}
#[op(async, name = "ftruncateAsync")]
fn op_ftruncate_async(fd: i32, len: f64) -> Result<(), SendError> {
    Ok(os::ftruncate(fd, len.max(0.0) as u64).uv()?)
}

#[op(name = "fsync")]
fn op_fsync(fd: i32, data_only: bool) -> Result<(), OpError> {
    Ok(os::fsync(fd, data_only).uv()?)
}
#[op(async, name = "fsyncAsync")]
fn op_fsync_async(fd: i32, data_only: bool) -> Result<(), SendError> {
    Ok(os::fsync(fd, data_only).uv()?)
}

#[op(name = "fchmod")]
fn op_fchmod(fd: i32, mode: u32) -> Result<(), OpError> {
    Ok(os::fchmod(fd, mode).uv()?)
}
#[op(async, name = "fchmodAsync")]
fn op_fchmod_async(fd: i32, mode: u32) -> Result<(), SendError> {
    Ok(os::fchmod(fd, mode).uv()?)
}

#[op(name = "fchown")]
fn op_fchown(fd: i32, uid: u32, gid: u32) -> Result<(), OpError> {
    Ok(os::fchown(fd, uid, gid).uv()?)
}
#[op(async, name = "fchownAsync")]
fn op_fchown_async(fd: i32, uid: u32, gid: u32) -> Result<(), SendError> {
    Ok(os::fchown(fd, uid, gid).uv()?)
}

#[op(name = "futimes")]
fn op_futimes(fd: i32, atime: f64, mtime: f64) -> Result<(), OpError> {
    Ok(os::futimes(fd, ts(atime), ts(mtime)).uv()?)
}
#[op(async, name = "futimesAsync")]
fn op_futimes_async(fd: i32, atime: f64, mtime: f64) -> Result<(), SendError> {
    Ok(os::futimes(fd, ts(atime), ts(mtime)).uv()?)
}

#[op(name = "utimes")]
fn op_utimes(path: &str, atime: f64, mtime: f64, follow: bool) -> Result<(), OpError> {
    Ok(os::utimes(path, ts(atime), ts(mtime), follow).uv()?)
}
#[op(async, name = "utimesAsync")]
fn op_utimes_async(path: String, atime: f64, mtime: f64, follow: bool) -> Result<(), SendError> {
    Ok(os::utimes(&path, ts(atime), ts(mtime), follow).uv()?)
}

#[op(name = "access")]
fn op_access(path: &str, mode: u32) -> Result<(), OpError> {
    Ok(os::access(path, mode).uv()?)
}
#[op(async, name = "accessAsync")]
fn op_access_async(path: String, mode: u32) -> Result<(), SendError> {
    Ok(os::access(&path, mode).uv()?)
}

#[op(name = "exists")]
fn op_exists(path: &str) -> bool {
    os::exists(path)
}

#[op(name = "chmod")]
fn op_chmod(path: &str, mode: u32) -> Result<(), OpError> {
    Ok(os::chmod(path, mode).uv()?)
}
#[op(async, name = "chmodAsync")]
fn op_chmod_async(path: String, mode: u32) -> Result<(), SendError> {
    Ok(os::chmod(&path, mode).uv()?)
}

#[op(name = "chown")]
fn op_chown(path: &str, uid: u32, gid: u32, follow: bool) -> Result<(), OpError> {
    Ok(os::chown(path, uid, gid, follow).uv()?)
}
#[op(async, name = "chownAsync")]
fn op_chown_async(path: String, uid: u32, gid: u32, follow: bool) -> Result<(), SendError> {
    Ok(os::chown(&path, uid, gid, follow).uv()?)
}

#[op(name = "mkdir")]
fn op_mkdir(path: &str, mode: u32, recursive: bool) -> Result<Option<String>, OpError> {
    Ok(os::mkdir(path, mode, recursive).uv()?)
}
#[op(async, name = "mkdirAsync")]
fn op_mkdir_async(path: String, mode: u32, recursive: bool) -> Result<Option<String>, SendError> {
    Ok(os::mkdir(&path, mode, recursive).uv()?)
}

#[op(name = "mkdtemp")]
fn op_mkdtemp(prefix: &str) -> Result<String, OpError> {
    Ok(os::mkdtemp(prefix).uv()?)
}
#[op(async, name = "mkdtempAsync")]
fn op_mkdtemp_async(prefix: String) -> Result<String, SendError> {
    Ok(os::mkdtemp(&prefix).uv()?)
}

#[op(name = "rmdir")]
fn op_rmdir(path: &str) -> Result<(), OpError> {
    Ok(os::rmdir(path).uv()?)
}
#[op(async, name = "rmdirAsync")]
fn op_rmdir_async(path: String) -> Result<(), SendError> {
    Ok(os::rmdir(&path).uv()?)
}

#[op(name = "unlink")]
fn op_unlink(path: &str) -> Result<(), OpError> {
    Ok(os::unlink(path).uv()?)
}
#[op(async, name = "unlinkAsync")]
fn op_unlink_async(path: String) -> Result<(), SendError> {
    Ok(os::unlink(&path).uv()?)
}

#[op(name = "rename")]
fn op_rename(from: &str, to: &str) -> Result<(), OpError> {
    Ok(os::rename(from, to).uv()?)
}
#[op(async, name = "renameAsync")]
fn op_rename_async(from: String, to: String) -> Result<(), SendError> {
    Ok(os::rename(&from, &to).uv()?)
}

#[op(name = "link")]
fn op_link(existing: &str, path: &str) -> Result<(), OpError> {
    Ok(os::link(existing, path).uv()?)
}
#[op(async, name = "linkAsync")]
fn op_link_async(existing: String, path: String) -> Result<(), SendError> {
    Ok(os::link(&existing, &path).uv()?)
}

#[op(name = "symlink")]
fn op_symlink(target: &str, path: &str, flags: u32) -> Result<(), OpError> {
    Ok(os::symlink(target, path, flags).uv()?)
}
#[op(async, name = "symlinkAsync")]
fn op_symlink_async(target: String, path: String, flags: u32) -> Result<(), SendError> {
    Ok(os::symlink(&target, &path, flags).uv()?)
}

#[op(name = "readlink")]
fn op_readlink(path: &str) -> Result<String, OpError> {
    Ok(os::readlink(path).uv()?)
}
#[op(async, name = "readlinkAsync")]
fn op_readlink_async(path: String) -> Result<String, SendError> {
    Ok(os::readlink(&path).uv()?)
}

#[op(name = "realpath")]
fn op_realpath(path: &str) -> Result<String, OpError> {
    Ok(os::realpath(path).uv()?)
}
#[op(async, name = "realpathAsync")]
fn op_realpath_async(path: String) -> Result<String, SendError> {
    Ok(os::realpath(&path).uv()?)
}

#[op(name = "copyFile")]
fn op_copy_file(src: &str, dst: &str, mode: u32) -> Result<(), OpError> {
    Ok(os::copy_file(src, dst, mode).uv()?)
}
#[op(async, name = "copyFileAsync")]
fn op_copy_file_async(src: String, dst: String, mode: u32) -> Result<(), SendError> {
    Ok(os::copy_file(&src, &dst, mode).uv()?)
}

/// `[names, types]` (types are UV_DIRENT_* values).
#[op(name = "readdir")]
fn op_readdir(path: &str) -> Result<(Vec<String>, Vec<f64>), OpError> {
    Ok(dirent_arrays(os::readdir(path).uv()?))
}
#[op(async, name = "readdirAsync")]
fn op_readdir_async(path: String) -> Result<(Vec<String>, Vec<f64>), SendError> {
    Ok(dirent_arrays(os::readdir(&path).uv()?))
}

/// A whole file by path (readFileSync's fast path).
#[op(name = "readFile")]
fn op_read_file(path: &str, flags: i32) -> Result<Vec<u8>, OpError> {
    Ok(os::read_file(path, flags).uv()?)
}
#[op(async, name = "readFileAsync")]
fn op_read_file_async(path: String, flags: i32) -> Result<Vec<u8>, SendError> {
    Ok(os::read_file(&path, flags).uv()?)
}

/// A whole file decoded as UTF-8 (invalid sequences become U+FFFD), for `readFileSync(p, 'utf8')`.
#[op(name = "readFileUtf8")]
fn op_read_file_utf8(path: &str, flags: i32) -> Result<String, OpError> {
    let bytes = os::read_file(path, flags).uv()?;
    let s = match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    };
    Ok(crate::codec::canonical(s))
}

/// Whole-file write by path: `flags` are Node's numeric open flags.
#[op(name = "writeFile")]
fn op_write_file(path: &str, data: &[u8], flags: i32, mode: u32) -> Result<(), OpError> {
    Ok(os::write_file(path, data, flags, mode).uv()?)
}
#[op(async, name = "writeFileAsync")]
fn op_write_file_async(path: String, data: Vec<u8>, flags: i32, mode: u32) -> Result<(), SendError> {
    Ok(os::write_file(&path, &data, flags, mode).uv()?)
}

// ---- registration -------------------------------------------------------------------------------


/// `__node.fsBinding()`: a fresh object holding every op above (called once by fs.js).
pub fn op_fs_binding(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    ctx.module_object::<Module>()
}
}
