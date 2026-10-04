//! An in-memory POSIX-flavoured file system: the [`FileSystem`] of targets that have no OS file
//! system (`wasm32-unknown-unknown`, where [`super::host`] is the process-wide [`mem`]
//! instance), and the store an embedder serves bundled files from (Python's embedded standard
//! library, an [`super::Overlay`] over the OS).
//!
//! Paths are `/`-separated and absolute (relative ones resolve against the instance's working
//! directory). Directories, files and symlinks live in one inode table behind a lock; file
//! descriptors (numbered from the instance's base, 3 by default) keep a position, so `read` /
//! `write` without an offset behave like the system calls. File contents may borrow `'static`
//! data (copied on the first write). A [`Backend`] can be [`MemFs::mount`]ed on a prefix: paths
//! below it are discovered lazily through the backend (an OPFS directory, a remote origin, a
//! bundle index) and their contents fetched on first read — the hook the browser embedding
//! serves through its suspending synchronous host call. Writes below a mount stay in memory.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use super::FileSystem;
use crate::errno::FsError;
use crate::fs::flags::{
    O_APPEND, O_CREAT, O_DIRECTORY, O_EXCL, O_NOFOLLOW, O_RDWR, O_TRUNC, O_WRONLY,
};
use crate::fs::{DirentKind, Stat, StatFs, Timespec, S_IFDIR, S_IFLNK, S_IFREG};

type R<T> = Result<T, FsError>;

const MAX_LINKS: u32 = 40;
const ENOENT: FsError = FsError("ENOENT");
const EEXIST: FsError = FsError("EEXIST");
const ENOTDIR: FsError = FsError("ENOTDIR");
const EISDIR: FsError = FsError("EISDIR");
const ENOTEMPTY: FsError = FsError("ENOTEMPTY");
const EINVAL: FsError = FsError("EINVAL");
const EBADF: FsError = FsError("EBADF");
const ELOOP: FsError = FsError("ELOOP");
const EPERM: FsError = FsError("EPERM");
const ENOSYS: FsError = FsError("ENOSYS");
const EIO: FsError = FsError("EIO");

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
/// file system locked: an implementation must not call back into it.
pub trait Backend: Send + Sync {
    fn stat(&self, path: &str) -> Option<RemoteStat>;
    fn read(&self, path: &str) -> Option<Vec<u8>>;
    fn list(&self, path: &str) -> Option<Vec<RemoteEntry>>;
}

/// Milliseconds since the Unix epoch on every target (`SystemTime::now` panics on wasm32).
fn unix_ms() -> f64 {
    web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

fn timespec(ms: f64) -> Timespec {
    Timespec::from_secs_f64(ms / 1000.0)
}

enum Data {
    File {
        bytes: Cow<'static, [u8]>,
        lazy: Option<String>,
        lazy_size: u64,
    },
    Dir {
        entries: BTreeMap<String, u64>,
        unlisted: Option<String>,
    },
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
        Node {
            data,
            mode,
            nlink: 1,
            uid: 0,
            gid: 0,
            atime: now,
            mtime: now,
            ctime: now,
            birth: now,
        }
    }

    fn is_dir(&self) -> bool {
        matches!(self.data, Data::Dir { .. })
    }

    fn dirent(&self) -> DirentKind {
        match self.data {
            Data::File { .. } => DirentKind::File,
            Data::Dir { .. } => DirentKind::Dir,
            Data::Link(_) => DirentKind::Link,
        }
    }

    fn type_bits(&self) -> u32 {
        match self.data {
            Data::File { .. } => S_IFREG,
            Data::Dir { .. } => S_IFDIR,
            Data::Link(_) => S_IFLNK,
        }
    }
}

fn empty_file() -> Data {
    Data::File {
        bytes: Cow::Borrowed(&[]),
        lazy: None,
        lazy_size: 0,
    }
}

fn empty_dir() -> Data {
    Data::Dir {
        entries: BTreeMap::new(),
        unlisted: None,
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
    fd_base: i32,
    mounts: Vec<(String, Arc<dyn Backend>)>,
    cwd: String,
    tmp_counter: u64,
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

    fn open_of(&self, fd: i32) -> R<&Open> {
        self.fds.get(&fd).ok_or(EBADF)
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
                prefix == "/"
                    || abs == prefix
                    || abs
                        .strip_prefix(prefix.as_str())
                        .is_some_and(|r| r.starts_with('/'))
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
            Data::Dir {
                entries: BTreeMap::new(),
                unlisted: Some(abs.clone()),
            }
        } else {
            Data::File {
                bytes: Cow::Borrowed(&[]),
                lazy: Some(abs.clone()),
                lazy_size: rs.size,
            }
        };
        let mode = if rs.is_dir { 0o755 } else { 0o644 };
        let ino = self.alloc(Node::new(data, mode));
        if let Data::Dir { entries, .. } = &mut self.node_mut(dir).data {
            entries.insert(name.to_string(), ino);
        }
        Some(ino)
    }

    fn resolve(&mut self, path: &str, follow_last: bool) -> R<Resolved> {
        if path.is_empty() {
            return Err(ENOENT);
        }
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
            let child = match &self.node(dir).data {
                Data::Dir { entries, .. } => entries.get(&name).copied(),
                _ => return Err(ENOTDIR),
            };
            let child = match child {
                Some(c) => c,
                None => self.materialize(dir, &cur, &name).ok_or(ENOENT)?,
            };
            let is_last = todo.is_empty();
            if let Data::Link(target) = &self.node(child).data {
                if !is_last || follow_last {
                    links += 1;
                    if links > MAX_LINKS {
                        return Err(ELOOP);
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
        Ok(Resolved {
            ino: *inos.last().unwrap(),
            path: cur,
        })
    }

    /// The directory that would hold `path`, and the final name.
    fn resolve_parent(&mut self, path: &str) -> R<(u64, Vec<String>, String)> {
        let abs = absolutize(&self.cwd, path);
        let mut comps: Vec<String> = components(&abs).map(str::to_string).collect();
        let name = comps.pop().ok_or(EEXIST)?;
        if name == "." || name == ".." {
            return Err(EEXIST);
        }
        let parent = self.resolve(&join(&comps), true)?;
        if !self.node(parent.ino).is_dir() {
            return Err(ENOTDIR);
        }
        Ok((parent.ino, parent.path, name))
    }

    fn load(&mut self, ino: u64) -> R<()> {
        let path = match &self.node(ino).data {
            Data::File { lazy: Some(p), .. } => p.clone(),
            _ => return Ok(()),
        };
        let backend = self.mount_for(&path).ok_or(EIO)?;
        let bytes = backend.read(&path).ok_or(EIO)?;
        if let Data::File {
            bytes: slot, lazy, ..
        } = &mut self.node_mut(ino).data
        {
            *slot = Cow::Owned(bytes);
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
        let Some(backend) = self.mount_for(&path) else {
            return;
        };
        let Some(list) = backend.list(&path) else {
            return;
        };
        for e in list {
            let exists = matches!(&self.node(ino).data, Data::Dir { entries, .. } if entries.contains_key(&e.name));
            if exists {
                continue;
            }
            let child_path = if path == "/" {
                format!("/{}", e.name)
            } else {
                format!("{path}/{}", e.name)
            };
            let (data, mode) = if e.is_dir {
                (
                    Data::Dir {
                        entries: BTreeMap::new(),
                        unlisted: Some(child_path),
                    },
                    0o755,
                )
            } else {
                (
                    Data::File {
                        bytes: Cow::Borrowed(&[]),
                        lazy: Some(child_path),
                        lazy_size: e.size,
                    },
                    0o644,
                )
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
            Data::File {
                bytes, lazy: None, ..
            } => bytes.len() as u64,
            Data::File { lazy_size, .. } => *lazy_size,
            Data::Dir { entries, .. } => (entries.len() as u64 + 2) * 32,
            Data::Link(t) => t.len() as u64,
        };
        Stat {
            dev: 1,
            mode: n.type_bits() | (n.mode & 0o7777),
            nlink: u64::from(n.nlink),
            uid: n.uid,
            gid: n.gid,
            rdev: 0,
            blksize: 4096,
            ino,
            size,
            blocks: size.div_ceil(512),
            atime: timespec(n.atime),
            mtime: timespec(n.mtime),
            ctime: timespec(n.ctime),
            birthtime: timespec(n.birth),
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

    fn read_node(&mut self, ino: u64) -> R<Vec<u8>> {
        match &self.node(ino).data {
            Data::Dir { .. } => return Err(EISDIR),
            Data::Link(_) => return Err(EINVAL),
            Data::File { .. } => {}
        }
        self.load(ino)?;
        match &self.node(ino).data {
            Data::File { bytes, .. } => Ok(bytes.to_vec()),
            _ => Err(EIO),
        }
    }

    /// The directory at absolute `abs`, created with its missing ancestors (mode `mode`).
    fn mkdir_p(&mut self, abs: &str, mode: u32) -> u64 {
        let mut dir = ROOT;
        for c in components(abs).map(str::to_string).collect::<Vec<_>>() {
            let next = match &self.node(dir).data {
                Data::Dir { entries, .. } => entries.get(&c).copied(),
                _ => None,
            };
            dir = match next {
                Some(n) => n,
                None => self.create(dir, &c, empty_dir(), mode),
            };
        }
        dir
    }

    fn lowest_free_fd(&self) -> i32 {
        let mut fd = self.fd_base;
        while self.fds.contains_key(&fd) {
            fd += 1;
        }
        fd
    }
}

/// An in-memory file system instance. See the module docs.
pub struct MemFs {
    inner: Mutex<Fs>,
}

impl Default for MemFs {
    fn default() -> MemFs {
        MemFs::new()
    }
}

impl MemFs {
    /// An empty tree (just `/`), descriptors from 3, working directory `/`.
    pub fn new() -> MemFs {
        MemFs::with_fd_base(3)
    }

    /// Descriptors numbered from `base`: an instance layered over the OS (see
    /// [`super::Overlay`]) picks a range the OS will not hand out.
    pub fn with_fd_base(base: i32) -> MemFs {
        let mut nodes = HashMap::new();
        nodes.insert(ROOT, Node::new(empty_dir(), 0o755));
        MemFs {
            inner: Mutex::new(Fs {
                nodes,
                next_ino: ROOT + 1,
                fds: BTreeMap::new(),
                fd_base: base,
                mounts: Vec::new(),
                cwd: "/".to_string(),
                tmp_counter: 0,
            }),
        }
    }

    fn fs(&self) -> MutexGuard<'_, Fs> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Adds a file at absolute `path` holding `contents` (borrowed when `'static`), creating its
    /// missing parent directories with `dir_mode`; replaces an existing file.
    pub fn insert(
        &self,
        path: &str,
        contents: impl Into<Cow<'static, [u8]>>,
        mode: u32,
        dir_mode: u32,
    ) {
        let mut fs = self.fs();
        let abs = normalize(&fs.cwd.clone(), path);
        let (parent, name) = abs.rsplit_once('/').unwrap_or(("", &abs));
        let dir = fs.mkdir_p(parent, dir_mode);
        let data = Data::File {
            bytes: contents.into(),
            lazy: None,
            lazy_size: 0,
        };
        let existing = match &fs.node(dir).data {
            Data::Dir { entries, .. } => entries.get(name).copied(),
            _ => None,
        };
        match existing {
            Some(ino) => fs.node_mut(ino).data = data,
            None => {
                fs.create(dir, name, data, mode);
            }
        }
    }

    /// Mount `backend` at `prefix` (created as a directory if absent).
    pub fn mount(&self, prefix: &str, backend: Arc<dyn Backend>) {
        let mut fs = self.fs();
        let abs = normalize(&fs.cwd.clone(), prefix);
        let dir = fs.mkdir_p(&abs, 0o755);
        if let Data::Dir { unlisted, .. } = &mut fs.node_mut(dir).data {
            *unlisted = Some(abs.clone());
        }
        fs.mounts.push((abs, backend));
    }

    /// Opens `path` without touching the path-resolution rules for the caller: the shared core of
    /// `open` and the whole-file helpers.
    fn open_in(fs: &mut Fs, path: &str, flags: i32, mode: u32) -> R<i32> {
        let acc = flags & 3;
        let follow = O_NOFOLLOW == 0 || flags & O_NOFOLLOW == 0;
        let ino = match fs.resolve(path, follow) {
            Ok(r) => {
                if flags & O_CREAT != 0 && flags & O_EXCL != 0 {
                    return Err(EEXIST);
                }
                let node = fs.node(r.ino);
                if node.is_dir() && acc != 0 {
                    return Err(EISDIR);
                }
                if O_DIRECTORY != 0 && flags & O_DIRECTORY != 0 && !node.is_dir() {
                    return Err(ENOTDIR);
                }
                if matches!(node.data, Data::Link(_)) {
                    return Err(ELOOP);
                }
                r.ino
            }
            Err(e) if e == ENOENT && flags & O_CREAT != 0 => {
                let (dir, _, name) = fs.resolve_parent(path)?;
                fs.create(dir, &name, empty_file(), mode & 0o7777)
            }
            Err(e) => return Err(e),
        };
        if flags & O_TRUNC != 0 && acc != 0 {
            if let Data::File { bytes, lazy, .. } = &mut fs.node_mut(ino).data {
                *bytes = Cow::Borrowed(&[]);
                *lazy = None;
            }
            fs.touch(ino, true);
        }
        let fd = fs.lowest_free_fd();
        fs.fds.insert(
            fd,
            Open {
                ino,
                readable: acc != O_WRONLY,
                writable: acc == O_WRONLY || acc == O_RDWR,
                append: flags & O_APPEND != 0,
                pos: 0,
            },
        );
        Ok(fd)
    }

    fn close_in(fs: &mut Fs, fd: i32) -> R<()> {
        let open = fs.fds.remove(&fd).ok_or(EBADF)?;
        if fs.nodes.get(&open.ino).is_some_and(|n| n.nlink == 0)
            && !fs.fds.values().any(|o| o.ino == open.ino)
        {
            fs.nodes.remove(&open.ino);
        }
        Ok(())
    }

    fn write_in(fs: &mut Fs, fd: i32, data: &[u8], pos: Option<u64>) -> R<usize> {
        let (ino, start, writable, append) = {
            let o = fs.open_of(fd)?;
            (o.ino, pos.unwrap_or(o.pos), o.writable, o.append)
        };
        if !writable {
            return Err(EBADF);
        }
        fs.load(ino)?;
        let end = match &mut fs.node_mut(ino).data {
            Data::File { bytes, .. } => {
                let bytes = bytes.to_mut();
                let start = if append { bytes.len() } else { start as usize };
                let end = start + data.len();
                if bytes.len() < end {
                    bytes.resize(end, 0);
                }
                bytes[start..end].copy_from_slice(data);
                end as u64
            }
            Data::Dir { .. } => return Err(EISDIR),
            Data::Link(_) => return Err(EINVAL),
        };
        if pos.is_none() {
            fs.fds.get_mut(&fd).unwrap().pos = end;
        }
        fs.touch(ino, true);
        Ok(data.len())
    }
}

/// The process-wide instance: the file system of targets without an OS (see [`super::host`]),
/// where the module loader, `node:fs` and `process.cwd()` all run on it.
pub fn mem() -> &'static MemFs {
    static FS: OnceLock<MemFs> = OnceLock::new();
    FS.get_or_init(MemFs::new)
}

impl FileSystem for MemFs {
    fn open(&self, path: &str, flags: i32, mode: u32) -> R<i32> {
        MemFs::open_in(&mut self.fs(), path, flags, mode)
    }

    fn close(&self, fd: i32) -> R<()> {
        MemFs::close_in(&mut self.fs(), fd)
    }

    fn is_open(&self, fd: i32) -> bool {
        self.fs().fds.contains_key(&fd)
    }

    fn read(&self, fd: i32, buf: &mut [u8], pos: Option<u64>) -> R<usize> {
        let mut fs = self.fs();
        let (ino, start, readable) = {
            let o = fs.open_of(fd)?;
            (o.ino, pos.unwrap_or(o.pos), o.readable)
        };
        if !readable {
            return Err(EBADF);
        }
        if fs.node(ino).is_dir() {
            return Err(EISDIR);
        }
        fs.load(ino)?;
        let n = match &fs.node(ino).data {
            Data::File { bytes, .. } => {
                let start = (start as usize).min(bytes.len());
                let n = buf.len().min(bytes.len() - start);
                buf[..n].copy_from_slice(&bytes[start..start + n]);
                n
            }
            _ => return Err(EINVAL),
        };
        if pos.is_none() {
            fs.fds.get_mut(&fd).unwrap().pos += n as u64;
        }
        fs.node_mut(ino).atime = unix_ms();
        Ok(n)
    }

    fn write(&self, fd: i32, data: &[u8], pos: Option<u64>) -> R<usize> {
        MemFs::write_in(&mut self.fs(), fd, data, pos)
    }

    fn lseek(&self, fd: i32, offset: i64, whence: i32) -> R<u64> {
        let mut fs = self.fs();
        let (ino, pos) = {
            let o = fs.open_of(fd)?;
            (o.ino, o.pos)
        };
        let end = fs.stat_node(ino).size;
        let base = match whence {
            0 => 0,
            1 => pos as i64,
            2 => end as i64,
            _ => return Err(EINVAL),
        };
        match base.checked_add(offset) {
            Some(p) if p >= 0 => {
                fs.fds.get_mut(&fd).unwrap().pos = p as u64;
                Ok(p as u64)
            }
            _ => Err(EINVAL),
        }
    }

    fn dup(&self, fd: i32) -> R<i32> {
        let mut fs = self.fs();
        let o = fs.open_of(fd)?;
        let copy = Open {
            ino: o.ino,
            readable: o.readable,
            writable: o.writable,
            append: o.append,
            pos: o.pos,
        };
        let nfd = fs.lowest_free_fd();
        fs.fds.insert(nfd, copy);
        Ok(nfd)
    }

    /// The standard streams (0-2) answer as character devices while not opened here.
    fn fstat(&self, fd: i32) -> R<Stat> {
        let fs = self.fs();
        match fs.fds.get(&fd) {
            Some(o) => Ok(fs.stat_node(o.ino)),
            None if (0..=2).contains(&fd) => Ok(Stat {
                dev: 1,
                mode: 0o020000 | 0o600,
                nlink: 1,
                ino: fd as u64,
                blksize: 4096,
                ..Stat::default()
            }),
            None => Err(EBADF),
        }
    }

    fn ftruncate(&self, fd: i32, len: u64) -> R<()> {
        let mut fs = self.fs();
        let (ino, writable) = {
            let o = fs.open_of(fd)?;
            (o.ino, o.writable)
        };
        if !writable {
            return Err(EINVAL);
        }
        fs.load(ino)?;
        match &mut fs.node_mut(ino).data {
            Data::File { bytes, .. } => bytes.to_mut().resize(len as usize, 0),
            _ => return Err(EINVAL),
        }
        fs.touch(ino, true);
        Ok(())
    }

    fn fsync(&self, fd: i32, _data_only: bool) -> R<()> {
        if (0..=2).contains(&fd) || self.is_open(fd) {
            Ok(())
        } else {
            Err(EBADF)
        }
    }

    fn fchmod(&self, fd: i32, mode: u32) -> R<()> {
        let mut fs = self.fs();
        let ino = fs.open_of(fd)?.ino;
        fs.node_mut(ino).mode = mode & 0o7777;
        fs.touch(ino, false);
        Ok(())
    }

    fn fchown(&self, fd: i32, uid: u32, gid: u32) -> R<()> {
        let mut fs = self.fs();
        let ino = fs.open_of(fd)?.ino;
        let n = fs.node_mut(ino);
        n.uid = uid;
        n.gid = gid;
        Ok(())
    }

    fn futimes(&self, fd: i32, atime: Timespec, mtime: Timespec) -> R<()> {
        let mut fs = self.fs();
        let ino = fs.open_of(fd)?.ino;
        let n = fs.node_mut(ino);
        n.atime = atime.as_secs_f64() * 1000.0;
        n.mtime = mtime.as_secs_f64() * 1000.0;
        Ok(())
    }

    fn stat(&self, path: &str, follow: bool) -> R<Stat> {
        let mut fs = self.fs();
        let r = fs.resolve(path, follow)?;
        Ok(fs.stat_node(r.ino))
    }

    fn statfs(&self, path: &str) -> R<StatFs> {
        self.stat(path, true)?;
        Ok(StatFs {
            bsize: 4096,
            blocks: 1 << 20,
            bfree: 1 << 19,
            bavail: 1 << 19,
            files: 1 << 20,
            ffree: 1 << 19,
        })
    }

    fn access(&self, path: &str, _mode: u32) -> R<()> {
        self.stat(path, true).map(|_| ())
    }

    fn chmod(&self, path: &str, mode: u32) -> R<()> {
        let mut fs = self.fs();
        let r = fs.resolve(path, true)?;
        fs.node_mut(r.ino).mode = mode & 0o7777;
        fs.touch(r.ino, false);
        Ok(())
    }

    fn chown(&self, path: &str, uid: u32, gid: u32, follow: bool) -> R<()> {
        let mut fs = self.fs();
        let r = fs.resolve(path, follow)?;
        let n = fs.node_mut(r.ino);
        n.uid = uid;
        n.gid = gid;
        Ok(())
    }

    fn utimes(&self, path: &str, atime: Timespec, mtime: Timespec, follow: bool) -> R<()> {
        let mut fs = self.fs();
        let r = fs.resolve(path, follow)?;
        let n = fs.node_mut(r.ino);
        n.atime = atime.as_secs_f64() * 1000.0;
        n.mtime = mtime.as_secs_f64() * 1000.0;
        Ok(())
    }

    /// Create `path`; with `recursive`, every missing ancestor too, returning the first created
    /// directory (Node's contract) and succeeding on an existing directory.
    fn mkdir(&self, path: &str, mode: u32, recursive: bool) -> R<Option<String>> {
        let mut fs = self.fs();
        if !recursive {
            let (dir, dir_path, name) = fs.resolve_parent(path)?;
            if fs.existing_child(dir, &dir_path, &name).is_some() {
                return Err(EEXIST);
            }
            fs.create(dir, &name, empty_dir(), mode & 0o7777);
            return Ok(None);
        }
        let abs = normalize(&fs.cwd.clone(), path);
        let total = components(&abs).count();
        let mut dir = ROOT;
        let mut cur: Vec<String> = Vec::new();
        let mut first = None;
        for c in components(&abs).map(str::to_string).collect::<Vec<_>>() {
            let next = match fs.existing_child(dir, &cur, &c) {
                Some(n) => {
                    let n = if matches!(fs.node(n).data, Data::Link(_)) {
                        fs.resolve(&format!("{}/{}", join(&cur), c), true)?.ino
                    } else {
                        n
                    };
                    if !fs.node(n).is_dir() {
                        return Err(if cur.len() + 1 == total {
                            EEXIST
                        } else {
                            ENOTDIR
                        });
                    }
                    n
                }
                None => {
                    cur.push(c.clone());
                    let created = join(&cur);
                    cur.pop();
                    first.get_or_insert(created);
                    fs.create(dir, &c, empty_dir(), mode & 0o7777)
                }
            };
            dir = next;
            cur.push(c);
        }
        Ok(first)
    }

    /// `mkdtemp`: create `prefix` + six unique characters.
    fn mkdtemp(&self, prefix: &str) -> R<String> {
        const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        for _ in 0..100 {
            let mut x = {
                let mut fs = self.fs();
                fs.tmp_counter += 1;
                (fs.tmp_counter ^ (unix_ms() as u64)).wrapping_mul(0x9E37_79B9_7F4A_7C15)
            };
            let mut name = String::from(prefix);
            for _ in 0..6 {
                name.push(CHARS[(x % CHARS.len() as u64) as usize] as char);
                x /= CHARS.len() as u64;
            }
            match self.mkdir(&name, 0o700, false) {
                Ok(_) => return Ok(name),
                Err(e) if e == EEXIST => continue,
                Err(e) => return Err(e),
            }
        }
        Err(EEXIST)
    }

    fn rmdir(&self, path: &str) -> R<()> {
        let mut fs = self.fs();
        let (dir, dir_path, name) = fs.resolve_parent(path)?;
        let ino = fs.existing_child(dir, &dir_path, &name).ok_or(ENOENT)?;
        fs.list_remote(ino);
        match &fs.node(ino).data {
            Data::Dir { entries, .. } if entries.is_empty() => {}
            Data::Dir { .. } => return Err(ENOTEMPTY),
            _ => return Err(ENOTDIR),
        }
        if let Data::Dir { entries, .. } = &mut fs.node_mut(dir).data {
            entries.remove(&name);
        }
        fs.node_mut(ino).nlink = 0;
        fs.release(ino);
        fs.touch(dir, true);
        Ok(())
    }

    fn unlink(&self, path: &str) -> R<()> {
        let mut fs = self.fs();
        let (dir, dir_path, name) = fs.resolve_parent(path)?;
        let ino = fs.existing_child(dir, &dir_path, &name).ok_or(ENOENT)?;
        if fs.node(ino).is_dir() {
            return Err(EISDIR);
        }
        if let Data::Dir { entries, .. } = &mut fs.node_mut(dir).data {
            entries.remove(&name);
        }
        fs.release(ino);
        fs.touch(dir, true);
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> R<()> {
        let mut fs = self.fs();
        let (sdir, sdir_path, sname) = fs.resolve_parent(from)?;
        let ino = fs.existing_child(sdir, &sdir_path, &sname).ok_or(ENOENT)?;
        let (ddir, ddir_path, dname) = fs.resolve_parent(to)?;
        let src_is_dir = fs.node(ino).is_dir();
        if src_is_dir {
            let abs = |dir: &[String], name: &str| {
                let mut p = dir.to_vec();
                p.push(name.to_string());
                join(&p)
            };
            let (src_abs, dst_abs) = (abs(&sdir_path, &sname), abs(&ddir_path, &dname));
            if dst_abs == src_abs {
                return Ok(());
            }
            if dst_abs.starts_with(&format!("{src_abs}/")) {
                return Err(EINVAL);
            }
        }
        if let Some(existing) = fs.existing_child(ddir, &ddir_path, &dname) {
            if existing == ino {
                return Ok(());
            }
            match (src_is_dir, fs.node(existing).is_dir()) {
                (true, false) => return Err(ENOTDIR),
                (false, true) => return Err(EISDIR),
                (true, true) => {
                    fs.list_remote(existing);
                    if matches!(&fs.node(existing).data, Data::Dir { entries, .. } if !entries.is_empty())
                    {
                        return Err(ENOTEMPTY);
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

    fn link(&self, existing: &str, path: &str) -> R<()> {
        let mut fs = self.fs();
        let src = fs.resolve(existing, false)?;
        if fs.node(src.ino).is_dir() {
            return Err(EPERM);
        }
        let (dir, dir_path, name) = fs.resolve_parent(path)?;
        if fs.existing_child(dir, &dir_path, &name).is_some() {
            return Err(EEXIST);
        }
        fs.node_mut(src.ino).nlink += 1;
        fs.link_into(dir, &name, src.ino);
        Ok(())
    }

    fn symlink(&self, target: &str, path: &str, _flags: u32) -> R<()> {
        let mut fs = self.fs();
        let (dir, dir_path, name) = fs.resolve_parent(path)?;
        if fs.existing_child(dir, &dir_path, &name).is_some() {
            return Err(EEXIST);
        }
        fs.create(dir, &name, Data::Link(target.to_string()), 0o777);
        Ok(())
    }

    fn readlink(&self, path: &str) -> R<String> {
        let mut fs = self.fs();
        let r = fs.resolve(path, false)?;
        match &fs.node(r.ino).data {
            Data::Link(t) => Ok(t.clone()),
            _ => Err(EINVAL),
        }
    }

    fn realpath(&self, path: &str) -> R<String> {
        let mut fs = self.fs();
        let r = fs.resolve(path, true)?;
        Ok(join(&r.path))
    }

    /// `mode` is libuv's copyfile flags: `COPYFILE_EXCL` (1) is honoured, `FICLONE_FORCE` (4)
    /// is unsupported.
    fn copy_file(&self, src: &str, dst: &str, mode: u32) -> R<()> {
        if mode & 4 != 0 {
            return Err(ENOSYS);
        }
        let mut fs = self.fs();
        let r = fs.resolve(src, true)?;
        let bytes = fs.read_node(r.ino)?;
        let perm = fs.node(r.ino).mode & 0o7777;
        let flags = O_WRONLY | O_CREAT | O_TRUNC | if mode & 1 != 0 { O_EXCL } else { 0 };
        let fd = MemFs::open_in(&mut fs, dst, flags, perm)?;
        let written = MemFs::write_in(&mut fs, fd, &bytes, None);
        let _ = MemFs::close_in(&mut fs, fd);
        written.map(|_| ())
    }

    fn readdir(&self, path: &str) -> R<Vec<(String, DirentKind)>> {
        let mut fs = self.fs();
        let r = fs.resolve(path, true)?;
        if !fs.node(r.ino).is_dir() {
            return Err(ENOTDIR);
        }
        fs.list_remote(r.ino);
        let Data::Dir { entries, .. } = &fs.node(r.ino).data else {
            unreachable!()
        };
        Ok(entries
            .iter()
            .map(|(name, ino)| (name.clone(), fs.node(*ino).dirent()))
            .collect())
    }

    fn read_file(&self, path: &str, _flags: i32) -> R<Vec<u8>> {
        let mut fs = self.fs();
        let r = fs.resolve(path, true)?;
        let bytes = fs.read_node(r.ino)?;
        fs.node_mut(r.ino).atime = unix_ms();
        Ok(bytes)
    }

    fn write_file(&self, path: &str, data: &[u8], flags: i32, mode: u32) -> R<()> {
        let mut fs = self.fs();
        let fd = MemFs::open_in(&mut fs, path, flags, mode)?;
        let written = MemFs::write_in(&mut fs, fd, data, None);
        let _ = MemFs::close_in(&mut fs, fd);
        written.map(|_| ())
    }

    fn cwd(&self) -> R<String> {
        Ok(self.fs().cwd.clone())
    }

    fn chdir(&self, path: &str) -> R<()> {
        let mut fs = self.fs();
        let r = fs.resolve(path, true)?;
        if !fs.node(r.ino).is_dir() {
            return Err(ENOTDIR);
        }
        fs.cwd = join(&r.path);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_directories_and_links_round_trip() {
        let fs = MemFs::new();
        fs.mkdir("/t1/a/b", 0o755, true).unwrap();
        fs.write_file(
            "/t1/a/b/f.txt",
            b"hello",
            O_WRONLY | O_CREAT | O_TRUNC,
            0o666,
        )
        .unwrap();
        assert_eq!(fs.read_file("/t1/a/b/f.txt", 0).unwrap(), b"hello");
        assert_eq!(fs.stat("/t1/a/b/f.txt", true).unwrap().size, 5);
        fs.symlink("/t1/a/b", "/t1/link", 0).unwrap();
        assert_eq!(fs.read_file("/t1/link/f.txt", 0).unwrap(), b"hello");
        assert_eq!(
            fs.realpath("/t1/a/../a/b/./f.txt").unwrap(),
            "/t1/a/b/f.txt"
        );
        assert_eq!(
            fs.stat("/t1/link", false).unwrap().mode & crate::fs::S_IFMT,
            S_IFLNK
        );
        assert_eq!(
            fs.readdir("/t1/a/b").unwrap(),
            vec![("f.txt".to_string(), DirentKind::File)]
        );
        assert_eq!(fs.rmdir("/t1/a/b"), Err(ENOTEMPTY));
        fs.rename("/t1/a/b/f.txt", "/t1/g.txt").unwrap();
        assert_eq!(fs.read_file("/t1/a/b/f.txt", 0), Err(ENOENT));
        fs.unlink("/t1/g.txt").unwrap();
        assert_eq!(fs.mkdir("/t1/a", 0o755, false), Err(EEXIST));
    }

    #[test]
    fn descriptors_keep_positions_and_append() {
        let fs = MemFs::new();
        let fd = fs.open("/t2.txt", O_RDWR | O_CREAT, 0o644).unwrap();
        assert_eq!(fd, 3);
        assert_eq!(fs.write(fd, b"abcdef", None).unwrap(), 6);
        let mut buf = [0u8; 3];
        assert_eq!(fs.read(fd, &mut buf, Some(2)).unwrap(), 3);
        assert_eq!(&buf, b"cde");
        fs.ftruncate(fd, 3).unwrap();
        fs.close(fd).unwrap();
        let fd = fs.open("/t2.txt", O_WRONLY | O_APPEND, 0).unwrap();
        fs.write(fd, b"XY", None).unwrap();
        fs.close(fd).unwrap();
        assert_eq!(fs.read_file("/t2.txt", 0).unwrap(), b"abcXY");
        assert_eq!(
            fs.open("/t2.txt", O_CREAT | O_EXCL | O_WRONLY, 0o644),
            Err(EEXIST)
        );
    }

    #[test]
    fn static_contents_are_copied_on_write() {
        static DATA: &[u8] = b"embedded";
        let fs = MemFs::with_fd_base(100);
        fs.insert("/lib/pkg/mod.py", DATA, 0o644, 0o755);
        assert_eq!(
            fs.readdir("/lib").unwrap(),
            vec![("pkg".to_string(), DirentKind::Dir)]
        );
        let fd = fs.open("/lib/pkg/mod.py", O_RDWR, 0).unwrap();
        assert_eq!(fd, 100);
        assert_eq!(fs.lseek(fd, 0, 2).unwrap(), 8);
        fs.write(fd, b"!", None).unwrap();
        assert_eq!(fs.read_file("/lib/pkg/mod.py", 0).unwrap(), b"embedded!");
        assert_eq!(DATA, b"embedded");
    }

    struct Remote;
    impl Backend for Remote {
        fn stat(&self, path: &str) -> Option<RemoteStat> {
            match path {
                "/mnt/r/dir" => Some(RemoteStat {
                    is_dir: true,
                    size: 0,
                }),
                "/mnt/r/dir/x.txt" => Some(RemoteStat {
                    is_dir: false,
                    size: 4,
                }),
                _ => None,
            }
        }
        fn read(&self, path: &str) -> Option<Vec<u8>> {
            (path == "/mnt/r/dir/x.txt").then(|| b"data".to_vec())
        }
        fn list(&self, path: &str) -> Option<Vec<RemoteEntry>> {
            (path == "/mnt/r/dir").then(|| {
                vec![RemoteEntry {
                    name: "x.txt".into(),
                    is_dir: false,
                    size: 4,
                }]
            })
        }
    }

    #[test]
    fn mounted_backends_are_discovered_lazily() {
        let fs = MemFs::new();
        fs.mount("/mnt/r", Arc::new(Remote));
        assert_eq!(fs.stat("/mnt/r/dir/x.txt", true).unwrap().size, 4);
        assert_eq!(fs.read_file("/mnt/r/dir/x.txt", 0).unwrap(), b"data");
        assert_eq!(fs.readdir("/mnt/r/dir").unwrap().len(), 1);
        assert_eq!(fs.read_file("/mnt/r/missing", 0), Err(ENOENT));
    }
}
