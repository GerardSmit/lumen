//! Alias classes for the translated code's memory accesses, so the optimizer can keep loads
//! across stores and calls that cannot touch them (and hoist loop-invariant guards, see
//! `lumen_codegen::version`).
//!
//! Most accesses are classified after translation by where their address comes from: the
//! unit's own [`JitFrame`] header, its slots and operand stack (always dereferenceable), its
//! constants, the interpreter, frames laid out on the shadow stack, and
//! everything else (object memory). The few accesses whose memory those rules cannot tell
//! apart are marked where they are emitted: reference counts ([`RC`]) and the [`Shadow`]
//! header ([`SHADOW_HDR`]). An address mixing origins keeps the default class, which may
//! alias anything.

use super::*;
use lumen_codegen::ir::{AliasClass, Function, InstData, MemFlags, Value};

/// The unit's own frame's slots.
const SLOTS: AliasClass = AliasClass(10);
/// The unit's own frame's operand stack.
const STACK: AliasClass = AliasClass::STACK;
/// The unit's own [`JitFrame`] header.
const HEADER: AliasClass = AliasClass::FRAME;
const OBJECTS: AliasClass = AliasClass::HEAP;
const CONSTS: AliasClass = AliasClass(5);
/// Strong and weak counts of reference-counted boxes.
pub(super) const RC: AliasClass = AliasClass(6);
/// Frames laid out above the shadow stack's top (inlined and direct calls).
const SHADOW: AliasClass = AliasClass(7);
/// The [`Shadow`] header (`top`, `end`).
pub(super) const SHADOW_HDR: AliasClass = AliasClass(8);
/// The `Interp` the unit's frame points at.
const INTERP: AliasClass = AliasClass(9);

pub(super) fn rc() -> MemFlags {
    MemFlags::new(RC)
}

pub(super) fn shadow_hdr() -> MemFlags {
    MemFlags::new(SHADOW_HDR).notrap()
}

// Address origins (bit sets).
const O_HEADER: u8 = 1;
const O_SLOTS: u8 = 2;
const O_STACK: u8 = 64;
const O_CONSTS: u8 = 4;
const O_SHADOW: u8 = 8;
const O_INTERP: u8 = 16;
const O_OTHER: u8 = 32;

/// Give each unclassified load and store of `func` (whose entry parameter is the unit's
/// frame) the class its address's origin determines.
pub(super) fn classify(func: &mut Function) {
    let entry = func.entry();
    let Some(&frame) = func.blocks[entry.index()].params.first() else {
        return;
    };
    let mut origin = vec![0u8; func.values.len()];
    origin[frame.index()] = O_HEADER;
    let blocks = func.layout.clone();
    // Block parameters merge their arguments' origins; iterate to a fixpoint.
    loop {
        let mut changed = false;
        for &b in &blocks {
            for &i in &func.blocks[b.index()].insts {
                let data = func.inst(i);
                let res: u8 = match data {
                    InstData::Load { addr, offset, .. } => {
                        let a = func.resolve(*addr);
                        if func.mem_flags(i).class == SHADOW_HDR {
                            O_SHADOW
                        } else if a == frame && *offset == FRAME_SLOTS {
                            O_SLOTS
                        } else if a == frame && *offset == FRAME_STACK {
                            O_STACK
                        } else if a == frame && *offset == FRAME_CONSTS {
                            O_CONSTS
                        } else if a == frame && *offset == FRAME_INTERP {
                            O_INTERP
                        } else if origin[a.index()] == O_INTERP
                            && i32::try_from(std::mem::offset_of!(crate::interpreter::Interp, jit_frames)) == Ok(*offset)
                        {
                            // The head of the pending call records, which live on the shadow stack.
                            O_SHADOW
                        } else if origin[a.index()] & O_SHADOW != 0 {
                            // A frame on the shadow stack holds pointers into it.
                            O_SHADOW | O_OTHER
                        } else {
                            O_OTHER
                        }
                    }
                    InstData::IntCmp { .. } | InstData::FloatCmp { .. } => 0,
                    InstData::Call { .. } | InstData::CallIndirect { .. } => O_OTHER,
                    d if d.is_terminator() => {
                        let mut out: Vec<(Value, Value)> = Vec::new();
                        for c in d.successors().iter() {
                            let params = &func.blocks[c.block.index()].params;
                            for (k, &a) in c.args.iter().enumerate() {
                                out.push((params[k], func.resolve(a)));
                            }
                        }
                        for (p, a) in out {
                            let o = origin[p.index()] | origin[a.index()];
                            if o != origin[p.index()] {
                                origin[p.index()] = o;
                                changed = true;
                            }
                        }
                        continue;
                    }
                    d => {
                        let mut o = 0;
                        d.for_each_arg(|a| o |= origin[func.resolve(a).index()]);
                        o
                    }
                };
                for &r in func.results(i) {
                    if origin[r.index()] | res != origin[r.index()] {
                        origin[r.index()] |= res;
                        changed = true;
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }
    for &b in &blocks {
        let insts = func.blocks[b.index()].insts.clone();
        for i in insts {
            let addr = match func.inst(i) {
                InstData::Load { addr, .. } | InstData::Store { addr, .. } => *addr,
                _ => continue,
            };
            if func.mem_flags(i).class.is_known() {
                continue;
            }
            let flags = match origin[func.resolve(addr).index()] {
                0 | O_OTHER => MemFlags::new(OBJECTS),
                O_HEADER => MemFlags::new(HEADER).notrap(),
                O_SLOTS => MemFlags::new(SLOTS).notrap(),
                O_STACK => MemFlags::new(STACK).notrap(),
                O_CONSTS => MemFlags::new(CONSTS).notrap(),
                O_SHADOW => MemFlags::new(SHADOW),
                O_INTERP => MemFlags::new(INTERP).notrap(),
                _ => continue,
            };
            func.set_mem_flags(i, flags);
        }
    }
}
