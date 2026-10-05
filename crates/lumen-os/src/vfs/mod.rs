//! One file-system interface for every runtime: [`FileSystem`] is the set of libuv-flavoured
//! operations (`open`/`read`/`stat`/`readdir`/`mkdir -p`/...) that Node's `fs` binding and
//! Python's `os`/`io` are written against. Backends:
//!
//! - [`OsFs`]: the host OS, through [`crate::fs`].
//! - [`MemFs`]: an in-memory tree (the whole file system on targets without an OS, see
//!   [`host`]; bundled files for an embedder).
//! - [`Overlay`]: a (read-only) tree layered over another, e.g. an embedded standard library
//!   over the OS.
//!
//! Errors are libuv codes ([`FsError`]). Every method has a default that fails with `ENOSYS`,
//! so a partial backend implements only what it supports.

mod mem;

pub use mem::{mem, Backend, MemFs, RemoteEntry, RemoteStat};

use std::sync::Arc;

use crate::errno::FsError;
use crate::fs::flags::{O_CREAT, O_EXCL, O_TRUNC, O_WRONLY};
use crate::fs::{self as os, DirentKind, Stat, StatFs, Timespec, S_IFDIR, S_IFMT};

type R<T> = Result<T, FsError>;

const ENOSYS: FsError = FsError("ENOSYS");

/// A file system. Paths are UTF-8; descriptors are small integers owned by the backend that
/// handed them out. All methods take `&self`: backends synchronise internally, so one instance
/// serves a runtime's worker threads.
pub trait FileSystem: Send + Sync {
    /// `open(path, O_* flags, mode)` with the platform's [`crate::fs::flags`] values.
    fn open(&self, _path: &str, _flags: i32, _mode: u32) -> R<i32> {
        Err(ENOSYS)
    }
    fn close(&self, _fd: i32) -> R<()> {
        Err(ENOSYS)
    }
    /// Whether `fd` was handed out by this backend and is still open.
    fn is_open(&self, _fd: i32) -> bool {
        false
    }
    /// Reads into `buf` at `pos`, or at (and advancing) the descriptor's offset when `None`.
    fn read(&self, _fd: i32, _buf: &mut [u8], _pos: Option<u64>) -> R<usize> {
        Err(ENOSYS)
    }
    fn write(&self, _fd: i32, _data: &[u8], _pos: Option<u64>) -> R<usize> {
        Err(ENOSYS)
    }
    fn lseek(&self, _fd: i32, _offset: i64, _whence: i32) -> R<u64> {
        Err(ENOSYS)
    }
    fn dup(&self, _fd: i32) -> R<i32> {
        Err(ENOSYS)
    }
    fn fstat(&self, _fd: i32) -> R<Stat> {
        Err(ENOSYS)
    }
    fn ftruncate(&self, _fd: i32, _len: u64) -> R<()> {
        Err(ENOSYS)
    }
    fn fsync(&self, _fd: i32, _data_only: bool) -> R<()> {
        Err(ENOSYS)
    }
    fn fchmod(&self, _fd: i32, _mode: u32) -> R<()> {
        Err(ENOSYS)
    }
    fn fchown(&self, _fd: i32, _uid: u32, _gid: u32) -> R<()> {
        Err(ENOSYS)
    }
    fn futimes(&self, _fd: i32, _atime: Timespec, _mtime: Timespec) -> R<()> {
        Err(ENOSYS)
    }

    fn stat(&self, _path: &str, _follow: bool) -> R<Stat> {
        Err(ENOSYS)
    }
    fn statfs(&self, _path: &str) -> R<StatFs> {
        Err(ENOSYS)
    }
    /// `access(path, F_OK/R_OK/W_OK/X_OK bits)`.
    fn access(&self, _path: &str, _mode: u32) -> R<()> {
        Err(ENOSYS)
    }
    fn exists(&self, path: &str) -> bool {
        self.stat(path, true).is_ok()
    }
    fn chmod(&self, _path: &str, _mode: u32) -> R<()> {
        Err(ENOSYS)
    }
    fn chown(&self, _path: &str, _uid: u32, _gid: u32, _follow: bool) -> R<()> {
        Err(ENOSYS)
    }
    fn utimes(&self, _path: &str, _atime: Timespec, _mtime: Timespec, _follow: bool) -> R<()> {
        Err(ENOSYS)
    }
    /// With `recursive`, creates the missing ancestors too and returns the first directory it
    /// created (Node's contract); an existing directory is then not an error.
    fn mkdir(&self, _path: &str, _mode: u32, _recursive: bool) -> R<Option<String>> {
        Err(ENOSYS)
    }
    fn mkdtemp(&self, _prefix: &str) -> R<String> {
        Err(ENOSYS)
    }
    fn rmdir(&self, _path: &str) -> R<()> {
        Err(ENOSYS)
    }
    fn unlink(&self, _path: &str) -> R<()> {
        Err(ENOSYS)
    }
    fn rename(&self, _from: &str, _to: &str) -> R<()> {
        Err(ENOSYS)
    }
    fn link(&self, _existing: &str, _path: &str) -> R<()> {
        Err(ENOSYS)
    }
    /// `flags` are libuv's (`UV_FS_SYMLINK_DIR` = 1, `UV_FS_SYMLINK_JUNCTION` = 2).
    fn symlink(&self, _target: &str, _path: &str, _flags: u32) -> R<()> {
        Err(ENOSYS)
    }
    fn readlink(&self, _path: &str) -> R<String> {
        Err(ENOSYS)
    }
    fn realpath(&self, _path: &str) -> R<String> {
        Err(ENOSYS)
    }
    /// `mode` is libuv's copyfile flags (`COPYFILE_EXCL` = 1, `FICLONE` = 2, `FICLONE_FORCE` = 4).
    fn copy_file(&self, _src: &str, _dst: &str, _mode: u32) -> R<()> {
        Err(ENOSYS)
    }
    /// The entries without `.` and `..`, with their kinds.
    fn readdir(&self, _path: &str) -> R<Vec<(String, DirentKind)>> {
        Err(ENOSYS)
    }
    /// A whole file; `flags` are the open flags (Node's `readFile(path, { flag })`).
    fn read_file(&self, path: &str, flags: i32) -> R<Vec<u8>> {
        let fd = self.open(path, flags, 0)?;
        let mut out = Vec::new();
        let mut chunk = vec![0u8; 64 * 1024];
        let result = loop {
            match self.read(fd, &mut chunk, None) {
                Ok(0) => break Ok(()),
                Ok(n) => out.extend_from_slice(&chunk[..n]),
                Err(e) => break Err(e),
            }
        };
        let _ = self.close(fd);
        result.map(|_| out)
    }
    fn write_file(&self, path: &str, data: &[u8], flags: i32, mode: u32) -> R<()> {
        let fd = self.open(path, flags, mode)?;
        let mut done = 0;
        let result = loop {
            if done == data.len() {
                break Ok(());
            }
            match self.write(fd, &data[done..], None) {
                Ok(0) => break Err(FsError("EIO")),
                Ok(n) => done += n,
                Err(e) => break Err(e),
            }
        };
        let _ = self.close(fd);
        result
    }
    /// The working directory that relative paths resolve against.
    fn cwd(&self) -> R<String> {
        Err(ENOSYS)
    }
    fn chdir(&self, _path: &str) -> R<()> {
        Err(ENOSYS)
    }
}

/// A backend that supports nothing: every operation fails with `ENOSYS`.
pub struct Unsupported;

impl FileSystem for Unsupported {}

/// The host operating system's file system.
pub struct OsFs;

impl FileSystem for OsFs {
    fn open(&self, path: &str, flags: i32, mode: u32) -> R<i32> {
        os::open(path, flags, mode)
    }
    fn close(&self, fd: i32) -> R<()> {
        os::close(fd)
    }
    fn is_open(&self, fd: i32) -> bool {
        os::is_open(fd)
    }
    fn read(&self, fd: i32, buf: &mut [u8], pos: Option<u64>) -> R<usize> {
        os::read(fd, buf, pos)
    }
    fn write(&self, fd: i32, data: &[u8], pos: Option<u64>) -> R<usize> {
        os::write(fd, data, pos)
    }
    fn lseek(&self, fd: i32, offset: i64, whence: i32) -> R<u64> {
        os::lseek(fd, offset, whence)
    }
    fn dup(&self, fd: i32) -> R<i32> {
        os::dup(fd)
    }
    fn fstat(&self, fd: i32) -> R<Stat> {
        os::fstat(fd)
    }
    fn ftruncate(&self, fd: i32, len: u64) -> R<()> {
        os::ftruncate(fd, len)
    }
    fn fsync(&self, fd: i32, data_only: bool) -> R<()> {
        os::fsync(fd, data_only)
    }
    fn fchmod(&self, fd: i32, mode: u32) -> R<()> {
        os::fchmod(fd, mode)
    }
    fn fchown(&self, fd: i32, uid: u32, gid: u32) -> R<()> {
        os::fchown(fd, uid, gid)
    }
    fn futimes(&self, fd: i32, atime: Timespec, mtime: Timespec) -> R<()> {
        os::futimes(fd, atime, mtime)
    }
    fn stat(&self, path: &str, follow: bool) -> R<Stat> {
        os::stat(path, follow)
    }
    fn statfs(&self, path: &str) -> R<StatFs> {
        os::statfs(path)
    }
    fn access(&self, path: &str, mode: u32) -> R<()> {
        os::access(path, mode)
    }
    fn exists(&self, path: &str) -> bool {
        os::exists(path)
    }
    fn chmod(&self, path: &str, mode: u32) -> R<()> {
        os::chmod(path, mode)
    }
    fn chown(&self, path: &str, uid: u32, gid: u32, follow: bool) -> R<()> {
        os::chown(path, uid, gid, follow)
    }
    fn utimes(&self, path: &str, atime: Timespec, mtime: Timespec, follow: bool) -> R<()> {
        os::utimes(path, atime, mtime, follow)
    }
    fn mkdir(&self, path: &str, mode: u32, recursive: bool) -> R<Option<String>> {
        os::mkdir(path, mode, recursive)
    }
    fn mkdtemp(&self, prefix: &str) -> R<String> {
        os::mkdtemp(prefix)
    }
    fn rmdir(&self, path: &str) -> R<()> {
        os::rmdir(path)
    }
    fn unlink(&self, path: &str) -> R<()> {
        os::unlink(path)
    }
    fn rename(&self, from: &str, to: &str) -> R<()> {
        os::rename(from, to)
    }
    fn link(&self, existing: &str, path: &str) -> R<()> {
        os::link(existing, path)
    }
    fn symlink(&self, target: &str, path: &str, flags: u32) -> R<()> {
        os::symlink(target, path, flags)
    }
    fn readlink(&self, path: &str) -> R<String> {
        os::readlink(path)
    }
    fn realpath(&self, path: &str) -> R<String> {
        os::realpath(path)
    }
    fn copy_file(&self, src: &str, dst: &str, mode: u32) -> R<()> {
        os::copy_file(src, dst, mode)
    }
    fn readdir(&self, path: &str) -> R<Vec<(String, DirentKind)>> {
        os::readdir(path)
    }
    fn read_file(&self, path: &str, flags: i32) -> R<Vec<u8>> {
        os::read_file(path, flags)
    }
    fn write_file(&self, path: &str, data: &[u8], flags: i32, mode: u32) -> R<()> {
        os::write_file(path, data, flags, mode)
    }
    fn cwd(&self) -> R<String> {
        crate::proc::getcwd()
    }
    fn chdir(&self, path: &str) -> R<()> {
        crate::proc::chdir(path)
    }
}

/// The process's file system: the OS natively, the process-wide [`mem`] tree on targets
/// without one (`wasm32-unknown-unknown`).
pub fn host() -> &'static dyn FileSystem {
    if let Some(overlay) = EMBEDDED_ASSETS.get() {
        return overlay;
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        &OsFs
    }
    #[cfg(target_arch = "wasm32")]
    {
        mem()
    }
}

static EMBEDDED_ASSETS: std::sync::OnceLock<Overlay> = std::sync::OnceLock::new();

/// Mount application assets read-only at `/lumen-assets` for filesystem APIs.
/// The process-wide mount is intended for a standalone executable's one app.
pub fn install_assets(blob: &[u8]) -> Result<(), &'static str> {
    let Some(archive) = lumen_common::aot::assets::from_blob(blob)? else {
        return Ok(());
    };
    if EMBEDDED_ASSETS.get().is_some() {
        return Err("embedded assets already installed");
    }
    let upper = MemFs::with_fd_base(1 << 28);
    for (name, bytes) in archive.entries() {
        upper.insert(
            &format!("/lumen-assets/{name}"),
            bytes.to_vec(),
            0o444,
            0o555,
        );
    }
    let lower: Arc<dyn FileSystem> = Arc::new(OsFs);
    EMBEDDED_ASSETS
        .set(Overlay::new(Arc::new(upper), lower, true))
        .map_err(|_| "embedded assets already installed")
}

/// `upper` layered over `lower`: an absolute path that exists in `upper` (other than `/`) is
/// served from it, everything else from `lower`. Descriptors go to the backend that owns them,
/// so `upper` must number its descriptors apart from `lower`'s (see [`MemFs::with_fd_base`]).
/// With `read_only`, writing to anything in `upper` fails with `EACCES`.
pub struct Overlay {
    upper: Arc<dyn FileSystem>,
    lower: Arc<dyn FileSystem>,
    read_only: bool,
}

const EACCES: FsError = FsError("EACCES");

impl Overlay {
    pub fn new(upper: Arc<dyn FileSystem>, lower: Arc<dyn FileSystem>, read_only: bool) -> Overlay {
        Overlay {
            upper,
            lower,
            read_only,
        }
    }

    pub fn upper(&self) -> &Arc<dyn FileSystem> {
        &self.upper
    }

    pub fn lower(&self) -> &Arc<dyn FileSystem> {
        &self.lower
    }

    fn in_upper(&self, path: &str) -> bool {
        path.starts_with('/') && path.len() > 1 && self.upper.stat(path, false).is_ok()
    }

    fn by_path(&self, path: &str) -> &dyn FileSystem {
        if self.in_upper(path) {
            &*self.upper
        } else {
            &*self.lower
        }
    }

    fn by_fd(&self, fd: i32) -> &dyn FileSystem {
        if self.upper.is_open(fd) {
            &*self.upper
        } else {
            &*self.lower
        }
    }

    /// The backend for a mutation of `path`: `lower`, unless the path is in a read-only `upper`.
    fn for_write(&self, path: &str) -> R<&dyn FileSystem> {
        match (self.in_upper(path), self.read_only) {
            (true, true) => Err(EACCES),
            (true, false) => Ok(&*self.upper),
            (false, _) => Ok(&*self.lower),
        }
    }

    fn for_write_fd(&self, fd: i32) -> R<&dyn FileSystem> {
        match (self.upper.is_open(fd), self.read_only) {
            (true, true) => Err(EACCES),
            (true, false) => Ok(&*self.upper),
            (false, _) => Ok(&*self.lower),
        }
    }
}

impl FileSystem for Overlay {
    fn open(&self, path: &str, flags: i32, mode: u32) -> R<i32> {
        let writes = flags & 3 != 0 || flags & (O_CREAT | O_TRUNC) != 0;
        if writes {
            self.for_write(path)?.open(path, flags, mode)
        } else {
            self.by_path(path).open(path, flags, mode)
        }
    }
    fn close(&self, fd: i32) -> R<()> {
        self.by_fd(fd).close(fd)
    }
    fn is_open(&self, fd: i32) -> bool {
        self.upper.is_open(fd) || self.lower.is_open(fd)
    }
    fn read(&self, fd: i32, buf: &mut [u8], pos: Option<u64>) -> R<usize> {
        self.by_fd(fd).read(fd, buf, pos)
    }
    fn write(&self, fd: i32, data: &[u8], pos: Option<u64>) -> R<usize> {
        self.by_fd(fd).write(fd, data, pos)
    }
    fn lseek(&self, fd: i32, offset: i64, whence: i32) -> R<u64> {
        self.by_fd(fd).lseek(fd, offset, whence)
    }
    fn dup(&self, fd: i32) -> R<i32> {
        self.by_fd(fd).dup(fd)
    }
    fn fstat(&self, fd: i32) -> R<Stat> {
        self.by_fd(fd).fstat(fd)
    }
    fn ftruncate(&self, fd: i32, len: u64) -> R<()> {
        self.for_write_fd(fd)?.ftruncate(fd, len)
    }
    fn fsync(&self, fd: i32, data_only: bool) -> R<()> {
        self.by_fd(fd).fsync(fd, data_only)
    }
    fn fchmod(&self, fd: i32, mode: u32) -> R<()> {
        self.for_write_fd(fd)?.fchmod(fd, mode)
    }
    fn fchown(&self, fd: i32, uid: u32, gid: u32) -> R<()> {
        self.for_write_fd(fd)?.fchown(fd, uid, gid)
    }
    fn futimes(&self, fd: i32, atime: Timespec, mtime: Timespec) -> R<()> {
        self.for_write_fd(fd)?.futimes(fd, atime, mtime)
    }
    fn stat(&self, path: &str, follow: bool) -> R<Stat> {
        self.by_path(path).stat(path, follow)
    }
    fn statfs(&self, path: &str) -> R<StatFs> {
        self.by_path(path).statfs(path)
    }
    fn access(&self, path: &str, mode: u32) -> R<()> {
        if mode & 2 != 0 {
            self.for_write(path)?.access(path, mode)
        } else {
            self.by_path(path).access(path, mode)
        }
    }
    fn exists(&self, path: &str) -> bool {
        self.in_upper(path) || self.lower.exists(path)
    }
    fn chmod(&self, path: &str, mode: u32) -> R<()> {
        self.for_write(path)?.chmod(path, mode)
    }
    fn chown(&self, path: &str, uid: u32, gid: u32, follow: bool) -> R<()> {
        self.for_write(path)?.chown(path, uid, gid, follow)
    }
    fn utimes(&self, path: &str, atime: Timespec, mtime: Timespec, follow: bool) -> R<()> {
        self.for_write(path)?.utimes(path, atime, mtime, follow)
    }
    fn mkdir(&self, path: &str, mode: u32, recursive: bool) -> R<Option<String>> {
        if self.read_only && self.in_upper(path) {
            let is_dir = self
                .upper
                .stat(path, true)
                .is_ok_and(|s| s.mode & S_IFMT == S_IFDIR);
            return if recursive && is_dir {
                Ok(None)
            } else {
                Err(FsError("EEXIST"))
            };
        }
        self.for_write(path)?.mkdir(path, mode, recursive)
    }
    fn mkdtemp(&self, prefix: &str) -> R<String> {
        self.lower.mkdtemp(prefix)
    }
    fn rmdir(&self, path: &str) -> R<()> {
        self.for_write(path)?.rmdir(path)
    }
    fn unlink(&self, path: &str) -> R<()> {
        self.for_write(path)?.unlink(path)
    }
    fn rename(&self, from: &str, to: &str) -> R<()> {
        let src = self.for_write(from)?;
        let dst = self.for_write(to)?;
        if !std::ptr::addr_eq(src, dst) {
            return Err(FsError("EXDEV"));
        }
        src.rename(from, to)
    }
    fn link(&self, existing: &str, path: &str) -> R<()> {
        let src = self.for_write(existing)?;
        let dst = self.for_write(path)?;
        if !std::ptr::addr_eq(src, dst) {
            return Err(FsError("EXDEV"));
        }
        src.link(existing, path)
    }
    fn symlink(&self, target: &str, path: &str, flags: u32) -> R<()> {
        self.for_write(path)?.symlink(target, path, flags)
    }
    fn readlink(&self, path: &str) -> R<String> {
        self.by_path(path).readlink(path)
    }
    fn realpath(&self, path: &str) -> R<String> {
        self.by_path(path).realpath(path)
    }
    fn copy_file(&self, src: &str, dst: &str, mode: u32) -> R<()> {
        let to = self.for_write(dst)?;
        if !self.in_upper(src) {
            return to.copy_file(src, dst, mode);
        }
        let data = self.upper.read_file(src, 0)?;
        let perm = self.upper.stat(src, true)?.mode & 0o7777;
        let excl = if mode & 1 != 0 { O_EXCL } else { 0 };
        to.write_file(dst, &data, O_WRONLY | O_CREAT | O_TRUNC | excl, perm)
    }
    fn readdir(&self, path: &str) -> R<Vec<(String, DirentKind)>> {
        self.by_path(path).readdir(path)
    }
    fn read_file(&self, path: &str, flags: i32) -> R<Vec<u8>> {
        if flags & 3 != 0 {
            return self.for_write(path)?.read_file(path, flags);
        }
        self.by_path(path).read_file(path, flags)
    }
    fn write_file(&self, path: &str, data: &[u8], flags: i32, mode: u32) -> R<()> {
        self.for_write(path)?.write_file(path, data, flags, mode)
    }
    fn cwd(&self) -> R<String> {
        self.lower.cwd()
    }
    fn chdir(&self, path: &str) -> R<()> {
        self.lower.chdir(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::flags::O_RDWR;

    #[test]
    fn overlay_serves_upper_read_only_and_defers_the_rest() {
        let upper = MemFs::with_fd_base(1 << 20);
        upper.insert("/frozen/lib/a.py", &b"A = 1\n"[..], 0o444, 0o555);
        let lower = Arc::new(MemFs::new());
        lower.mkdir("/frozen", 0o755, false).unwrap();
        lower
            .write_file("/tmp.txt", b"lower", O_WRONLY | O_CREAT, 0o644)
            .unwrap();
        let fs = Overlay::new(Arc::new(upper), lower.clone(), true);
        assert_eq!(fs.read_file("/frozen/lib/a.py", 0).unwrap(), b"A = 1\n");
        assert_eq!(fs.read_file("/tmp.txt", 0).unwrap(), b"lower");
        assert_eq!(fs.readdir("/").unwrap().len(), 2, "the root is lower's");
        assert_eq!(fs.open("/frozen/lib/a.py", O_RDWR, 0), Err(EACCES));
        assert_eq!(fs.access("/frozen/lib/a.py", 2), Err(EACCES));
        let fd = fs.open("/frozen/lib/a.py", 0, 0).unwrap();
        assert!(fd >= 1 << 20);
        let mut buf = [0u8; 3];
        assert_eq!(fs.read(fd, &mut buf, None).unwrap(), 3);
        assert_eq!(fs.lseek(fd, 0, 1).unwrap(), 3);
        fs.close(fd).unwrap();
        fs.copy_file("/frozen/lib/a.py", "/copy.py", 0).unwrap();
        assert_eq!(lower.read_file("/copy.py", 0).unwrap(), b"A = 1\n");
        assert_eq!(fs.unlink("/frozen/lib/a.py"), Err(EACCES));
    }

    #[test]
    fn unsupported_backend_reports_enosys() {
        assert_eq!(Unsupported.stat("/", true), Err(ENOSYS));
        assert!(!Unsupported.exists("/"));
    }
}
