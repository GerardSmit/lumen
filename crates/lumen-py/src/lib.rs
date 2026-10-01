//! lumen-py: a from-scratch Python 3 implementation in Rust, std only.
//!
//! Pipeline: lexer (tokens, including INDENT/DEDENT), parser (`ast::Module`), compiler
//! (symbol tables then bytecode) and a stack VM with heap-allocated frames, so generators and
//! coroutines suspend by keeping their frame. Objects are reference counted (`Rc`), which gives
//! CPython-like deterministic finalization.

pub mod ast;
pub mod builtins;
pub mod bytecode;
pub mod call;
pub mod cli;
pub mod compile;
pub mod containers;
pub mod dict;
pub mod exc;
pub mod import;
pub mod iter;
pub mod lexer;
pub mod limits;
pub mod num;
pub mod object;
pub mod ops;
pub mod parser;
pub mod pyint;
pub mod repr;
pub mod symtable;
pub mod types;
pub mod unicode;
pub mod vm;

pub use parser::{parse, SyntaxError};
pub use cli::run_main;
pub use limits::InterruptHandle;
pub use vm::{Interp, Output, ProcessOutput};
