//! The pure-Python standard library served as the lowest-priority `sys.path` entry through an
//! in-memory file system. By default it is embedded compressed (see `build.rs`) and a chunk is
//! decompressed the first time one of its modules is read; with the `py-stdlib-external` feature
//! it is read from a directory instead.

use crate::platform::{MemFs, MemPlatform};
use lumen_os::vfs::{Backend, RemoteEntry, RemoteStat};
#[cfg(not(feature = "py-stdlib-external"))]
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, OnceLock};

/// Virtual directory that holds the stdlib; `__file__` of a stdlib module is
/// `<FROZEN_DIR>/<relative path>`.
pub const FROZEN_DIR: &str = "/<frozen>/lib";

mod generated {
    include!(concat!(env!("OUT_DIR"), "/frozen_table.rs"));
}

/// The path below [`FROZEN_DIR`] with no leading slash (empty for the directory itself).
fn relative(path: &str) -> Option<&str> {
    path.strip_prefix(FROZEN_DIR)?.strip_prefix('/').or(if path == FROZEN_DIR { Some("") } else { None })
}

#[cfg(not(feature = "py-stdlib-external"))]
struct Archive {
    files: HashMap<&'static str, (u32, u32, u32)>,
    dirs: HashMap<String, BTreeMap<String, bool>>,
    chunks: Vec<OnceLock<Vec<u8>>>,
}

#[cfg(not(feature = "py-stdlib-external"))]
impl Archive {
    fn new() -> Archive {
        let mut files = HashMap::new();
        let mut dirs: HashMap<String, BTreeMap<String, bool>> = HashMap::new();
        dirs.insert(String::new(), BTreeMap::new());
        for &(path, chunk, offset, len) in generated::FILES {
            files.insert(path, (chunk, offset, len));
            let mut dir = path;
            while let Some((parent, name)) = dir.rsplit_once('/') {
                dirs.entry(parent.to_string()).or_default().insert(name.to_string(), dir != path);
                dir = parent;
            }
            dirs.entry(String::new()).or_default().insert(dir.to_string(), dir != path);
        }
        Archive { files, dirs, chunks: generated::CHUNKS.iter().map(|_| OnceLock::new()).collect() }
    }

    fn chunk(&self, index: u32) -> &[u8] {
        self.chunks[index as usize].get_or_init(|| {
            let (offset, packed, len) = generated::CHUNKS[index as usize];
            let packed = &generated::BLOB[offset as usize..(offset + packed) as usize];
            lumen_common::compress::brotli_decompress_limited(packed, len as usize).expect("embedded stdlib chunk is valid")
        })
    }
}

#[cfg(not(feature = "py-stdlib-external"))]
impl Backend for Archive {
    fn stat(&self, path: &str) -> Option<RemoteStat> {
        let rel = relative(path)?;
        if let Some(&(_, _, len)) = self.files.get(rel) {
            return Some(RemoteStat { is_dir: false, size: len as u64 });
        }
        self.dirs.contains_key(rel).then_some(RemoteStat { is_dir: true, size: 0 })
    }

    fn read(&self, path: &str) -> Option<Vec<u8>> {
        let &(chunk, offset, len) = self.files.get(relative(path)?)?;
        Some(self.chunk(chunk)[offset as usize..(offset + len) as usize].to_vec())
    }

    fn list(&self, path: &str) -> Option<Vec<RemoteEntry>> {
        let rel = relative(path)?;
        let entries = self.dirs.get(rel)?;
        Some(
            entries
                .iter()
                .map(|(name, &is_dir)| {
                    let full = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
                    let size = self.files.get(full.as_str()).map_or(0, |f| f.2 as u64);
                    RemoteEntry { name: name.clone(), is_dir, size }
                })
                .collect(),
        )
    }
}

/// The stdlib read from a directory on the host file system.
#[cfg(feature = "py-stdlib-external")]
struct DirBackend {
    root: String,
}

#[cfg(feature = "py-stdlib-external")]
impl DirBackend {
    fn host_path(&self, path: &str) -> Option<String> {
        let rel = relative(path)?;
        (!rel.split('/').any(|part| part == "..")).then(|| if rel.is_empty() { self.root.clone() } else { format!("{}/{rel}", self.root) })
    }
}

#[cfg(feature = "py-stdlib-external")]
impl Backend for DirBackend {
    fn stat(&self, path: &str) -> Option<RemoteStat> {
        use lumen_os::fs::{S_IFDIR, S_IFMT};
        use lumen_os::vfs::{FileSystem, OsFs};
        let st = OsFs.stat(&self.host_path(path)?, true).ok()?;
        Some(RemoteStat { is_dir: st.mode & S_IFMT == S_IFDIR, size: st.size })
    }

    fn read(&self, path: &str) -> Option<Vec<u8>> {
        use lumen_os::vfs::{FileSystem, OsFs};
        OsFs.read_file(&self.host_path(path)?, 0).ok()
    }

    fn list(&self, path: &str) -> Option<Vec<RemoteEntry>> {
        use lumen_os::vfs::{FileSystem, OsFs};
        let host = self.host_path(path)?;
        let entries = OsFs.readdir(&host).ok()?;
        Some(
            entries
                .into_iter()
                .filter_map(|(name, _)| {
                    let st = self.stat(&format!("{}/{name}", path.trim_end_matches('/')))?;
                    Some(RemoteEntry { name, is_dir: st.is_dir, size: st.size })
                })
                .collect(),
        )
    }
}

#[cfg(feature = "py-stdlib-external")]
static EXTERNAL_DIR: OnceLock<String> = OnceLock::new();

/// Sets the directory the stdlib is read from (feature `py-stdlib-external`); call before the
/// first interpreter is created. Without it, `LUMEN_PY_STDLIB` is used, then the vendored `lib/`.
#[cfg(feature = "py-stdlib-external")]
pub fn set_external_dir(dir: impl Into<String>) {
    let _ = EXTERNAL_DIR.set(dir.into());
}

#[cfg(feature = "py-stdlib-external")]
fn backend() -> Arc<dyn Backend> {
    let root = EXTERNAL_DIR
        .get()
        .cloned()
        .or_else(|| std::env::var("LUMEN_PY_STDLIB").ok())
        .unwrap_or_else(|| generated::EXTERNAL_DIR.to_string());
    Arc::new(DirBackend { root: root.trim_end_matches('/').to_string() })
}

#[cfg(not(feature = "py-stdlib-external"))]
fn backend() -> Arc<dyn Backend> {
    Arc::new(Archive::new())
}

pub fn fs() -> MemFs {
    let fs = MemPlatform::bundle();
    fs.mount(FROZEN_DIR, backend());
    fs
}
