//! Memory-mapped files and anonymous memory (`mmap(2)`, `munmap`, `msync`, `madvise`,
//! `mremap`): the region behind Python's `mmap.mmap`, whose bytes are exposed to both runtimes as
//! an external `lumen_common::buffer::ByteStore` kept alive by a [`Mapping`]. Off Unix mapping
//! fails with `ENOSYS`.

use crate::errno::FsError;
use std::cell::Cell;

pub type R<T> = Result<T, FsError>;

/// A mapped region, unmapped on drop. The address and length change only through
/// [`Mapping::remap`], which the owner may call only while nothing borrows the bytes.
pub struct Mapping {
    ptr: Cell<*mut u8>,
    len: Cell<usize>,
}

/// `MAP_PRIVATE`.
#[cfg(unix)]
pub const MAP_PRIVATE: i32 = libc::MAP_PRIVATE;
#[cfg(not(unix))]
pub const MAP_PRIVATE: i32 = 2;

/// Whether [`Mapping::remap`] (Linux `mremap`) exists here.
pub const HAVE_MREMAP: bool = cfg!(any(target_os = "linux", target_os = "android"));

impl Mapping {
    /// Maps `len` bytes of `fd` at `offset` (`fd` of -1: anonymous memory, `MAP_ANON` added to
    /// `flags`).
    pub fn new(len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> R<Mapping> {
        #[cfg(unix)]
        {
            let flags = if fd == -1 { flags | libc::MAP_ANON } else { flags };
            // SAFETY: a fresh mapping at a kernel-chosen address; every argument is validated by
            // the kernel.
            let p = unsafe { libc::mmap(std::ptr::null_mut(), len, prot, flags, fd, offset as libc::off_t) };
            if p == libc::MAP_FAILED {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(Mapping { ptr: Cell::new(p.cast()), len: Cell::new(len) })
        }
        #[cfg(not(unix))]
        {
            let _ = (len, prot, flags, fd, offset);
            Err(FsError("ENOSYS"))
        }
    }

    pub fn ptr(&self) -> *mut u8 {
        self.ptr.get()
    }

    pub fn len(&self) -> usize {
        self.len.get()
    }

    pub fn is_empty(&self) -> bool {
        self.len.get() == 0
    }

    /// `msync(MS_SYNC)` of `len` bytes from `offset`.
    pub fn sync(&self, offset: usize, len: usize) -> R<()> {
        #[cfg(unix)]
        {
            // SAFETY: the caller checked the range against the mapping.
            if unsafe { libc::msync(self.ptr.get().add(offset).cast(), len, libc::MS_SYNC) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = (offset, len);
            Err(FsError("ENOSYS"))
        }
    }

    /// `madvise(2)` of `len` bytes from `offset`.
    pub fn advise(&self, offset: usize, len: usize, advice: i32) -> R<()> {
        #[cfg(unix)]
        {
            // SAFETY: the caller checked the range against the mapping.
            if unsafe { libc::madvise(self.ptr.get().add(offset).cast(), len, advice) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = (offset, len, advice);
            Err(FsError("ENOSYS"))
        }
    }

    /// Resizes the region in place or by moving it (`mremap(MREMAP_MAYMOVE)`); the address may
    /// change. The caller must hold no pointer into the old region.
    pub fn remap(&self, new_len: usize) -> R<()> {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            // SAFETY: the region is ours; the caller holds no pointer into it.
            let p = unsafe { libc::mremap(self.ptr.get().cast(), self.len.get(), new_len, libc::MREMAP_MAYMOVE) };
            if p == libc::MAP_FAILED {
                return Err(std::io::Error::last_os_error().into());
            }
            self.ptr.set(p.cast());
            self.len.set(new_len);
            Ok(())
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            let _ = new_len;
            Err(FsError("ENOSYS"))
        }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        #[cfg(unix)]
        // SAFETY: the region was mapped by `new` or moved by `remap` and is unmapped once.
        unsafe {
            libc::munmap(self.ptr.get().cast(), self.len.get());
        }
    }
}

/// The page size (`mmap.PAGESIZE`; also the allocation granularity on Unix).
pub fn pagesize() -> i64 {
    crate::rlimit::pagesize()
}

/// The `mmap` module's `PROT_*`, `MAP_*` and `MADV_*` constants on this platform.
pub fn constants() -> Vec<(&'static str, i64)> {
    let mut v: Vec<(&'static str, i64)> = Vec::new();
    #[cfg(unix)]
    v.extend([
        ("PROT_READ", libc::PROT_READ as i64),
        ("PROT_WRITE", libc::PROT_WRITE as i64),
        ("PROT_EXEC", libc::PROT_EXEC as i64),
        ("MAP_SHARED", libc::MAP_SHARED as i64),
        ("MAP_PRIVATE", libc::MAP_PRIVATE as i64),
        ("MAP_ANON", libc::MAP_ANON as i64),
        ("MAP_ANONYMOUS", libc::MAP_ANON as i64),
        ("MADV_NORMAL", libc::MADV_NORMAL as i64),
        ("MADV_RANDOM", libc::MADV_RANDOM as i64),
        ("MADV_SEQUENTIAL", libc::MADV_SEQUENTIAL as i64),
        ("MADV_WILLNEED", libc::MADV_WILLNEED as i64),
        ("MADV_DONTNEED", libc::MADV_DONTNEED as i64),
    ]);
    #[cfg(any(target_os = "linux", target_os = "android"))]
    v.extend([
        ("MAP_DENYWRITE", 0x0800),
        ("MAP_EXECUTABLE", 0x1000),
        ("MAP_POPULATE", 0x8000),
        ("MAP_STACK", 0x20000),
        ("MADV_FREE", 8),
        ("MADV_REMOVE", 9),
        ("MADV_DONTFORK", 10),
        ("MADV_DOFORK", 11),
        ("MADV_MERGEABLE", 12),
        ("MADV_UNMERGEABLE", 13),
        ("MADV_HUGEPAGE", 14),
        ("MADV_NOHUGEPAGE", 15),
        ("MADV_DONTDUMP", 16),
        ("MADV_DODUMP", 17),
        ("MADV_HWPOISON", 100),
    ]);
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    v.push(("MADV_FREE", 5));
    v.push(("PAGESIZE", pagesize()));
    v.push(("ALLOCATIONGRANULARITY", pagesize()));
    v
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn anonymous_mapping_is_writable() {
        let m = Mapping::new(4096, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_PRIVATE, -1, 0).unwrap();
        // SAFETY: the mapping is 4096 bytes long and ours.
        unsafe {
            *m.ptr() = 7;
            assert_eq!(*m.ptr(), 7);
        }
        m.sync(0, 4096).ok();
        m.advise(0, 4096, libc::MADV_NORMAL).unwrap();
    }
}
