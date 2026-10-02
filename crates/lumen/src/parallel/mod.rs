//! Exclusive value-graph ownership between single-threaded realms.
mod parcel;
mod build;
mod adopt;
pub use parcel::{Limits, Parcel};
#[cfg(test)]
mod tests;
