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
        let errno = match kind {
            IoErrorKind::NotFound => 2,
            IoErrorKind::PermissionDenied => 13,
            IoErrorKind::AlreadyExists => 17,
            IoErrorKind::IsADirectory => 21,
            IoErrorKind::NotADirectory => 20,
            IoErrorKind::Other => 5,
        };
        IoError { kind, errno }
    }
}

impl IoError {
    pub fn message(&self) -> &'static str {
        match self.kind {
            IoErrorKind::NotFound => "No such file or directory",
            IoErrorKind::PermissionDenied => "Permission denied",
            IoErrorKind::AlreadyExists => "File exists",
            IoErrorKind::IsADirectory => "Is a directory",
            IoErrorKind::NotADirectory => "Not a directory",
            IoErrorKind::Other => "Input/output error",
        }
    }
}

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
    use std::io::ErrorKind::*;
    let kind = match e.kind() {
        NotFound => IoErrorKind::NotFound,
        PermissionDenied => IoErrorKind::PermissionDenied,
        AlreadyExists => IoErrorKind::AlreadyExists,
        _ => match e.raw_os_error() {
            Some(21) => IoErrorKind::IsADirectory,
            Some(20) => IoErrorKind::NotADirectory,
            _ => IoErrorKind::Other,
        },
    };
    let mut err = IoError::new(kind);
    if kind == IoErrorKind::Other {
        err.errno = e.raw_os_error().unwrap_or(5);
    }
    err
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
        use std::hash::{BuildHasher, Hasher};
        for chunk in buf.chunks_mut(8) {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u64(self.wall_time_ns());
            chunk.copy_from_slice(&h.finish().to_le_bytes()[..chunk.len()]);
        }
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
}

/// Serves files from a [`MemFs`] first and defers everything else to `inner`, so a standard
/// library embedded in the binary can be imported without touching the host filesystem.
pub struct MemPlatform {
    inner: Box<dyn Platform>,
    fs: MemFs,
    open: BTreeMap<FileHandle, Vec<u8>>,
    next: FileHandle,
}

impl MemPlatform {
    pub fn new(inner: Box<dyn Platform>, fs: MemFs) -> MemPlatform {
        MemPlatform { inner, fs, open: BTreeMap::new(), next: 1 << 30 }
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
}
