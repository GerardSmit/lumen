//! A direct interpreter for the IR: the oracle the optimizer and the backends are tested against.
//!
//! Memory and calls go through [`Env`], so tests can run functions over a byte buffer. An
//! undefined operation (see [`crate::ir`]) panics: the front ends must guard them, and a panic
//! here points at the front end, not the backend.

use crate::eval;
use crate::ir::*;

pub trait Env {
    fn load(&mut self, addr: u64, bytes: u32) -> u64;
    fn store(&mut self, addr: u64, bytes: u32, value: u64);
    fn call(&mut self, func: &ExtFunc, sig: &Signature, args: &[u64]) -> Result<Vec<u64>, u32>;
    fn call_indirect(&mut self, sig: &Signature, callee: u64, args: &[u64]) -> Result<Vec<u64>, u32>;
}

/// An [`Env`] over a byte buffer addressed from 0, with no callable functions.
pub struct BufferEnv {
    pub mem: Vec<u8>,
}

impl Env for BufferEnv {
    fn load(&mut self, addr: u64, bytes: u32) -> u64 {
        let a = addr as usize;
        let mut buf = [0u8; 8];
        buf[..bytes as usize].copy_from_slice(&self.mem[a..a + bytes as usize]);
        u64::from_le_bytes(buf)
    }
    fn store(&mut self, addr: u64, bytes: u32, value: u64) {
        let a = addr as usize;
        self.mem[a..a + bytes as usize].copy_from_slice(&value.to_le_bytes()[..bytes as usize]);
    }
    fn call(&mut self, func: &ExtFunc, _: &Signature, _: &[u64]) -> Result<Vec<u64>, u32> {
        panic!("BufferEnv: call to fn id {}", func.id)
    }
    fn call_indirect(&mut self, _: &Signature, callee: u64, _: &[u64]) -> Result<Vec<u64>, u32> {
        panic!("BufferEnv: indirect call to {callee:#x}")
    }
}

/// Run `func` on `args` (raw bits, see [`eval`]). `Err(code)` is a trap.
pub fn run(func: &Function, env: &mut dyn Env, args: &[u64]) -> Result<Vec<u64>, u32> {
    let mut vals = vec![0u64; func.values.len()];
    let mut block = func.entry();
    let mut incoming: Vec<u64> = args
        .iter()
        .zip(&func.sig.params)
        .map(|(&a, &t)| eval::norm(t, a))
        .collect();
    assert_eq!(incoming.len(), func.sig.params.len(), "argument count");
    loop {
        let data = &func.blocks[block.index()];
        for (&p, &v) in data.params.iter().zip(&incoming) {
            vals[p.index()] = v;
        }
        let get = |vals: &[u64], v: Value| vals[func.resolve(v).index()];
        let mut next = None;
        for &inst in &data.insts {
            let d = func.inst(inst);
            let results = func.results(inst);
            match d {
                InstData::Load { kind, addr, offset } => {
                    // An I32 address is held zero-extended, so both pointer widths work as is.
                    let a = get(&vals, *addr).wrapping_add(*offset as i64 as u64);
                    let raw = env.load(a, kind.bytes());
                    let v = if kind.is_signed() {
                        let sh = 64 - 8 * kind.bytes();
                        (((raw << sh) as i64) >> sh) as u64
                    } else {
                        raw
                    };
                    vals[results[0].index()] = eval::norm(kind.ty(), v);
                }
                InstData::Store {
                    kind,
                    addr,
                    value,
                    offset,
                } => {
                    let a = get(&vals, *addr).wrapping_add(*offset as i64 as u64);
                    env.store(a, kind.bytes(), get(&vals, *value));
                }
                InstData::Call { func: f, args } => {
                    let ext = &func.funcs[f.index()];
                    let a: Vec<u64> = args.iter().map(|&v| get(&vals, v)).collect();
                    let r = env.call(ext, &func.sigs[ext.sig.index()], &a)?;
                    for (&res, v) in results.iter().zip(r) {
                        vals[res.index()] = eval::norm(func.value_type(res), v);
                    }
                }
                InstData::CallIndirect { sig, callee, args } => {
                    let a: Vec<u64> = args.iter().map(|&v| get(&vals, v)).collect();
                    let r = env.call_indirect(&func.sigs[sig.index()], get(&vals, *callee), &a)?;
                    for (&res, v) in results.iter().zip(r) {
                        vals[res.index()] = eval::norm(func.value_type(res), v);
                    }
                }
                InstData::Trap { code } => return Err(*code),
                InstData::TrapIf { cond, code } => {
                    if get(&vals, *cond) as u32 != 0 {
                        return Err(*code);
                    }
                }
                InstData::Jump { dest } => next = Some(dest),
                InstData::Brif { cond, then, else_ } => {
                    next = Some(if get(&vals, *cond) as u32 != 0 { then } else { else_ });
                }
                InstData::BrTable {
                    index,
                    targets,
                    default,
                } => {
                    let i = get(&vals, *index) as u32 as usize;
                    next = Some(targets.get(i).unwrap_or(default));
                }
                InstData::Return { args } => {
                    return Ok(args.iter().map(|&v| get(&vals, v)).collect());
                }
                _ => {
                    let v = eval::pure_inst(func, d, |v| get(&vals, v))
                        .unwrap_or_else(|| panic!("{inst}: undefined operation {d:?}"));
                    vals[results[0].index()] = v;
                }
            }
        }
        let call = next.expect("block fell through without a terminator");
        incoming = call.args.iter().map(|&v| get(&vals, v)).collect();
        block = call.block;
    }
}
