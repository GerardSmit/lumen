//! User and group identity: name lookups, `set*id`, supplementary groups and `execve`. Off Unix
//! the lookups find nothing and the setters fail with `ENOSYS`.

use crate::errno::FsError;

pub type R<T> = Result<T, FsError>;

#[cfg(unix)]
fn check(rc: libc::c_int) -> R<()> {
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().into())
    }
}

#[cfg(unix)]
fn cstr(s: &str) -> R<std::ffi::CString> {
    std::ffi::CString::new(s).map_err(|_| FsError("EINVAL"))
}

/// A complete password-database entry (`struct passwd`), as the Python `pwd` module reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PasswdRecord {
    pub name: String,
    pub passwd: String,
    pub uid: u32,
    pub gid: u32,
    pub gecos: String,
    pub dir: String,
    pub shell: String,
}

/// A group-database entry (`struct group`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupRecord {
    pub name: String,
    pub passwd: String,
    pub gid: u32,
    pub members: Vec<String>,
}

#[cfg(unix)]
fn c_text(p: *const libc::c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: a non-null database string field is a NUL-terminated C string.
    unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

#[cfg(unix)]
fn passwd_record(e: &libc::passwd) -> PasswdRecord {
    #[cfg(not(target_os = "android"))]
    let gecos = c_text(e.pw_gecos);
    #[cfg(target_os = "android")]
    let gecos = String::new();
    PasswdRecord {
        name: c_text(e.pw_name),
        passwd: c_text(e.pw_passwd),
        uid: e.pw_uid as u32,
        gid: e.pw_gid as u32,
        gecos,
        dir: c_text(e.pw_dir),
        shell: c_text(e.pw_shell),
    }
}

#[cfg(unix)]
fn group_record(e: &libc::group) -> GroupRecord {
    let mut members = Vec::new();
    if !e.gr_mem.is_null() {
        let mut p = e.gr_mem;
        // SAFETY: gr_mem is a null-terminated array of C strings; the array is read unaligned
        // because some libcs (macOS) place it at an arbitrary offset inside their lookup buffer.
        unsafe {
            loop {
                let member = p.read_unaligned();
                if member.is_null() {
                    break;
                }
                members.push(c_text(member));
                p = p.add(1);
            }
        }
    }
    GroupRecord { name: c_text(e.gr_name), passwd: c_text(e.gr_passwd), gid: e.gr_gid as u32, members }
}

/// The entry of user `name` (`getpwnam`).
pub fn getpwnam(name: &str) -> Option<PasswdRecord> {
    #[cfg(unix)]
    {
        let name = cstr(name).ok()?;
        // SAFETY: getpwnam returns null or a pointer to static storage valid until the next call.
        let entry = unsafe { libc::getpwnam(name.as_ptr()) };
        (!entry.is_null()).then(|| passwd_record(unsafe { &*entry }))
    }
    #[cfg(not(unix))]
    {
        let _ = name;
        None
    }
}

/// The entry of user `uid` (`getpwuid`).
pub fn getpwuid(uid: u32) -> Option<PasswdRecord> {
    #[cfg(unix)]
    {
        // SAFETY: getpwuid returns null or a pointer to static storage valid until the next call.
        let entry = unsafe { libc::getpwuid(uid as libc::uid_t) };
        (!entry.is_null()).then(|| passwd_record(unsafe { &*entry }))
    }
    #[cfg(not(unix))]
    {
        let _ = uid;
        None
    }
}

/// Every entry of the password database (`getpwent` until the end).
pub fn getpwall() -> Vec<PasswdRecord> {
    #[cfg(all(unix, not(target_os = "android")))]
    {
        let mut out = Vec::new();
        // SAFETY: the iteration functions return static storage valid until the next call.
        unsafe {
            libc::setpwent();
            loop {
                let entry = libc::getpwent();
                if entry.is_null() {
                    break;
                }
                out.push(passwd_record(&*entry));
            }
            libc::endpwent();
        }
        out
    }
    #[cfg(not(all(unix, not(target_os = "android"))))]
    Vec::new()
}

/// The entry of group `name` (`getgrnam`).
pub fn getgrnam(name: &str) -> Option<GroupRecord> {
    #[cfg(unix)]
    {
        let name = cstr(name).ok()?;
        // SAFETY: getgrnam returns null or a pointer to static storage valid until the next call.
        let entry = unsafe { libc::getgrnam(name.as_ptr()) };
        (!entry.is_null()).then(|| group_record(unsafe { &*entry }))
    }
    #[cfg(not(unix))]
    {
        let _ = name;
        None
    }
}

/// The entry of group `gid` (`getgrgid`).
pub fn getgrgid(gid: u32) -> Option<GroupRecord> {
    #[cfg(unix)]
    {
        // SAFETY: getgrgid returns null or a pointer to static storage valid until the next call.
        let entry = unsafe { libc::getgrgid(gid as libc::gid_t) };
        (!entry.is_null()).then(|| group_record(unsafe { &*entry }))
    }
    #[cfg(not(unix))]
    {
        let _ = gid;
        None
    }
}

/// Every entry of the group database.
pub fn getgrall() -> Vec<GroupRecord> {
    #[cfg(all(unix, not(target_os = "android")))]
    {
        let mut out = Vec::new();
        // SAFETY: the iteration functions return static storage valid until the next call.
        unsafe {
            libc::setgrent();
            loop {
                let entry = libc::getgrent();
                if entry.is_null() {
                    break;
                }
                out.push(group_record(&*entry));
            }
            libc::endgrent();
        }
        out
    }
    #[cfg(not(all(unix, not(target_os = "android"))))]
    Vec::new()
}

/// The uid of user `name` (`getpwnam`).
pub fn uid_of(name: &str) -> Option<u32> {
    getpwnam(name).map(|e| e.uid)
}

/// The gid of group `name` (`getgrnam`).
pub fn gid_of(name: &str) -> Option<u32> {
    getgrnam(name).map(|e| e.gid)
}

/// A password-database entry. Without one (or off Unix) the ids are still set (-1 off Unix) and
/// the strings are absent.
pub struct PasswdEntry {
    pub uid: i64,
    pub gid: i64,
    pub name: Option<String>,
    pub dir: Option<String>,
    pub shell: Option<String>,
}

/// The password-database entry of `uid` (`getpwuid`), `None` without one.
pub fn passwd(uid: u32) -> Option<PasswdEntry> {
    let e = getpwuid(uid)?;
    Some(PasswdEntry { uid: e.uid as i64, gid: e.gid as i64, name: Some(e.name), dir: Some(e.dir), shell: Some(e.shell) })
}

/// The entry of the real user id, keeping the real uid and gid when the database has none.
pub fn current_user() -> PasswdEntry {
    #[cfg(unix)]
    {
        // SAFETY: no arguments, cannot fail.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        let mut entry = passwd(uid).unwrap_or(PasswdEntry { uid: 0, gid: 0, name: None, dir: None, shell: None });
        entry.uid = uid as i64;
        entry.gid = gid as i64;
        entry
    }
    #[cfg(not(unix))]
    PasswdEntry { uid: -1, gid: -1, name: None, dir: None, shell: None }
}

/// The login name of `uid` (`getpwuid`).
pub fn user_name(uid: u32) -> Option<String> {
    passwd(uid)?.name
}

macro_rules! set_id {
    ($name:ident, $ty:ident) => {
        pub fn $name(id: u32) -> R<()> {
            #[cfg(unix)]
            {
                // SAFETY: takes a plain id; the kernel validates it.
                check(unsafe { libc::$name(id as libc::$ty) })
            }
            #[cfg(not(unix))]
            {
                let _ = id;
                Err(FsError("ENOSYS"))
            }
        }
    };
}

set_id!(setuid, uid_t);
set_id!(seteuid, uid_t);
set_id!(setgid, gid_t);
set_id!(setegid, gid_t);

/// The supplementary group ids of the process (`getgroups`).
pub fn groups() -> R<Vec<u32>> {
    #[cfg(unix)]
    {
        // SAFETY: a zero-sized query writes nothing.
        let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
        if count < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut groups = vec![0 as libc::gid_t; count as usize];
        // SAFETY: `groups` has room for `count` ids.
        let count = unsafe { libc::getgroups(count, groups.as_mut_ptr()) };
        if count < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        groups.truncate(count as usize);
        Ok(groups)
    }
    #[cfg(not(unix))]
    Ok(Vec::new())
}

/// `setgroups(2)`.
pub fn setgroups(groups: &[u32]) -> R<()> {
    #[cfg(unix)]
    {
        let groups: Vec<libc::gid_t> = groups.iter().map(|&g| g as libc::gid_t).collect();
        // SAFETY: `groups` holds `len` ids.
        check(unsafe { libc::setgroups(groups.len() as _, groups.as_ptr()) })
    }
    #[cfg(not(unix))]
    {
        let _ = groups;
        Err(FsError("ENOSYS"))
    }
}

/// `initgroups(3)`: the groups of `user` plus `group`.
pub fn initgroups(user: &str, group: u32) -> R<()> {
    #[cfg(unix)]
    {
        let user = cstr(user)?;
        // SAFETY: `user` is a NUL-terminated string.
        check(unsafe { libc::initgroups(user.as_ptr(), group as _) })
    }
    #[cfg(not(unix))]
    {
        let _ = (user, group);
        Err(FsError("ENOSYS"))
    }
}

/// `execve(2)`: replaces the process image; returns only on failure.
pub fn execve(path: &str, argv: &[&str], env: &[&str]) -> FsError {
    let bytes = |l: &[&str]| l.iter().map(|s| s.as_bytes().to_vec()).collect::<Vec<_>>();
    crate::posix::exec(path.as_bytes(), None, &bytes(argv), Some(&bytes(env)))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn lookups() {
        let uid = crate::proc::getuid();
        if let Some(name) = user_name(uid) {
            assert_eq!(uid_of(&name), Some(uid));
        }
        assert!(uid_of("no-such-user-lumen").is_none());
        assert!(groups().is_ok());
    }
}
