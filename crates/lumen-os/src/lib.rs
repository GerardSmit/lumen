//! Engine-neutral OS services: files, networking, process primitives, and
//! executable memory mappings shared by language runtimes and code generators.

pub mod child;
pub mod consts;
pub mod embedded;
pub mod errno;
pub mod fdctl;
pub mod fs;
pub mod ident;
pub mod http_body;
pub mod jitmem;
pub mod native;
pub mod net;
pub mod poll;
pub mod proc;
pub mod signal;
pub mod spawn;
pub mod sysinfo;
pub mod time;
pub mod uv;
pub mod vfs;

pub use errno::FsError;
