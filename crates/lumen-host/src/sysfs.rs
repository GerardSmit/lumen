//! The few file-system and process queries the runtime layer makes outside `node:fs`: module
//! loading, config discovery, the working directory. Natively they are `std`; on
//! `wasm32-unknown-unknown`, where `std::fs` always fails, they read the in-memory [`crate::vfs`].

use std::io;
use std::path::{Path, PathBuf};

#[cfg(not(target_arch = "wasm32"))]
pub fn read_to_string(path: impl AsRef<Path>) -> io::Result<String> {
    std::fs::read_to_string(path)
}

#[cfg(target_arch = "wasm32")]
pub fn read_to_string(path: impl AsRef<Path>) -> io::Result<String> {
    let bytes = read(path)?;
    String::from_utf8(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(not(target_arch = "wasm32"))]
pub fn read(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    std::fs::read(path)
}

#[cfg(target_arch = "wasm32")]
pub fn read(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    crate::vfs::read_file(&path.as_ref().to_string_lossy()).map_err(io::Error::from)
}

#[cfg(not(target_arch = "wasm32"))]
pub fn current_dir() -> io::Result<PathBuf> {
    std::env::current_dir()
}

#[cfg(target_arch = "wasm32")]
pub fn current_dir() -> io::Result<PathBuf> {
    Ok(PathBuf::from(crate::vfs::cwd()))
}

#[cfg(not(target_arch = "wasm32"))]
pub fn is_dir(path: impl AsRef<Path>) -> bool {
    path.as_ref().is_dir()
}

#[cfg(target_arch = "wasm32")]
pub fn is_dir(path: impl AsRef<Path>) -> bool {
    crate::vfs::is_dir(&path.as_ref().to_string_lossy())
}

/// Whether `path` exists and is not itself a symbolic link.
#[cfg(not(target_arch = "wasm32"))]
pub fn exists_not_symlink(path: impl AsRef<Path>) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| !m.file_type().is_symlink())
}

#[cfg(target_arch = "wasm32")]
pub fn exists_not_symlink(path: impl AsRef<Path>) -> bool {
    crate::vfs::stat(&path.as_ref().to_string_lossy(), false)
        .is_ok_and(|s| s.kind != crate::vfs::Kind::Symlink)
}

/// The OS process id; `std::process::id` panics where there are no processes.
#[cfg(not(target_arch = "wasm32"))]
pub fn process_id() -> u32 {
    std::process::id()
}

#[cfg(target_arch = "wasm32")]
pub fn process_id() -> u32 {
    1
}

/// `Path::is_file` / `is_dir`, answered from the in-memory file system on wasm.
pub trait PathExt {
    fn fs_is_file(&self) -> bool;
    fn fs_is_dir(&self) -> bool;
}

impl PathExt for Path {
    #[cfg(not(target_arch = "wasm32"))]
    fn fs_is_file(&self) -> bool {
        self.is_file()
    }

    #[cfg(target_arch = "wasm32")]
    fn fs_is_file(&self) -> bool {
        crate::vfs::is_file(&self.to_string_lossy())
    }

    fn fs_is_dir(&self) -> bool {
        is_dir(self)
    }
}

impl PathExt for PathBuf {
    fn fs_is_file(&self) -> bool {
        self.as_path().fs_is_file()
    }

    fn fs_is_dir(&self) -> bool {
        is_dir(self)
    }
}
