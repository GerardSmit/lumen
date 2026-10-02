//! Engine-neutral OS services: `errno` maps OS errors to libuv names and numeric errnos, `fs`
//! holds the file-system primitives both the Node and the Python runtimes are built on.

pub mod consts;
pub mod errno;
pub mod fdctl;
pub mod fs;
pub mod proc;
pub mod time;

pub use errno::FsError;
