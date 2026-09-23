//! IR-to-IR rewrites that run before lowering, so emitters only see operations the target has a
//! direct pattern for.
//!
//! Expansions that need control flow (unsigned 64-bit conversions, saturating truncations) split
//! the block and join through a block parameter, so the register allocator sees every temporary
//! instead of emitters needing hidden scratch registers.

use crate::ir::*;

/// Which operations the target implements natively.
#[derive(Clone, Copy, Debug)]
pub struct Legal {
    /// `FromUint` from I64.
    pub from_u64: bool,
    /// `ToUint` (unchecked).
    pub to_uint: bool,
    /// Saturating float → int conversions.
    pub to_int_sat: bool,
    /// `srem` defined for `MIN % -1` (x64 `idiv` faults on it).
    pub srem_min_neg1: bool,
    pub fcopysign: bool,
}

pub fn legalize(func: &mut Function, legal: Legal) {
    // Work over a snapshot of placed instructions; expansions queue what they create.
    let mut work: Vec<Inst> = func
        .layout
        .iter()
        .flat_map(|&b| func.blocks[b.index()].insts.clone())
        .collect();
    work.reverse();
    while let Some(inst) = work.pop() {
        let Some((block, pos)) = locate(func, inst) else {
            continue;
        };
        let data = func.inst(inst).clone();
        match data {
            InstData::Unary {
                op: UnaryOp::Eqz,
                arg,
            } => {
                let ty = func.value_type(arg);
                let zero = insert(func, block, pos, InstData::Iconst { ty, imm: 0 });
                func.insts[inst.index()] = InstData::IntCmp {
                    cc: IntCC::Eq,
                    args: [arg, zero],
                };
            }
            InstData::Binary {
                op: BinaryOp::Srem,
                args: [a, b],
            } if !legal.srem_min_neg1 => {
                // x % -1 == x % 1 == 0, and a divisor of 1 cannot fault.
                let ty = func.value_type(b);
                let m1 = insert(func, block, pos, InstData::Iconst { ty, imm: -1 });
                let one = insert(func, block, pos + 1, InstData::Iconst { ty, imm: 1 });
                let is = insert(
                    func,
                    block,
                    pos + 2,
                    InstData::IntCmp {
                        cc: IntCC::Eq,
                        args: [b, m1],
                    },
                );
                let safe = insert(
                    func,
                    block,
                    pos + 3,
                    InstData::Select {
                        cond: is,
                        if_true: one,
                        if_false: b,
                    },
                );
                func.insts[inst.index()] = InstData::Binary {
                    op: BinaryOp::Srem,
                    args: [a, safe],
                };
            }
            InstData::Binary {
                op: BinaryOp::Fcopysign,
                args: [a, b],
            } if !legal.fcopysign => {
                let fty = func.value_type(a);
                let ity = if fty == Type::F32 { Type::I32 } else { Type::I64 };
                let sign: i64 = if fty == Type::F32 { 0x8000_0000u32 as i32 as i64 } else { i64::MIN };
                let mut p = pos;
                let mut ins = |func: &mut Function, d: InstData| {
                    let v = insert(func, block, p, d);
                    p += 1;
                    v
                };
                let ai = ins(func, InstData::Convert { op: ConvOp::Bitcast, to: ity, arg: a });
                let bi = ins(func, InstData::Convert { op: ConvOp::Bitcast, to: ity, arg: b });
                let s = ins(func, InstData::Iconst { ty: ity, imm: sign });
                let ns = ins(func, InstData::Iconst { ty: ity, imm: !sign });
                let am = ins(func, InstData::Binary { op: BinaryOp::Band, args: [ai, ns] });
                let bm = ins(func, InstData::Binary { op: BinaryOp::Band, args: [bi, s] });
                let r = ins(func, InstData::Binary { op: BinaryOp::Bor, args: [am, bm] });
                func.insts[inst.index()] = InstData::Convert {
                    op: ConvOp::Bitcast,
                    to: fty,
                    arg: r,
                };
            }
            InstData::Convert {
                op: ConvOp::FromUint,
                to,
                arg,
            } if !legal.from_u64 => {
                if func.value_type(arg) == Type::I32 {
                    let wide = insert(
                        func,
                        block,
                        pos,
                        InstData::Convert {
                            op: ConvOp::Uext,
                            to: Type::I64,
                            arg,
                        },
                    );
                    func.insts[inst.index()] = InstData::Convert {
                        op: ConvOp::FromSint,
                        to,
                        arg: wide,
                    };
                } else {
                    expand_from_u64(func, block, pos, inst, to, arg);
                }
            }
            InstData::Convert {
                op: ConvOp::ToUint,
                to,
                arg,
            } if !legal.to_uint => {
                if to == Type::I32 {
                    let wide = insert(
                        func,
                        block,
                        pos,
                        InstData::Convert {
                            op: ConvOp::ToSint,
                            to: Type::I64,
                            arg,
                        },
                    );
                    func.insts[inst.index()] = InstData::Convert {
                        op: ConvOp::Wrap,
                        to: Type::I32,
                        arg: wide,
                    };
                } else {
                    expand_to_u64(func, block, pos, inst, arg);
                }
            }
            InstData::Convert {
                op: op @ (ConvOp::ToSintSat | ConvOp::ToUintSat),
                to,
                arg,
            } if !legal.to_int_sat => {
                let created = expand_sat(func, block, pos, inst, op == ConvOp::ToSintSat, to, arg);
                work.extend(created);
            }
            _ => {}
        }
    }
    func.resolve_aliases();
}

fn locate(func: &Function, inst: Inst) -> Option<(Block, usize)> {
    for &b in &func.layout {
        if let Some(p) = func.blocks[b.index()].insts.iter().position(|&i| i == inst) {
            return Some((b, p));
        }
    }
    None
}

/// Insert a single-result instruction at `pos` in `block`.
fn insert(func: &mut Function, block: Block, pos: usize, data: InstData) -> Value {
    let inst = func.make_inst(data);
    func.blocks[block.index()].insts.insert(pos, inst);
    func.results(inst)[0]
}

fn append(func: &mut Function, block: Block, data: InstData) -> Option<Value> {
    let inst = func.make_inst(data);
    func.blocks[block.index()].insts.push(inst);
    func.results(inst).first().copied()
}

fn new_block_after(func: &mut Function, after: Block) -> Block {
    let b = func.create_block();
    let at = func.layout.iter().position(|&x| x == after).unwrap();
    func.layout.insert(at + 1, b);
    b
}

/// Split `block` before instruction index `pos`: the instructions from `pos` on (which must start
/// with `inst`, removed) move to a new join block whose single parameter replaces `inst`'s result.
/// Returns `(join, param)`; `block` is left unterminated.
fn split_at(func: &mut Function, block: Block, pos: usize, inst: Inst, ty: Type) -> (Block, Value) {
    let join = new_block_after(func, block);
    let tail = func.blocks[block.index()].insts.split_off(pos);
    debug_assert_eq!(tail[0], inst);
    func.blocks[join.index()].insts = tail[1..].to_vec();
    let p = func.append_block_param(join, ty);
    let r = func.results(inst)[0];
    func.replace_uses(r, p);
    (join, p)
}

fn jump(func: &mut Function, from: Block, to: Block, args: Vec<Value>) {
    append(
        func,
        from,
        InstData::Jump {
            dest: BlockCall { block: to, args },
        },
    );
}

fn brif(func: &mut Function, from: Block, cond: Value, then: Block, else_: Block) {
    append(
        func,
        from,
        InstData::Brif {
            cond,
            then: BlockCall {
                block: then,
                args: vec![],
            },
            else_: BlockCall {
                block: else_,
                args: vec![],
            },
        },
    );
}

fn fconst(ty: Type, v: f64) -> InstData {
    if ty == Type::F32 {
        InstData::F32const {
            bits: (v as f32).to_bits(),
        }
    } else {
        InstData::F64const { bits: v.to_bits() }
    }
}

/// `u64 → float`: values below 2^63 convert signed; larger ones are halved (keeping the low bit
/// for correct rounding), converted, and doubled.
fn expand_from_u64(func: &mut Function, block: Block, pos: usize, inst: Inst, to: Type, x: Value) {
    let (join, _) = split_at(func, block, pos, inst, to);
    let small = new_block_after(func, block);
    let big = new_block_after(func, small);
    let zero = append(func, block, InstData::Iconst { ty: Type::I64, imm: 0 }).unwrap();
    let neg = append(func, block, InstData::IntCmp { cc: IntCC::Slt, args: [x, zero] }).unwrap();
    brif(func, block, neg, big, small);

    let r1 = append(func, small, InstData::Convert { op: ConvOp::FromSint, to, arg: x }).unwrap();
    jump(func, small, join, vec![r1]);

    let one = append(func, big, InstData::Iconst { ty: Type::I64, imm: 1 }).unwrap();
    let h = append(func, big, InstData::Binary { op: BinaryOp::Ushr, args: [x, one] }).unwrap();
    let l = append(func, big, InstData::Binary { op: BinaryOp::Band, args: [x, one] }).unwrap();
    let t = append(func, big, InstData::Binary { op: BinaryOp::Bor, args: [h, l] }).unwrap();
    let f = append(func, big, InstData::Convert { op: ConvOp::FromSint, to, arg: t }).unwrap();
    let r2 = append(func, big, InstData::Binary { op: BinaryOp::Fadd, args: [f, f] }).unwrap();
    jump(func, big, join, vec![r2]);
}

/// Unchecked `float → u64` for inputs in `[0, 2^64)`.
fn expand_to_u64(func: &mut Function, block: Block, pos: usize, inst: Inst, x: Value) {
    let fty = func.value_type(x);
    let (join, _) = split_at(func, block, pos, inst, Type::I64);
    let small = new_block_after(func, block);
    let big = new_block_after(func, small);
    let c = append(func, block, fconst(fty, 9223372036854775808.0)).unwrap();
    let ge = append(func, block, InstData::FloatCmp { cc: FloatCC::Ge, args: [x, c] }).unwrap();
    brif(func, block, ge, big, small);

    let r1 = append(func, small, InstData::Convert { op: ConvOp::ToSint, to: Type::I64, arg: x }).unwrap();
    jump(func, small, join, vec![r1]);

    let d = append(func, big, InstData::Binary { op: BinaryOp::Fsub, args: [x, c] }).unwrap();
    let t = append(func, big, InstData::Convert { op: ConvOp::ToSint, to: Type::I64, arg: d }).unwrap();
    let m = append(func, big, InstData::Iconst { ty: Type::I64, imm: i64::MIN }).unwrap();
    let r2 = append(func, big, InstData::Binary { op: BinaryOp::Bxor, args: [t, m] }).unwrap();
    jump(func, big, join, vec![r2]);
}

/// Saturating truncation: NaN → 0, below range → MIN, above → MAX, else the unchecked form.
/// Returns created instructions that may need legalizing themselves.
fn expand_sat(
    func: &mut Function,
    block: Block,
    pos: usize,
    inst: Inst,
    signed: bool,
    to: Type,
    x: Value,
) -> Vec<Inst> {
    let fty = func.value_type(x);
    // Exclusive bounds: in range iff lo < x < hi (see the wasm translator's `trunc`).
    let (lo, hi): (f64, f64) = match (to, signed, fty) {
        (Type::I32, true, Type::F64) => (-2147483649.0, 2147483648.0),
        (Type::I32, true, _) => (-2147483904.0, 2147483648.0),
        (Type::I32, false, _) => (-1.0, 4294967296.0),
        (Type::I64, true, Type::F64) => (-9223372036854777856.0, 9223372036854775808.0),
        (Type::I64, true, _) => (-9223373136366403584.0, 9223372036854775808.0),
        (Type::I64, false, _) => (-1.0, 18446744073709551616.0),
        _ => unreachable!(),
    };
    let (min, max): (i64, i64) = match (to, signed) {
        (Type::I32, true) => (i32::MIN as i64, i32::MAX as i64),
        (Type::I32, false) => (0, u32::MAX as i32 as i64),
        (Type::I64, true) => (i64::MIN, i64::MAX),
        (Type::I64, false) => (0, -1),
        _ => unreachable!(),
    };
    let (join, _) = split_at(func, block, pos, inst, to);
    let nan_b = new_block_after(func, block);
    let lo_b = new_block_after(func, nan_b);
    let min_b = new_block_after(func, lo_b);
    let hi_b = new_block_after(func, min_b);
    let max_b = new_block_after(func, hi_b);
    let conv_b = new_block_after(func, max_b);

    let nan = append(func, block, InstData::FloatCmp { cc: FloatCC::Ne, args: [x, x] }).unwrap();
    brif(func, block, nan, nan_b, lo_b);
    let z = append(func, nan_b, InstData::Iconst { ty: to, imm: 0 }).unwrap();
    jump(func, nan_b, join, vec![z]);

    let lc = append(func, lo_b, fconst(fty, lo)).unwrap();
    let below = append(func, lo_b, InstData::FloatCmp { cc: FloatCC::Le, args: [x, lc] }).unwrap();
    brif(func, lo_b, below, min_b, hi_b);
    let mv = append(func, min_b, InstData::Iconst { ty: to, imm: min }).unwrap();
    jump(func, min_b, join, vec![mv]);

    let hc = append(func, hi_b, fconst(fty, hi)).unwrap();
    let above = append(func, hi_b, InstData::FloatCmp { cc: FloatCC::Ge, args: [x, hc] }).unwrap();
    brif(func, hi_b, above, max_b, conv_b);
    let xv = append(func, max_b, InstData::Iconst { ty: to, imm: max }).unwrap();
    jump(func, max_b, join, vec![xv]);

    let op = if signed { ConvOp::ToSint } else { ConvOp::ToUint };
    let conv = func.make_inst(InstData::Convert { op, to, arg: x });
    func.blocks[conv_b.index()].insts.push(conv);
    let r = func.results(conv)[0];
    jump(func, conv_b, join, vec![r]);
    vec![conv]
}

/// Split every critical edge that carries block arguments, so parallel moves for block
/// parameters always sit before an unconditional jump. The new block is placed right after its
/// source, which keeps a loop latch's back edge a single taken jump.
pub fn split_critical_edges(func: &mut Function) {
    let layout = func.layout.clone();
    for b in layout {
        let Some(term) = func.terminator(b) else {
            continue;
        };
        let nsucc = func.inst(term).successors().len();
        if nsucc < 2 {
            continue;
        }
        let mut after = b;
        for slot in 0..nsucc {
            let call = func.inst(term).successors()[slot].clone();
            if call.args.is_empty() {
                continue;
            }
            let s = func.create_block();
            let at = func.layout.iter().position(|&x| x == after).unwrap();
            func.layout.insert(at + 1, s);
            after = s;
            let j = func.make_inst(InstData::Jump { dest: call });
            func.blocks[s.index()].insts.push(j);
            *func.insts[term.index()].successors_mut()[slot] = BlockCall {
                block: s,
                args: vec![],
            };
        }
    }
}
