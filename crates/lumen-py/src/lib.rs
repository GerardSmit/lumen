//! lumen-py: a from-scratch Python 3 implementation in Rust, std only.
//!
//! Pipeline: lexer (tokens, including INDENT/DEDENT), parser (`ast::Module`), compiler
//! (symbol tables then bytecode) and a stack VM with heap-allocated frames, so generators and
//! coroutines suspend by keeping their frame. Objects are reference counted (`Rc`), which gives
//! CPython-like deterministic finalization.

extern crate self as lumen_py;

pub mod ast;
pub mod bind;
pub mod builtins;
pub mod bytecode;
pub mod call;
pub mod cli;
pub mod codecs;
pub mod compile;
pub mod containers;
pub mod dict;
pub mod exc;
pub mod import;
pub mod iter;
pub mod lexer;
pub mod fmath;
pub mod frozen;
pub mod limits;
pub mod num;
pub mod object;
pub mod ops;
pub mod parser;
pub mod platform;
pub mod pyint;
pub mod repr;
pub mod symtable;
pub mod types;
pub mod unicode;
pub mod vm;
pub mod weak;

pub use parser::{parse, SyntaxError};
pub use cli::run_main;
pub use lumen_common::limits::InterruptHandle;
pub use platform::{MemFs, MemPlatform, Platform, StdPlatform};
pub use vm::{Interp, Output, ProcessOutput};
/// The binding macros (see `lumen_bind`; the Python host is [`bind`]).
pub use lumen_bind::{class, methods, module, op};
