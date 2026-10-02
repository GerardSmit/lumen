//! Exclusive value-graph ownership between single-threaded realms.
mod adopt;
pub(crate) mod api;
mod build;
mod classes;
mod parcel;
mod runtime;
pub use api::{install, install_with_limits, shutdown};
pub use parcel::{Limits, Parcel};
#[cfg(not(target_os = "none"))]
pub use runtime::ThreadHost;
pub use runtime::{
    CancelReason, CpuHint, Job, ParallelHost, Placement, SpawnError, TaskHandle, Turn, Worker,
};
#[cfg(test)]
mod tests;
