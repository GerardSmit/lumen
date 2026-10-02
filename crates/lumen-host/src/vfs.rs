//! An in-memory POSIX-flavoured file system for targets that have no OS file system
//! (`wasm32-unknown-unknown`). `node:fs`, the module loader and `process.cwd()` run on it there;
//! natively the same code compiles but nothing consults it.
//!
//! Paths are `/`-separated and absolute (relative ones resolve against [`set_cwd`]). Directories,
//! files and symlinks live in one inode table behind a process-wide lock; file descriptors
//! (numbered from 3) keep a position, so `read`/`write` without an offset behave like the
//! system calls. A [`Backend`] can be [`mount`]ed on a prefix: paths below it are discovered
//! lazily through the backend (an OPFS directory, a remote origin, a bundle index) and their
//! contents fetched on first read — the hook the browser embedding serves through its
//! suspending synchronous host call. Writes below a mount stay in memory.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};

use crate::time::unix_ms;

pub const O_WRONLY: i32 = 1;
pub const O_RDWR: i32 = 2;
pub const O_CREAT: i32 = 0o100;
pub const O_EXCL: i32 = 0o200;
pub const O_TRUNC: i32 = 0o1000;
pub const O_APPEND: i32 = 0o2000;
pub const O_DIRECTORY: i32 = 0o200000;
pub const O_NOFOLLOW: i32 = 0o400000;

const S_IFREG: u32 = 0o100000;
const S_IFDIR: u32 = 0o040000;
const S_IFLNK: u32 = 0o120000;
const MAX_LINKS: u32 = 40;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Errno {
    NoEnt,
    Exist,
    NotDir,
    IsDir,
    NotEmpty,
    Inval,
    BadF,
    Acces,
    Loop,
    Perm,
    NoSys,
    Io,
}

impl Errno {
    /// libuv's name for the error (`ENOENT`, ...).
    pub fn code(self) -> &'static str {
        match self {
            Errno::NoEnt => "ENOENT",
            Errno::Exist => "EEXIST",
            Errno::NotDir => "ENOTDIR",
            Errno::IsDir => "EISDIR",
            Errno::NotEmpty => "ENOTEMPTY",
            Errno::Inval => "EINVAL",
            Errno::BadF => "EBADF",
            Errno::Acces => "EACCES",
            Errno::Loop => "ELOOP",
            Errno::Perm => "EPERM",
            Errno::NoSys => "ENOSYS",
            Errno::Io => "EIO",
        }
    }

    /// libuv's description of the error.
    pub fn message(self) -> &'static str {
        lumen_os::uv::message(self.code()).unwrap_or("unknown error")
    }

    pub fn to_io(self) -> std::io::Error {
        use std::io::ErrorKind as K;
        let kind = match self {
            Errno::NoEnt => K::NotFound,
            Errno::Exist => K::AlreadyExists,
            Errno::NotDir => K::NotADirectory,
            Errno::IsDir => K::IsADirectory,
            Errno::NotEmpty => K::DirectoryNotEmpty,
            Errno::Inval => K::InvalidInput,
            Errno::Acces | Errno::Perm => K::PermissionDenied,
            Errno::NoSys => K::Unsupported,
            _ => K::Other,
        };
        std::io::Error::new(kind, self.message())
    }
}

impl From<Errno> for std::io::Error {
    fn from(e: Errno) -> Self {
        e.to_io()
    }
}

pub type Result<T> = std::result::Result<T, Errno>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
}

#[derive(Clone, Debug)]
pub struct Stat {
    pub kind: Kind,
    pub mode: u32,
    pub size: u64,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub ino: u64,
    pub atime_ms: f64,
    pub mtime_ms: f64,
    pub ctime_ms: f64,
    pub birthtime_ms: f64,
}

/// What a mounted [`Backend`] reports about one path.
pub struct RemoteStat {
    pub is_dir: bool,
    pub size: u64,
}

pub struct RemoteEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

/// Serves the paths below a mount point. All paths are absolute and normalised. Called with the
/// file system locked: an implementation must not call back into this module.
pub trait Backend: Send + Sync {
    fn stat(&self, path: &str) -> Option<RemoteStat>;
    fn read(&self, path: &str) -> Option<Vec<u8>>;
    fn list(&self, path: &str) -> Option<Vec<RemoteEntry>>;
}

enum Data {
    File { bytes: Vec<u8>, lazy: Option<String>, lazy_size: u64 },
    Dir { entries: BTreeMap<String, u64>, unlisted: Option<String> },
    Link(String),
}

struct Node {
    data: Data,
    mode: u32,
    nlink: u32,
    uid: u32,
    gid: u32,
    atime: f64,
    mtime: f64,
    ctime: f64,
    birth: f64,
}

impl Node {
    fn new(data: Data, mode: u32) -> Node {
        let now = unix_ms();
        Node { data, mode, nlink: 1, uid: 0, gid: 0, atime: now, mtime: now, ctime: now, birth: now }
    }

    fn kind(&self) -> Kind {
        match self.data {
            Data::File { .. } => Kind::File,
            Data::Dir { .. } => Kind::Dir,
            Data::Link(_) => Kind::Symlink,
        }
    }

    fn type_bits(&self) -> u32 {
        match self.kind() {
            Kind::File => S_IFREG,
            Kind::Dir => S_IFDIR,
            Kind::Symlink => S_IFLNK,
        }
    }
}

struct Open {
    ino: u64,
    readable: bool,
    writable: bool,
    append: bool,
    pos: u64,
}

const ROOT: u64 = 1;

struct Fs {
    nodes: HashMap<u64, Node>,
    next_ino: u64,
    fds: BTreeMap<i32, Open>,
    mounts: Vec<(String, Arc<dyn Backend>)>,
    cwd: String,
    tmp_counter: u64,
}

fn fs() -> std::sync::MutexGuard<'static, Fs> {
    static FS: OnceLock<Mutex<Fs>> = OnceLock::new();
    FS.get_or_init(|| {
        let mut nodes = HashMap::new();
        nodes.insert(
            ROOT,
            Node::new(Data::Dir { entries: BTreeMap::new(), unlisted: None }, 0o755),
        );
        Mutex::new(Fs {
            nodes,
            next_ino: ROOT + 1,
            fds: BTreeMap::new(),
            mounts: Vec::new(),
            cwd: "/".to_string(),
            tmp_counter: 0,
        })
    })
    .lock()
    .unwrap_or_else(|p| p.into_inner())
}

fn components(path: &str) -> impl DoubleEndedIterator<Item = &str> {
    path.split('/').filter(|c| !c.is_empty())
}

fn join(comps: &[String]) -> String {
    if comps.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", comps.join("/"))
    }
}

/// `path` made absolute against `cwd`, keeping `.` and `..` for the walk to apply (a `..` after
/// a symlink must climb out of the link's target, not out of the link's own directory).
fn absolutize(cwd: &str, path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else {
        format!("{cwd}/{path}")
    }
}

/// `path` made absolute against `cwd` and lexically normalised (`.` and `..` folded).
fn normalize(cwd: &str, path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let joined;
    let full = if path.starts_with('/') {
        path
    } else {
        joined = format!("{cwd}/{path}");
        &joined
    };
    for c in components(full) {
        match c {
            "." => {}
            ".." => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    if out.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", out.join("/"))
    }
}

struct Resolved {
    ino: u64,
    path: Vec<String>,
}

impl Fs {
    fn node(&self, ino: u64) -> &Node {
        &self.nodes[&ino]
    }

    fn node_mut(&mut self, ino: u64) -> &mut Node {
        self.nodes.get_mut(&ino).expect("live inode")
    }

    fn alloc(&mut self, node: Node) -> u64 {
        let ino = self.next_ino;
        self.next_ino += 1;
        self.nodes.insert(ino, node);
        ino
    }

    fn mount_for(&self, abs: &str) -> Option<Arc<dyn Backend>> {
        self.mounts
            .iter()
            .filter(|(prefix, _)| {
                prefix == "/" || abs == prefix || abs.strip_prefix(prefix.as_str()).is_some_and(|r| r.starts_with('/'))
            })
            .max_by_key(|(prefix, _)| prefix.len())
            .map(|(_, b)| Arc::clone(b))
    }

    /// Discover `name` below `dir` through the mount covering it, if any.
    fn materialize(&mut self, dir: u64, dir_path: &[String], name: &str) -> Option<u64> {
        let mut comps = dir_path.to_vec();
        comps.push(name.to_string());
        let abs = join(&comps);
        let backend = self.mount_for(&abs)?;
        let rs = backend.stat(&abs)?;
        let data = if rs.is_dir {
            Data::Dir { entries: BTreeMap::new(), unlisted: Some(abs.clone()) }
        } else {
            Data::File { bytes: Vec::new(), lazy: Some(abs.clone()), lazy_size: rs.size }
        };
        let mode = if rs.is_dir { 0o755 } else { 0o644 };
        let ino = self.alloc(Node::new(data, mode));
        if let Data::Dir { entries, .. } = &mut self.node_mut(dir).data {
            entries.insert(name.to_string(), ino);
        }
        Some(ino)
    }

    fn resolve(&mut self, path: &str, follow_last: bool) -> Result<Resolved> {
        let abs = absolutize(&self.cwd, path);
        let mut todo: VecDeque<String> = components(&abs).map(str::to_string).collect();
        let mut inos = vec![ROOT];
        let mut cur: Vec<String> = Vec::new();
        let mut links = 0;
        while let Some(name) = todo.pop_front() {
            match name.as_str() {
                "." => continue,
                ".." => {
                    if inos.len() > 1 {
                        inos.pop();
                        cur.pop();
                    }
                    continue;
                }
                _ => {}
            }
            let dir = *inos.last().unwrap();
            let child = {
                match &self.node(dir).data {
                    Data::Dir { entries, .. } => entries.get(&name).copied(),
                    _ => return Err(Errno::NotDir),
                }
            };
            let child = match child {
                Some(c) => c,
                None => match self.materialize(dir, &cur, &name) {
                    Some(c) => c,
                    None => return Err(Errno::NoEnt),
                },
            };
            let is_last = todo.is_empty();
            if let Data::Link(target) = &self.node(child).data {
                if !is_last || follow_last {
                    links += 1;
                    if links > MAX_LINKS {
                        return Err(Errno::Loop);
                    }
                    let target = target.clone();
                    if target.starts_with('/') {
                        inos.truncate(1);
                        cur.clear();
                    }
                    for c in components(&target).rev() {
                        todo.push_front(c.to_string());
                    }
                    continue;
                }
            }
            inos.push(child);
            cur.push(name);
        }
        Ok(Resolved { ino: *inos.last().unwrap(), path: cur })
    }

    /// The directory that would hold `path`, and the final name.
    fn resolve_parent(&mut self, path: &str) -> Result<(u64, Vec<String>, String)> {
        let abs = absolutize(&self.cwd, path);
        let mut comps: Vec<String> = components(&abs).map(str::to_string).collect();
        let name = comps.pop().ok_or(Errno::Exist)?;
        if name == "." || name == ".." {
            return Err(Errno::Exist);
        }
        let parent = self.resolve(&join(&comps), true)?;
        if !matches!(self.node(parent.ino).data, Data::Dir { .. }) {
            return Err(Errno::NotDir);
        }
        Ok((parent.ino, parent.path, name))
    }

    fn load(&mut self, ino: u64) -> Result<()> {
        let path = match &self.node(ino).data {
            Data::File { lazy: Some(p), .. } => p.clone(),
            _ => return Ok(()),
        };
        let backend = self.mount_for(&path).ok_or(Errno::Io)?;
        let bytes = backend.read(&path).ok_or(Errno::Io)?;
        if let Data::File { bytes: slot, lazy, .. } = &mut self.node_mut(ino).data {
            *slot = bytes;
            *lazy = None;
        }
        Ok(())
    }

    fn list_remote(&mut self, ino: u64) {
        let path = match &mut self.node_mut(ino).data {
            Data::Dir { unlisted, .. } => match unlisted.take() {
                Some(p) => p,
                None => return,
            },
            _ => return,
        };
        let Some(backend) = self.mount_for(&path) else { return };
        let Some(list) = backend.list(&path) else { return };
        for e in list {
            let exists = matches!(&self.node(ino).data, Data::Dir { entries, .. } if entries.contains_key(&e.name));
            if exists {
                continue;
            }
            let child_path = if path == "/" { format!("/{}", e.name) } else { format!("{path}/{}", e.name) };
            let (data, mode) = if e.is_dir {
                (Data::Dir { entries: BTreeMap::new(), unlisted: Some(child_path) }, 0o755)
            } else {
                (Data::File { bytes: Vec::new(), lazy: Some(child_path), lazy_size: e.size }, 0o644)
            };
            let child = self.alloc(Node::new(data, mode));
            if let Data::Dir { entries, .. } = &mut self.node_mut(ino).data {
                entries.insert(e.name, child);
            }
        }
    }

    fn stat_node(&self, ino: u64) -> Stat {
        let n = self.node(ino);
        let size = match &n.data {
            Data::File { bytes, lazy: None, .. } => bytes.len() as u64,
            Data::File { lazy_size, .. } => *lazy_size,
            Data::Dir { entries, .. } => (entries.len() as u64 + 2) * 32,
            Data::Link(t) => t.len() as u64,
        };
        Stat {
            kind: n.kind(),
            mode: n.type_bits() | (n.mode & 0o7777),
            size,
            nlink: n.nlink,
            uid: n.uid,
            gid: n.gid,
            ino,
            atime_ms: n.atime,
            mtime_ms: n.mtime,
            ctime_ms: n.ctime,
            birthtime_ms: n.birth,
        }
    }

    fn touch(&mut self, ino: u64, content: bool) {
        let now = unix_ms();
        let n = self.node_mut(ino);
        n.ctime = now;
        if content {
            n.mtime = now;
        }
    }

    fn link_into(&mut self, dir: u64, name: &str, ino: u64) {
        if let Data::Dir { entries, .. } = &mut self.node_mut(dir).data {
            entries.insert(name.to_string(), ino);
        }
        self.touch(dir, true);
    }

    fn create(&mut self, dir: u64, name: &str, data: Data, mode: u32) -> u64 {
        let ino = self.alloc(Node::new(data, mode));
        self.link_into(dir, name, ino);
        ino
    }

    fn existing_child(&mut self, dir: u64, dir_path: &[String], name: &str) -> Option<u64> {
        let found = match &self.node(dir).data {
            Data::Dir { entries, .. } => entries.get(name).copied(),
            _ => None,
        };
        found.or_else(|| self.materialize(dir, dir_path, name))
    }

    fn release(&mut self, ino: u64) {
        let n = self.node_mut(ino);
        n.nlink = n.nlink.saturating_sub(1);
        if n.nlink == 0 && !self.fds.values().any(|o| o.ino == ino) {
            self.nodes.remove(&ino);
        }
    }

}

fn read_node(fs: &mut Fs, ino: u64) -> Result<Vec<u8>> {
    match &fs.node(ino).data {
        Data::Dir { .. } => return Err(Errno::IsDir),
        Data::Link(_) => return Err(Errno::Inval),
        Data::File { .. } => {}
    }
    fs.load(ino)?;
    match &mut fs.node_mut(ino).data {
        Data::File { bytes, .. } => Ok(bytes.clone()),
        _ => Err(Errno::Io),
    }
}

// ---- public API ---------------------------------------------------------------------------

/// Mount `backend` at `prefix` (created as a directory if absent).
pub fn mount(prefix: &str, backend: Arc<dyn Backend>) {
    let mut fs = fs();
    let abs = normalize(&fs.cwd.clone(), prefix);
    let mut dir = ROOT;
    let mut cur: Vec<String> = Vec::new();
    for c in components(&abs).map(str::to_string).collect::<Vec<_>>() {
        let next = match &fs.node(dir).data {
            Data::Dir { entries, .. } => entries.get(&c).copied(),
            _ => None,
        };
        dir = match next {
            Some(n) => n,
            None => fs.create(dir, &c, Data::Dir { entries: BTreeMap::new(), unlisted: None }, 0o755),
        };
        cur.push(c);
    }
    if let Data::Dir { unlisted, .. } = &mut fs.node_mut(dir).data {
        *unlisted = Some(abs.clone());
    }
    fs.mounts.push((abs, backend));
}

pub fn cwd() -> String {
    fs().cwd.clone()
}

pub fn set_cwd(path: &str) -> Result<()> {
    let mut fs = fs();
    let r = fs.resolve(path, true)?;
    if !matches!(fs.node(r.ino).data, Data::Dir { .. }) {
        return Err(Errno::NotDir);
    }
    fs.cwd = join(&r.path);
    Ok(())
}

pub fn stat(path: &str, follow: bool) -> Result<Stat> {
    let mut fs = fs();
    let r = fs.resolve(path, follow)?;
    Ok(fs.stat_node(r.ino))
}

pub fn exists(path: &str) -> bool {
    stat(path, true).is_ok()
}

pub fn is_file(path: &str) -> bool {
    stat(path, true).is_ok_and(|s| s.kind == Kind::File)
}

pub fn is_dir(path: &str) -> bool {
    stat(path, true).is_ok_and(|s| s.kind == Kind::Dir)
}

pub fn read_file(path: &str) -> Result<Vec<u8>> {
    let mut fs = fs();
    let r = fs.resolve(path, true)?;
    let bytes = read_node(&mut fs, r.ino)?;
    let now = unix_ms();
    fs.node_mut(r.ino).atime = now;
    Ok(bytes)
}

pub fn write_file(path: &str, data: &[u8]) -> Result<()> {
    let fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0o666)?;
    let written = write(fd, data, None);
    let _ = close(fd);
    written.map(|_| ())
}

pub fn realpath(path: &str) -> Result<String> {
    let mut fs = fs();
    let r = fs.resolve(path, true)?;
    Ok(join(&r.path))
}

pub fn open(path: &str, flags: i32, mode: u32) -> Result<i32> {
    let mut fs = fs();
    let acc = flags & 3;
    let follow = flags & O_NOFOLLOW == 0;
    let ino = match fs.resolve(path, follow) {
        Ok(r) => {
            if flags & O_CREAT != 0 && flags & O_EXCL != 0 {
                return Err(Errno::Exist);
            }
            if matches!(fs.node(r.ino).data, Data::Dir { .. }) && acc != 0 {
                return Err(Errno::IsDir);
            }
            if flags & O_DIRECTORY != 0 && !matches!(fs.node(r.ino).data, Data::Dir { .. }) {
                return Err(Errno::NotDir);
            }
            if matches!(fs.node(r.ino).data, Data::Link(_)) {
                return Err(Errno::Loop);
            }
            r.ino
        }
        Err(Errno::NoEnt) if flags & O_CREAT != 0 => {
            let (dir, _, name) = fs.resolve_parent(path)?;
            fs.create(dir, &name, Data::File { bytes: Vec::new(), lazy: None, lazy_size: 0 }, mode & 0o7777)
        }
        Err(e) => return Err(e),
    };
    if flags & O_TRUNC != 0 && acc != 0 {
        if let Data::File { bytes, lazy, .. } = &mut fs.node_mut(ino).data {
            bytes.clear();
            *lazy = None;
        }
        fs.touch(ino, true);
    }
    let mut fd = 3;
    while fs.fds.contains_key(&fd) {
        fd += 1;
    }
    fs.fds.insert(
        fd,
        Open { ino, readable: acc != O_WRONLY, writable: acc == O_WRONLY || acc == O_RDWR, append: flags & O_APPEND != 0, pos: 0 },
    );
    Ok(fd)
}

pub fn close(fd: i32) -> Result<()> {
    let mut fs = fs();
    let open = fs.fds.remove(&fd).ok_or(Errno::BadF)?;
    if fs.nodes.get(&open.ino).is_some_and(|n| n.nlink == 0) && !fs.fds.values().any(|o| o.ino == open.ino) {
        fs.nodes.remove(&open.ino);
    }
    Ok(())
}

/// Read into `buf` at `pos` (or the descriptor's position when `None`).
pub fn read(fd: i32, buf: &mut [u8], pos: Option<u64>) -> Result<usize> {
    let mut fs = fs();
    let (ino, start, readable) = {
        let o = fs.fds.get(&fd).ok_or(Errno::BadF)?;
        (o.ino, pos.unwrap_or(o.pos), o.readable)
    };
    if !readable {
        return Err(Errno::BadF);
    }
    if matches!(fs.node(ino).data, Data::Dir { .. }) {
        return Err(Errno::IsDir);
    }
    fs.load(ino)?;
    let n = match &fs.node(ino).data {
        Data::File { bytes, .. } => {
            let start = (start as usize).min(bytes.len());
            let n = buf.len().min(bytes.len() - start);
            buf[..n].copy_from_slice(&bytes[start..start + n]);
            n
        }
        _ => return Err(Errno::Inval),
    };
    if pos.is_none() {
        fs.fds.get_mut(&fd).unwrap().pos += n as u64;
    }
    fs.node_mut(ino).atime = unix_ms();
    Ok(n)
}

pub fn write(fd: i32, data: &[u8], pos: Option<u64>) -> Result<usize> {
    let mut fs = fs();
    let (ino, start, writable, append) = {
        let o = fs.fds.get(&fd).ok_or(Errno::BadF)?;
        (o.ino, pos.unwrap_or(o.pos), o.writable, o.append)
    };
    if !writable {
        return Err(Errno::BadF);
    }
    fs.load(ino)?;
    let end = match &mut fs.node_mut(ino).data {
        Data::File { bytes, .. } => {
            let start = if append { bytes.len() } else { start as usize };
            if bytes.len() < start {
                bytes.resize(start, 0);
            }
            let end = start + data.len();
            if bytes.len() < end {
                bytes.resize(end, 0);
            }
            bytes[start..end].copy_from_slice(data);
            end as u64
        }
        Data::Dir { .. } => return Err(Errno::IsDir),
        Data::Link(_) => return Err(Errno::Inval),
    };
    if pos.is_none() {
        fs.fds.get_mut(&fd).unwrap().pos = end;
    }
    fs.touch(ino, true);
    Ok(data.len())
}

pub fn fstat(fd: i32) -> Result<Stat> {
    let fs = fs();
    let o = fs.fds.get(&fd).ok_or(Errno::BadF)?;
    Ok(fs.stat_node(o.ino))
}

pub fn ftruncate(fd: i32, len: u64) -> Result<()> {
    let mut fs = fs();
    let (ino, writable) = {
        let o = fs.fds.get(&fd).ok_or(Errno::BadF)?;
        (o.ino, o.writable)
    };
    if !writable {
        return Err(Errno::Inval);
    }
    fs.load(ino)?;
    match &mut fs.node_mut(ino).data {
        Data::File { bytes, .. } => bytes.resize(len as usize, 0),
        _ => return Err(Errno::Inval),
    }
    fs.touch(ino, true);
    Ok(())
}

pub fn is_open(fd: i32) -> bool {
    fs().fds.contains_key(&fd)
}

/// Create `path`; with `recursive`, every missing ancestor too, returning the first created
/// directory (Node's contract) and succeeding on an existing directory.
pub fn mkdir(path: &str, mode: u32, recursive: bool) -> Result<Option<String>> {
    let mut fs = fs();
    if !recursive {
        let (dir, dir_path, name) = fs.resolve_parent(path)?;
        if fs.existing_child(dir, &dir_path, &name).is_some() {
            return Err(Errno::Exist);
        }
        fs.create(dir, &name, Data::Dir { entries: BTreeMap::new(), unlisted: None }, mode & 0o7777);
        return Ok(None);
    }
    let abs = normalize(&fs.cwd.clone(), path);
    let mut dir = ROOT;
    let mut cur: Vec<String> = Vec::new();
    let mut first = None;
    for c in components(&abs).map(str::to_string).collect::<Vec<_>>() {
        let existing = fs.existing_child(dir, &cur, &c);
        let next = match existing {
            Some(n) => {
                let n = if matches!(fs.node(n).data, Data::Link(_)) {
                    fs.resolve(&format!("{}/{}", join(&cur), c), true)?.ino
                } else {
                    n
                };
                if !matches!(fs.node(n).data, Data::Dir { .. }) {
                    return Err(if cur.len() + 1 == components(&abs).count() { Errno::Exist } else { Errno::NotDir });
                }
                n
            }
            None => {
                cur.push(c.clone());
                let created = join(&cur);
                cur.pop();
                if first.is_none() {
                    first = Some(created);
                }
                fs.create(dir, &c, Data::Dir { entries: BTreeMap::new(), unlisted: None }, mode & 0o7777)
            }
        };
        dir = next;
        cur.push(c);
    }
    Ok(first)
}

pub fn rmdir(path: &str) -> Result<()> {
    let mut fs = fs();
    let (dir, dir_path, name) = fs.resolve_parent(path)?;
    let ino = fs.existing_child(dir, &dir_path, &name).ok_or(Errno::NoEnt)?;
    fs.list_remote(ino);
    match &fs.node(ino).data {
        Data::Dir { entries, .. } if entries.is_empty() => {}
        Data::Dir { .. } => return Err(Errno::NotEmpty),
        _ => return Err(Errno::NotDir),
    }
    if let Data::Dir { entries, .. } = &mut fs.node_mut(dir).data {
        entries.remove(&name);
    }
    fs.node_mut(ino).nlink = 0;
    fs.release(ino);
    fs.touch(dir, true);
    Ok(())
}

pub fn unlink(path: &str) -> Result<()> {
    let mut fs = fs();
    let (dir, dir_path, name) = fs.resolve_parent(path)?;
    let ino = fs.existing_child(dir, &dir_path, &name).ok_or(Errno::NoEnt)?;
    if matches!(fs.node(ino).data, Data::Dir { .. }) {
        return Err(Errno::IsDir);
    }
    if let Data::Dir { entries, .. } = &mut fs.node_mut(dir).data {
        entries.remove(&name);
    }
    fs.release(ino);
    fs.touch(dir, true);
    Ok(())
}

pub fn readdir(path: &str) -> Result<Vec<(String, Kind)>> {
    let mut fs = fs();
    let r = fs.resolve(path, true)?;
    if !matches!(fs.node(r.ino).data, Data::Dir { .. }) {
        return Err(Errno::NotDir);
    }
    fs.list_remote(r.ino);
    let entries: Vec<(String, u64)> = match &fs.node(r.ino).data {
        Data::Dir { entries, .. } => entries.iter().map(|(k, v)| (k.clone(), *v)).collect(),
        _ => unreachable!(),
    };
    Ok(entries.into_iter().map(|(name, ino)| (name, fs.node(ino).kind())).collect())
}

pub fn rename(from: &str, to: &str) -> Result<()> {
    let mut fs = fs();
    let (sdir, sdir_path, sname) = fs.resolve_parent(from)?;
    let ino = fs.existing_child(sdir, &sdir_path, &sname).ok_or(Errno::NoEnt)?;
    let (ddir, ddir_path, dname) = fs.resolve_parent(to)?;
    let src_is_dir = matches!(fs.node(ino).data, Data::Dir { .. });
    if src_is_dir {
        let src_abs = join(&{
            let mut p = sdir_path.clone();
            p.push(sname.clone());
            p
        });
        let dst_abs = join(&{
            let mut p = ddir_path.clone();
            p.push(dname.clone());
            p
        });
        if dst_abs == src_abs {
            return Ok(());
        }
        if dst_abs.starts_with(&format!("{src_abs}/")) {
            return Err(Errno::Inval);
        }
    }
    if let Some(existing) = fs.existing_child(ddir, &ddir_path, &dname) {
        if existing == ino {
            return Ok(());
        }
        let dst_is_dir = matches!(fs.node(existing).data, Data::Dir { .. });
        match (src_is_dir, dst_is_dir) {
            (true, false) => return Err(Errno::NotDir),
            (false, true) => return Err(Errno::IsDir),
            (true, true) => {
                fs.list_remote(existing);
                if matches!(&fs.node(existing).data, Data::Dir { entries, .. } if !entries.is_empty()) {
                    return Err(Errno::NotEmpty);
                }
            }
            _ => {}
        }
        fs.release(existing);
    }
    if let Data::Dir { entries, .. } = &mut fs.node_mut(sdir).data {
        entries.remove(&sname);
    }
    fs.link_into(ddir, &dname, ino);
    fs.touch(sdir, true);
    fs.touch(ino, false);
    Ok(())
}

pub fn symlink(target: &str, path: &str) -> Result<()> {
    let mut fs = fs();
    let (dir, dir_path, name) = fs.resolve_parent(path)?;
    if fs.existing_child(dir, &dir_path, &name).is_some() {
        return Err(Errno::Exist);
    }
    fs.create(dir, &name, Data::Link(target.to_string()), 0o777);
    Ok(())
}

pub fn readlink(path: &str) -> Result<String> {
    let mut fs = fs();
    let r = fs.resolve(path, false)?;
    match &fs.node(r.ino).data {
        Data::Link(t) => Ok(t.clone()),
        _ => Err(Errno::Inval),
    }
}

pub fn link(existing: &str, path: &str) -> Result<()> {
    let mut fs = fs();
    let src = fs.resolve(existing, false)?;
    if matches!(fs.node(src.ino).data, Data::Dir { .. }) {
        return Err(Errno::Perm);
    }
    let (dir, dir_path, name) = fs.resolve_parent(path)?;
    if fs.existing_child(dir, &dir_path, &name).is_some() {
        return Err(Errno::Exist);
    }
    fs.node_mut(src.ino).nlink += 1;
    fs.link_into(dir, &name, src.ino);
    Ok(())
}

pub fn copy_file(src: &str, dst: &str, exclusive: bool) -> Result<()> {
    let bytes = {
        let mut fs = fs();
        let r = fs.resolve(src, true)?;
        read_node(&mut fs, r.ino)?
    };
    let mode = stat(src, true)?.mode & 0o7777;
    let flags = O_WRONLY | O_CREAT | O_TRUNC | if exclusive { O_EXCL } else { 0 };
    let fd = open(dst, flags, mode)?;
    let r = write(fd, &bytes, None);
    let _ = close(fd);
    r.map(|_| ())
}

pub fn chmod(path: &str, mode: u32) -> Result<()> {
    let mut fs = fs();
    let r = fs.resolve(path, true)?;
    fs.node_mut(r.ino).mode = mode & 0o7777;
    fs.touch(r.ino, false);
    Ok(())
}

pub fn fchmod(fd: i32, mode: u32) -> Result<()> {
    let mut fs = fs();
    let ino = fs.fds.get(&fd).ok_or(Errno::BadF)?.ino;
    fs.node_mut(ino).mode = mode & 0o7777;
    fs.touch(ino, false);
    Ok(())
}

pub fn chown(path: &str, uid: u32, gid: u32, follow: bool) -> Result<()> {
    let mut fs = fs();
    let r = fs.resolve(path, follow)?;
    let n = fs.node_mut(r.ino);
    n.uid = uid;
    n.gid = gid;
    Ok(())
}

pub fn utimes(path: &str, atime_ms: f64, mtime_ms: f64, follow: bool) -> Result<()> {
    let mut fs = fs();
    let r = fs.resolve(path, follow)?;
    let n = fs.node_mut(r.ino);
    n.atime = atime_ms;
    n.mtime = mtime_ms;
    Ok(())
}

pub fn futimes(fd: i32, atime_ms: f64, mtime_ms: f64) -> Result<()> {
    let mut fs = fs();
    let ino = fs.fds.get(&fd).ok_or(Errno::BadF)?.ino;
    let n = fs.node_mut(ino);
    n.atime = atime_ms;
    n.mtime = mtime_ms;
    Ok(())
}

/// `mkdtemp`: create `prefix` + six unique characters.
pub fn mkdtemp(prefix: &str) -> Result<String> {
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    for _ in 0..100 {
        let mut x = {
            let mut fs = fs();
            fs.tmp_counter += 1;
            (fs.tmp_counter ^ (unix_ms() as u64)).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        };
        let mut name = String::from(prefix);
        for _ in 0..6 {
            name.push(CHARS[(x % CHARS.len() as u64) as usize] as char);
            x /= CHARS.len() as u64;
        }
        match mkdir(&name, 0o700, false) {
            Ok(_) => return Ok(name),
            Err(Errno::Exist) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(Errno::Exist)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_directories_and_links_round_trip() {
        mkdir("/t1/a/b", 0o755, true).unwrap();
        write_file("/t1/a/b/f.txt", b"hello").unwrap();
        assert_eq!(read_file("/t1/a/b/f.txt").unwrap(), b"hello");
        assert_eq!(stat("/t1/a/b/f.txt", true).unwrap().size, 5);
        symlink("/t1/a/b", "/t1/link").unwrap();
        assert_eq!(read_file("/t1/link/f.txt").unwrap(), b"hello");
        assert_eq!(realpath("/t1/a/../a/b/./f.txt").unwrap(), "/t1/a/b/f.txt");
        assert_eq!(stat("/t1/link", false).unwrap().kind, Kind::Symlink);
        assert_eq!(readdir("/t1/a/b").unwrap(), vec![("f.txt".to_string(), Kind::File)]);
        assert_eq!(rmdir("/t1/a/b"), Err(Errno::NotEmpty));
        rename("/t1/a/b/f.txt", "/t1/g.txt").unwrap();
        assert_eq!(read_file("/t1/a/b/f.txt"), Err(Errno::NoEnt));
        unlink("/t1/g.txt").unwrap();
        assert_eq!(mkdir("/t1/a", 0o755, false), Err(Errno::Exist));
    }

    #[test]
    fn descriptors_keep_positions_and_append() {
        let fd = open("/t2.txt", O_RDWR | O_CREAT, 0o644).unwrap();
        assert_eq!(write(fd, b"abcdef", None).unwrap(), 6);
        let mut buf = [0u8; 3];
        assert_eq!(read(fd, &mut buf, Some(2)).unwrap(), 3);
        assert_eq!(&buf, b"cde");
        ftruncate(fd, 3).unwrap();
        close(fd).unwrap();
        let fd = open("/t2.txt", O_WRONLY | O_APPEND, 0).unwrap();
        write(fd, b"XY", None).unwrap();
        close(fd).unwrap();
        assert_eq!(read_file("/t2.txt").unwrap(), b"abcXY");
        assert_eq!(open("/t2.txt", O_CREAT | O_EXCL | O_WRONLY, 0o644), Err(Errno::Exist));
    }

    struct Remote;
    impl Backend for Remote {
        fn stat(&self, path: &str) -> Option<RemoteStat> {
            match path {
                "/mnt/r/dir" => Some(RemoteStat { is_dir: true, size: 0 }),
                "/mnt/r/dir/x.txt" => Some(RemoteStat { is_dir: false, size: 4 }),
                _ => None,
            }
        }
        fn read(&self, path: &str) -> Option<Vec<u8>> {
            (path == "/mnt/r/dir/x.txt").then(|| b"data".to_vec())
        }
        fn list(&self, path: &str) -> Option<Vec<RemoteEntry>> {
            (path == "/mnt/r/dir").then(|| vec![RemoteEntry { name: "x.txt".into(), is_dir: false, size: 4 }])
        }
    }

    #[test]
    fn mounted_backends_are_discovered_lazily() {
        mount("/mnt/r", Arc::new(Remote));
        assert_eq!(stat("/mnt/r/dir/x.txt", true).unwrap().size, 4);
        assert_eq!(read_file("/mnt/r/dir/x.txt").unwrap(), b"data");
        assert_eq!(readdir("/mnt/r/dir").unwrap().len(), 1);
        assert_eq!(read_file("/mnt/r/missing"), Err(Errno::NoEnt));
    }
}
