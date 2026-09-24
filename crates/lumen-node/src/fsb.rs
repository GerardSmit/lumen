//! Node's `internalBinding('fs')` over `std::fs`: the primitives lib/fs.js, fs/promises and the
//! fs streams are written against (open/read/write/stat/readdir/...). Each is a sync op and an
//! `...Async` op that runs on the runtime's worker pool, so the callback and promise APIs never
//! block the event loop. Errors carry libuv's code (`ENOENT`, `EPERM`, ...), mapped from the OS
//! error the way libuv maps it; fs.js builds Node's `uvException` (errno, syscall, path, dest)
//! from that code.
//!
//! File descriptors live in a process-wide table so an async op on a worker thread can reach
//! them. On Unix the key is the OS descriptor itself (so an fd is usable by anything else that
//! takes one); on Windows it is a small integer handed out from 3 up, lowest free first, as the
//! CRT would. fds 0-2 are the realm's standard streams (lumen-host routes them), handled by the
//! sync ops only.

use std::collections::{BTreeSet, HashMap};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lumen::embed::{Ctx, OpDesc, OpError, SendError, Value};

// ---- errors -------------------------------------------------------------------------------------

/// A libuv error code; fs.js adds errno, description, syscall and paths.
pub struct UvErr(pub &'static str);

impl From<UvErr> for OpError {
    fn from(e: UvErr) -> OpError {
        OpError::new("Error", e.0).with_code(e.0)
    }
}

impl From<UvErr> for SendError {
    fn from(e: UvErr) -> SendError {
        SendError::new("Error", e.0.to_string()).with_code(e.0)
    }
}

impl From<std::io::Error> for UvErr {
    fn from(e: std::io::Error) -> UvErr {
        UvErr(uv_code(&e))
    }
}

type R<T> = Result<T, UvErr>;

/// libuv's name for an OS error (`uv_translate_sys_error` on Windows, errno names on Unix).
pub fn uv_code(e: &std::io::Error) -> &'static str {
    if let Some(code) = e.raw_os_error().and_then(os_code) {
        return code;
    }
    use std::io::ErrorKind as K;
    match e.kind() {
        K::NotFound => "ENOENT",
        K::PermissionDenied => "EACCES",
        K::AlreadyExists => "EEXIST",
        K::InvalidInput => "EINVAL",
        K::IsADirectory => "EISDIR",
        K::NotADirectory => "ENOTDIR",
        K::DirectoryNotEmpty => "ENOTEMPTY",
        K::BrokenPipe => "EPIPE",
        K::Unsupported => "ENOSYS",
        K::OutOfMemory => "ENOMEM",
        _ => "EIO",
    }
}

#[cfg(windows)]
fn os_code(n: i32) -> Option<&'static str> {
    Some(match n {
        1 => "EISDIR",   // ERROR_INVALID_FUNCTION (a read on a directory handle)
        2 | 3 => "ENOENT", // FILE_NOT_FOUND, PATH_NOT_FOUND
        4 => "EMFILE",
        5 => "EPERM", // ACCESS_DENIED
        6 => "EBADF",
        8 | 14 => "ENOMEM",
        15 => "ENOENT", // INVALID_DRIVE
        17 => "EXDEV",  // NOT_SAME_DEVICE
        19 => "EROFS",  // WRITE_PROTECT
        31 => "EIO",
        32 | 33 => "EBUSY", // SHARING_VIOLATION, LOCK_VIOLATION
        38 => "EOF",
        39 | 112 => "ENOSPC",
        50 => "ENOTSUP",
        80 | 183 => "EEXIST",
        87 => "EINVAL",
        109 => "EOF", // BROKEN_PIPE
        123 | 126 | 161 => "ENOENT", // INVALID_NAME, MOD_NOT_FOUND, BAD_PATHNAME
        131 => "EINVAL",             // NEGATIVE_SEEK
        145 => "ENOTEMPTY",
        206 => "ENAMETOOLONG",
        230 => "EPIPE",
        231 => "EBUSY",
        232 => "EPIPE",
        267 => "ENOENT", // DIRECTORY (libuv maps "invalid directory name" to ENOENT)
        740 | 998 | 1920 => "EACCES",
        1314 => "EPERM", // PRIVILEGE_NOT_HELD
        1921 => "ELOOP",
        4390 => "EINVAL", // NOT_A_REPARSE_POINT
        4393 => "EINVAL", // INVALID_REPARSE_DATA
        _ => return None,
    })
}

#[cfg(unix)]
fn os_code(n: i32) -> Option<&'static str> {
    Some(match n {
        1 => "EPERM",
        2 => "ENOENT",
        3 => "ESRCH",
        4 => "EINTR",
        5 => "EIO",
        6 => "ENXIO",
        7 => "E2BIG",
        9 => "EBADF",
        12 => "ENOMEM",
        13 => "EACCES",
        14 => "EFAULT",
        16 => "EBUSY",
        17 => "EEXIST",
        18 => "EXDEV",
        19 => "ENODEV",
        20 => "ENOTDIR",
        21 => "EISDIR",
        22 => "EINVAL",
        23 => "ENFILE",
        24 => "EMFILE",
        25 => "ENOTTY",
        26 => "ETXTBSY",
        27 => "EFBIG",
        28 => "ENOSPC",
        29 => "ESPIPE",
        30 => "EROFS",
        31 => "EMLINK",
        32 => "EPIPE",
        34 => "ERANGE",
        _ => return os_code_platform(n),
    })
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn os_code_platform(n: i32) -> Option<&'static str> {
    Some(match n {
        11 => "EAGAIN",
        36 => "ENAMETOOLONG",
        38 => "ENOSYS",
        39 => "ENOTEMPTY",
        40 => "ELOOP",
        95 => "ENOTSUP",
        75 => "EOVERFLOW",
        _ => return None,
    })
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
fn os_code_platform(n: i32) -> Option<&'static str> {
    Some(match n {
        35 => "EAGAIN",
        62 => "ELOOP",
        63 => "ENAMETOOLONG",
        66 => "ENOTEMPTY",
        78 => "ENOSYS",
        45 => "ENOTSUP",
        84 => "EOVERFLOW",
        _ => return None,
    })
}

// ---- the descriptor table -----------------------------------------------------------------------

struct Entry {
    file: File,
    /// Serializes a Windows positional read/write with its restore of the file pointer.
    #[cfg_attr(unix, allow(dead_code))]
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

fn get(fd: i32) -> R<Arc<Entry>> {
    lock_table().map.get(&fd).cloned().ok_or(UvErr("EBADF"))
}

fn remove(fd: i32) -> R<()> {
    let mut t = lock_table();
    match t.map.remove(&fd) {
        Some(_entry) => {
            #[cfg(not(unix))]
            t.free.insert(fd);
            Ok(())
        }
        None => Err(UvErr("EBADF")),
    }
}

// ---- open flags ---------------------------------------------------------------------------------

#[cfg(windows)]
mod flags {
    pub const O_WRONLY: i32 = 1;
    pub const O_RDWR: i32 = 2;
    pub const O_APPEND: i32 = 8;
    pub const O_CREAT: i32 = 256;
    pub const O_TRUNC: i32 = 512;
    pub const O_EXCL: i32 = 1024;
}
#[cfg(any(target_os = "linux", target_os = "android"))]
mod flags {
    pub const O_WRONLY: i32 = 1;
    pub const O_RDWR: i32 = 2;
    pub const O_CREAT: i32 = 0o100;
    pub const O_EXCL: i32 = 0o200;
    pub const O_TRUNC: i32 = 0o1000;
    pub const O_APPEND: i32 = 0o2000;
}
#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
mod flags {
    pub const O_WRONLY: i32 = 1;
    pub const O_RDWR: i32 = 2;
    pub const O_APPEND: i32 = 8;
    pub const O_CREAT: i32 = 0x200;
    pub const O_TRUNC: i32 = 0x400;
    pub const O_EXCL: i32 = 0x800;
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
    o
}

fn open_file(path: &str, fl: i32, mode: u32) -> R<File> {
    match open_options(fl, mode).open(path) {
        Ok(f) => Ok(f),
        Err(e) => {
            // Opening a directory for writing: libuv reports EISDIR.
            let is_dir = std::fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false);
            if is_dir && (fl & 3 != 0) {
                return Err(UvErr("EISDIR"));
            }
            Err(e.into())
        }
    }
}

fn open_impl(path: &str, fl: i32, mode: u32) -> R<f64> {
    Ok(insert(open_file(path, fl, mode)?) as f64)
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

fn read_entry_raw(e: &Entry, buf: &mut [u8], pos: f64) -> R<usize> {
    if pos < 0.0 {
        return eof_ok((&e.file).read(buf));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        eof_ok(e.file.read_at(buf, pos as u64))
    }
    #[cfg(windows)]
    {
        use std::io::{Seek, SeekFrom};
        use std::os::windows::fs::FileExt;
        // A positional read moves the Windows file pointer; libuv puts it back.
        let _g = e.lock.lock().unwrap_or_else(|p| p.into_inner());
        let cur = (&e.file).stream_position();
        let r = eof_ok(e.file.seek_read(buf, pos as u64));
        if let Ok(cur) = cur {
            let _ = (&e.file).seek(SeekFrom::Start(cur));
        }
        r
    }
}

fn write_entry_raw(e: &Entry, data: &[u8], pos: f64) -> R<usize> {
    if pos < 0.0 {
        return Ok((&e.file).write(data)?);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        Ok(e.file.write_at(data, pos as u64)?)
    }
    #[cfg(windows)]
    {
        use std::io::{Seek, SeekFrom};
        use std::os::windows::fs::FileExt;
        let _g = e.lock.lock().unwrap_or_else(|p| p.into_inner());
        let cur = (&e.file).stream_position();
        let r = e.file.seek_write(data, pos as u64);
        if let Ok(cur) = cur {
            let _ = (&e.file).seek(SeekFrom::Start(cur));
        }
        Ok(r?)
    }
}

/// libuv reports a read or write the handle's access mode forbids (ERROR_ACCESS_DENIED on Windows)
/// as EBADF, as Unix does.
fn read_entry(e: &Entry, buf: &mut [u8], pos: f64) -> R<usize> {
    read_entry_raw(e, buf, pos).map_err(access_to_ebadf)
}
fn write_entry(e: &Entry, data: &[u8], pos: f64) -> R<usize> {
    write_entry_raw(e, data, pos).map_err(access_to_ebadf)
}
fn access_to_ebadf(e: UvErr) -> UvErr {
    if cfg!(windows) && (e.0 == "EPERM" || e.0 == "EACCES") {
        UvErr("EBADF")
    } else {
        e
    }
}

fn is_std(fd: i32) -> bool {
    (0..=2).contains(&fd)
}

fn std_read(ctx: &mut Ctx, fd: i32, buf: &mut [u8]) -> R<usize> {
    if fd != 0 {
        return Err(UvErr("EBADF"));
    }
    eof_ok(lumen_host::read_stdin_fd(ctx, buf))
}

fn std_write(ctx: &mut Ctx, fd: i32, data: &[u8]) -> R<usize> {
    if fd == 0 {
        return Err(UvErr("EBADF"));
    }
    lumen_host::write_std_fd(ctx, fd as u32, data)?;
    Ok(data.len())
}

// ---- stat ---------------------------------------------------------------------------------------

/// Node's stat array: dev, mode, nlink, uid, gid, rdev, blksize, ino, size, blocks, then
/// (seconds, nanoseconds) for atime, mtime, ctime, birthtime.
type StatVec = Vec<f64>;

const S_IFREG: f64 = 0o100000 as f64;
const S_IFDIR: f64 = 0o040000 as f64;
const S_IFLNK: f64 = 0o120000 as f64;
#[cfg(windows)]
const S_IFCHR: f64 = 0o020000 as f64;
#[cfg(windows)]
const S_IFIFO: f64 = 0o010000 as f64;

#[cfg_attr(windows, allow(dead_code))]
fn split_time(t: Option<SystemTime>) -> (f64, f64) {
    let Some(t) = t else { return (0.0, 0.0) };
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as f64, d.subsec_nanos() as f64),
        Err(e) => {
            let d = e.duration();
            let (s, n) = (d.as_secs() as f64, d.subsec_nanos() as f64);
            if n == 0.0 {
                (-s, 0.0)
            } else {
                (-s - 1.0, 1e9 - n)
            }
        }
    }
}

#[cfg(unix)]
fn meta_vec(m: &std::fs::Metadata) -> StatVec {
    use std::os::unix::fs::MetadataExt;
    let (bs, bn) = split_time(m.created().ok());
    vec![
        m.dev() as f64,
        m.mode() as f64,
        m.nlink() as f64,
        m.uid() as f64,
        m.gid() as f64,
        m.rdev() as f64,
        m.blksize() as f64,
        m.ino() as f64,
        m.size() as f64,
        m.blocks() as f64,
        m.atime() as f64,
        m.atime_nsec() as f64,
        m.mtime() as f64,
        m.mtime_nsec() as f64,
        m.ctime() as f64,
        m.ctime_nsec() as f64,
        bs,
        bn,
    ]
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

/// A Windows FILETIME-style count (100 ns since 1601) as Unix (seconds, nanoseconds).
#[cfg(windows)]
fn filetime(t: i64) -> (f64, f64) {
    let unix = t - 116_444_736_000_000_000;
    (unix.div_euclid(10_000_000) as f64, (unix.rem_euclid(10_000_000) * 100) as f64)
}

/// libuv's `fs__stat_handle`: stat by handle, as the CRT never could (ino, dev, nlink, ctime).
#[cfg(windows)]
fn handle_vec(f: &File) -> R<StatVec> {
    use std::os::windows::io::AsRawHandle;
    let h = f.as_raw_handle() as win::HANDLE;
    // SAFETY: `h` is a live handle owned by `f`; each out-struct is sized for its info class.
    unsafe {
        let kind = win::GetFileType(h);
        if kind == 2 || kind == 3 {
            // A console or a pipe: libuv reports a character device / FIFO with no times.
            let mode = if kind == 2 { S_IFCHR } else { S_IFIFO } + 0o666 as f64;
            let mut v = vec![0.0; 18];
            v[1] = mode;
            v[2] = 1.0;
            v[6] = 4096.0;
            return Ok(v);
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
        let mode = if dir { S_IFDIR } else { S_IFREG } + perm as f64;
        let size = ((bh.size_hi as u64) << 32 | bh.size_lo as u64) as f64;
        let ino = ((bh.index_hi as u64) << 32 | bh.index_lo as u64) as f64;
        let (as_, an) = filetime(basic.access);
        let (ms, mn) = filetime(basic.write);
        let (cs, cn) = filetime(basic.change);
        let (bs, bn) = filetime(basic.creation);
        Ok(vec![
            bh.volume_serial as f64,
            mode,
            bh.nlinks as f64,
            0.0,
            0.0,
            0.0,
            4096.0,
            ino,
            size,
            (stdi.alloc >> 9) as f64,
            as_,
            an,
            ms,
            mn,
            cs,
            cn,
            bs,
            bn,
        ])
    }
}

#[cfg(windows)]
fn stat_path(path: &str, follow: bool) -> R<StatVec> {
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
    let mut v = handle_vec(&f)?;
    if is_link {
        // lstat of a link: S_IFLNK, and the size is the target's length (libuv).
        v[1] = S_IFLNK + 0o666 as f64;
        v[8] = std::fs::read_link(path)
            .map(|t| strip_verbatim(&t.to_string_lossy()).len() as f64)
            .unwrap_or(0.0);
    }
    Ok(v)
}

#[cfg(unix)]
fn stat_path(path: &str, follow: bool) -> R<StatVec> {
    let m = if follow {
        std::fs::metadata(path)?
    } else {
        std::fs::symlink_metadata(path)?
    };
    Ok(meta_vec(&m))
}

#[cfg(unix)]
fn fstat_file(f: &File) -> R<StatVec> {
    Ok(meta_vec(&f.metadata()?))
}

#[cfg(windows)]
fn fstat_file(f: &File) -> R<StatVec> {
    handle_vec(f)
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
            return Err(UvErr("EBADF"));
        }
        // SAFETY: the std handle outlives the call; ManuallyDrop never closes it.
        let f = std::mem::ManuallyDrop::new(unsafe { File::from_raw_handle(h) });
        op(&f)
    }
}

fn fstat_impl(fd: i32) -> R<StatVec> {
    if is_std(fd) && get(fd).is_err() {
        return with_std_file(fd, fstat_file);
    }
    fstat_file(&get(fd)?.file)
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

fn to_time(secs: f64) -> SystemTime {
    if !secs.is_finite() {
        return UNIX_EPOCH;
    }
    if secs >= 0.0 {
        UNIX_EPOCH + Duration::from_secs_f64(secs)
    } else {
        UNIX_EPOCH - Duration::from_secs_f64(-secs)
    }
}

fn file_times(atime: f64, mtime: f64) -> std::fs::FileTimes {
    std::fs::FileTimes::new()
        .set_accessed(to_time(atime))
        .set_modified(to_time(mtime))
}

#[cfg(unix)]
mod ux {
    use std::os::raw::{c_char, c_int, c_long};
    #[repr(C)]
    pub struct Timespec {
        pub tv_sec: i64,
        pub tv_nsec: c_long,
    }
    extern "C" {
        pub fn utimensat(dirfd: c_int, path: *const c_char, times: *const Timespec, flags: c_int) -> c_int;
        pub fn access(path: *const c_char, mode: c_int) -> c_int;
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub const AT_FDCWD: c_int = -100;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub const AT_SYMLINK_NOFOLLOW: c_int = 0x100;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    pub const AT_FDCWD: c_int = -2;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    pub const AT_SYMLINK_NOFOLLOW: c_int = 0x20;

    pub fn cpath(p: &str) -> Result<std::ffi::CString, super::UvErr> {
        std::ffi::CString::new(p).map_err(|_| super::UvErr("EINVAL"))
    }
}

fn utimes_impl(path: &str, atime: f64, mtime: f64, follow: bool) -> R<()> {
    #[cfg(unix)]
    {
        let ts = |t: f64| {
            let (s, n) = split_time(Some(to_time(t)));
            ux::Timespec {
                tv_sec: s as i64,
                tv_nsec: n as _,
            }
        };
        let times = [ts(atime), ts(mtime)];
        let c = ux::cpath(path)?;
        let flags = if follow { 0 } else { ux::AT_SYMLINK_NOFOLLOW };
        // SAFETY: `c` is NUL-terminated and `times` holds the two timespecs utimensat reads.
        if unsafe { ux::utimensat(ux::AT_FDCWD, c.as_ptr(), times.as_ptr(), flags) } != 0 {
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
}

fn access_impl(path: &str, mode: u32) -> R<()> {
    #[cfg(unix)]
    {
        let c = ux::cpath(path)?;
        // SAFETY: `c` is NUL-terminated.
        if unsafe { ux::access(c.as_ptr(), mode as i32) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        // libuv: existence, plus W_OK fails on a read-only file.
        let m = std::fs::metadata(path)?;
        if mode & 2 != 0 && m.permissions().readonly() && !m.is_dir() {
            return Err(UvErr("EPERM"));
        }
        Ok(())
    }
}

fn chmod_path(path: &str, mode: u32) -> R<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(windows)]
    {
        let mut p = std::fs::metadata(path)?.permissions();
        p.set_readonly(mode & 0o200 == 0);
        std::fs::set_permissions(path, p)?;
    }
    Ok(())
}

fn fchmod_impl(fd: i32, mode: u32) -> R<()> {
    let e = get(fd)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        e.file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(windows)]
    {
        let mut p = e.file.metadata()?.permissions();
        p.set_readonly(mode & 0o200 == 0);
        e.file.set_permissions(p)?;
    }
    Ok(())
}

fn chown_impl(path: &str, uid: u32, gid: u32, follow: bool) -> R<()> {
    #[cfg(unix)]
    {
        if follow {
            std::os::unix::fs::chown(path, Some(uid), Some(gid))?;
        } else {
            std::os::unix::fs::lchown(path, Some(uid), Some(gid))?;
        }
    }
    #[cfg(windows)]
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

fn fchown_impl(fd: i32, uid: u32, gid: u32) -> R<()> {
    let e = get(fd)?;
    #[cfg(unix)]
    std::os::unix::fs::fchown(&e.file, Some(uid), Some(gid))?;
    #[cfg(windows)]
    let _ = (e, uid, gid);
    Ok(())
}

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

/// `mkdir(path, mode, recursive)`: the first directory created (recursive), else nothing.
fn mkdir_impl(path: &str, mode: u32, recursive: bool) -> R<Option<String>> {
    if !recursive {
        mkdir_one(path, mode)?;
        return Ok(None);
    }
    let p = std::path::Path::new(path);
    if let Ok(m) = std::fs::metadata(p) {
        return if m.is_dir() {
            Ok(None)
        } else {
            Err(UvErr("EEXIST"))
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
            Ok(_) => return Err(UvErr("ENOTDIR")),
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

fn unlink_impl(path: &str) -> R<()> {
    #[cfg(windows)]
    {
        if let Ok(m) = std::fs::symlink_metadata(path) {
            // A directory symlink or junction is removed like a directory (libuv).
            if m.file_type().is_symlink() && m.is_dir() {
                return Ok(std::fs::remove_dir(path)?);
            }
            if m.is_dir() {
                return Err(UvErr("EPERM"));
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
    #[cfg(unix)]
    {
        Ok(std::fs::remove_file(path)?)
    }
}

fn rmdir_impl(path: &str) -> R<()> {
    match std::fs::remove_dir(path) {
        Ok(()) => Ok(()),
        #[cfg(unix)]
        Err(e) => Err(e.into()),
        #[cfg(windows)]
        Err(e) => {
            if e.raw_os_error() == Some(267) || e.raw_os_error() == Some(5) {
                // A file: libuv maps ERROR_DIRECTORY to ENOENT (Node reports that on Windows).
                if let Ok(m) = std::fs::symlink_metadata(path) {
                    if !m.is_dir() {
                        return Err(UvErr("ENOENT"));
                    }
                }
            }
            Err(e.into())
        }
    }
}

fn readdir_impl(path: &str) -> R<(Vec<String>, Vec<f64>)> {
    let rd = match std::fs::read_dir(path) {
        Ok(rd) => rd,
        Err(e) => {
            if std::fs::metadata(path).map(|m| !m.is_dir()).unwrap_or(false) {
                return Err(UvErr("ENOTDIR"));
            }
            return Err(e.into());
        }
    };
    let mut names = Vec::new();
    let mut types = Vec::new();
    for entry in rd {
        let entry = entry?;
        names.push(entry.file_name().to_string_lossy().into_owned());
        // UV_DIRENT_*: 0 unknown, 1 file, 2 dir, 3 link, 4 fifo, 5 socket, 6 char, 7 block.
        let t = match entry.file_type() {
            Ok(ft) if ft.is_symlink() => 3.0,
            Ok(ft) if ft.is_dir() => 2.0,
            Ok(ft) if ft.is_file() => 1.0,
            #[cfg(unix)]
            Ok(ft) => {
                use std::os::unix::fs::FileTypeExt;
                if ft.is_fifo() {
                    4.0
                } else if ft.is_socket() {
                    5.0
                } else if ft.is_char_device() {
                    6.0
                } else if ft.is_block_device() {
                    7.0
                } else {
                    0.0
                }
            }
            _ => 0.0,
        };
        types.push(t);
    }
    Ok((names, types))
}

fn symlink_impl(target: &str, path: &str, flags: u32) -> R<()> {
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

fn readlink_impl(path: &str) -> R<String> {
    let t = std::fs::read_link(path)?;
    Ok(strip_verbatim(&t.to_string_lossy()))
}

fn realpath_impl(path: &str) -> R<String> {
    let t = std::fs::canonicalize(path)?;
    Ok(strip_verbatim(&t.to_string_lossy()))
}

fn copyfile_impl(src: &str, dst: &str, mode: u32) -> R<()> {
    if mode & 4 != 0 {
        // COPYFILE_FICLONE_FORCE: no copy-on-write clone support.
        return Err(UvErr("ENOSYS"));
    }
    if mode & 1 != 0 && std::fs::symlink_metadata(dst).is_ok() {
        return Err(UvErr("EEXIST"));
    }
    if std::fs::metadata(src).map(|m| m.is_dir()).unwrap_or(false) {
        return Err(UvErr("EISDIR"));
    }
    std::fs::copy(src, dst)?;
    Ok(())
}

fn mkdtemp_impl(prefix: &str) -> R<String> {
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
    Err(UvErr("EEXIST"))
}

fn statfs_impl(path: &str) -> R<Vec<f64>> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let c = ux::cpath(path)?;
        let mut buf: std::mem::MaybeUninit<crate::Statvfs> = std::mem::MaybeUninit::zeroed();
        // SAFETY: as in lib.rs `op_statfs`: NUL-terminated path, oversized zeroed out-struct.
        if unsafe { crate::statvfs(c.as_ptr(), buf.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: statvfs succeeded and filled the struct.
        let s = unsafe { buf.assume_init() };
        Ok(vec![
            0.0,
            s.f_bsize as f64,
            s.f_blocks as f64,
            s.f_bfree as f64,
            s.f_bavail as f64,
            s.f_files as f64,
            s.f_ffree as f64,
        ])
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
        Ok(vec![
            0.0,
            (spc as f64) * (bps as f64),
            total as f64,
            free as f64,
            free as f64,
            0.0,
            0.0,
        ])
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        let _ = path;
        Err(UvErr("ENOSYS"))
    }
}

fn read_file_impl(path: &str, fl: i32) -> R<Vec<u8>> {
    let mut f = open_file(path, fl, 0o666)?;
    let meta = f.metadata()?;
    if meta.is_dir() {
        return Err(UvErr("EISDIR"));
    }
    let mut out = Vec::with_capacity(meta.len() as usize + 1);
    match f.read_to_end(&mut out) {
        Ok(_) => Ok(out),
        #[cfg(windows)]
        Err(e) if e.raw_os_error() == Some(1) => Err(UvErr("EISDIR")),
        Err(e) => Err(e.into()),
    }
}

// ---- ops ----------------------------------------------------------------------------------------
// Every op has a sync form (`open`) and a worker-pool form (`openAsync`).

#[lumen::op(name = "open")]
fn op_open(path: &str, flags: i32, mode: u32) -> Result<f64, OpError> {
    Ok(open_impl(path, flags, mode)?)
}
#[lumen::op(async, name = "openAsync")]
fn op_open_async(path: String, flags: i32, mode: u32) -> Result<f64, SendError> {
    Ok(open_impl(&path, flags, mode)?)
}

#[lumen::op(name = "close")]
fn op_close(fd: i32) -> Result<(), OpError> {
    if is_std(fd) && get(fd).is_err() {
        return Ok(());
    }
    Ok(remove(fd)?)
}
#[lumen::op(async, name = "closeAsync")]
fn op_close_async(fd: i32) -> Result<(), SendError> {
    if is_std(fd) && get(fd).is_err() {
        return Ok(());
    }
    Ok(remove(fd)?)
}

/// `read(fd, view, position)`: fills the view (JS passes the `[offset, offset+length)` window).
#[lumen::op(name = "read")]
fn op_read(ctx: &mut Ctx, fd: i32, buf: &mut [u8], pos: f64) -> Result<f64, OpError> {
    if is_std(fd) && get(fd).is_err() {
        return Ok(std_read(ctx, fd, buf)? as f64);
    }
    Ok(read_entry(&*get(fd)?, buf, pos)? as f64)
}
/// The bytes read (JS copies them into the caller's buffer).
#[lumen::op(async, name = "readAsync")]
fn op_read_async(fd: i32, len: u32, pos: f64) -> Result<Vec<u8>, SendError> {
    let e = get(fd)?;
    let mut buf = vec![0u8; len as usize];
    let n = read_entry(&e, &mut buf, pos)?;
    buf.truncate(n);
    Ok(buf)
}

#[lumen::op(name = "write")]
fn op_write(ctx: &mut Ctx, fd: i32, data: &[u8], pos: f64) -> Result<f64, OpError> {
    if is_std(fd) && get(fd).is_err() {
        return Ok(std_write(ctx, fd, data)? as f64);
    }
    Ok(write_entry(&*get(fd)?, data, pos)? as f64)
}
#[lumen::op(async, name = "writeAsync")]
fn op_write_async(fd: i32, data: Vec<u8>, pos: f64) -> Result<f64, SendError> {
    Ok(write_entry(&*get(fd)?, &data, pos)? as f64)
}

#[lumen::op(name = "fstat")]
fn op_fstat(fd: i32) -> Result<Vec<f64>, OpError> {
    Ok(fstat_impl(fd)?)
}
#[lumen::op(async, name = "fstatAsync")]
fn op_fstat_async(fd: i32) -> Result<Vec<f64>, SendError> {
    Ok(fstat_impl(fd)?)
}

#[lumen::op(name = "stat")]
fn op_stat(path: &str) -> Result<Vec<f64>, OpError> {
    Ok(stat_path(path, true)?)
}
#[lumen::op(async, name = "statAsync")]
fn op_stat_async(path: String) -> Result<Vec<f64>, SendError> {
    Ok(stat_path(&path, true)?)
}

#[lumen::op(name = "lstat")]
fn op_lstat(path: &str) -> Result<Vec<f64>, OpError> {
    Ok(stat_path(path, false)?)
}
#[lumen::op(async, name = "lstatAsync")]
fn op_lstat_async(path: String) -> Result<Vec<f64>, SendError> {
    Ok(stat_path(&path, false)?)
}

/// Like `stat` but a missing path is `undefined`, not an error (`throwIfNoEntry: false`).
#[lumen::op(name = "statMaybe")]
fn op_stat_maybe(path: &str, follow: bool) -> Result<Option<Vec<f64>>, OpError> {
    match stat_path(path, follow) {
        Ok(v) => Ok(Some(v)),
        Err(UvErr("ENOENT")) | Err(UvErr("ENOTDIR")) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[lumen::op(name = "statfs")]
fn op_statfs(path: &str) -> Result<Vec<f64>, OpError> {
    Ok(statfs_impl(path)?)
}
#[lumen::op(async, name = "statfsAsync")]
fn op_statfs_async(path: String) -> Result<Vec<f64>, SendError> {
    Ok(statfs_impl(&path)?)
}

#[lumen::op(name = "ftruncate")]
fn op_ftruncate(fd: i32, len: f64) -> Result<(), OpError> {
    Ok(get(fd)?.file.set_len(len.max(0.0) as u64).map_err(UvErr::from)?)
}
#[lumen::op(async, name = "ftruncateAsync")]
fn op_ftruncate_async(fd: i32, len: f64) -> Result<(), SendError> {
    Ok(get(fd)?.file.set_len(len.max(0.0) as u64).map_err(UvErr::from)?)
}

fn fsync_impl(fd: i32, data_only: bool) -> R<()> {
    let e = get(fd)?;
    if data_only {
        e.file.sync_data()?;
    } else {
        e.file.sync_all()?;
    }
    Ok(())
}
#[lumen::op(name = "fsync")]
fn op_fsync(fd: i32, data_only: bool) -> Result<(), OpError> {
    Ok(fsync_impl(fd, data_only)?)
}
#[lumen::op(async, name = "fsyncAsync")]
fn op_fsync_async(fd: i32, data_only: bool) -> Result<(), SendError> {
    Ok(fsync_impl(fd, data_only)?)
}

#[lumen::op(name = "fchmod")]
fn op_fchmod(fd: i32, mode: u32) -> Result<(), OpError> {
    Ok(fchmod_impl(fd, mode)?)
}
#[lumen::op(async, name = "fchmodAsync")]
fn op_fchmod_async(fd: i32, mode: u32) -> Result<(), SendError> {
    Ok(fchmod_impl(fd, mode)?)
}

#[lumen::op(name = "fchown")]
fn op_fchown(fd: i32, uid: u32, gid: u32) -> Result<(), OpError> {
    Ok(fchown_impl(fd, uid, gid)?)
}
#[lumen::op(async, name = "fchownAsync")]
fn op_fchown_async(fd: i32, uid: u32, gid: u32) -> Result<(), SendError> {
    Ok(fchown_impl(fd, uid, gid)?)
}

fn futimes_impl(fd: i32, atime: f64, mtime: f64) -> R<()> {
    Ok(get(fd)?.file.set_times(file_times(atime, mtime))?)
}
#[lumen::op(name = "futimes")]
fn op_futimes(fd: i32, atime: f64, mtime: f64) -> Result<(), OpError> {
    Ok(futimes_impl(fd, atime, mtime)?)
}
#[lumen::op(async, name = "futimesAsync")]
fn op_futimes_async(fd: i32, atime: f64, mtime: f64) -> Result<(), SendError> {
    Ok(futimes_impl(fd, atime, mtime)?)
}

#[lumen::op(name = "utimes")]
fn op_utimes(path: &str, atime: f64, mtime: f64, follow: bool) -> Result<(), OpError> {
    Ok(utimes_impl(path, atime, mtime, follow)?)
}
#[lumen::op(async, name = "utimesAsync")]
fn op_utimes_async(path: String, atime: f64, mtime: f64, follow: bool) -> Result<(), SendError> {
    Ok(utimes_impl(&path, atime, mtime, follow)?)
}

#[lumen::op(name = "access")]
fn op_access(path: &str, mode: u32) -> Result<(), OpError> {
    Ok(access_impl(path, mode)?)
}
#[lumen::op(async, name = "accessAsync")]
fn op_access_async(path: String, mode: u32) -> Result<(), SendError> {
    Ok(access_impl(&path, mode)?)
}

#[lumen::op(name = "exists")]
fn op_exists(path: &str) -> bool {
    std::fs::metadata(path).is_ok()
}

#[lumen::op(name = "chmod")]
fn op_chmod(path: &str, mode: u32) -> Result<(), OpError> {
    Ok(chmod_path(path, mode)?)
}
#[lumen::op(async, name = "chmodAsync")]
fn op_chmod_async(path: String, mode: u32) -> Result<(), SendError> {
    Ok(chmod_path(&path, mode)?)
}

#[lumen::op(name = "chown")]
fn op_chown(path: &str, uid: u32, gid: u32, follow: bool) -> Result<(), OpError> {
    Ok(chown_impl(path, uid, gid, follow)?)
}
#[lumen::op(async, name = "chownAsync")]
fn op_chown_async(path: String, uid: u32, gid: u32, follow: bool) -> Result<(), SendError> {
    Ok(chown_impl(&path, uid, gid, follow)?)
}

#[lumen::op(name = "mkdir")]
fn op_mkdir(path: &str, mode: u32, recursive: bool) -> Result<Option<String>, OpError> {
    Ok(mkdir_impl(path, mode, recursive)?)
}
#[lumen::op(async, name = "mkdirAsync")]
fn op_mkdir_async(path: String, mode: u32, recursive: bool) -> Result<Option<String>, SendError> {
    Ok(mkdir_impl(&path, mode, recursive)?)
}

#[lumen::op(name = "mkdtemp")]
fn op_mkdtemp(prefix: &str) -> Result<String, OpError> {
    Ok(mkdtemp_impl(prefix)?)
}
#[lumen::op(async, name = "mkdtempAsync")]
fn op_mkdtemp_async(prefix: String) -> Result<String, SendError> {
    Ok(mkdtemp_impl(&prefix)?)
}

#[lumen::op(name = "rmdir")]
fn op_rmdir(path: &str) -> Result<(), OpError> {
    Ok(rmdir_impl(path)?)
}
#[lumen::op(async, name = "rmdirAsync")]
fn op_rmdir_async(path: String) -> Result<(), SendError> {
    Ok(rmdir_impl(&path)?)
}

#[lumen::op(name = "unlink")]
fn op_unlink(path: &str) -> Result<(), OpError> {
    Ok(unlink_impl(path)?)
}
#[lumen::op(async, name = "unlinkAsync")]
fn op_unlink_async(path: String) -> Result<(), SendError> {
    Ok(unlink_impl(&path)?)
}

#[lumen::op(name = "rename")]
fn op_rename(from: &str, to: &str) -> Result<(), OpError> {
    Ok(std::fs::rename(from, to).map_err(UvErr::from)?)
}
#[lumen::op(async, name = "renameAsync")]
fn op_rename_async(from: String, to: String) -> Result<(), SendError> {
    Ok(std::fs::rename(from, to).map_err(UvErr::from)?)
}

#[lumen::op(name = "link")]
fn op_link(existing: &str, path: &str) -> Result<(), OpError> {
    Ok(std::fs::hard_link(existing, path).map_err(UvErr::from)?)
}
#[lumen::op(async, name = "linkAsync")]
fn op_link_async(existing: String, path: String) -> Result<(), SendError> {
    Ok(std::fs::hard_link(existing, path).map_err(UvErr::from)?)
}

#[lumen::op(name = "symlink")]
fn op_symlink(target: &str, path: &str, flags: u32) -> Result<(), OpError> {
    Ok(symlink_impl(target, path, flags)?)
}
#[lumen::op(async, name = "symlinkAsync")]
fn op_symlink_async(target: String, path: String, flags: u32) -> Result<(), SendError> {
    Ok(symlink_impl(&target, &path, flags)?)
}

#[lumen::op(name = "readlink")]
fn op_readlink(path: &str) -> Result<String, OpError> {
    Ok(readlink_impl(path)?)
}
#[lumen::op(async, name = "readlinkAsync")]
fn op_readlink_async(path: String) -> Result<String, SendError> {
    Ok(readlink_impl(&path)?)
}

#[lumen::op(name = "realpath")]
fn op_realpath(path: &str) -> Result<String, OpError> {
    Ok(realpath_impl(path)?)
}
#[lumen::op(async, name = "realpathAsync")]
fn op_realpath_async(path: String) -> Result<String, SendError> {
    Ok(realpath_impl(&path)?)
}

#[lumen::op(name = "copyFile")]
fn op_copy_file(src: &str, dst: &str, mode: u32) -> Result<(), OpError> {
    Ok(copyfile_impl(src, dst, mode)?)
}
#[lumen::op(async, name = "copyFileAsync")]
fn op_copy_file_async(src: String, dst: String, mode: u32) -> Result<(), SendError> {
    Ok(copyfile_impl(&src, &dst, mode)?)
}

/// `[names, types]` (types are UV_DIRENT_* values).
#[lumen::op(name = "readdir")]
fn op_readdir(path: &str) -> Result<(Vec<String>, Vec<f64>), OpError> {
    Ok(readdir_impl(path)?)
}
#[lumen::op(async, name = "readdirAsync")]
fn op_readdir_async(path: String) -> Result<(Vec<String>, Vec<f64>), SendError> {
    Ok(readdir_impl(&path)?)
}

/// A whole file by path (readFileSync's fast path).
#[lumen::op(name = "readFile")]
fn op_read_file(path: &str, flags: i32) -> Result<Vec<u8>, OpError> {
    Ok(read_file_impl(path, flags)?)
}
#[lumen::op(async, name = "readFileAsync")]
fn op_read_file_async(path: String, flags: i32) -> Result<Vec<u8>, SendError> {
    Ok(read_file_impl(&path, flags)?)
}

/// A whole file decoded as UTF-8 (invalid sequences become U+FFFD), for `readFileSync(p, 'utf8')`.
#[lumen::op(name = "readFileUtf8")]
fn op_read_file_utf8(path: &str, flags: i32) -> Result<String, OpError> {
    let bytes = read_file_impl(path, flags)?;
    let s = match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    };
    Ok(crate::codec::canonical(s))
}

/// Whole-file write by path: `flags` are Node's numeric open flags.
fn write_file_impl(path: &str, data: &[u8], fl: i32, mode: u32) -> R<()> {
    let mut f = open_file(path, fl, mode)?;
    f.write_all(data)?;
    Ok(())
}
#[lumen::op(name = "writeFile")]
fn op_write_file(path: &str, data: &[u8], flags: i32, mode: u32) -> Result<(), OpError> {
    Ok(write_file_impl(path, data, flags, mode)?)
}
#[lumen::op(async, name = "writeFileAsync")]
fn op_write_file_async(path: String, data: Vec<u8>, flags: i32, mode: u32) -> Result<(), SendError> {
    Ok(write_file_impl(&path, &data, flags, mode)?)
}

// ---- registration -------------------------------------------------------------------------------

const OPS: &[&OpDesc] = lumen::ops![
    op_open,
    op_open_async,
    op_close,
    op_close_async,
    op_read,
    op_read_async,
    op_write,
    op_write_async,
    op_fstat,
    op_fstat_async,
    op_stat,
    op_stat_async,
    op_lstat,
    op_lstat_async,
    op_stat_maybe,
    op_statfs,
    op_statfs_async,
    op_ftruncate,
    op_ftruncate_async,
    op_fsync,
    op_fsync_async,
    op_fchmod,
    op_fchmod_async,
    op_fchown,
    op_fchown_async,
    op_futimes,
    op_futimes_async,
    op_utimes,
    op_utimes_async,
    op_access,
    op_access_async,
    op_exists,
    op_chmod,
    op_chmod_async,
    op_chown,
    op_chown_async,
    op_mkdir,
    op_mkdir_async,
    op_mkdtemp,
    op_mkdtemp_async,
    op_rmdir,
    op_rmdir_async,
    op_unlink,
    op_unlink_async,
    op_rename,
    op_rename_async,
    op_link,
    op_link_async,
    op_symlink,
    op_symlink_async,
    op_readlink,
    op_readlink_async,
    op_realpath,
    op_realpath_async,
    op_copy_file,
    op_copy_file_async,
    op_readdir,
    op_readdir_async,
    op_read_file,
    op_read_file_async,
    op_read_file_utf8,
    op_write_file,
    op_write_file_async,
];

/// `__node.fsBinding()`: a fresh object holding every op above (called once by fs.js).
pub fn op_fs_binding(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    let ns = Value::Obj(ctx.new_object());
    for op in OPS {
        let f = ctx.op_function(op);
        let _ = ctx.set_member(&ns, op.name, f);
    }
    Ok(ns)
}
