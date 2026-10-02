//! The pure-Python standard library embedded in the binary, served as the lowest-priority
//! `sys.path` entry through an in-memory file system.

use crate::platform::MemFs;

/// Virtual directory that holds the embedded modules; `__file__` of an embedded module is
/// `<FROZEN_DIR>/<relative path>`.
pub const FROZEN_DIR: &str = "/<frozen>/lib";

mod generated {
    include!(concat!(env!("OUT_DIR"), "/frozen_table.rs"));
}

/// Relative path to source for every embedded module. Everything that reads embedded sources
/// goes through this table, so its representation can change (compressed, stripped) in one place.
pub fn table() -> &'static [(&'static str, &'static str)] {
    generated::TABLE
}

pub fn fs() -> MemFs {
    let mut fs = MemFs::new();
    for (rel, src) in table() {
        fs.insert_static(&format!("{}/{}", FROZEN_DIR, rel), src.as_bytes());
    }
    fs
}
