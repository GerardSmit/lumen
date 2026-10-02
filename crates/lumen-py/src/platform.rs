//! The boundary between the interpreter and the host. Everything the core needs from an
//! operating system (streams, files, clocks, entropy, environment, module sources) goes through
//! [`Platform`], so an embedder without an OS can supply its own implementation.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

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
        IoError { kind, errno: lumen_os::errno::errno_of_code(code).unwrap_or(5) }
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

pub type FileHandle = u32;

/// How [`Platform::open`] treats the file. `Write` and `Append` create it when missing; `Write`
/// truncates; `CreateNew` fails if it exists.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OpenMode {
    Read,
    Write,
    Append,
    CreateNew,
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

    /// Reads up to and including the next newline; empty at end of input.
    fn read_stdin_line(&mut self) -> Vec<u8>;
    fn read_stdin_to_end(&mut self) -> Vec<u8>;

    fn monotonic_ns(&self) -> u64;
    fn wall_time_ns(&self) -> u64;
    fn sleep(&mut self, secs: f64);
    fn entropy(&mut self, buf: &mut [u8]);

    fn open(&mut self, path: &str, mode: OpenMode) -> Result<FileHandle, IoError>;
    fn read_to_end(&mut self, h: FileHandle) -> Result<Vec<u8>, IoError>;
    fn write_all(&mut self, h: FileHandle, data: &[u8]) -> Result<(), IoError>;
    fn close(&mut self, h: FileHandle);

    fn is_file(&mut self, path: &str) -> bool;
    fn is_dir(&mut self, path: &str) -> bool;

    fn read_file(&mut self, path: &str) -> Result<Vec<u8>, IoError> {
        let h = self.open(path, OpenMode::Read)?;
        let data = self.read_to_end(h);
        self.close(h);
        data
    }

    fn canonicalize(&mut self, path: &str) -> String {
        path.to_string()
    }

    /// Resolves `name` (a single path component) against the entries in order, preferring a
    /// package (`name/__init__.py`) over a plain module (`name.py`) in the same entry.
    fn find_module(&mut self, entries: &[String], name: &str) -> Option<FoundModule> {
        for dir in entries {
            let pkg = join_path(&join_path(dir, name), "__init__.py");
            if self.is_file(&pkg) {
                return self.read_file(&pkg).ok().map(|source| FoundModule { filename: pkg, source, is_package: true });
            }
            let file = join_path(dir, &format!("{}.py", name));
            if self.is_file(&file) {
                return self.read_file(&file).ok().map(|source| FoundModule { filename: file, source, is_package: false });
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
    // Descriptors 0, 1 and 2 are the standard streams. Every method defaults to ENOSYS so a
    // host without an operating system only implements what it offers.

    fn fd_open(&mut self, _path: &str, _flags: i32, _mode: u32) -> PResult<Fd> {
        Err(no_sys())
    }
    fn fd_close(&mut self, _fd: Fd) -> PResult<()> {
        Err(no_sys())
    }
    /// Reads at the current position, or at `pos` without moving it. 0 is end of file.
    fn fd_read(&mut self, _fd: Fd, _buf: &mut [u8], _pos: Option<u64>) -> PResult<usize> {
        Err(no_sys())
    }
    fn fd_write(&mut self, _fd: Fd, _data: &[u8], _pos: Option<u64>) -> PResult<usize> {
        Err(no_sys())
    }
    /// `whence`: 0 from the start, 1 from the current position, 2 from the end.
    fn fd_seek(&mut self, _fd: Fd, _offset: i64, _whence: i32) -> PResult<u64> {
        Err(no_sys())
    }
    fn fd_stat(&mut self, _fd: Fd) -> PResult<OsStat> {
        Err(no_sys())
    }
    fn fd_truncate(&mut self, _fd: Fd, _len: u64) -> PResult<()> {
        Err(no_sys())
    }
    fn fd_sync(&mut self, _fd: Fd, _data_only: bool) -> PResult<()> {
        Err(no_sys())
    }
    fn fd_isatty(&mut self, _fd: Fd) -> bool {
        false
    }
    fn fd_dup(&mut self, _fd: Fd) -> PResult<Fd> {
        Err(no_sys())
    }
    fn fd_pipe(&mut self) -> PResult<(Fd, Fd)> {
        Err(no_sys())
    }
    fn fd_chmod(&mut self, _fd: Fd, _mode: u32) -> PResult<()> {
        Err(no_sys())
    }
    fn fd_utimes(&mut self, _fd: Fd, _atime: Timespec, _mtime: Timespec) -> PResult<()> {
        Err(no_sys())
    }
    fn terminal_size(&mut self, _fd: Fd) -> PResult<(u32, u32)> {
        Err(no_sys())
    }

    /// `stat` (`follow`) or `lstat`.
    fn stat(&mut self, _path: &str, _follow: bool) -> PResult<OsStat> {
        Err(no_sys())
    }
    /// Directory entries without `.` and `..`, in OS order.
    fn listdir(&mut self, _path: &str) -> PResult<Vec<(String, DirentKind)>> {
        Err(no_sys())
    }
    fn mkdir(&mut self, _path: &str, _mode: u32) -> PResult<()> {
        Err(no_sys())
    }
    fn rmdir(&mut self, _path: &str) -> PResult<()> {
        Err(no_sys())
    }
    fn unlink(&mut self, _path: &str) -> PResult<()> {
        Err(no_sys())
    }
    fn rename(&mut self, _from: &str, _to: &str) -> PResult<()> {
        Err(no_sys())
    }
    fn link(&mut self, _existing: &str, _path: &str) -> PResult<()> {
        Err(no_sys())
    }
    fn symlink(&mut self, _target: &str, _path: &str, _is_dir: bool) -> PResult<()> {
        Err(no_sys())
    }
    fn readlink(&mut self, _path: &str) -> PResult<String> {
        Err(no_sys())
    }
    fn realpath(&mut self, _path: &str) -> PResult<String> {
        Err(no_sys())
    }
    /// `mode` bits `F_OK`=0, `X_OK`=1, `W_OK`=2, `R_OK`=4.
    fn access(&mut self, _path: &str, _mode: u32) -> PResult<()> {
        Err(no_sys())
    }
    fn chmod(&mut self, _path: &str, _mode: u32) -> PResult<()> {
        Err(no_sys())
    }
    fn chown(&mut self, _path: &str, _uid: u32, _gid: u32, _follow: bool) -> PResult<()> {
        Err(no_sys())
    }
    fn utimes(&mut self, _path: &str, _atime: Timespec, _mtime: Timespec, _follow: bool) -> PResult<()> {
        Err(no_sys())
    }
    fn getcwd(&mut self) -> PResult<String> {
        Err(no_sys())
    }
    fn chdir(&mut self, _path: &str) -> PResult<()> {
        Err(no_sys())
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
        ["lumen".to_string(), "localhost".to_string(), "0".to_string(), "0".to_string(), "unknown".to_string()]
    }
    /// `[user, system, children_user, children_system, elapsed]` in seconds.
    fn process_times(&self) -> PResult<[f64; 5]> {
        Err(no_sys())
    }
    fn cpu_count(&self) -> usize {
        1
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
    files: std::collections::HashMap<FileHandle, std::fs::File>,
    next: FileHandle,
}

impl Default for StdPlatform {
    fn default() -> StdPlatform {
        StdPlatform::new()
    }
}

impl StdPlatform {
    pub fn new() -> StdPlatform {
        StdPlatform { start: std::time::Instant::now(), files: std::collections::HashMap::new(), next: 3 }
    }
}

fn convert_io(e: &std::io::Error) -> IoError {
    IoError::from_errno(lumen_os::errno::errno(e))
}

impl Platform for StdPlatform {
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

    fn read_stdin_line(&mut self) -> Vec<u8> {
        use std::io::BufRead;
        let mut line = Vec::new();
        let _ = std::io::stdin().lock().read_until(b'\n', &mut line);
        line
    }

    fn read_stdin_to_end(&mut self) -> Vec<u8> {
        use std::io::Read;
        let mut v = Vec::new();
        let _ = std::io::stdin().lock().read_to_end(&mut v);
        v
    }

    fn monotonic_ns(&self) -> u64 {
        self.start.elapsed().as_nanos() as u64
    }

    fn wall_time_ns(&self) -> u64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos() as u64
    }

    fn sleep(&mut self, secs: f64) {
        std::thread::sleep(std::time::Duration::try_from_secs_f64(secs).unwrap_or_default());
    }

    fn entropy(&mut self, buf: &mut [u8]) {
        let _ = lumen_os::proc::entropy(buf);
    }

    fn open(&mut self, path: &str, mode: OpenMode) -> Result<FileHandle, IoError> {
        let mut oo = std::fs::OpenOptions::new();
        match mode {
            OpenMode::Read => oo.read(true),
            OpenMode::Write => oo.write(true).create(true).truncate(true),
            OpenMode::Append => oo.append(true).create(true),
            OpenMode::CreateNew => oo.write(true).create_new(true),
        };
        let f = oo.open(path).map_err(|e| convert_io(&e))?;
        let h = self.next;
        self.next += 1;
        self.files.insert(h, f);
        Ok(h)
    }

    fn read_to_end(&mut self, h: FileHandle) -> Result<Vec<u8>, IoError> {
        use std::io::Read;
        let f = self.files.get_mut(&h).ok_or(IoError::new(IoErrorKind::Other))?;
        let mut v = Vec::new();
        f.read_to_end(&mut v).map_err(|e| convert_io(&e))?;
        Ok(v)
    }

    fn write_all(&mut self, h: FileHandle, data: &[u8]) -> Result<(), IoError> {
        use std::io::Write;
        let f = self.files.get_mut(&h).ok_or(IoError::new(IoErrorKind::Other))?;
        f.write_all(data).map_err(|e| convert_io(&e))
    }

    fn close(&mut self, h: FileHandle) {
        self.files.remove(&h);
    }

    fn is_file(&mut self, path: &str) -> bool {
        std::path::Path::new(path).is_file()
    }

    fn is_dir(&mut self, path: &str) -> bool {
        std::path::Path::new(path).is_dir()
    }

    fn canonicalize(&mut self, path: &str) -> String {
        std::fs::canonicalize(path).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| path.to_string())
    }

    fn env_var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    fn platform_name(&self) -> String {
        if cfg!(target_os = "macos") { "darwin" } else { std::env::consts::OS }.to_string()
    }

    fn fd_open(&mut self, path: &str, flags: i32, mode: u32) -> PResult<Fd> {
        Ok(lumen_os::fs::open(path, flags, mode)?)
    }

    fn fd_close(&mut self, fd: Fd) -> PResult<()> {
        if lumen_os::fs::is_std(fd) && !lumen_os::fs::is_open(fd) {
            return Ok(());
        }
        Ok(lumen_os::fs::close(fd)?)
    }

    fn fd_read(&mut self, fd: Fd, buf: &mut [u8], pos: Option<u64>) -> PResult<usize> {
        if fd == 0 && !lumen_os::fs::is_open(0) {
            use std::io::Read;
            return std::io::stdin().lock().read(buf).map_err(|e| convert_io(&e));
        }
        Ok(lumen_os::fs::read(fd, buf, pos)?)
    }

    fn fd_write(&mut self, fd: Fd, data: &[u8], pos: Option<u64>) -> PResult<usize> {
        match fd {
            1 if !lumen_os::fs::is_open(1) => self.write_stdout(data),
            2 if !lumen_os::fs::is_open(2) => self.write_stderr(data),
            _ => return Ok(lumen_os::fs::write(fd, data, pos)?),
        }
        Ok(data.len())
    }

    fn fd_seek(&mut self, fd: Fd, offset: i64, whence: i32) -> PResult<u64> {
        Ok(lumen_os::fs::lseek(fd, offset, whence)?)
    }

    fn fd_stat(&mut self, fd: Fd) -> PResult<OsStat> {
        Ok(lumen_os::fs::fstat(fd)?)
    }

    fn fd_truncate(&mut self, fd: Fd, len: u64) -> PResult<()> {
        Ok(lumen_os::fs::ftruncate(fd, len)?)
    }

    fn fd_sync(&mut self, fd: Fd, data_only: bool) -> PResult<()> {
        Ok(lumen_os::fs::fsync(fd, data_only)?)
    }

    fn fd_isatty(&mut self, fd: Fd) -> bool {
        lumen_os::fs::isatty(fd)
    }

    fn fd_dup(&mut self, fd: Fd) -> PResult<Fd> {
        Ok(lumen_os::fs::dup(fd)?)
    }

    fn fd_pipe(&mut self) -> PResult<(Fd, Fd)> {
        Ok(lumen_os::fs::pipe()?)
    }

    fn fd_chmod(&mut self, fd: Fd, mode: u32) -> PResult<()> {
        Ok(lumen_os::fs::fchmod(fd, mode)?)
    }

    fn fd_utimes(&mut self, fd: Fd, atime: Timespec, mtime: Timespec) -> PResult<()> {
        Ok(lumen_os::fs::futimes(fd, atime, mtime)?)
    }

    fn terminal_size(&mut self, fd: Fd) -> PResult<(u32, u32)> {
        Ok(lumen_os::proc::terminal_size(fd)?)
    }

    fn stat(&mut self, path: &str, follow: bool) -> PResult<OsStat> {
        Ok(lumen_os::fs::stat(path, follow)?)
    }

    fn listdir(&mut self, path: &str) -> PResult<Vec<(String, DirentKind)>> {
        Ok(lumen_os::fs::readdir(path)?)
    }

    fn mkdir(&mut self, path: &str, mode: u32) -> PResult<()> {
        lumen_os::fs::mkdir(path, mode, false)?;
        Ok(())
    }

    fn rmdir(&mut self, path: &str) -> PResult<()> {
        Ok(lumen_os::fs::rmdir(path)?)
    }

    fn unlink(&mut self, path: &str) -> PResult<()> {
        Ok(lumen_os::fs::unlink(path)?)
    }

    fn rename(&mut self, from: &str, to: &str) -> PResult<()> {
        Ok(lumen_os::fs::rename(from, to)?)
    }

    fn link(&mut self, existing: &str, path: &str) -> PResult<()> {
        Ok(lumen_os::fs::link(existing, path)?)
    }

    fn symlink(&mut self, target: &str, path: &str, is_dir: bool) -> PResult<()> {
        Ok(lumen_os::fs::symlink(target, path, is_dir as u32)?)
    }

    fn readlink(&mut self, path: &str) -> PResult<String> {
        Ok(lumen_os::fs::readlink(path)?)
    }

    fn realpath(&mut self, path: &str) -> PResult<String> {
        Ok(lumen_os::fs::realpath(path)?)
    }

    fn access(&mut self, path: &str, mode: u32) -> PResult<()> {
        Ok(lumen_os::fs::access(path, mode)?)
    }

    fn chmod(&mut self, path: &str, mode: u32) -> PResult<()> {
        Ok(lumen_os::fs::chmod(path, mode)?)
    }

    fn chown(&mut self, path: &str, uid: u32, gid: u32, follow: bool) -> PResult<()> {
        Ok(lumen_os::fs::chown(path, uid, gid, follow)?)
    }

    fn utimes(&mut self, path: &str, atime: Timespec, mtime: Timespec, follow: bool) -> PResult<()> {
        Ok(lumen_os::fs::utimes(path, atime, mtime, follow)?)
    }

    fn getcwd(&mut self) -> PResult<String> {
        Ok(lumen_os::proc::getcwd()?)
    }

    fn chdir(&mut self, path: &str) -> PResult<()> {
        Ok(lumen_os::proc::chdir(path)?)
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
        [lumen_os::proc::getuid(), lumen_os::proc::geteuid(), lumen_os::proc::getgid(), lumen_os::proc::getegid()]
    }

    fn umask(&mut self, mask: u32) -> u32 {
        lumen_os::proc::umask(mask)
    }

    fn uname(&self) -> [String; 5] {
        lumen_os::proc::uname().unwrap_or_else(|_| ["lumen".to_string(), String::new(), String::new(), String::new(), String::new()])
    }

    fn process_times(&self) -> PResult<[f64; 5]> {
        Ok(lumen_os::proc::times()?)
    }

    fn cpu_count(&self) -> usize {
        lumen_os::proc::cpu_count()
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
        Ok(lumen_os::proc::getgroups()?)
    }
}

/// An in-memory read-only file tree: path to contents.
#[derive(Default, Clone)]
pub struct MemFs {
    files: BTreeMap<String, Cow<'static, [u8]>>,
}

impl MemFs {
    pub fn new() -> MemFs {
        MemFs::default()
    }

    pub fn from_table(table: &[(&str, &str)]) -> MemFs {
        let mut fs = MemFs::new();
        for (path, source) in table {
            fs.insert(path, source.as_bytes());
        }
        fs
    }

    pub fn insert(&mut self, path: &str, contents: &[u8]) {
        self.files.insert(path.to_string(), Cow::Owned(contents.to_vec()));
    }

    pub fn insert_static(&mut self, path: &str, contents: &'static [u8]) {
        self.files.insert(path.to_string(), Cow::Borrowed(contents));
    }

    pub fn get(&self, path: &str) -> Option<&[u8]> {
        self.files.get(path).map(|v| v.as_ref())
    }

    pub fn is_file(&self, path: &str) -> bool {
        self.files.contains_key(path)
    }

    pub fn is_dir(&self, path: &str) -> bool {
        let prefix = if path.ends_with('/') { path.to_string() } else { format!("{}/", path) };
        self.files.range(prefix.clone()..).next().is_some_and(|(k, _)| k.starts_with(&prefix))
    }

    /// The immediate children of directory `path`, or `None` when it is not a directory here.
    pub fn list(&self, path: &str) -> Option<Vec<(String, DirentKind)>> {
        let prefix = if path.ends_with('/') { path.to_string() } else { format!("{}/", path) };
        let mut out: Vec<(String, DirentKind)> = Vec::new();
        for (k, _) in self.files.range(prefix.clone()..) {
            let Some(rest) = k.strip_prefix(&prefix) else { break };
            let (name, kind) = match rest.split_once('/') {
                Some((dir, _)) => (dir, DirentKind::Dir),
                None => (rest, DirentKind::File),
            };
            if out.last().is_none_or(|(n, _)| n != name) {
                out.push((name.to_string(), kind));
            }
        }
        if out.is_empty() { None } else { Some(out) }
    }
}

/// Serves files from a [`MemFs`] first and defers everything else to `inner`, so a standard
/// library embedded in the binary can be imported without touching the host filesystem.
pub struct MemPlatform {
    inner: Box<dyn Platform>,
    fs: MemFs,
    open: BTreeMap<FileHandle, Vec<u8>>,
    next: FileHandle,
    fds: BTreeMap<Fd, MemFd>,
    next_fd: Fd,
}

impl MemPlatform {
    pub fn new(inner: Box<dyn Platform>, fs: MemFs) -> MemPlatform {
        MemPlatform { inner, fs, open: BTreeMap::new(), next: 1 << 30, fds: BTreeMap::new(), next_fd: 1 << 29 }
    }

    pub fn with_table(inner: Box<dyn Platform>, table: &[(&str, &str)]) -> MemPlatform {
        MemPlatform::new(inner, MemFs::from_table(table))
    }
}

impl Platform for MemPlatform {
    fn write_stdout(&mut self, bytes: &[u8]) {
        self.inner.write_stdout(bytes)
    }

    fn write_stderr(&mut self, bytes: &[u8]) {
        self.inner.write_stderr(bytes)
    }

    fn flush_stdout(&mut self) {
        self.inner.flush_stdout()
    }

    fn read_stdin_line(&mut self) -> Vec<u8> {
        self.inner.read_stdin_line()
    }

    fn read_stdin_to_end(&mut self) -> Vec<u8> {
        self.inner.read_stdin_to_end()
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

    fn entropy(&mut self, buf: &mut [u8]) {
        self.inner.entropy(buf)
    }

    fn open(&mut self, path: &str, mode: OpenMode) -> Result<FileHandle, IoError> {
        match (self.fs.get(path), mode) {
            (Some(data), OpenMode::Read) => {
                let h = self.next;
                self.next += 1;
                self.open.insert(h, data.to_vec());
                Ok(h)
            }
            (Some(_), _) => Err(IoError::new(IoErrorKind::PermissionDenied)),
            (None, _) if self.fs.is_dir(path) => Err(IoError::new(IoErrorKind::IsADirectory)),
            (None, _) => self.inner.open(path, mode),
        }
    }

    fn read_to_end(&mut self, h: FileHandle) -> Result<Vec<u8>, IoError> {
        match self.open.get_mut(&h) {
            Some(data) => Ok(std::mem::take(data)),
            None => self.inner.read_to_end(h),
        }
    }

    fn write_all(&mut self, h: FileHandle, data: &[u8]) -> Result<(), IoError> {
        if self.open.contains_key(&h) {
            return Err(IoError::new(IoErrorKind::PermissionDenied));
        }
        self.inner.write_all(h, data)
    }

    fn close(&mut self, h: FileHandle) {
        if self.open.remove(&h).is_none() {
            self.inner.close(h);
        }
    }

    fn is_file(&mut self, path: &str) -> bool {
        self.fs.is_file(path) || self.inner.is_file(path)
    }

    fn is_dir(&mut self, path: &str) -> bool {
        self.fs.is_dir(path) || self.inner.is_dir(path)
    }

    fn canonicalize(&mut self, path: &str) -> String {
        self.inner.canonicalize(path)
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

    fn fd_open(&mut self, path: &str, flags: i32, mode: u32) -> PResult<Fd> {
        let acc = flags & 3;
        match self.fs.get(path) {
            Some(data) if acc == 0 => {
                let fd = self.next_fd;
                self.next_fd += 1;
                self.fds.insert(fd, MemFd { data: data.to_vec(), pos: 0 });
                Ok(fd)
            }
            Some(_) => Err(IoError::from_code("EACCES")),
            None if self.fs.is_dir(path) => Err(IoError::from_code("EISDIR")),
            None => self.inner.fd_open(path, flags, mode),
        }
    }

    fn fd_close(&mut self, fd: Fd) -> PResult<()> {
        match self.fds.remove(&fd) {
            Some(_) => Ok(()),
            None => self.inner.fd_close(fd),
        }
    }

    fn fd_read(&mut self, fd: Fd, buf: &mut [u8], pos: Option<u64>) -> PResult<usize> {
        let Some(m) = self.fds.get_mut(&fd) else { return self.inner.fd_read(fd, buf, pos) };
        let start = (pos.unwrap_or(m.pos) as usize).min(m.data.len());
        let n = buf.len().min(m.data.len() - start);
        buf[..n].copy_from_slice(&m.data[start..start + n]);
        if pos.is_none() {
            m.pos += n as u64;
        }
        Ok(n)
    }

    fn fd_write(&mut self, fd: Fd, data: &[u8], pos: Option<u64>) -> PResult<usize> {
        if self.fds.contains_key(&fd) {
            return Err(IoError::from_code("EBADF"));
        }
        self.inner.fd_write(fd, data, pos)
    }

    fn fd_seek(&mut self, fd: Fd, offset: i64, whence: i32) -> PResult<u64> {
        let Some(m) = self.fds.get_mut(&fd) else { return self.inner.fd_seek(fd, offset, whence) };
        let base = match whence {
            0 => 0,
            1 => m.pos as i64,
            2 => m.data.len() as i64,
            _ => return Err(IoError::from_code("EINVAL")),
        };
        match base.checked_add(offset) {
            Some(p) if p >= 0 => {
                m.pos = p as u64;
                Ok(m.pos)
            }
            _ => Err(IoError::from_code("EINVAL")),
        }
    }

    fn fd_stat(&mut self, fd: Fd) -> PResult<OsStat> {
        match self.fds.get(&fd) {
            Some(m) => Ok(mem_stat(lumen_os::fs::S_IFREG | 0o444, m.data.len() as u64)),
            None => self.inner.fd_stat(fd),
        }
    }

    fn fd_truncate(&mut self, fd: Fd, len: u64) -> PResult<()> {
        self.inner.fd_truncate(fd, len)
    }

    fn fd_sync(&mut self, fd: Fd, data_only: bool) -> PResult<()> {
        self.inner.fd_sync(fd, data_only)
    }

    fn fd_isatty(&mut self, fd: Fd) -> bool {
        !self.fds.contains_key(&fd) && self.inner.fd_isatty(fd)
    }

    fn fd_dup(&mut self, fd: Fd) -> PResult<Fd> {
        match self.fds.get(&fd) {
            Some(m) => {
                let copy = MemFd { data: m.data.clone(), pos: m.pos };
                let nfd = self.next_fd;
                self.next_fd += 1;
                self.fds.insert(nfd, copy);
                Ok(nfd)
            }
            None => self.inner.fd_dup(fd),
        }
    }

    fn fd_pipe(&mut self) -> PResult<(Fd, Fd)> {
        self.inner.fd_pipe()
    }

    fn fd_chmod(&mut self, fd: Fd, mode: u32) -> PResult<()> {
        self.inner.fd_chmod(fd, mode)
    }

    fn fd_utimes(&mut self, fd: Fd, atime: Timespec, mtime: Timespec) -> PResult<()> {
        self.inner.fd_utimes(fd, atime, mtime)
    }

    fn terminal_size(&mut self, fd: Fd) -> PResult<(u32, u32)> {
        self.inner.terminal_size(fd)
    }

    fn stat(&mut self, path: &str, follow: bool) -> PResult<OsStat> {
        if let Some(data) = self.fs.get(path) {
            return Ok(mem_stat(lumen_os::fs::S_IFREG | 0o444, data.len() as u64));
        }
        if self.fs.is_dir(path) {
            return Ok(mem_stat(lumen_os::fs::S_IFDIR | 0o555, 0));
        }
        self.inner.stat(path, follow)
    }

    fn listdir(&mut self, path: &str) -> PResult<Vec<(String, DirentKind)>> {
        match self.fs.list(path) {
            Some(entries) => Ok(entries),
            None => self.inner.listdir(path),
        }
    }

    fn mkdir(&mut self, path: &str, mode: u32) -> PResult<()> {
        self.inner.mkdir(path, mode)
    }

    fn rmdir(&mut self, path: &str) -> PResult<()> {
        self.inner.rmdir(path)
    }

    fn unlink(&mut self, path: &str) -> PResult<()> {
        self.inner.unlink(path)
    }

    fn rename(&mut self, from: &str, to: &str) -> PResult<()> {
        self.inner.rename(from, to)
    }

    fn link(&mut self, existing: &str, path: &str) -> PResult<()> {
        self.inner.link(existing, path)
    }

    fn symlink(&mut self, target: &str, path: &str, is_dir: bool) -> PResult<()> {
        self.inner.symlink(target, path, is_dir)
    }

    fn readlink(&mut self, path: &str) -> PResult<String> {
        self.inner.readlink(path)
    }

    fn realpath(&mut self, path: &str) -> PResult<String> {
        if self.fs.is_file(path) || self.fs.is_dir(path) {
            return Ok(path.to_string());
        }
        self.inner.realpath(path)
    }

    fn access(&mut self, path: &str, mode: u32) -> PResult<()> {
        if self.fs.is_file(path) || self.fs.is_dir(path) {
            return if mode & 2 == 0 { Ok(()) } else { Err(IoError::from_code("EACCES")) };
        }
        self.inner.access(path, mode)
    }

    fn chmod(&mut self, path: &str, mode: u32) -> PResult<()> {
        self.inner.chmod(path, mode)
    }

    fn chown(&mut self, path: &str, uid: u32, gid: u32, follow: bool) -> PResult<()> {
        self.inner.chown(path, uid, gid, follow)
    }

    fn utimes(&mut self, path: &str, atime: Timespec, mtime: Timespec, follow: bool) -> PResult<()> {
        self.inner.utimes(path, atime, mtime, follow)
    }

    fn getcwd(&mut self) -> PResult<String> {
        self.inner.getcwd()
    }

    fn chdir(&mut self, path: &str) -> PResult<()> {
        self.inner.chdir(path)
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

    fn fd_dup2(&mut self, fd: Fd, fd2: Fd, inheritable: bool) -> PResult<Fd> {
        if self.fds.contains_key(&fd) || self.fds.contains_key(&fd2) {
            return Err(IoError::from_code("EBADF"));
        }
        self.inner.fd_dup2(fd, fd2, inheritable)
    }

    fn fd_get_inheritable(&mut self, fd: Fd) -> PResult<bool> {
        if self.fds.contains_key(&fd) {
            return Ok(false);
        }
        self.inner.fd_get_inheritable(fd)
    }

    fn fd_set_inheritable(&mut self, fd: Fd, inheritable: bool) -> PResult<()> {
        if self.fds.contains_key(&fd) {
            return Ok(());
        }
        self.inner.fd_set_inheritable(fd, inheritable)
    }

    fn fd_get_blocking(&mut self, fd: Fd) -> PResult<bool> {
        if self.fds.contains_key(&fd) {
            return Ok(true);
        }
        self.inner.fd_get_blocking(fd)
    }

    fn fd_set_blocking(&mut self, fd: Fd, blocking: bool) -> PResult<()> {
        if self.fds.contains_key(&fd) {
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

struct MemFd {
    data: Vec<u8>,
    pos: u64,
}

fn mem_stat(mode: u32, size: u64) -> OsStat {
    OsStat { mode, nlink: 1, size, blksize: 4096, ..OsStat::default() }
}
