//! The boundary between the interpreter and the host. Everything the core needs from an
//! operating system (streams, files, clocks, entropy, environment, module sources) goes through
//! [`Platform`], so an embedder without an OS can supply its own implementation. Files go
//! through the platform's [`FileSystem`] (`lumen_os::vfs`), the same backends Node's `fs` runs on.

use lumen_common::civil::Tm;
pub use lumen_os::vfs::{FileSystem, MemFs, OsFs, Overlay, Unsupported};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IoErrorKind {
    NotFound,
    PermissionDenied,
    AlreadyExists,
    IsADirectory,
    NotADirectory,
    Other,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct IoError {
    pub kind: IoErrorKind,
    pub errno: i32,
}

impl IoError {
    pub fn new(kind: IoErrorKind) -> IoError {
        let code = match kind {
            IoErrorKind::NotFound => "ENOENT",
            IoErrorKind::PermissionDenied => "EACCES",
            IoErrorKind::AlreadyExists => "EEXIST",
            IoErrorKind::IsADirectory => "EISDIR",
            IoErrorKind::NotADirectory => "ENOTDIR",
            IoErrorKind::Other => "EIO",
        };
        IoError {
            kind,
            errno: lumen_os::errno::errno_of_code(code).unwrap_or(5),
        }
    }

    pub fn from_errno(errno: i32) -> IoError {
        let kind = match lumen_os::errno::code_of_errno(errno) {
            Some("ENOENT") => IoErrorKind::NotFound,
            Some("EACCES" | "EPERM") => IoErrorKind::PermissionDenied,
            Some("EEXIST") => IoErrorKind::AlreadyExists,
            Some("EISDIR") => IoErrorKind::IsADirectory,
            Some("ENOTDIR") => IoErrorKind::NotADirectory,
            _ => IoErrorKind::Other,
        };
        IoError { kind, errno }
    }

    pub fn from_code(code: &str) -> IoError {
        IoError::from_errno(lumen_os::errno::errno_of_code(code).unwrap_or(5))
    }

    pub fn message(&self) -> &'static str {
        lumen_os::errno::code_of_errno(self.errno).map_or("Unknown error", lumen_os::errno::message)
    }

    /// The host C library's text for this errno, as `os.strerror` reports it.
    pub fn strerror(&self) -> String {
        lumen_os::errno::strerror(self.errno)
    }
}

impl From<lumen_os::FsError> for IoError {
    fn from(e: lumen_os::FsError) -> IoError {
        IoError::from_errno(e.errno())
    }
}

pub type PResult<T> = Result<T, IoError>;

pub type Fd = i32;

pub use lumen_os::fs::{DirentKind, Stat as OsStat, Timespec};

impl std::fmt::Display for IoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[Errno {}] {}", self.errno, self.message())
    }
}

pub struct FoundModule {
    pub filename: String,
    pub source: Vec<u8>,
    pub is_package: bool,
}

pub type PlatformRef = Rc<RefCell<Box<dyn Platform>>>;

pub fn join_path(dir: &str, leaf: &str) -> String {
    if dir.is_empty() {
        leaf.to_string()
    } else if dir.ends_with('/') {
        format!("{}{}", dir, leaf)
    } else {
        format!("{}/{}", dir, leaf)
    }
}

pub fn parent_dir(path: &str) -> String {
    match path.rfind(['/', '\\']) {
        Some(0) => path[..1].to_string(),
        Some(i) => path[..i].to_string(),
        None => ".".to_string(),
    }
}

pub trait Platform {
    fn write_stdout(&mut self, bytes: &[u8]);
    fn write_stderr(&mut self, bytes: &[u8]);
    fn flush_stdout(&mut self) {}

    /// Reads what is available from standard input (fd 0 when the file system has no fd 0
    /// open); 0 at end of input.
    fn read_stdin(&mut self, _buf: &mut [u8]) -> PResult<usize> {
        Err(no_sys())
    }

    fn monotonic_ns(&self) -> u64;
    fn wall_time_ns(&self) -> u64;
    fn sleep(&mut self, secs: f64);
    fn entropy(&mut self, buf: &mut [u8]);

    /// Whether Python threads may run on OS threads of this host.
    fn supports_threads(&self) -> bool {
        false
    }

    /// The file system every path and descriptor operation below goes through.
    fn filesystem(&self) -> Arc<dyn FileSystem> {
        static NONE: OnceLock<Arc<dyn FileSystem>> = OnceLock::new();
        NONE.get_or_init(|| Arc::new(Unsupported)).clone()
    }

    fn is_file(&mut self, path: &str) -> bool {
        self.filesystem()
            .stat(path, true)
            .is_ok_and(|s| s.mode & lumen_os::fs::S_IFMT == lumen_os::fs::S_IFREG)
    }
    fn is_dir(&mut self, path: &str) -> bool {
        self.filesystem()
            .stat(path, true)
            .is_ok_and(|s| s.mode & lumen_os::fs::S_IFMT == lumen_os::fs::S_IFDIR)
    }

    fn read_file(&mut self, path: &str) -> Result<Vec<u8>, IoError> {
        Ok(self.filesystem().read_file(path, 0)?)
    }

    fn canonicalize(&mut self, path: &str) -> String {
        self.filesystem()
            .realpath(path)
            .unwrap_or_else(|_| path.to_string())
    }

    /// Resolves `name` (a single path component) against the entries in order, preferring a
    /// package (`name/__init__.py`) over a plain module (`name.py`) in the same entry.
    fn find_module(&mut self, entries: &[String], name: &str) -> Option<FoundModule> {
        for dir in entries {
            let pkg = join_path(&join_path(dir, name), "__init__.py");
            if self.is_file(&pkg) {
                return self.read_file(&pkg).ok().map(|source| FoundModule {
                    filename: pkg,
                    source,
                    is_package: true,
                });
            }
            let file = join_path(dir, &format!("{}.py", name));
            if self.is_file(&file) {
                return self.read_file(&file).ok().map(|source| FoundModule {
                    filename: file,
                    source,
                    is_package: false,
                });
            }
        }
        None
    }

    fn env_var(&self, name: &str) -> Option<String>;
    fn platform_name(&self) -> String;
    fn executable(&self) -> String {
        "lumen-py".to_string()
    }
    fn argv(&self) -> Vec<String> {
        Vec::new()
    }

    // ---- descriptor-level file system (what `posix` and `_io.FileIO` are built on) -----------
    //
    // Thin adapters over [`Platform::filesystem`]. Descriptors 0, 1 and 2 are the standard
    // streams unless the file system has them open.

    fn fd_open(&mut self, path: &str, flags: i32, mode: u32) -> PResult<Fd> {
        Ok(self.filesystem().open(path, flags, mode)?)
    }
    fn fd_close(&mut self, fd: Fd) -> PResult<()> {
        let fs = self.filesystem();
        if lumen_os::fs::is_std(fd) && !fs.is_open(fd) {
            return Ok(());
        }
        Ok(fs.close(fd)?)
    }
    /// Reads at the current position, or at `pos` without moving it. 0 is end of file.
    fn fd_read(&mut self, fd: Fd, buf: &mut [u8], pos: Option<u64>) -> PResult<usize> {
        let fs = self.filesystem();
        if fd == 0 && !fs.is_open(0) {
            return self.read_stdin(buf);
        }
        Ok(fs.read(fd, buf, pos)?)
    }
    fn fd_write(&mut self, fd: Fd, data: &[u8], pos: Option<u64>) -> PResult<usize> {
        let fs = self.filesystem();
        match fd {
            1 if !fs.is_open(1) => self.write_stdout(data),
            2 if !fs.is_open(2) => self.write_stderr(data),
            _ => return Ok(fs.write(fd, data, pos)?),
        }
        Ok(data.len())
    }
    /// `whence`: 0 from the start, 1 from the current position, 2 from the end.
    fn fd_seek(&mut self, fd: Fd, offset: i64, whence: i32) -> PResult<u64> {
        Ok(self.filesystem().lseek(fd, offset, whence)?)
    }
    fn fd_stat(&mut self, fd: Fd) -> PResult<OsStat> {
        Ok(self.filesystem().fstat(fd)?)
    }
    fn fd_truncate(&mut self, fd: Fd, len: u64) -> PResult<()> {
        Ok(self.filesystem().ftruncate(fd, len)?)
    }
    fn fd_sync(&mut self, fd: Fd, data_only: bool) -> PResult<()> {
        Ok(self.filesystem().fsync(fd, data_only)?)
    }
    fn fd_dup(&mut self, fd: Fd) -> PResult<Fd> {
        Ok(self.filesystem().dup(fd)?)
    }
    fn fd_chmod(&mut self, fd: Fd, mode: u32) -> PResult<()> {
        Ok(self.filesystem().fchmod(fd, mode)?)
    }
    fn fd_utimes(&mut self, fd: Fd, atime: Timespec, mtime: Timespec) -> PResult<()> {
        Ok(self.filesystem().futimes(fd, atime, mtime)?)
    }
    fn fd_isatty(&mut self, _fd: Fd) -> bool {
        false
    }
    fn fd_pipe(&mut self) -> PResult<(Fd, Fd)> {
        Err(no_sys())
    }
    fn terminal_size(&mut self, _fd: Fd) -> PResult<(u32, u32)> {
        Err(no_sys())
    }

    /// `stat` (`follow`) or `lstat`.
    fn stat(&mut self, path: &str, follow: bool) -> PResult<OsStat> {
        Ok(self.filesystem().stat(path, follow)?)
    }
    /// Directory entries without `.` and `..`, in OS order.
    fn listdir(&mut self, path: &str) -> PResult<Vec<(String, DirentKind)>> {
        Ok(self.filesystem().readdir(path)?)
    }
    fn mkdir(&mut self, path: &str, mode: u32) -> PResult<()> {
        self.filesystem().mkdir(path, mode, false)?;
        Ok(())
    }
    fn rmdir(&mut self, path: &str) -> PResult<()> {
        Ok(self.filesystem().rmdir(path)?)
    }
    fn unlink(&mut self, path: &str) -> PResult<()> {
        Ok(self.filesystem().unlink(path)?)
    }
    fn rename(&mut self, from: &str, to: &str) -> PResult<()> {
        Ok(self.filesystem().rename(from, to)?)
    }
    fn link(&mut self, existing: &str, path: &str) -> PResult<()> {
        Ok(self.filesystem().link(existing, path)?)
    }
    fn symlink(&mut self, target: &str, path: &str, is_dir: bool) -> PResult<()> {
        Ok(self.filesystem().symlink(target, path, is_dir as u32)?)
    }
    fn readlink(&mut self, path: &str) -> PResult<String> {
        Ok(self.filesystem().readlink(path)?)
    }
    fn realpath(&mut self, path: &str) -> PResult<String> {
        Ok(self.filesystem().realpath(path)?)
    }
    /// `mode` bits `F_OK`=0, `X_OK`=1, `W_OK`=2, `R_OK`=4.
    fn access(&mut self, path: &str, mode: u32) -> PResult<()> {
        Ok(self.filesystem().access(path, mode)?)
    }
    fn chmod(&mut self, path: &str, mode: u32) -> PResult<()> {
        Ok(self.filesystem().chmod(path, mode)?)
    }
    fn chown(&mut self, path: &str, uid: u32, gid: u32, follow: bool) -> PResult<()> {
        Ok(self.filesystem().chown(path, uid, gid, follow)?)
    }
    fn utimes(
        &mut self,
        path: &str,
        atime: Timespec,
        mtime: Timespec,
        follow: bool,
    ) -> PResult<()> {
        Ok(self.filesystem().utimes(path, atime, mtime, follow)?)
    }
    fn getcwd(&mut self) -> PResult<String> {
        Ok(self.filesystem().cwd()?)
    }
    fn chdir(&mut self, path: &str) -> PResult<()> {
        Ok(self.filesystem().chdir(path)?)
    }

    // ---- process ------------------------------------------------------------------------------

    /// The environment as raw byte pairs.
    fn environ(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        Vec::new()
    }
    fn setenv(&mut self, _key: &str, _value: &str) -> PResult<()> {
        Err(no_sys())
    }
    fn unsetenv(&mut self, _key: &str) -> PResult<()> {
        Err(no_sys())
    }
    fn process_id(&self) -> u32 {
        1
    }
    fn parent_process_id(&self) -> u32 {
        0
    }
    /// `[uid, euid, gid, egid]`.
    fn user_ids(&self) -> [u32; 4] {
        [0; 4]
    }
    /// Sets the file-mode creation mask and returns the previous one.
    fn umask(&mut self, _mask: u32) -> u32 {
        0o022
    }
    /// `[sysname, nodename, release, version, machine]`.
    fn uname(&self) -> [String; 5] {
        [
            "lumen".to_string(),
            "localhost".to_string(),
            "0".to_string(),
            "0".to_string(),
            "unknown".to_string(),
        ]
    }
    /// `[user, system, children_user, children_system, elapsed]` in seconds.
    fn process_times(&self) -> PResult<[f64; 5]> {
        Err(no_sys())
    }
    fn cpu_count(&self) -> usize {
        1
    }

    // ---- local time (`time`) -----------------------------------------------------------------

    /// The local broken-down time of an instant; UTC on a host without time zones.
    fn localtime(&self, sec: i64) -> PResult<Tm> {
        let mut tm = Tm::from_epoch(sec, 0);
        tm.zone = Some("UTC".to_string());
        Ok(tm)
    }
    /// The instant of local time `tm`, `None` when it cannot be represented.
    fn mktime(&self, tm: &Tm) -> Option<i64> {
        Some(tm.to_epoch_utc())
    }
    /// Re-reads the local time zone from the environment.
    fn tzset(&mut self) {}
    /// CPU time of the process (or the calling thread) in nanoseconds.
    fn cpu_time_ns(&self, _thread: bool) -> PResult<i128> {
        Ok(self.monotonic_ns() as i128)
    }
    /// `clock_gettime` (or with `res`, `clock_getres`) in nanoseconds.
    fn clock_ns(&self, _id: i64, _res: bool) -> PResult<i128> {
        Err(no_sys())
    }
    fn clock_set_ns(&mut self, _id: i64, _ns: i128) -> PResult<()> {
        Err(no_sys())
    }

    // ---- descriptor control and process signals (`posix`) ------------------------------------

    /// `dup2`; the new descriptor is close-on-exec unless `inheritable`.
    fn fd_dup2(&mut self, _fd: Fd, _fd2: Fd, _inheritable: bool) -> PResult<Fd> {
        Err(no_sys())
    }
    fn fd_get_inheritable(&mut self, _fd: Fd) -> PResult<bool> {
        Err(no_sys())
    }
    fn fd_set_inheritable(&mut self, _fd: Fd, _inheritable: bool) -> PResult<()> {
        Err(no_sys())
    }
    fn fd_get_blocking(&mut self, _fd: Fd) -> PResult<bool> {
        Err(no_sys())
    }
    fn fd_set_blocking(&mut self, _fd: Fd, _blocking: bool) -> PResult<()> {
        Err(no_sys())
    }
    fn kill(&mut self, _pid: i32, _sig: i32) -> PResult<()> {
        Err(no_sys())
    }
    fn sysconf(&self, _name: i32) -> PResult<i64> {
        Err(IoError::from_code("EINVAL"))
    }
    /// `waitpid(2)`: `(pid, raw status)`.
    fn waitpid(&mut self, _pid: i32, _options: i32) -> PResult<(i32, i32)> {
        Err(no_sys())
    }
    /// `system(3)`: the raw wait status.
    fn system(&mut self, _command: &str) -> PResult<i32> {
        Err(no_sys())
    }
    fn process_group(&mut self, _call: ProcGroup) -> PResult<i32> {
        Err(no_sys())
    }
    fn getlogin(&mut self) -> PResult<String> {
        Err(no_sys())
    }
    fn getgroups(&mut self) -> PResult<Vec<u32>> {
        Err(no_sys())
    }
    /// Ends the process at once (`os._exit`).
    fn exit_process(&mut self, code: i32) -> ! {
        self.flush_stdout();
        std::process::exit(code)
    }
    fn abort_process(&mut self) -> ! {
        self.flush_stdout();
        std::process::abort()
    }
}

/// The process-group and session calls behind [`Platform::process_group`].
#[derive(Clone, Copy, Debug)]
pub enum ProcGroup {
    GetPgrp,
    GetPgid(i32),
    GetSid(i32),
    SetPgid(i32, i32),
    SetSid,
}

fn no_sys() -> IoError {
    IoError::from_code("ENOSYS")
}

/// The host process: its standard streams, filesystem, clocks and environment.
pub struct StdPlatform {
    start: std::time::Instant,
    fs: Arc<dyn FileSystem>,
}

impl Default for StdPlatform {
    fn default() -> StdPlatform {
        StdPlatform::new()
    }
}

impl StdPlatform {
    pub fn new() -> StdPlatform {
        StdPlatform {
            start: std::time::Instant::now(),
            fs: Arc::new(OsFs),
        }
    }
}

impl Platform for StdPlatform {
    fn executable(&self) -> String {
        std::env::current_exe().map_or_else(
            |_| "lumen-py".to_string(),
            |p| p.to_string_lossy().into_owned(),
        )
    }

    fn write_stdout(&mut self, bytes: &[u8]) {
        use std::io::Write;
        let _ = std::io::stdout().lock().write_all(bytes);
    }

    fn write_stderr(&mut self, bytes: &[u8]) {
        use std::io::Write;
        let _ = std::io::stderr().write_all(bytes);
    }

    fn flush_stdout(&mut self) {
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }

    fn read_stdin(&mut self, buf: &mut [u8]) -> PResult<usize> {
        use std::io::Read;
        std::io::stdin()
            .lock()
            .read(buf)
            .map_err(|e| IoError::from_errno(lumen_os::errno::errno(&e)))
    }

    fn filesystem(&self) -> Arc<dyn FileSystem> {
        self.fs.clone()
    }

    fn monotonic_ns(&self) -> u64 {
        self.start.elapsed().as_nanos() as u64
    }

    fn wall_time_ns(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64
    }

    fn sleep(&mut self, secs: f64) {
        std::thread::sleep(std::time::Duration::try_from_secs_f64(secs).unwrap_or_default());
    }

    fn supports_threads(&self) -> bool {
        cfg!(not(target_arch = "wasm32"))
    }

    fn entropy(&mut self, buf: &mut [u8]) {
        let _ = lumen_os::proc::entropy(buf);
    }

    fn env_var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    fn platform_name(&self) -> String {
        if cfg!(target_os = "macos") {
            "darwin"
        } else {
            std::env::consts::OS
        }
        .to_string()
    }

    fn fd_isatty(&mut self, fd: Fd) -> bool {
        lumen_os::fs::isatty(fd)
    }

    fn fd_pipe(&mut self) -> PResult<(Fd, Fd)> {
        Ok(lumen_os::fs::pipe()?)
    }

    fn terminal_size(&mut self, fd: Fd) -> PResult<(u32, u32)> {
        Ok(lumen_os::proc::terminal_size(fd)?)
    }

    fn environ(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        lumen_os::proc::environ()
    }

    fn setenv(&mut self, key: &str, value: &str) -> PResult<()> {
        Ok(lumen_os::proc::setenv(key, value)?)
    }

    fn unsetenv(&mut self, key: &str) -> PResult<()> {
        Ok(lumen_os::proc::unsetenv(key)?)
    }

    fn process_id(&self) -> u32 {
        lumen_os::proc::getpid()
    }

    fn parent_process_id(&self) -> u32 {
        lumen_os::proc::getppid()
    }

    fn user_ids(&self) -> [u32; 4] {
        [
            lumen_os::proc::getuid(),
            lumen_os::proc::geteuid(),
            lumen_os::proc::getgid(),
            lumen_os::proc::getegid(),
        ]
    }

    fn umask(&mut self, mask: u32) -> u32 {
        lumen_os::proc::umask(mask)
    }

    fn uname(&self) -> [String; 5] {
        lumen_os::proc::uname().unwrap_or_else(|_| {
            [
                "lumen".to_string(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
            ]
        })
    }

    fn process_times(&self) -> PResult<[f64; 5]> {
        Ok(lumen_os::proc::times()?)
    }

    fn cpu_count(&self) -> usize {
        lumen_os::sysinfo::cpu_count()
    }

    fn localtime(&self, sec: i64) -> PResult<Tm> {
        Ok(lumen_os::time::localtime(sec)?)
    }

    fn mktime(&self, tm: &Tm) -> Option<i64> {
        lumen_os::time::mktime(tm)
    }

    fn tzset(&mut self) {
        lumen_os::time::tzset()
    }

    fn cpu_time_ns(&self, thread: bool) -> PResult<i128> {
        Ok(lumen_os::time::cpu_time_ns(thread)?)
    }

    fn clock_ns(&self, id: i64, res: bool) -> PResult<i128> {
        Ok(lumen_os::time::clock_ns(id, res)?)
    }

    fn clock_set_ns(&mut self, id: i64, ns: i128) -> PResult<()> {
        Ok(lumen_os::time::clock_set_ns(id, ns)?)
    }

    fn fd_dup2(&mut self, fd: Fd, fd2: Fd, inheritable: bool) -> PResult<Fd> {
        Ok(lumen_os::fdctl::dup2(fd, fd2, inheritable)?)
    }

    fn fd_get_inheritable(&mut self, fd: Fd) -> PResult<bool> {
        Ok(lumen_os::fdctl::get_inheritable(fd)?)
    }

    fn fd_set_inheritable(&mut self, fd: Fd, inheritable: bool) -> PResult<()> {
        Ok(lumen_os::fdctl::set_inheritable(fd, inheritable)?)
    }

    fn fd_get_blocking(&mut self, fd: Fd) -> PResult<bool> {
        Ok(lumen_os::fdctl::get_blocking(fd)?)
    }

    fn fd_set_blocking(&mut self, fd: Fd, blocking: bool) -> PResult<()> {
        Ok(lumen_os::fdctl::set_blocking(fd, blocking)?)
    }

    fn kill(&mut self, pid: i32, sig: i32) -> PResult<()> {
        Ok(lumen_os::proc::kill(pid, sig)?)
    }

    fn sysconf(&self, name: i32) -> PResult<i64> {
        Ok(lumen_os::proc::sysconf(name)?)
    }

    fn waitpid(&mut self, pid: i32, options: i32) -> PResult<(i32, i32)> {
        Ok(lumen_os::proc::waitpid(pid, options)?)
    }

    fn system(&mut self, command: &str) -> PResult<i32> {
        self.flush_stdout();
        Ok(lumen_os::proc::system(command)?)
    }

    fn process_group(&mut self, call: ProcGroup) -> PResult<i32> {
        use lumen_os::proc::group;
        Ok(match call {
            ProcGroup::GetPgrp => group::getpgrp(),
            ProcGroup::GetPgid(pid) => group::getpgid(pid)?,
            ProcGroup::GetSid(pid) => group::getsid(pid)?,
            ProcGroup::SetPgid(pid, pgrp) => group::setpgid(pid, pgrp).map(|_| 0)?,
            ProcGroup::SetSid => group::setsid()?,
        })
    }

    fn getlogin(&mut self) -> PResult<String> {
        Ok(lumen_os::proc::getlogin()?)
    }

    fn getgroups(&mut self) -> PResult<Vec<u32>> {
        Ok(lumen_os::ident::groups()?)
    }
}

/// Serves the files of a read-only [`MemFs`] (an embedded standard library, an embedder's
/// bundle) over the `inner` platform's file system, and defers everything else to `inner`, so
/// bundled modules import without touching the host file system.
pub struct MemPlatform {
    inner: Box<dyn Platform>,
    mem: Arc<MemFs>,
    fs: Arc<dyn FileSystem>,
}

/// Descriptors of the bundle start here, clear of the ones the OS hands out.
const BUNDLE_FD_BASE: i32 = 1 << 29;

impl MemPlatform {
    pub fn new(inner: Box<dyn Platform>, mem: MemFs) -> MemPlatform {
        let mem = Arc::new(mem);
        let fs: Arc<dyn FileSystem> = Arc::new(Overlay::new(mem.clone(), inner.filesystem(), true));
        MemPlatform { inner, mem, fs }
    }

    /// An empty bundle for [`MemPlatform::new`]: files added with [`MemFs::insert`] are served
    /// read-only (mode 0o444, directories 0o555).
    pub fn bundle() -> MemFs {
        MemFs::with_fd_base(BUNDLE_FD_BASE)
    }

    /// A bundle of `(absolute path, source)` pairs.
    pub fn with_table(inner: Box<dyn Platform>, table: &[(&str, &str)]) -> MemPlatform {
        let mem = MemPlatform::bundle();
        for (path, source) in table {
            mem.insert(path, source.as_bytes().to_vec(), 0o444, 0o555);
        }
        MemPlatform::new(inner, mem)
    }

    fn owns(&self, fd: Fd) -> bool {
        self.mem.is_open(fd)
    }
}

impl Platform for MemPlatform {
    fn filesystem(&self) -> Arc<dyn FileSystem> {
        self.fs.clone()
    }

    fn write_stdout(&mut self, bytes: &[u8]) {
        self.inner.write_stdout(bytes)
    }

    fn write_stderr(&mut self, bytes: &[u8]) {
        self.inner.write_stderr(bytes)
    }

    fn flush_stdout(&mut self) {
        self.inner.flush_stdout()
    }

    fn read_stdin(&mut self, buf: &mut [u8]) -> PResult<usize> {
        self.inner.read_stdin(buf)
    }

    fn monotonic_ns(&self) -> u64 {
        self.inner.monotonic_ns()
    }

    fn wall_time_ns(&self) -> u64 {
        self.inner.wall_time_ns()
    }

    fn sleep(&mut self, secs: f64) {
        self.inner.sleep(secs)
    }

    fn supports_threads(&self) -> bool {
        self.inner.supports_threads()
    }

    fn entropy(&mut self, buf: &mut [u8]) {
        self.inner.entropy(buf)
    }

    fn env_var(&self, name: &str) -> Option<String> {
        self.inner.env_var(name)
    }

    fn platform_name(&self) -> String {
        self.inner.platform_name()
    }

    fn executable(&self) -> String {
        self.inner.executable()
    }

    fn argv(&self) -> Vec<String> {
        self.inner.argv()
    }

    fn fd_isatty(&mut self, fd: Fd) -> bool {
        !self.owns(fd) && self.inner.fd_isatty(fd)
    }

    fn fd_pipe(&mut self) -> PResult<(Fd, Fd)> {
        self.inner.fd_pipe()
    }

    fn terminal_size(&mut self, fd: Fd) -> PResult<(u32, u32)> {
        self.inner.terminal_size(fd)
    }

    fn environ(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.inner.environ()
    }

    fn setenv(&mut self, key: &str, value: &str) -> PResult<()> {
        self.inner.setenv(key, value)
    }

    fn unsetenv(&mut self, key: &str) -> PResult<()> {
        self.inner.unsetenv(key)
    }

    fn process_id(&self) -> u32 {
        self.inner.process_id()
    }

    fn parent_process_id(&self) -> u32 {
        self.inner.parent_process_id()
    }

    fn user_ids(&self) -> [u32; 4] {
        self.inner.user_ids()
    }

    fn umask(&mut self, mask: u32) -> u32 {
        self.inner.umask(mask)
    }

    fn uname(&self) -> [String; 5] {
        self.inner.uname()
    }

    fn process_times(&self) -> PResult<[f64; 5]> {
        self.inner.process_times()
    }

    fn cpu_count(&self) -> usize {
        self.inner.cpu_count()
    }

    fn localtime(&self, sec: i64) -> PResult<Tm> {
        self.inner.localtime(sec)
    }

    fn mktime(&self, tm: &Tm) -> Option<i64> {
        self.inner.mktime(tm)
    }

    fn tzset(&mut self) {
        self.inner.tzset()
    }

    fn cpu_time_ns(&self, thread: bool) -> PResult<i128> {
        self.inner.cpu_time_ns(thread)
    }

    fn clock_ns(&self, id: i64, res: bool) -> PResult<i128> {
        self.inner.clock_ns(id, res)
    }

    fn clock_set_ns(&mut self, id: i64, ns: i128) -> PResult<()> {
        self.inner.clock_set_ns(id, ns)
    }

    fn fd_dup2(&mut self, fd: Fd, fd2: Fd, inheritable: bool) -> PResult<Fd> {
        if self.owns(fd) || self.owns(fd2) {
            return Err(IoError::from_code("EBADF"));
        }
        self.inner.fd_dup2(fd, fd2, inheritable)
    }

    fn fd_get_inheritable(&mut self, fd: Fd) -> PResult<bool> {
        if self.owns(fd) {
            return Ok(false);
        }
        self.inner.fd_get_inheritable(fd)
    }

    fn fd_set_inheritable(&mut self, fd: Fd, inheritable: bool) -> PResult<()> {
        if self.owns(fd) {
            return Ok(());
        }
        self.inner.fd_set_inheritable(fd, inheritable)
    }

    fn fd_get_blocking(&mut self, fd: Fd) -> PResult<bool> {
        if self.owns(fd) {
            return Ok(true);
        }
        self.inner.fd_get_blocking(fd)
    }

    fn fd_set_blocking(&mut self, fd: Fd, blocking: bool) -> PResult<()> {
        if self.owns(fd) {
            return Ok(());
        }
        self.inner.fd_set_blocking(fd, blocking)
    }

    fn kill(&mut self, pid: i32, sig: i32) -> PResult<()> {
        self.inner.kill(pid, sig)
    }

    fn sysconf(&self, name: i32) -> PResult<i64> {
        self.inner.sysconf(name)
    }

    fn waitpid(&mut self, pid: i32, options: i32) -> PResult<(i32, i32)> {
        self.inner.waitpid(pid, options)
    }

    fn system(&mut self, command: &str) -> PResult<i32> {
        self.inner.system(command)
    }

    fn process_group(&mut self, call: ProcGroup) -> PResult<i32> {
        self.inner.process_group(call)
    }

    fn getlogin(&mut self) -> PResult<String> {
        self.inner.getlogin()
    }

    fn getgroups(&mut self) -> PResult<Vec<u32>> {
        self.inner.getgroups()
    }

    fn exit_process(&mut self, code: i32) -> ! {
        self.inner.exit_process(code)
    }

    fn abort_process(&mut self) -> ! {
        self.inner.abort_process()
    }
}
