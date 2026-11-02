//! The `libc` crate plus the bionic calls and constants it does not bind for Android. Modules that
//! need them import this as `libc`. Every function here exists in bionic at API level 30 or lower.

#![allow(non_camel_case_types)]

pub use libc::*;

pub type posix_spawnattr_t = *mut c_void;
pub type posix_spawn_file_actions_t = *mut c_void;

pub const POSIX_SPAWN_RESETIDS: c_int = 1;
pub const POSIX_SPAWN_SETPGROUP: c_int = 2;
pub const POSIX_SPAWN_SETSIGDEF: c_int = 4;
pub const POSIX_SPAWN_SETSIGMASK: c_int = 8;
pub const POSIX_SPAWN_SETSID: c_int = 128;

pub const MREMAP_MAYMOVE: c_int = 1;
pub const SCHED_OTHER: c_int = 0;
pub const AT_EACCESS: c_int = 0x200;

extern "C" {
    /// API 26.
    pub fn pthread_getname_np(thread: pthread_t, name: *mut c_char, len: size_t) -> c_int;
    /// API 29.
    pub fn getloadavg(loadavg: *mut c_double, nelem: c_int) -> c_int;
    pub fn mremap(addr: *mut c_void, len: size_t, new_len: size_t, flags: c_int, ...) -> *mut c_void;

    // API 28.
    pub fn posix_spawn(
        pid: *mut pid_t,
        path: *const c_char,
        file_actions: *const posix_spawn_file_actions_t,
        attrp: *const posix_spawnattr_t,
        argv: *const *mut c_char,
        envp: *const *mut c_char,
    ) -> c_int;
    pub fn posix_spawnp(
        pid: *mut pid_t,
        file: *const c_char,
        file_actions: *const posix_spawn_file_actions_t,
        attrp: *const posix_spawnattr_t,
        argv: *const *mut c_char,
        envp: *const *mut c_char,
    ) -> c_int;
    pub fn posix_spawnattr_init(attr: *mut posix_spawnattr_t) -> c_int;
    pub fn posix_spawnattr_destroy(attr: *mut posix_spawnattr_t) -> c_int;
    pub fn posix_spawnattr_setflags(attr: *mut posix_spawnattr_t, flags: c_short) -> c_int;
    pub fn posix_spawnattr_setpgroup(attr: *mut posix_spawnattr_t, pgroup: pid_t) -> c_int;
    pub fn posix_spawnattr_setsigmask(attr: *mut posix_spawnattr_t, mask: *const sigset_t) -> c_int;
    pub fn posix_spawnattr_setsigdefault(attr: *mut posix_spawnattr_t, mask: *const sigset_t) -> c_int;
    pub fn posix_spawn_file_actions_init(actions: *mut posix_spawn_file_actions_t) -> c_int;
    pub fn posix_spawn_file_actions_destroy(actions: *mut posix_spawn_file_actions_t) -> c_int;
    pub fn posix_spawn_file_actions_addopen(
        actions: *mut posix_spawn_file_actions_t,
        fd: c_int,
        path: *const c_char,
        oflag: c_int,
        mode: mode_t,
    ) -> c_int;
    pub fn posix_spawn_file_actions_addclose(actions: *mut posix_spawn_file_actions_t, fd: c_int) -> c_int;
    pub fn posix_spawn_file_actions_adddup2(actions: *mut posix_spawn_file_actions_t, fd: c_int, newfd: c_int) -> c_int;
}
