//! `__node.fsBinding()` for targets with no OS file system: the same op names and shapes as
//! `fsb.rs`, backed by `lumen_host::vfs`. Async forms complete inline (there is no worker pool).

use lumen::embed::{Ctx, OpDesc, OpError, SendError, Value};
use lumen_host::vfs::{self, Errno, Kind, Stat};

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

impl From<Errno> for UvErr {
    fn from(e: Errno) -> UvErr {
        UvErr(e.code())
    }
}

impl From<std::io::Error> for UvErr {
    fn from(e: std::io::Error) -> UvErr {
        UvErr(uv_code(&e))
    }
}

type R<T> = Result<T, UvErr>;

pub fn uv_code(e: &std::io::Error) -> &'static str {
    use std::io::ErrorKind as K;
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

fn is_std(fd: i32) -> bool {
    (0..=2).contains(&fd) && !vfs::is_open(fd)
}

fn std_read(ctx: &mut Ctx, fd: i32, buf: &mut [u8]) -> R<usize> {
    if fd != 0 {
        return Err(UvErr("EBADF"));
    }
    match lumen_host::read_stdin_fd(ctx, buf) {
        Ok(n) => Ok(n),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(0),
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

fn pos_arg(pos: f64) -> Option<u64> {
    if pos < 0.0 {
        None
    } else {
        Some(pos as u64)
    }
}

fn split_ms(ms: f64) -> (f64, f64) {
    let secs = (ms / 1000.0).floor();
    (secs, ((ms - secs * 1000.0) * 1e6).round())
}

/// Node's stat array: dev, mode, nlink, uid, gid, rdev, blksize, ino, size, blocks, then
/// (seconds, nanoseconds) for atime, mtime, ctime, birthtime.
fn stat_vec(s: &Stat) -> Vec<f64> {
    let (as_, an) = split_ms(s.atime_ms);
    let (ms_, mn) = split_ms(s.mtime_ms);
    let (cs, cn) = split_ms(s.ctime_ms);
    let (bs, bn) = split_ms(s.birthtime_ms);
    let type_bits = match s.kind {
        Kind::File => 0o100000,
        Kind::Dir => 0o040000,
        Kind::Symlink => 0o120000,
    };
    vec![
        1.0,
        (type_bits | (s.mode & 0o7777)) as f64,
        s.nlink as f64,
        s.uid as f64,
        s.gid as f64,
        0.0,
        4096.0,
        s.ino as f64,
        s.size as f64,
        (s.size as f64 / 512.0).ceil(),
        as_, an, ms_, mn, cs, cn, bs, bn,
    ]
}

fn stat_path(path: &str, follow: bool) -> R<Vec<f64>> {
    Ok(stat_vec(&vfs::stat(path, follow)?))
}

fn fstat_impl(fd: i32) -> R<Vec<f64>> {
    if is_std(fd) {
        return Ok(stat_vec(&Stat {
            kind: Kind::File,
            mode: 0o600,
            size: 0,
            nlink: 1,
            uid: 0,
            gid: 0,
            ino: fd as u64,
            atime_ms: 0.0,
            mtime_ms: 0.0,
            ctime_ms: 0.0,
            birthtime_ms: 0.0,
        }));
    }
    Ok(stat_vec(&vfs::fstat(fd)?))
}

fn access_impl(path: &str, _mode: u32) -> R<()> {
    vfs::stat(path, true)?;
    Ok(())
}

fn readdir_impl(path: &str) -> R<(Vec<String>, Vec<f64>)> {
    let entries = vfs::readdir(path)?;
    let mut names = Vec::with_capacity(entries.len());
    let mut types = Vec::with_capacity(entries.len());
    for (name, kind) in entries {
        names.push(name);
        types.push(match kind {
            Kind::File => 1.0,
            Kind::Dir => 2.0,
            Kind::Symlink => 3.0,
        });
    }
    Ok((names, types))
}

fn statfs_impl(path: &str) -> R<Vec<f64>> {
    vfs::stat(path, true)?;
    Ok(vec![0.0, 4096.0, 1048576.0, 524288.0, 524288.0, 1048576.0, 524288.0])
}

fn copyfile_impl(src: &str, dst: &str, mode: u32) -> R<()> {
    if mode & 4 != 0 {
        return Err(UvErr("ENOSYS"));
    }
    Ok(vfs::copy_file(src, dst, mode & 1 != 0)?)
}

fn read_file_impl(path: &str, _fl: i32) -> R<Vec<u8>> {
    Ok(vfs::read_file(path)?)
}

fn write_file_impl(path: &str, data: &[u8], fl: i32, mode: u32) -> R<()> {
    let fd = vfs::open(path, fl, mode)?;
    let r = vfs::write(fd, data, None);
    let _ = vfs::close(fd);
    r?;
    Ok(())
}

fn read_fd(fd: i32, buf: &mut [u8], pos: f64) -> R<usize> {
    Ok(vfs::read(fd, buf, pos_arg(pos))?)
}

fn write_fd(fd: i32, data: &[u8], pos: f64) -> R<usize> {
    Ok(vfs::write(fd, data, pos_arg(pos))?)
}

fn close_impl(fd: i32) -> R<()> {
    if is_std(fd) {
        return Ok(());
    }
    Ok(vfs::close(fd)?)
}

fn mkdir_impl(path: &str, mode: u32, recursive: bool) -> R<Option<String>> {
    Ok(vfs::mkdir(path, mode, recursive)?)
}

fn symlink_impl(target: &str, path: &str) -> R<()> {
    Ok(vfs::symlink(target, path)?)
}

fn secs_ms(secs: f64) -> f64 {
    secs * 1000.0
}

// ---- ops ----

#[lumen::op(name = "open")]
fn op_open(path: &str, flags: i32, mode: u32) -> Result<f64, OpError> {
    Ok(vfs::open(path, flags, mode).map_err(UvErr::from)? as f64)
}

#[lumen::op(async, name = "openAsync")]
fn op_open_async(path: String, flags: i32, mode: u32) -> Result<f64, SendError> {
    Ok(vfs::open(&path, flags, mode).map_err(UvErr::from)? as f64)
}

#[lumen::op(name = "close")]
fn op_close(fd: i32) -> Result<(), OpError> {
    Ok(close_impl(fd)?)
}

#[lumen::op(async, name = "closeAsync")]
fn op_close_async(fd: i32) -> Result<(), SendError> {
    Ok(close_impl(fd)?)
}

#[lumen::op(name = "read")]
fn op_read(ctx: &mut Ctx, fd: i32, buf: &mut [u8], pos: f64) -> Result<f64, OpError> {
    if is_std(fd) {
        return Ok(std_read(ctx, fd, buf)? as f64);
    }
    Ok(read_fd(fd, buf, pos)? as f64)
}
#[lumen::op(async, name = "readAsync")]
fn op_read_async(fd: i32, len: u32, pos: f64) -> Result<Vec<u8>, SendError> {
    let mut buf = vec![0u8; len as usize];
    let n = read_fd(fd, &mut buf, pos)?;
    buf.truncate(n);
    Ok(buf)
}

#[lumen::op(name = "write")]
fn op_write(ctx: &mut Ctx, fd: i32, data: &[u8], pos: f64) -> Result<f64, OpError> {
    if is_std(fd) {
        return Ok(std_write(ctx, fd, data)? as f64);
    }
    Ok(write_fd(fd, data, pos)? as f64)
}
#[lumen::op(async, name = "writeAsync")]
fn op_write_async(fd: i32, data: Vec<u8>, pos: f64) -> Result<f64, SendError> {
    Ok(write_fd(fd, &data, pos)? as f64)
}

#[lumen::op(name = "fstat")]
fn op_fstat(fd: i32) -> Result<Vec<f64>, OpError> {
    Ok(fstat_impl(fd)?)
}

#[lumen::op(async, name = "fstatAsync")]
fn op_fstat_async(fd: i32) -> Result<Vec<f64>, SendError> {
    Ok(fstat_impl(fd)?)
}

#[lumen::op(name = "stat")]
fn op_stat(path: &str) -> Result<Vec<f64>, OpError> {
    Ok(stat_path(path, true)?)
}

#[lumen::op(async, name = "statAsync")]
fn op_stat_async(path: String) -> Result<Vec<f64>, SendError> {
    Ok(stat_path(&path, true)?)
}

#[lumen::op(name = "lstat")]
fn op_lstat(path: &str) -> Result<Vec<f64>, OpError> {
    Ok(stat_path(path, false)?)
}

#[lumen::op(async, name = "lstatAsync")]
fn op_lstat_async(path: String) -> Result<Vec<f64>, SendError> {
    Ok(stat_path(&path, false)?)
}

#[lumen::op(name = "statMaybe")]
fn op_stat_maybe(path: &str, follow: bool) -> Result<Option<Vec<f64>>, OpError> {
    match stat_path(path, follow) {
        Ok(v) => Ok(Some(v)),
        Err(UvErr("ENOENT")) | Err(UvErr("ENOTDIR")) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[lumen::op(name = "statfs")]
fn op_statfs(path: &str) -> Result<Vec<f64>, OpError> {
    Ok(statfs_impl(path)?)
}

#[lumen::op(async, name = "statfsAsync")]
fn op_statfs_async(path: String) -> Result<Vec<f64>, SendError> {
    Ok(statfs_impl(&path)?)
}

#[lumen::op(name = "ftruncate")]
fn op_ftruncate(fd: i32, len: f64) -> Result<(), OpError> {
    Ok(vfs::ftruncate(fd, len.max(0.0) as u64).map_err(UvErr::from)?)
}

#[lumen::op(async, name = "ftruncateAsync")]
fn op_ftruncate_async(fd: i32, len: f64) -> Result<(), SendError> {
    Ok(vfs::ftruncate(fd, len.max(0.0) as u64).map_err(UvErr::from)?)
}

#[lumen::op(name = "fsync")]
fn op_fsync(fd: i32, data_only: bool) -> Result<(), OpError> {
    {
        let _ = data_only;
        if is_std(fd) || vfs::is_open(fd) {
            Ok(())
        } else {
            Err(UvErr("EBADF").into())
        }
    }
}

#[lumen::op(async, name = "fsyncAsync")]
fn op_fsync_async(fd: i32, data_only: bool) -> Result<(), SendError> {
    {
        let _ = data_only;
        if is_std(fd) || vfs::is_open(fd) {
            Ok(())
        } else {
            Err(UvErr("EBADF").into())
        }
    }
}

#[lumen::op(name = "fchmod")]
fn op_fchmod(fd: i32, mode: u32) -> Result<(), OpError> {
    Ok(vfs::fchmod(fd, mode).map_err(UvErr::from)?)
}

#[lumen::op(async, name = "fchmodAsync")]
fn op_fchmod_async(fd: i32, mode: u32) -> Result<(), SendError> {
    Ok(vfs::fchmod(fd, mode).map_err(UvErr::from)?)
}

#[lumen::op(name = "fchown")]
fn op_fchown(fd: i32, uid: u32, gid: u32) -> Result<(), OpError> {
    {
        let _ = (uid, gid);
        vfs::fstat(fd).map_err(UvErr::from)?;
        Ok(())
    }
}

#[lumen::op(async, name = "fchownAsync")]
fn op_fchown_async(fd: i32, uid: u32, gid: u32) -> Result<(), SendError> {
    {
        let _ = (uid, gid);
        vfs::fstat(fd).map_err(UvErr::from)?;
        Ok(())
    }
}

#[lumen::op(name = "futimes")]
fn op_futimes(fd: i32, atime: f64, mtime: f64) -> Result<(), OpError> {
    Ok(vfs::futimes(fd, secs_ms(atime), secs_ms(mtime)).map_err(UvErr::from)?)
}

#[lumen::op(async, name = "futimesAsync")]
fn op_futimes_async(fd: i32, atime: f64, mtime: f64) -> Result<(), SendError> {
    Ok(vfs::futimes(fd, secs_ms(atime), secs_ms(mtime)).map_err(UvErr::from)?)
}

#[lumen::op(name = "utimes")]
fn op_utimes(path: &str, atime: f64, mtime: f64, follow: bool) -> Result<(), OpError> {
    Ok(vfs::utimes(path, secs_ms(atime), secs_ms(mtime), follow).map_err(UvErr::from)?)
}

#[lumen::op(async, name = "utimesAsync")]
fn op_utimes_async(path: String, atime: f64, mtime: f64, follow: bool) -> Result<(), SendError> {
    Ok(vfs::utimes(&path, secs_ms(atime), secs_ms(mtime), follow).map_err(UvErr::from)?)
}

#[lumen::op(name = "access")]
fn op_access(path: &str, mode: u32) -> Result<(), OpError> {
    Ok(access_impl(path, mode)?)
}

#[lumen::op(async, name = "accessAsync")]
fn op_access_async(path: String, mode: u32) -> Result<(), SendError> {
    Ok(access_impl(&path, mode)?)
}

#[lumen::op(name = "exists")]
fn op_exists(path: &str) -> bool {
    vfs::exists(path)
}

#[lumen::op(name = "chmod")]
fn op_chmod(path: &str, mode: u32) -> Result<(), OpError> {
    Ok(vfs::chmod(path, mode).map_err(UvErr::from)?)
}

#[lumen::op(async, name = "chmodAsync")]
fn op_chmod_async(path: String, mode: u32) -> Result<(), SendError> {
    Ok(vfs::chmod(&path, mode).map_err(UvErr::from)?)
}

#[lumen::op(name = "chown")]
fn op_chown(path: &str, uid: u32, gid: u32, follow: bool) -> Result<(), OpError> {
    Ok(vfs::chown(path, uid, gid, follow).map_err(UvErr::from)?)
}

#[lumen::op(async, name = "chownAsync")]
fn op_chown_async(path: String, uid: u32, gid: u32, follow: bool) -> Result<(), SendError> {
    Ok(vfs::chown(&path, uid, gid, follow).map_err(UvErr::from)?)
}

#[lumen::op(name = "mkdir")]
fn op_mkdir(path: &str, mode: u32, recursive: bool) -> Result<Option<String>, OpError> {
    Ok(mkdir_impl(path, mode, recursive)?)
}

#[lumen::op(async, name = "mkdirAsync")]
fn op_mkdir_async(path: String, mode: u32, recursive: bool) -> Result<Option<String>, SendError> {
    Ok(mkdir_impl(&path, mode, recursive)?)
}

#[lumen::op(name = "mkdtemp")]
fn op_mkdtemp(prefix: &str) -> Result<String, OpError> {
    Ok(vfs::mkdtemp(prefix).map_err(UvErr::from)?)
}

#[lumen::op(async, name = "mkdtempAsync")]
fn op_mkdtemp_async(prefix: String) -> Result<String, SendError> {
    Ok(vfs::mkdtemp(&prefix).map_err(UvErr::from)?)
}

#[lumen::op(name = "rmdir")]
fn op_rmdir(path: &str) -> Result<(), OpError> {
    Ok(vfs::rmdir(path).map_err(UvErr::from)?)
}

#[lumen::op(async, name = "rmdirAsync")]
fn op_rmdir_async(path: String) -> Result<(), SendError> {
    Ok(vfs::rmdir(&path).map_err(UvErr::from)?)
}

#[lumen::op(name = "unlink")]
fn op_unlink(path: &str) -> Result<(), OpError> {
    Ok(vfs::unlink(path).map_err(UvErr::from)?)
}

#[lumen::op(async, name = "unlinkAsync")]
fn op_unlink_async(path: String) -> Result<(), SendError> {
    Ok(vfs::unlink(&path).map_err(UvErr::from)?)
}

#[lumen::op(name = "rename")]
fn op_rename(from: &str, to: &str) -> Result<(), OpError> {
    Ok(vfs::rename(from, to).map_err(UvErr::from)?)
}

#[lumen::op(async, name = "renameAsync")]
fn op_rename_async(from: String, to: String) -> Result<(), SendError> {
    Ok(vfs::rename(&from, &to).map_err(UvErr::from)?)
}

#[lumen::op(name = "link")]
fn op_link(existing: &str, path: &str) -> Result<(), OpError> {
    Ok(vfs::link(existing, path).map_err(UvErr::from)?)
}

#[lumen::op(async, name = "linkAsync")]
fn op_link_async(existing: String, path: String) -> Result<(), SendError> {
    Ok(vfs::link(&existing, &path).map_err(UvErr::from)?)
}

#[lumen::op(name = "symlink")]
fn op_symlink(target: &str, path: &str, flags: u32) -> Result<(), OpError> {
    {
        let _ = flags;
        Ok(symlink_impl(target, path)?)
    }
}

#[lumen::op(async, name = "symlinkAsync")]
fn op_symlink_async(target: String, path: String, flags: u32) -> Result<(), SendError> {
    {
        let _ = flags;
        Ok(symlink_impl(&target, &path)?)
    }
}

#[lumen::op(name = "readlink")]
fn op_readlink(path: &str) -> Result<String, OpError> {
    Ok(vfs::readlink(path).map_err(UvErr::from)?)
}

#[lumen::op(async, name = "readlinkAsync")]
fn op_readlink_async(path: String) -> Result<String, SendError> {
    Ok(vfs::readlink(&path).map_err(UvErr::from)?)
}

#[lumen::op(name = "realpath")]
fn op_realpath(path: &str) -> Result<String, OpError> {
    Ok(vfs::realpath(path).map_err(UvErr::from)?)
}

#[lumen::op(async, name = "realpathAsync")]
fn op_realpath_async(path: String) -> Result<String, SendError> {
    Ok(vfs::realpath(&path).map_err(UvErr::from)?)
}

#[lumen::op(name = "copyFile")]
fn op_copy_file(src: &str, dst: &str, mode: u32) -> Result<(), OpError> {
    Ok(copyfile_impl(src, dst, mode)?)
}

#[lumen::op(async, name = "copyFileAsync")]
fn op_copy_file_async(src: String, dst: String, mode: u32) -> Result<(), SendError> {
    Ok(copyfile_impl(&src, &dst, mode)?)
}

#[lumen::op(name = "readdir")]
fn op_readdir(path: &str) -> Result<(Vec<String>, Vec<f64>), OpError> {
    Ok(readdir_impl(path)?)
}

#[lumen::op(async, name = "readdirAsync")]
fn op_readdir_async(path: String) -> Result<(Vec<String>, Vec<f64>), SendError> {
    Ok(readdir_impl(&path)?)
}

#[lumen::op(name = "readFile")]
fn op_read_file(path: &str, flags: i32) -> Result<Vec<u8>, OpError> {
    Ok(read_file_impl(path, flags)?)
}

#[lumen::op(async, name = "readFileAsync")]
fn op_read_file_async(path: String, flags: i32) -> Result<Vec<u8>, SendError> {
    Ok(read_file_impl(&path, flags)?)
}

#[lumen::op(name = "readFileUtf8")]
fn op_read_file_utf8(path: &str, flags: i32) -> Result<String, OpError> {
    let bytes = read_file_impl(path, flags)?;
    let s = match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    };
    Ok(crate::codec::canonical(s))
}

#[lumen::op(name = "writeFile")]
fn op_write_file(path: &str, data: &[u8], flags: i32, mode: u32) -> Result<(), OpError> {
    Ok(write_file_impl(path, data, flags, mode)?)
}

#[lumen::op(async, name = "writeFileAsync")]
fn op_write_file_async(path: String, data: Vec<u8>, flags: i32, mode: u32) -> Result<(), SendError> {
    Ok(write_file_impl(&path, &data, flags, mode)?)
}

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
