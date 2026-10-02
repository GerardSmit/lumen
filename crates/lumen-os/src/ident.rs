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

/// The uid of user `name` (`getpwnam`).
pub fn uid_of(name: &str) -> Option<u32> {
    #[cfg(unix)]
    {
        let name = cstr(name).ok()?;
        // SAFETY: getpwnam returns null or a pointer to static storage valid until the next call.
        let entry = unsafe { libc::getpwnam(name.as_ptr()) };
        (!entry.is_null()).then(|| unsafe { (*entry).pw_uid } as u32)
    }
    #[cfg(not(unix))]
    {
        let _ = name;
        None
    }
}

/// The gid of group `name` (`getgrnam`).
pub fn gid_of(name: &str) -> Option<u32> {
    #[cfg(unix)]
    {
        let name = cstr(name).ok()?;
        // SAFETY: getgrnam returns null or a pointer to static storage valid until the next call.
        let entry = unsafe { libc::getgrnam(name.as_ptr()) };
        (!entry.is_null()).then(|| unsafe { (*entry).gr_gid } as u32)
    }
    #[cfg(not(unix))]
    {
        let _ = name;
        None
    }
}

/// The login name of `uid` (`getpwuid`).
pub fn user_name(uid: u32) -> Option<String> {
    #[cfg(unix)]
    {
        // SAFETY: getpwuid returns null or a pointer to static storage valid until the next call;
        // a non-null pw_name is a NUL-terminated C string.
        unsafe {
            let entry = libc::getpwuid(uid as libc::uid_t);
            if entry.is_null() || (*entry).pw_name.is_null() {
                return None;
            }
            Some(std::ffi::CStr::from_ptr((*entry).pw_name).to_string_lossy().into_owned())
        }
    }
    #[cfg(not(unix))]
    {
        let _ = uid;
        None
    }
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
    #[cfg(unix)]
    {
        let (Ok(path), Ok(argv), Ok(env)) = (
            cstr(path),
            argv.iter().map(|a| cstr(a)).collect::<R<Vec<_>>>(),
            env.iter().map(|e| cstr(e)).collect::<R<Vec<_>>>(),
        ) else {
            return FsError("EINVAL");
        };
        let mut argv_ptrs: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
        argv_ptrs.push(std::ptr::null());
        let mut env_ptrs: Vec<*const libc::c_char> = env.iter().map(|e| e.as_ptr()).collect();
        env_ptrs.push(std::ptr::null());
        // SAFETY: every pointer array is NUL-terminated and its strings outlive the call.
        unsafe { libc::execve(path.as_ptr(), argv_ptrs.as_ptr(), env_ptrs.as_ptr()) };
        std::io::Error::last_os_error().into()
    }
    #[cfg(not(unix))]
    {
        let _ = (path, argv, env);
        FsError("ENOSYS")
    }
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
