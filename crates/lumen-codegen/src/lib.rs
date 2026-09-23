//! lumen-codegen: a target-neutral optimizing compiler backend.
//!
//! Pipeline: front end → [`builder`] (SSA IR, [`ir`]) → [`opt`] → lowering → register
//! allocation → machine code for x86-64 or ARM64, or a WebAssembly module ([`wasm`]). [`interp`] runs IR directly and is the
//! reference every later stage is differentially tested against. See docs/jit.md.

pub mod builder;
pub mod cfg;
pub mod eval;
pub mod interp;
pub mod ir;
pub mod jitmem;
pub mod guard;
pub mod legalize;
pub mod machinst;
pub mod opt;
pub mod regalloc;
pub mod verify;
pub mod wasm;
pub mod x64;

pub use builder::{FunctionBuilder, Variable};
pub use ir::*;

#[cfg(test)]
mod tests;
