//! File-system primitives with libuv's semantics (open/read/write/stat/readdir/mkdir -p/...),
//! over `std::fs`. Errors carry the libuv code as [`FsError`].
//!
//! File descriptors live in a process-wide table so an operation on a worker thread can reach
//! them. On Unix the key is the OS descriptor itself (so an fd is usable by anything else that
//! takes one); on Windows it is a small integer handed out from 3 up, lowest free first, as the
//! CRT would. fds 0-2 are the process's standard streams: `read`/`write` refuse them (the
//! embedder routes those), `fstat` answers for the real handle.

use std::collections::{BTreeSet, HashMap};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::errno::FsError;

pub type R<T> = Result<T, FsError>;

// ---- the descriptor table -----------------------------------------------------------------------

struct Entry {
    file: File,
    /// Serializes a Windows positional read/write with its restore of the file pointer.
    #[cfg_attr(not(windows), allow(dead_code))]
    lock: Mutex<()>,
}

#[derive(Default)]
struct Table {
    map: HashMap<i32, Arc<Entry>>,
    #[cfg_attr(unix, allow(dead_code))]
    free: BTreeSet<i32>,
    #[cfg_attr(unix, allow(dead_code))]
    next: i32,
}

fn table() -> &'static Mutex<Table> {
    static T: OnceLock<Mutex<Table>> = OnceLock::new();
    T.get_or_init(|| {
        Mutex::new(Table {
            next: 3,
            ..Table::default()
        })
    })
}

fn lock_table() -> std::sync::MutexGuard<'static, Table> {
    table().lock().unwrap_or_else(|p| p.into_inner())
}

fn insert(file: File) -> i32 {
    let entry = Arc::new(Entry {
        file,
        lock: Mutex::new(()),
    });
    let mut t = lock_table();
    #[cfg(unix)]
    let fd = {
        use std::os::unix::io::AsRawFd;
        entry.file.as_raw_fd()
    };
    #[cfg(not(unix))]
    let fd = match t.free.pop_first() {
        Some(fd) => fd,
        None => {
            t.next += 1;
            t.next - 1
        }
    };
    t.map.insert(fd, entry);
    fd
}

/// Whether `fd` is one of the standard streams (0-2).
#[inline]
pub fn is_std(fd: i32) -> bool {
    (0..=2).contains(&fd)
}

/// Whether `fd` was handed out by [`open`] and not yet closed.
pub fn is_open(fd: i32) -> bool {
    lock_table().map.contains_key(&fd)
}

fn get(fd: i32) -> R<Arc<Entry>> {
    if let Some(entry) = lock_table().map.get(&fd).cloned() {
        return Ok(entry);
    }
    #[cfg(unix)]
    {
        // Native addons may return descriptors opened outside our table. Duplicate through
        // the kernel rather than assuming ownership of the caller's descriptor. The
        // temporary entry closes only the duplicate and retains the same open-file offset.
        if is_std(fd) {
            return Err(FsError("EBADF"));
        }
        // Atomic CLOEXEC prevents leaking this temporary fd into a concurrently spawned
        // child. Invalid or already closed descriptors are rejected by the kernel.
        let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
        if duplicate < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        use std::os::unix::io::FromRawFd;
        // SAFETY: dup returned a fresh owned descriptor; only this File closes it.
        let file = unsafe { File::from_raw_fd(duplicate) };
        Ok(Arc::new(Entry {
            file,
            lock: Mutex::new(()),
        }))
    }
    #[cfg(not(unix))]
    Err(FsError("EBADF"))
}

/// Close a descriptor from [`open`]; on Unix also a native descriptor opened elsewhere.
pub fn close(fd: i32) -> R<()> {
    let mut t = lock_table();
    match t.map.remove(&fd) {
        Some(_entry) => {
            #[cfg(not(unix))]
            t.free.insert(fd);
            Ok(())
        }
        None => {
            #[cfg(unix)]
            {
                if unsafe { libc::close(fd) } == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error().into())
                }
            }
            #[cfg(not(unix))]
            Err(FsError("EBADF"))
        }
    }
}

// ---- open flags ---------------------------------------------------------------------------------

/// The `O_*` values of the platform whose numbers Node (and `os.O_*` in Python) expose.
pub mod flags {
    #[cfg(windows)]
    mod v {
        pub const O_WRONLY: i32 = 1;
        pub const O_RDWR: i32 = 2;
        pub const O_APPEND: i32 = 8;
        pub const O_CREAT: i32 = 256;
        pub const O_TRUNC: i32 = 512;
        pub const O_EXCL: i32 = 1024;
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    mod v {
        pub const O_WRONLY: i32 = 1;
        pub const O_RDWR: i32 = 2;
        pub const O_CREAT: i32 = 0o100;
        pub const O_EXCL: i32 = 0o200;
        pub const O_TRUNC: i32 = 0o1000;
        pub const O_APPEND: i32 = 0o2000;
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "android")))]
    mod v {
        pub const O_WRONLY: i32 = 1;
        pub const O_RDWR: i32 = 2;
        pub const O_APPEND: i32 = 8;
        pub const O_CREAT: i32 = 0x200;
        pub const O_TRUNC: i32 = 0x400;
        pub const O_EXCL: i32 = 0x800;
    }
    pub use v::*;
}
use flags::*;

#[cfg(windows)]
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
#[cfg(windows)]
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
#[cfg(windows)]
const FILE_ATTRIBUTE_READONLY: u32 = 1;

fn open_options(fl: i32, mode: u32) -> OpenOptions {
    let acc = fl & 3;
    let mut o = OpenOptions::new();
    o.read(acc != O_WRONLY);
    o.write(acc == O_WRONLY || acc == O_RDWR);
    if fl & O_APPEND != 0 {
        o.append(true);
    }
    let creat = fl & O_CREAT != 0;
    if creat && fl & O_EXCL != 0 {
        o.create_new(true);
    } else if creat {
        o.create(true);
    }
    if fl & O_TRUNC != 0 {
        o.truncate(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(mode);
        o.custom_flags(fl & !(3 | O_APPEND | O_CREAT | O_EXCL | O_TRUNC));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // As libuv: directories open (reads on them fail with EISDIR), and a file created
        // without the owner-write bit is created read-only.
        o.custom_flags(FILE_FLAG_BACKUP_SEMANTICS);
        if creat && mode & 0o200 == 0 {
            o.attributes(FILE_ATTRIBUTE_READONLY);
        }
    }
    #[cfg(not(any(unix, windows)))]
    let _ = mode;
    o
}

fn open_file(path: &str, fl: i32, mode: u32) -> R<File> {
    match open_options(fl, mode).open(path) {
        Ok(f) => Ok(f),
        Err(e) => {
            // Opening a directory for writing: libuv reports EISDIR.
            let is_dir = std::fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false);
            if is_dir && (fl & 3 != 0) {
                return Err(FsError("EISDIR"));
            }
            Err(e.into())
        }
    }
}

/// `open(path, O_* flags, mode)`: a descriptor registered in the table.
pub fn open(path: &str, fl: i32, mode: u32) -> R<i32> {
    Ok(insert(open_file(path, fl, mode)?))
}

// ---- read / write -------------------------------------------------------------------------------

/// End of file on a pipe or at the end of a file reads as 0 bytes, like `read(2)`.
fn eof_ok(r: std::io::Result<usize>) -> R<usize> {
    match r {
        Ok(n) => Ok(n),
        #[cfg(windows)]
        Err(e) if matches!(e.raw_os_error(), Some(38) | Some(109)) => Ok(0),
        Err(e) => Err(e.into()),
    }
}

fn read_entry_raw(e: &Entry, buf: &mut [u8], pos: Option<u64>) -> R<usize> {
    let Some(pos) = pos else {
        return eof_ok((&e.file).read(buf));
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        eof_ok(e.file.read_at(buf, pos))
    }
    #[cfg(windows)]
    {
        use std::io::{Seek, SeekFrom};
        use std::os::windows::fs::FileExt;
        // A positional read moves the Windows file pointer; libuv puts it back.
        let _g = e.lock.lock().unwrap_or_else(|p| p.into_inner());
        let cur = (&e.file).stream_position();
        let r = eof_ok(e.file.seek_read(buf, pos));
        if let Ok(cur) = cur {
            let _ = (&e.file).seek(SeekFrom::Start(cur));
        }
        r
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pos;
        Err(FsError("ENOSYS"))
    }
}

fn write_entry_raw(e: &Entry, data: &[u8], pos: Option<u64>) -> R<usize> {
    let Some(pos) = pos else {
        return Ok((&e.file).write(data)?);
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        Ok(e.file.write_at(data, pos)?)
    }
    #[cfg(windows)]
    {
        use std::io::{Seek, SeekFrom};
        use std::os::windows::fs::FileExt;
        let _g = e.lock.lock().unwrap_or_else(|p| p.into_inner());
        let cur = (&e.file).stream_position();
        let r = e.file.seek_write(data, pos);
        if let Ok(cur) = cur {
            let _ = (&e.file).seek(SeekFrom::Start(cur));
        }
        Ok(r?)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pos;
        Err(FsError("ENOSYS"))
    }
}

/// libuv reports a read or write the handle's access mode forbids (ERROR_ACCESS_DENIED on Windows)
/// as EBADF, as Unix does.
fn access_to_ebadf(e: FsError) -> FsError {
    if cfg!(windows) && (e.0 == "EPERM" || e.0 == "EACCES") {
        FsError("EBADF")
    } else {
        e
    }
}

/// Read into `buf` at `pos` (`None`: the current file position). 0 means end of file.
pub fn read(fd: i32, buf: &mut [u8], pos: Option<u64>) -> R<usize> {
    read_entry_raw(&*get(fd)?, buf, pos).map_err(access_to_ebadf)
}

/// Write `data` at `pos` (`None`: the current file position, or the end when opened for append).
pub fn write(fd: i32, data: &[u8], pos: Option<u64>) -> R<usize> {
    write_entry_raw(&*get(fd)?, data, pos).map_err(access_to_ebadf)
}

// ---- stat ---------------------------------------------------------------------------------------

/// Seconds and nanoseconds since the epoch, the nanoseconds always in `0..1_000_000_000` (a time
/// before the epoch is a negative `sec` and a positive `nsec`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Timespec {
    pub sec: i64,
    pub nsec: u32,
}

impl Timespec {
    pub fn from_secs_f64(t: f64) -> Timespec {
        if !t.is_finite() {
            return Timespec::default();
        }
        let sec = t.floor();
        let nsec = (((t - sec) * 1e9).round() as u32).min(999_999_999);
        Timespec { sec: sec as i64, nsec }
    }

    pub fn as_secs_f64(self) -> f64 {
        self.sec as f64 + self.nsec as f64 / 1e9
    }

    #[cfg_attr(windows, allow(dead_code))]
    fn from_system_time(t: Option<SystemTime>) -> Timespec {
        let Some(t) = t else { return Timespec::default() };
        match t.duration_since(UNIX_EPOCH) {
            Ok(d) => Timespec { sec: d.as_secs() as i64, nsec: d.subsec_nanos() },
            Err(e) => {
                let d = e.duration();
                if d.subsec_nanos() == 0 {
                    Timespec { sec: -(d.as_secs() as i64), nsec: 0 }
                } else {
                    Timespec { sec: -(d.as_secs() as i64) - 1, nsec: 1_000_000_000 - d.subsec_nanos() }
                }
            }
        }
    }

    fn to_system_time(self) -> SystemTime {
        if self.sec >= 0 {
            UNIX_EPOCH + Duration::new(self.sec as u64, self.nsec)
        } else if self.nsec == 0 {
            UNIX_EPOCH - Duration::from_secs(self.sec.unsigned_abs())
        } else {
            UNIX_EPOCH - Duration::new(self.sec.unsigned_abs() - 1, 1_000_000_000 - self.nsec)
        }
    }
}

/// What `stat(2)` reports; `mode` includes the file-type bits (`S_IFMT`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Stat {
    pub dev: u64,
    pub mode: u32,
    pub nlink: u64,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u64,
    pub blksize: u64,
    pub ino: u64,
    pub size: u64,
    pub blocks: u64,
    pub atime: Timespec,
    pub mtime: Timespec,
    pub ctime: Timespec,
    pub birthtime: Timespec,
}

pub const S_IFMT: u32 = 0o170000;
pub const S_IFREG: u32 = 0o100000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFLNK: u32 = 0o120000;
#[cfg(windows)]
const S_IFCHR: u32 = 0o020000;
#[cfg(windows)]
const S_IFIFO: u32 = 0o010000;

#[cfg(unix)]
fn stat_of(m: &std::fs::Metadata) -> Stat {
    use std::os::unix::fs::MetadataExt;
    Stat {
        dev: m.dev(),
        mode: m.mode(),
        nlink: m.nlink(),
        uid: m.uid(),
        gid: m.gid(),
        rdev: m.rdev(),
        blksize: m.blksize(),
        ino: m.ino(),
        size: m.size(),
        blocks: m.blocks(),
        atime: Timespec { sec: m.atime(), nsec: m.atime_nsec() as u32 },
        mtime: Timespec { sec: m.mtime(), nsec: m.mtime_nsec() as u32 },
        ctime: Timespec { sec: m.ctime(), nsec: m.ctime_nsec() as u32 },
        birthtime: Timespec::from_system_time(m.created().ok()),
    }
}

#[cfg(not(any(unix, windows)))]
fn stat_of(m: &std::fs::Metadata) -> Stat {
    Stat {
        mode: if m.is_dir() { S_IFDIR } else { S_IFREG },
        nlink: 1,
        size: m.len(),
        mtime: Timespec::from_system_time(m.modified().ok()),
        atime: Timespec::from_system_time(m.accessed().ok()),
        ..Stat::default()
    }
}

#[cfg(windows)]
mod win {
    #![allow(non_snake_case, clippy::upper_case_acronyms)]
    use std::ffi::c_void;
    pub type HANDLE = *mut c_void;

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    pub struct FILETIME {
        pub lo: u32,
        pub hi: u32,
    }
    #[repr(C)]
    #[derive(Default)]
    pub struct ByHandleInfo {
        pub attrs: u32,
        pub creation: FILETIME,
        pub access: FILETIME,
        pub write: FILETIME,
        pub volume_serial: u32,
        pub size_hi: u32,
        pub size_lo: u32,
        pub nlinks: u32,
        pub index_hi: u32,
        pub index_lo: u32,
    }
    #[repr(C)]
    #[derive(Default)]
    pub struct FileBasicInfo {
        pub creation: i64,
        pub access: i64,
        pub write: i64,
        pub change: i64,
        pub attrs: u32,
    }
    #[repr(C)]
    #[derive(Default)]
    pub struct FileStandardInfo {
        pub alloc: i64,
        pub eof: i64,
        pub nlinks: u32,
        pub delete_pending: u8,
        pub directory: u8,
    }
    #[link(name = "kernel32")]
    extern "system" {
        pub fn GetFileInformationByHandle(h: HANDLE, info: *mut ByHandleInfo) -> i32;
        pub fn GetFileInformationByHandleEx(h: HANDLE, class: i32, info: *mut c_void, size: u32) -> i32;
        pub fn GetFileType(h: HANDLE) -> u32;
        pub fn GetVolumePathNameW(path: *const u16, out: *mut u16, len: u32) -> i32;
        pub fn GetDiskFreeSpaceW(
            root: *const u16,
            sectors_per_cluster: *mut u32,
            bytes_per_sector: *mut u32,
            free_clusters: *mut u32,
            total_clusters: *mut u32,
        ) -> i32;
        pub fn DeviceIoControl(
            h: HANDLE,
            code: u32,
            inbuf: *const c_void,
            inlen: u32,
            outbuf: *mut c_void,
            outlen: u32,
            returned: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
    }

    pub fn wide(s: &str) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        std::ffi::OsStr::new(s).encode_wide().chain(Some(0)).collect()
    }
}

/// A Windows FILETIME-style count (100 ns since 1601) as Unix seconds and nanoseconds.
#[cfg(windows)]
fn filetime(t: i64) -> Timespec {
    let unix = t - 116_444_736_000_000_000;
    Timespec { sec: unix.div_euclid(10_000_000), nsec: (unix.rem_euclid(10_000_000) * 100) as u32 }
}

/// libuv's `fs__stat_handle`: stat by handle, as the CRT never could (ino, dev, nlink, ctime).
#[cfg(windows)]
fn handle_stat(f: &File) -> R<Stat> {
    use std::os::windows::io::AsRawHandle;
    let h = f.as_raw_handle() as win::HANDLE;
    // SAFETY: `h` is a live handle owned by `f`; each out-struct is sized for its info class.
    unsafe {
        let kind = win::GetFileType(h);
        if kind == 2 || kind == 3 {
            // A console or a pipe: libuv reports a character device / FIFO with no times.
            let ty = if kind == 2 { S_IFCHR } else { S_IFIFO };
            return Ok(Stat { mode: ty + 0o666, nlink: 1, blksize: 4096, ..Stat::default() });
        }
        let mut bh = win::ByHandleInfo::default();
        if win::GetFileInformationByHandle(h, &mut bh) == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut basic = win::FileBasicInfo::default();
        if win::GetFileInformationByHandleEx(
            h,
            0,
            &mut basic as *mut _ as *mut std::ffi::c_void,
            std::mem::size_of::<win::FileBasicInfo>() as u32,
        ) == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut stdi = win::FileStandardInfo::default();
        let _ = win::GetFileInformationByHandleEx(
            h,
            1,
            &mut stdi as *mut _ as *mut std::ffi::c_void,
            std::mem::size_of::<win::FileStandardInfo>() as u32,
        );
        let dir = bh.attrs & 0x10 != 0;
        let perm = if bh.attrs & 1 != 0 { 0o444 } else { 0o666 };
        Ok(Stat {
            dev: bh.volume_serial as u64,
            mode: if dir { S_IFDIR } else { S_IFREG } + perm,
            nlink: bh.nlinks as u64,
            blksize: 4096,
            ino: (bh.index_hi as u64) << 32 | bh.index_lo as u64,
            size: (bh.size_hi as u64) << 32 | bh.size_lo as u64,
            blocks: (stdi.alloc >> 9) as u64,
            atime: filetime(basic.access),
            mtime: filetime(basic.write),
            ctime: filetime(basic.change),
            birthtime: filetime(basic.creation),
            ..Stat::default()
        })
    }
}

/// `stat` (`follow`) or `lstat` of a path.
#[cfg(windows)]
pub fn stat(path: &str, follow: bool) -> R<Stat> {
    use std::os::windows::fs::OpenOptionsExt;
    let is_link = !follow
        && std::fs::symlink_metadata(path)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
    let mut flags = FILE_FLAG_BACKUP_SEMANTICS;
    if is_link {
        flags |= FILE_FLAG_OPEN_REPARSE_POINT;
    }
    let f = OpenOptions::new()
        .access_mode(0x80) // FILE_READ_ATTRIBUTES
        .share_mode(7)
        .custom_flags(flags)
        .open(path)?;
    let mut s = handle_stat(&f)?;
    if is_link {
        // lstat of a link: S_IFLNK, and the size is the target's length (libuv).
        s.mode = S_IFLNK + 0o666;
        s.size = std::fs::read_link(path)
            .map(|t| strip_verbatim(&t.to_string_lossy()).len() as u64)
            .unwrap_or(0);
    }
    Ok(s)
}

/// `stat` (`follow`) or `lstat` of a path.
#[cfg(not(windows))]
pub fn stat(path: &str, follow: bool) -> R<Stat> {
    let m = if follow {
        std::fs::metadata(path)?
    } else {
        std::fs::symlink_metadata(path)?
    };
    Ok(stat_of(&m))
}

fn fstat_file(f: &File) -> R<Stat> {
    #[cfg(windows)]
    {
        handle_stat(f)
    }
    #[cfg(not(windows))]
    {
        Ok(stat_of(&f.metadata()?))
    }
}

/// Run `op` on a borrowed standard stream as a `File` (for fstat of fd 0-2).
fn with_std_file<T>(fd: i32, op: impl FnOnce(&File) -> R<T>) -> R<T> {
    #[cfg(unix)]
    {
        use std::os::unix::io::FromRawFd;
        // SAFETY: fd 0-2 stay open for the process; ManuallyDrop never closes them.
        let f = std::mem::ManuallyDrop::new(unsafe { File::from_raw_fd(fd) });
        op(&f)
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::{AsRawHandle, FromRawHandle};
        let h = match fd {
            0 => std::io::stdin().as_raw_handle(),
            1 => std::io::stdout().as_raw_handle(),
            _ => std::io::stderr().as_raw_handle(),
        };
        if h.is_null() {
            return Err(FsError("EBADF"));
        }
        // SAFETY: the std handle outlives the call; ManuallyDrop never closes it.
        let f = std::mem::ManuallyDrop::new(unsafe { File::from_raw_handle(h) });
        op(&f)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (fd, op);
        Err(FsError("EBADF"))
    }
}

/// `fstat`; descriptors 0-2 not opened through [`open`] answer for the process's standard streams.
pub fn fstat(fd: i32) -> R<Stat> {
    if is_std(fd) && !is_open(fd) {
        return with_std_file(fd, fstat_file);
    }
    fstat_file(&get(fd)?.file)
}

/// `statvfs` numbers; `blocks`/`bfree`/`bavail` are in units of `bsize`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StatFs {
    pub bsize: u64,
    pub blocks: u64,
    pub bfree: u64,
    pub bavail: u64,
    pub files: u64,
    pub ffree: u64,
}

#[allow(clippy::unnecessary_cast)] // the statvfs field widths differ per platform
pub fn statfs(path: &str) -> R<StatFs> {
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "android"))]
    {
        let c = cpath(path)?;
        let mut buf: std::mem::MaybeUninit<libc::statvfs> = std::mem::MaybeUninit::zeroed();
        // SAFETY: NUL-terminated path and a zeroed out-struct of the platform's `statvfs`.
        if unsafe { libc::statvfs(c.as_ptr(), buf.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: statvfs succeeded and filled the struct.
        let s = unsafe { buf.assume_init() };
        Ok(StatFs {
            bsize: s.f_bsize as u64,
            blocks: s.f_blocks as u64,
            bfree: s.f_bfree as u64,
            bavail: s.f_bavail as u64,
            files: s.f_files as u64,
            ffree: s.f_ffree as u64,
        })
    }
    #[cfg(windows)]
    {
        std::fs::metadata(path)?;
        let wpath = win::wide(path);
        let mut root = vec![0u16; 1024];
        let (mut spc, mut bps, mut free, mut total) = (0u32, 0u32, 0u32, 0u32);
        // SAFETY: NUL-terminated input, `root` sized as passed; the out-params are plain u32s.
        unsafe {
            if win::GetVolumePathNameW(wpath.as_ptr(), root.as_mut_ptr(), root.len() as u32) == 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            if win::GetDiskFreeSpaceW(root.as_ptr(), &mut spc, &mut bps, &mut free, &mut total) == 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        Ok(StatFs {
            bsize: spc as u64 * bps as u64,
            blocks: total as u64,
            bfree: free as u64,
            bavail: free as u64,
            files: 0,
            ffree: 0,
        })
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "android", windows)))]
    {
        let _ = path;
        Err(FsError("ENOSYS"))
    }
}

// ---- paths --------------------------------------------------------------------------------------

/// `\\?\C:\x` -> `C:\x`, `\\?\UNC\h\s` -> `\\h\s` (what libuv hands back from readlink/realpath).
fn strip_verbatim(s: &str) -> String {
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    if let Some(rest) = s.strip_prefix(r"\\?\").or_else(|| s.strip_prefix(r"\??\")) {
        return rest.to_string();
    }
    s.to_string()
}

#[cfg(unix)]
fn cpath(p: &str) -> R<std::ffi::CString> {
    std::ffi::CString::new(p).map_err(|_| FsError("EINVAL"))
}

fn file_times(atime: Timespec, mtime: Timespec) -> std::fs::FileTimes {
    std::fs::FileTimes::new()
        .set_accessed(atime.to_system_time())
        .set_modified(mtime.to_system_time())
}

/// Set access and modification times of a path (`follow`: through a final symlink).
pub fn utimes(path: &str, atime: Timespec, mtime: Timespec, follow: bool) -> R<()> {
    #[cfg(unix)]
    {
        let ts = |t: Timespec| libc::timespec {
            tv_sec: t.sec as _,
            tv_nsec: t.nsec as _,
        };
        let times = [ts(atime), ts(mtime)];
        let c = cpath(path)?;
        let flags = if follow { 0 } else { libc::AT_SYMLINK_NOFOLLOW };
        // SAFETY: `c` is NUL-terminated and `times` holds the two timespecs utimensat reads.
        if unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), flags) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        let mut flags = FILE_FLAG_BACKUP_SEMANTICS;
        if !follow {
            flags |= FILE_FLAG_OPEN_REPARSE_POINT;
        }
        let f = OpenOptions::new()
            .access_mode(0x100) // FILE_WRITE_ATTRIBUTES
            .share_mode(7)
            .custom_flags(flags)
            .open(path)?;
        f.set_times(file_times(atime, mtime))?;
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (path, atime, mtime, follow);
        Err(FsError("ENOSYS"))
    }
}

pub fn futimes(fd: i32, atime: Timespec, mtime: Timespec) -> R<()> {
    Ok(get(fd)?.file.set_times(file_times(atime, mtime))?)
}

/// `access(2)` with `mode` bits `F_OK`=0, `X_OK`=1, `W_OK`=2, `R_OK`=4.
pub fn access(path: &str, mode: u32) -> R<()> {
    #[cfg(unix)]
    {
        let c = cpath(path)?;
        // SAFETY: `c` is NUL-terminated.
        if unsafe { libc::access(c.as_ptr(), mode as i32) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        // libuv: existence, plus W_OK fails on a read-only file.
        let m = std::fs::metadata(path)?;
        if mode & 2 != 0 && m.permissions().readonly() && !m.is_dir() {
            return Err(FsError("EPERM"));
        }
        Ok(())
    }
}

/// Whether the path exists (following symlinks).
pub fn exists(path: &str) -> bool {
    std::fs::metadata(path).is_ok()
}

pub fn chmod(path: &str, mode: u32) -> R<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    {
        let mut p = std::fs::metadata(path)?.permissions();
        p.set_readonly(mode & 0o200 == 0);
        std::fs::set_permissions(path, p)?;
    }
    Ok(())
}

pub fn fchmod(fd: i32, mode: u32) -> R<()> {
    let e = get(fd)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        e.file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    {
        let mut p = e.file.metadata()?.permissions();
        p.set_readonly(mode & 0o200 == 0);
        e.file.set_permissions(p)?;
    }
    Ok(())
}

pub fn chown(path: &str, uid: u32, gid: u32, follow: bool) -> R<()> {
    #[cfg(unix)]
    {
        if follow {
            std::os::unix::fs::chown(path, Some(uid), Some(gid))?;
        } else {
            std::os::unix::fs::lchown(path, Some(uid), Some(gid))?;
        }
    }
    #[cfg(not(unix))]
    {
        // libuv's chown is a no-op on Windows, once the path is known to exist.
        let _ = (uid, gid);
        if follow {
            std::fs::metadata(path)?;
        } else {
            std::fs::symlink_metadata(path)?;
        }
    }
    Ok(())
}

pub fn fchown(fd: i32, uid: u32, gid: u32) -> R<()> {
    let e = get(fd)?;
    #[cfg(unix)]
    std::os::unix::fs::fchown(&e.file, Some(uid), Some(gid))?;
    #[cfg(not(unix))]
    let _ = (e, uid, gid);
    Ok(())
}

pub fn ftruncate(fd: i32, len: u64) -> R<()> {
    Ok(get(fd)?.file.set_len(len)?)
}

pub fn fsync(fd: i32, data_only: bool) -> R<()> {
    let e = get(fd)?;
    if data_only {
        e.file.sync_data()?;
    } else {
        e.file.sync_all()?;
    }
    Ok(())
}

// ---- directories --------------------------------------------------------------------------------

fn mkdir_one(path: &str, mode: u32) -> std::io::Result<()> {
    #[allow(unused_mut)]
    let mut b = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    b.create(path)
}

/// `mkdir(path, mode)`, or with `recursive` every missing ancestor too: the first directory
/// created (`None` when nothing was created, or non-recursive).
pub fn mkdir(path: &str, mode: u32, recursive: bool) -> R<Option<String>> {
    if !recursive {
        mkdir_one(path, mode)?;
        return Ok(None);
    }
    let p = std::path::Path::new(path);
    if let Ok(m) = std::fs::metadata(p) {
        return if m.is_dir() {
            Ok(None)
        } else {
            Err(FsError("EEXIST"))
        };
    }
    // Walk up to the deepest existing ancestor; it must be a directory.
    let mut missing = vec![p.to_path_buf()];
    let mut cur = p.parent();
    while let Some(dir) = cur {
        if dir.as_os_str().is_empty() {
            break;
        }
        match std::fs::metadata(dir) {
            Ok(m) if m.is_dir() => break,
            Ok(_) => return Err(FsError("ENOTDIR")),
            Err(_) => missing.push(dir.to_path_buf()),
        }
        cur = dir.parent();
    }
    let first = missing.last().map(|d| d.to_string_lossy().into_owned());
    for dir in missing.iter().rev() {
        match mkdir_one(&dir.to_string_lossy(), mode) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && dir.is_dir() => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(first)
}

pub fn unlink(path: &str) -> R<()> {
    #[cfg(windows)]
    {
        if let Ok(m) = std::fs::symlink_metadata(path) {
            // A directory symlink or junction is removed like a directory (libuv).
            if m.file_type().is_symlink() && m.is_dir() {
                return Ok(std::fs::remove_dir(path)?);
            }
            if m.is_dir() {
                return Err(FsError("EPERM"));
            }
        }
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.raw_os_error() == Some(5) => {
                // libuv clears the read-only attribute and retries.
                let m = std::fs::metadata(path)?;
                if !m.permissions().readonly() {
                    return Err(e.into());
                }
                let mut p = m.permissions();
                p.set_readonly(false);
                std::fs::set_permissions(path, p)?;
                Ok(std::fs::remove_file(path)?)
            }
            Err(e) => Err(e.into()),
        }
    }
    #[cfg(not(windows))]
    {
        Ok(std::fs::remove_file(path)?)
    }
}

pub fn rmdir(path: &str) -> R<()> {
    match std::fs::remove_dir(path) {
        Ok(()) => Ok(()),
        #[cfg(not(windows))]
        Err(e) => Err(e.into()),
        #[cfg(windows)]
        Err(e) => {
            if e.raw_os_error() == Some(267) || e.raw_os_error() == Some(5) {
                // A file: libuv maps ERROR_DIRECTORY to ENOENT (Node reports that on Windows).
                if let Ok(m) = std::fs::symlink_metadata(path) {
                    if !m.is_dir() {
                        return Err(FsError("ENOENT"));
                    }
                }
            }
            Err(e.into())
        }
    }
}

pub fn rename(from: &str, to: &str) -> R<()> {
    Ok(std::fs::rename(from, to)?)
}

/// A hard link `path` to `existing`.
pub fn link(existing: &str, path: &str) -> R<()> {
    Ok(std::fs::hard_link(existing, path)?)
}

/// The kind of a directory entry; the discriminants are libuv's `UV_DIRENT_*` values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum DirentKind {
    Unknown = 0,
    File = 1,
    Dir = 2,
    Link = 3,
    Fifo = 4,
    Socket = 5,
    Char = 6,
    Block = 7,
}

/// The entries of a directory (without `.` and `..`) in OS order, with their kinds.
pub fn readdir(path: &str) -> R<Vec<(String, DirentKind)>> {
    let rd = match std::fs::read_dir(path) {
        Ok(rd) => rd,
        Err(e) => {
            if std::fs::metadata(path).map(|m| !m.is_dir()).unwrap_or(false) {
                return Err(FsError("ENOTDIR"));
            }
            return Err(e.into());
        }
    };
    let mut out = Vec::new();
    for entry in rd {
        let entry = entry?;
        let kind = match entry.file_type() {
            Ok(ft) if ft.is_symlink() => DirentKind::Link,
            Ok(ft) if ft.is_dir() => DirentKind::Dir,
            Ok(ft) if ft.is_file() => DirentKind::File,
            #[cfg(unix)]
            Ok(ft) => {
                use std::os::unix::fs::FileTypeExt;
                if ft.is_fifo() {
                    DirentKind::Fifo
                } else if ft.is_socket() {
                    DirentKind::Socket
                } else if ft.is_char_device() {
                    DirentKind::Char
                } else if ft.is_block_device() {
                    DirentKind::Block
                } else {
                    DirentKind::Unknown
                }
            }
            _ => DirentKind::Unknown,
        };
        out.push((entry.file_name().to_string_lossy().into_owned(), kind));
    }
    Ok(out)
}

/// Create a symlink at `path` pointing at `target`. `flags` are libuv's `UV_FS_SYMLINK_*`
/// (1 = directory, 2 = junction); they only matter on Windows.
pub fn symlink(target: &str, path: &str, flags: u32) -> R<()> {
    #[cfg(unix)]
    {
        let _ = flags;
        Ok(std::os::unix::fs::symlink(target, path)?)
    }
    #[cfg(windows)]
    {
        if flags & 2 != 0 {
            return junction(target, path);
        }
        if flags & 1 != 0 {
            Ok(std::os::windows::fs::symlink_dir(target, path)?)
        } else {
            Ok(std::os::windows::fs::symlink_file(target, path)?)
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (target, path, flags);
        Err(FsError("ENOSYS"))
    }
}

/// A directory junction (mount point) — needs no symlink privilege: create the directory, then
/// set an IO_REPARSE_TAG_MOUNT_POINT reparse point naming `\??\<target>`.
#[cfg(windows)]
fn junction(target: &str, path: &str) -> R<()> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    let target = strip_verbatim(&target.replace('/', "\\"));
    let sub: Vec<u16> = format!(r"\??\{target}").encode_utf16().collect();
    let print: Vec<u16> = target.encode_utf16().collect();
    let sub_len = (sub.len() * 2) as u16;
    let print_len = (print.len() * 2) as u16;
    let data_len = 8 + sub_len + 2 + print_len + 2;
    let mut buf: Vec<u8> = Vec::with_capacity(8 + data_len as usize);
    buf.extend_from_slice(&0xA000_0003u32.to_le_bytes());
    buf.extend_from_slice(&data_len.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes()); // SubstituteNameOffset
    buf.extend_from_slice(&sub_len.to_le_bytes());
    buf.extend_from_slice(&(sub_len + 2).to_le_bytes()); // PrintNameOffset
    buf.extend_from_slice(&print_len.to_le_bytes());
    for u in sub.iter().chain(&[0]).chain(print.iter()).chain(&[0]) {
        buf.extend_from_slice(&u.to_le_bytes());
    }
    std::fs::create_dir(path)?;
    let set = || -> R<()> {
        let f = OpenOptions::new()
            .write(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let mut returned = 0u32;
        // SAFETY: `buf` is a complete REPARSE_DATA_BUFFER of `buf.len()` bytes.
        let ok = unsafe {
            win::DeviceIoControl(
                f.as_raw_handle() as win::HANDLE,
                0x0009_00A4, // FSCTL_SET_REPARSE_POINT
                buf.as_ptr() as *const std::ffi::c_void,
                buf.len() as u32,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    };
    set().inspect_err(|_| {
        let _ = std::fs::remove_dir(path);
    })
}

pub fn readlink(path: &str) -> R<String> {
    let t = std::fs::read_link(path)?;
    Ok(strip_verbatim(&t.to_string_lossy()))
}

pub fn realpath(path: &str) -> R<String> {
    let t = std::fs::canonicalize(path)?;
    Ok(strip_verbatim(&t.to_string_lossy()))
}

/// Copy a file. `mode` bits: 1 = fail if the destination exists, 4 = require a copy-on-write
/// clone (unsupported).
pub fn copy_file(src: &str, dst: &str, mode: u32) -> R<()> {
    if mode & 4 != 0 {
        return Err(FsError("ENOSYS"));
    }
    if mode & 1 != 0 && std::fs::symlink_metadata(dst).is_ok() {
        return Err(FsError("EEXIST"));
    }
    if std::fs::metadata(src).map(|m| m.is_dir()).unwrap_or(false) {
        return Err(FsError("EISDIR"));
    }
    std::fs::copy(src, dst)?;
    Ok(())
}

/// Create a directory named `prefix` plus six random characters.
pub fn mkdtemp(prefix: &str) -> R<String> {
    use std::hash::{BuildHasher, Hasher};
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    for attempt in 0..100u64 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(attempt);
        h.write_u128(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        );
        let mut x = h.finish();
        let mut name = String::from(prefix);
        for _ in 0..6 {
            name.push(CHARS[(x % CHARS.len() as u64) as usize] as char);
            x /= CHARS.len() as u64;
        }
        match std::fs::create_dir(&name) {
            Ok(()) => return Ok(name),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(FsError("EEXIST"))
}

// ---- whole files --------------------------------------------------------------------------------

/// A whole file by path; `fl` are `O_*` open flags (`O_RDONLY` for a plain read).
pub fn read_file(path: &str, fl: i32) -> R<Vec<u8>> {
    let mut f = open_file(path, fl, 0o666)?;
    let meta = f.metadata()?;
    if meta.is_dir() {
        return Err(FsError("EISDIR"));
    }
    let mut out = Vec::with_capacity(meta.len() as usize + 1);
    match f.read_to_end(&mut out) {
        Ok(_) => Ok(out),
        #[cfg(windows)]
        Err(e) if e.raw_os_error() == Some(1) => Err(FsError("EISDIR")),
        Err(e) => Err(e.into()),
    }
}

/// Whole-file write by path; `fl` are `O_*` open flags.
pub fn write_file(path: &str, data: &[u8], fl: i32, mode: u32) -> R<()> {
    let mut f = open_file(path, fl, mode)?;
    f.write_all(data)?;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::io::{AsRawFd, IntoRawFd};

    fn tmp(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "lumen-os-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ))
    }

    #[test]
    fn native_descriptor_metadata_and_reads_preserve_owner_and_shared_offset() {
        let path = tmp("fd");
        let file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        (&file).write_all(b"native").unwrap();
        use std::io::{Seek, SeekFrom};
        (&file).seek(SeekFrom::Start(0)).unwrap();
        let fd = file.as_raw_fd();
        assert!(!is_open(fd));
        let entry = get(fd).expect("borrow native descriptor");
        assert_ne!(entry.file.as_raw_fd(), fd);
        assert_ne!(unsafe { libc::fcntl(entry.file.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC, 0);
        assert_eq!(fstat(fd).expect("stat").size, 6);
        let mut bytes = [0; 3];
        assert_eq!(read_entry_raw(&entry, &mut bytes, None).ok(), Some(3));
        assert_eq!(&bytes, b"nat");
        drop(entry);
        assert_eq!(file.metadata().unwrap().len(), 6);
        assert_eq!((&file).read(&mut bytes).unwrap(), 3);
        assert_eq!(&bytes, b"ive");
        assert!(!is_open(fd));
        drop(file);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn explicit_native_close_rejects_invalid_descriptors_and_closes_only_requested_fd() {
        let file = File::open(std::env::temp_dir()).unwrap();
        let fd = file.into_raw_fd();
        assert!(close(fd).is_ok());
        // Check immediately: no intervening open may reuse the descriptor number.
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);
        assert_eq!(get(-1).err().expect("bad descriptor").0, "EBADF");
        assert_eq!(close(-1).expect_err("bad close").0, "EBADF");
        assert_eq!(get(0).err().expect("std stream").0, "EBADF");
    }

    #[test]
    fn open_write_read_stat_round_trip() {
        let dir = tmp("rt");
        assert_eq!(mkdir(dir.to_str().unwrap(), 0o755, true).unwrap().as_deref(), dir.to_str());
        let path = dir.join("f.txt");
        let p = path.to_str().unwrap();
        let fd = open(p, O_CREAT | O_RDWR, 0o644).unwrap();
        assert_eq!(write(fd, b"hello world", None).unwrap(), 11);
        let mut buf = [0u8; 5];
        assert_eq!(read(fd, &mut buf, Some(6)).unwrap(), 5);
        assert_eq!(&buf, b"world");
        let st = fstat(fd).unwrap();
        assert_eq!((st.size, st.mode & S_IFMT), (11, S_IFREG));
        ftruncate(fd, 5).unwrap();
        assert_eq!(stat(p, true).unwrap().size, 5);
        close(fd).unwrap();
        assert_eq!(close(fd).unwrap_err().code(), "EBADF");

        utimes(p, Timespec { sec: 1_000_000, nsec: 500 }, Timespec { sec: -5, nsec: 250_000_000 }, true).unwrap();
        let st = stat(p, true).unwrap();
        assert_eq!(st.atime.sec, 1_000_000);
        assert_eq!(st.mtime, Timespec { sec: -5, nsec: 250_000_000 });

        let names: Vec<_> = readdir(dir.to_str().unwrap()).unwrap();
        assert_eq!(names, vec![("f.txt".to_string(), DirentKind::File)]);
        assert_eq!(readdir(p).unwrap_err().code(), "ENOTDIR");
        assert_eq!(mkdir(p, 0o755, true).unwrap_err().code(), "EEXIST");
        assert_eq!(stat(dir.join("none").to_str().unwrap(), true).unwrap_err().code(), "ENOENT");

        let link = dir.join("l");
        symlink(p, link.to_str().unwrap(), 0).unwrap();
        assert_eq!(readlink(link.to_str().unwrap()).unwrap(), p);
        assert_eq!(stat(link.to_str().unwrap(), false).unwrap().mode & S_IFMT, S_IFLNK);
        assert_eq!(realpath(link.to_str().unwrap()).unwrap(), std::fs::canonicalize(p).unwrap().to_str().unwrap());

        let copy = dir.join("c");
        copy_file(p, copy.to_str().unwrap(), 0).unwrap();
        assert_eq!(copy_file(p, copy.to_str().unwrap(), 1).unwrap_err().code(), "EEXIST");
        assert_eq!(read_file(copy.to_str().unwrap(), 0).unwrap(), b"hello");
        write_file(copy.to_str().unwrap(), b"x", O_WRONLY | O_TRUNC, 0o644).unwrap();
        assert_eq!(read_file(copy.to_str().unwrap(), 0).unwrap(), b"x");
        assert!(access(p, 4).is_ok());
        assert_eq!(rmdir(dir.to_str().unwrap()).unwrap_err().code(), "ENOTEMPTY");
        for n in ["l", "c", "f.txt"] {
            unlink(dir.join(n).to_str().unwrap()).unwrap();
        }
        rmdir(dir.to_str().unwrap()).unwrap();
        assert!(!exists(dir.to_str().unwrap()));
    }

    #[test]
    fn timespec_conversions() {
        assert_eq!(Timespec::from_secs_f64(1.5), Timespec { sec: 1, nsec: 500_000_000 });
        assert_eq!(Timespec::from_secs_f64(-0.25), Timespec { sec: -1, nsec: 750_000_000 });
        assert_eq!(Timespec::from_secs_f64(f64::NAN), Timespec::default());
        let t = Timespec { sec: -3, nsec: 100 };
        assert_eq!(Timespec::from_system_time(Some(t.to_system_time())), t);
    }

    #[test]
    fn statfs_reports_blocks() {
        let s = statfs(std::env::temp_dir().to_str().unwrap()).unwrap();
        assert!(s.bsize > 0 && s.blocks > 0);
    }

    #[test]
    fn mkdtemp_creates_unique_dirs() {
        let prefix = tmp("mk").to_string_lossy().into_owned();
        let a = mkdtemp(&prefix).unwrap();
        let b = mkdtemp(&prefix).unwrap();
        assert_ne!(a, b);
        rmdir(&a).unwrap();
        rmdir(&b).unwrap();
    }
}
