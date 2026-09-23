//! The WebAssembly backend: IR functions → one wasm module, for hosts where lumen itself runs as
//! wasm32 and native code cannot be generated. The embedder instantiates the module against
//! lumen's own linear memory and function table, adds the exported functions to that table, and
//! calls them through function pointers (on wasm32 a function pointer is a table index).
//!
//! Module shape:
//! - imports `env.memory` (memory32, min 0 pages) and `env.table` (funcref, min 0);
//! - defines one function per IR function, exported as `"f0"`, `"f1"`, …;
//! - `Call` to an external function with id `id` becomes `call_indirect` through table slot
//!   `resolve(id)`; `CallIndirect`'s callee value is itself a table index.
//!
//! No legalization runs: wasm implements every IR operation directly (the unchecked float → int
//! conversions trap out of range, which the IR leaves undefined). Structured control flow is
//! rebuilt from the CFG in [`func`]; the binary format is written by hand in [`encode`].

mod encode;
mod func;

use crate::ir::{Function, Signature};
use encode::*;

#[derive(Clone, Copy, Debug, Default)]
pub struct Config {
    /// Accept I32 address operands (Load/Store addresses and `CallIndirect` callees). I64
    /// addresses are always accepted and wrapped to 32 bits: memory is memory32 either way.
    pub ptr32: bool,
}

/// The module's function types, deduplicated.
#[derive(Default)]
pub(crate) struct Types(Vec<Signature>);

impl Types {
    fn index(&mut self, sig: &Signature) -> u32 {
        let i = self.0.iter().position(|s| s == sig).unwrap_or_else(|| {
            self.0.push(sig.clone());
            self.0.len() - 1
        });
        i as u32
    }
}

/// Compile `funcs` (each of which must verify) into one module. `resolve(id)` gives the table
/// index of external function `id`.
pub fn compile_module(
    funcs: &[Function],
    cfg: &Config,
    resolve: impl Fn(u32) -> Option<u32>,
) -> Result<Vec<u8>, String> {
    let mut types = Types::default();
    let mut func_types = Vec::with_capacity(funcs.len());
    let mut bodies = Vec::with_capacity(funcs.len());
    for f in funcs {
        let mut f = f.clone();
        f.resolve_aliases();
        crate::opt::remove_unreachable(&mut f);
        func_types.push(types.index(&f.sig));
        bodies.push(func::compile(&f, cfg, &mut types, &resolve)?);
    }

    let mut out = b"\0asm\x01\0\0\0".to_vec();
    let mut s = Vec::new();
    vec(&mut s, &types.0, |o, sig| {
        o.push(0x60);
        vec(o, &sig.params, |o, &t| o.push(valtype(t)));
        vec(o, &sig.results, |o, &t| o.push(valtype(t)));
    });
    section(&mut out, 1, &s);

    s.clear();
    uleb(&mut s, 2);
    name(&mut s, "env");
    name(&mut s, "memory");
    s.extend_from_slice(&[0x02, 0x00, 0x00]); // memory, limits {min 0}
    name(&mut s, "env");
    name(&mut s, "table");
    s.extend_from_slice(&[0x01, 0x70, 0x00, 0x00]); // table funcref, limits {min 0}
    section(&mut out, 2, &s);

    s.clear();
    vec(&mut s, &func_types, |o, &t| uleb(o, t as u64));
    section(&mut out, 3, &s);

    s.clear();
    uleb(&mut s, funcs.len() as u64);
    for i in 0..funcs.len() {
        name(&mut s, &format!("f{i}"));
        s.push(0x00);
        uleb(&mut s, i as u64);
    }
    section(&mut out, 7, &s);

    s.clear();
    vec(&mut s, &bodies, |o, b| {
        uleb(o, b.len() as u64);
        o.extend_from_slice(b);
    });
    section(&mut out, 10, &s);
    Ok(out)
}
