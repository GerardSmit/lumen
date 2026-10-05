//! Engine-neutral OS services: `errno` maps OS errors to libuv names and numeric errnos, `fs`
//! holds the file-system primitives both the Node and the Python runtimes are built on, `channel` the
//! thread-safe message queue behind JS message ports and Python interpreter channels, plus
//! executable memory mappings shared by language runtimes and code generators.

pub mod channel;
pub mod child;
pub mod consts;
#[cfg(unix)]
pub mod dynlib;
pub mod embedded;
pub mod errno;
pub mod event;
pub mod fdctl;
pub mod fs;
pub mod http_body;
pub mod ident;
pub mod ipc;
pub mod jitmem;
pub mod mmap;
pub mod native;
pub mod net;
pub mod poll;
pub mod posix;
pub mod proc;
pub mod rlimit;
pub mod signal;
pub mod spawn;
pub mod sysinfo;
pub mod syslog;
pub mod thread;
pub mod time;
pub mod tty;
pub mod uv;
pub mod vfs;

pub use errno::FsError;
