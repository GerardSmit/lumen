//! IR → x64 [`MInst`]s over virtual registers.
//!
//! Instruction selection follows RyuJIT's containment (docs/jit-notes/lowering-emit.md):
//! - integer constants that fit an `imm32` become immediates of their user (commutative
//!   operands are swapped to get there); constants needed in a register are rematerialized at
//!   each use, so they never hold a register across a loop;
//! - a compare whose only use is a `brif`/`select`/`trap_if` in the same block is emitted right
//!   before it and feeds `jcc`/`cmov` directly from the flags;
//! - an `iadd` whose only use is a load or store address folds into the address mode;
//! - `uext` is free: I32 values are always kept zero-extended in their 64-bit register.

use super::inst::*;
use super::regs::*;
use super::Features;
use crate::cfg::Cfg;
use crate::eval;
use crate::ir::*;
use crate::machinst::*;

pub struct Lowered {
    pub vcode: VCode<MInst>,
    /// Bytes of outgoing argument space (including Win64 home space) any call needs.
    pub outgoing: u32,
    pub has_traps: bool,
}

/// Where an argument lives: a register, or a stack offset from the first stack argument slot
/// (on Win64 that includes the 32 bytes of home space).
#[derive(Clone, Copy)]
pub enum ArgLoc {
    Reg(PReg),
    Stack(i32),
}

/// Assign argument locations, and the size of the stack argument area.
pub fn arg_locs(abi: &Abi, tys: &[Type]) -> (Vec<ArgLoc>, u32) {
    let mut out = Vec::with_capacity(tys.len());
    let (mut ni, mut nf, mut ns) = (0, 0, 0);
    for (i, t) in tys.iter().enumerate() {
        let (regs, n) = if t.is_float() {
            (abi.float_args, &mut nf)
        } else {
            (abi.int_args, &mut ni)
        };
        let idx = if abi.positional { i } else { *n };
        if idx < regs.len() {
            out.push(ArgLoc::Reg(regs[idx]));
            *n += 1;
        } else if abi.positional {
            out.push(ArgLoc::Stack(8 * i as i32));
        } else {
            out.push(ArgLoc::Stack(8 * ns));
            ns += 1;
        }
    }
    let size = if abi.positional {
        abi.shadow.max(8 * tys.len() as u32)
    } else {
        8 * ns as u32
    };
    (out, size)
}

fn ret_reg(t: Type) -> PReg {
    if t.is_float() {
        xmm(0)
    } else {
        RAX
    }
}

fn size_of(t: Type) -> Size {
    if t.bits() == 64 {
        Size::S64
    } else {
        Size::S32
    }
}

fn class_of(t: Type) -> RegClass {
    if t.is_float() {
        RegClass::Float
    } else {
        RegClass::Int
    }
}

fn int_cc(cc: IntCC) -> CC {
    match cc {
        IntCC::Eq => CC::E,
        IntCC::Ne => CC::NE,
        IntCC::Slt => CC::L,
        IntCC::Sle => CC::LE,
        IntCC::Sgt => CC::G,
        IntCC::Sge => CC::GE,
        IntCC::Ult => CC::B,
        IntCC::Ule => CC::BE,
        IntCC::Ugt => CC::A,
        IntCC::Uge => CC::AE,
    }
}

struct Lower<'a> {
    f: &'a Function,
    abi: &'a Abi,
    feat: &'a Features,
    vc: VCode<MInst>,
    vregs: Vec<Option<VReg>>,
    /// `uext` results map to their operand.
    alias: Vec<Value>,
    /// Instructions emitted at their single user instead of at their definition.
    sunk: Vec<bool>,
    bmap: Vec<usize>,
    outgoing: u32,
    has_traps: bool,
}

pub fn lower(f: &Function, cfg: &Cfg, abi: &Abi, feat: &Features) -> Result<Lowered, String> {
    if f.sig.results.len() > 1 {
        return Err("x64: multiple results are not supported".into());
    }
    let nv = f.values.len();
    let mut alias: Vec<Value> = (0..nv as u32).map(Value).collect();
    let mut uses = vec![0u32; nv];
    let mut user: Vec<Option<Inst>> = vec![None; nv];
    let mut inst_block: Vec<Option<Block>> = vec![None; f.insts.len()];
    for &b in &cfg.rpo {
        for &i in &f.blocks[b.index()].insts {
            inst_block[i.index()] = Some(b);
            f.inst(i).for_each_arg(|a| {
                uses[a.index()] += 1;
                user[a.index()] = Some(i);
            });
            if let InstData::Convert {
                op: ConvOp::Uext,
                arg,
                ..
            } = f.inst(i)
            {
                let r = f.results(i)[0];
                alias[r.index()] = alias[arg.index()];
            }
        }
    }
    let mut sunk = vec![false; nv];
    for &b in &cfg.rpo {
        for &i in &f.blocks[b.index()].insts {
            let [r] = f.results(i) else { continue };
            let Some(u) = user[r.index()] else { continue };
            if uses[r.index()] != 1 || inst_block[u.index()] != Some(b) {
                continue;
            }
            let ok = match f.inst(i) {
                InstData::IntCmp { .. } | InstData::FloatCmp { .. } => matches!(f.inst(u),
                    InstData::Brif { cond, .. } | InstData::Select { cond, .. }
                        | InstData::TrapIf { cond, .. } if cond == r),
                InstData::Binary {
                    op: BinaryOp::Iadd, ..
                } => {
                    f.value_type(*r) == Type::I64
                        && matches!(f.inst(u),
                            InstData::Load { addr, .. } | InstData::Store { addr, .. } if addr == r)
                }
                _ => false,
            };
            sunk[r.index()] = ok;
        }
    }

    let entry = f.entry();
    let pre_entry = !cfg.preds[entry.index()].is_empty();
    let order = rotate_loops(
        f,
        cfg,
        f.layout.iter().copied().filter(|&b| cfg.is_reachable(b)).collect(),
    );
    let mut bmap = vec![usize::MAX; f.blocks.len()];
    for (i, &b) in order.iter().enumerate() {
        bmap[b.index()] = i + pre_entry as usize;
    }

    let mut l = Lower {
        f,
        abi,
        feat,
        vc: VCode::new(),
        vregs: vec![None; nv],
        alias,
        sunk,
        bmap,
        outgoing: 0,
        has_traps: false,
    };

    if pre_entry {
        let params: Vec<VReg> = f.blocks[entry.index()]
            .params
            .iter()
            .map(|&p| l.vc.new_vreg(class_of(f.value_type(p))))
            .collect();
        l.args(&params);
        let args = params.clone();
        l.vc.insts.push(MInst::Jmp { target: 1, args });
        l.vc.blocks.push(VBlock {
            params: Vec::new(),
            start: 0,
            end: l.vc.insts.len(),
            succs: vec![1],
            loop_depth: 0,
        });
    }

    for &b in &order {
        let start = l.vc.insts.len();
        let bparams: Vec<Value> = f.blocks[b.index()].params.clone();
        let params = if b == entry && !pre_entry {
            let targets: Vec<VReg> = bparams.iter().map(|&p| l.vreg(p)).collect();
            l.args(&targets);
            Vec::new()
        } else {
            bparams.iter().map(|&p| l.vreg(p)).collect()
        };
        for &i in &f.blocks[b.index()].insts {
            l.inst(i)?;
        }
        let succs = f.successors(b).iter().map(|s| l.bmap[s.index()]).collect();
        l.vc.blocks.push(VBlock {
            params,
            start,
            end: l.vc.insts.len(),
            succs,
            loop_depth: cfg.loop_depth[b.index()],
        });
    }

    Ok(Lowered {
        vcode: l.vc,
        outgoing: l.outgoing,
        has_traps: l.has_traps,
    })
}

/// Loop rotation by layout: a loop header that ends in a conditional branch moves to just after
/// its last latch, so the back edge falls through into the exit test (`cmp; jcc body`) and each
/// iteration takes one branch instead of two. Entry jumps to the test once.
fn rotate_loops(f: &Function, cfg: &Cfg, mut order: Vec<Block>) -> Vec<Block> {
    let is_brif = |b: Block| {
        f.terminator(b)
            .is_some_and(|t| matches!(f.inst(t), InstData::Brif { .. }))
    };
    let is_jump_to = |b: Block, h: Block| {
        f.terminator(b)
            .is_some_and(|t| matches!(f.inst(t), InstData::Jump { dest } if dest.block == h))
    };
    let headers: Vec<Block> = order
        .iter()
        .copied()
        .filter(|&h| h != f.entry() && is_brif(h))
        .collect();
    for h in headers {
        let pos = |order: &[Block], b: Block| order.iter().position(|&x| x == b);
        let Some(hp) = pos(&order, h) else { continue };
        // The back edge from the latest block in layout (a latch the header dominates).
        let latch = cfg.preds[h.index()]
            .iter()
            .copied()
            .filter(|&p| cfg.dominates(h, p))
            .filter_map(|p| pos(&order, p).map(|i| (i, p)))
            .max();
        let Some((lp, l)) = latch else { continue };
        if lp <= hp || !is_jump_to(l, h) {
            continue;
        }
        order.remove(hp);
        order.insert(lp, h);
    }
    order
}

impl Lower<'_> {
    fn push(&mut self, i: MInst) {
        self.vc.insts.push(i);
    }

    fn ty(&self, v: Value) -> Type {
        self.f.value_type(v)
    }

    fn vreg(&mut self, v: Value) -> VReg {
        let v = self.alias[v.index()];
        if let Some(r) = self.vregs[v.index()] {
            return r;
        }
        let r = self.vc.new_vreg(class_of(self.ty(v)));
        self.vregs[v.index()] = Some(r);
        r
    }

    fn fresh(&mut self, t: Type) -> VReg {
        self.vc.new_vreg(class_of(t))
    }

    fn def(&self, v: Value) -> Option<&InstData> {
        match self.f.values[self.alias[v.index()].index()].def {
            ValueDef::Result(i, 0) => Some(self.f.inst(i)),
            _ => None,
        }
    }

    /// The constant bits of an integer constant (normalized to its type).
    fn iconst(&self, v: Value) -> Option<u64> {
        match self.def(v) {
            Some(InstData::Iconst { ty, imm }) => Some(eval::iconst(*ty, *imm)),
            _ => None,
        }
    }

    /// `v` as the `imm32` of a `size` operation.
    fn imm(&self, v: Value, size: Size) -> Option<i32> {
        let bits = self.iconst(v)?;
        match size {
            Size::S32 => Some(bits as u32 as i32),
            Size::S64 => i32::try_from(bits as i64).ok(),
        }
    }

    /// `v` in a virtual register, rematerializing constants at the use.
    fn use_val(&mut self, v: Value) -> VReg {
        match self.def(v).cloned() {
            Some(InstData::Iconst { .. }) | Some(InstData::F32const { .. }) | Some(InstData::F64const { .. }) => {
                let r = self.fresh(self.ty(self.alias[v.index()]));
                self.copy_into(r, v);
                r
            }
            _ => self.vreg(v),
        }
    }

    /// `dst = v`
    fn copy_into(&mut self, dst: VReg, v: Value) {
        match self.def(v).cloned() {
            Some(InstData::Iconst { .. }) => {
                let imm = self.iconst(v).unwrap();
                self.push(MInst::MovImm {
                    size: Size::S64,
                    dst,
                    imm,
                });
            }
            Some(InstData::F32const { bits }) => self.push(MInst::XmmConst {
                double: false,
                dst,
                bits: bits as u64,
            }),
            Some(InstData::F64const { bits }) => self.push(MInst::XmmConst {
                double: true,
                dst,
                bits,
            }),
            _ => {
                let src = self.vreg(v);
                if self.ty(v).is_float() {
                    self.push(MInst::XmmMov {
                        double: true,
                        dst,
                        src,
                    });
                } else {
                    self.push(MInst::Mov {
                        size: Size::S64,
                        dst,
                        src,
                    });
                }
            }
        }
    }

    fn reg_imm(&mut self, v: Value, size: Size) -> RegImm {
        match self.imm(v, size) {
            Some(i) => RegImm::Imm(i),
            None => RegImm::Reg(self.use_val(v)),
        }
    }

    /// Function entry: move parameters from their ABI locations into `targets`, normalizing
    /// I32 parameters (the ABI leaves their upper halves undefined).
    fn args(&mut self, targets: &[VReg]) {
        let tys = self.f.sig.params.clone();
        let (locs, _) = arg_locs(self.abi, &tys);
        let mut regs = Vec::new();
        let mut stack = Vec::new();
        let mut post = Vec::new();
        for (i, (&t, loc)) in tys.iter().zip(locs).enumerate() {
            let d = if t == Type::I32 {
                let tmp = self.fresh(t);
                post.push(MInst::Mov {
                    size: Size::S32,
                    dst: targets[i],
                    src: tmp,
                });
                tmp
            } else {
                targets[i]
            };
            match loc {
                ArgLoc::Reg(r) => regs.push((d, r)),
                ArgLoc::Stack(off) => stack.push((d, 16 + off)),
            }
        }
        self.push(MInst::Args { regs, stack });
        for m in post {
            self.push(m);
        }
    }

    fn int_cmp(&mut self, cc: IntCC, a: Value, b: Value) -> CC {
        let size = size_of(self.ty(a));
        if self.imm(b, size) == Some(0) {
            let r = self.use_val(a);
            self.push(MInst::Test { size, a: r, b: r });
            int_cc(cc)
        } else if let Some(i) = self.imm(b, size) {
            let r = self.use_val(a);
            self.push(MInst::Cmp {
                size,
                a: r,
                b: RegImm::Imm(i),
            });
            int_cc(cc)
        } else if let Some(i) = self.imm(a, size) {
            let r = self.use_val(b);
            self.push(MInst::Cmp {
                size,
                a: r,
                b: RegImm::Imm(i),
            });
            int_cc(cc.swap())
        } else {
            let ra = self.use_val(a);
            let rb = self.use_val(b);
            self.push(MInst::Cmp {
                size,
                a: ra,
                b: RegImm::Reg(rb),
            });
            int_cc(cc)
        }
    }

    fn float_cmp(&mut self, cc: FloatCC, a: Value, b: Value) -> CC {
        let double = self.ty(a) == Type::F64;
        let (x, y, c) = match cc {
            FloatCC::Eq => (a, b, CC::FEq),
            FloatCC::Ne => (a, b, CC::FNe),
            FloatCC::Gt => (a, b, CC::A),
            FloatCC::Ge => (a, b, CC::AE),
            FloatCC::Lt => (b, a, CC::A),
            FloatCC::Le => (b, a, CC::AE),
        };
        let ra = self.use_val(x);
        let rb = self.use_val(y);
        self.push(MInst::Ucomis {
            double,
            a: ra,
            b: rb,
        });
        c
    }

    /// Set the flags for `cond != 0` and return the condition to test. Nothing that clobbers
    /// flags may be emitted between this and the consumer.
    fn cond(&mut self, cond: Value) -> CC {
        if self.sunk[cond.index()] {
            match self.def(cond).cloned() {
                Some(InstData::IntCmp { cc, args }) => return self.int_cmp(cc, args[0], args[1]),
                Some(InstData::FloatCmp { cc, args }) => {
                    return self.float_cmp(cc, args[0], args[1])
                }
                _ => {}
            }
        }
        let r = self.use_val(cond);
        self.push(MInst::Test {
            size: Size::S32,
            a: r,
            b: r,
        });
        CC::NE
    }

    fn amode(&mut self, addr: Value, offset: i32) -> Amode {
        if self.sunk[addr.index()] {
            if let Some(InstData::Binary { args: [a, b], .. }) = self.def(addr).cloned() {
                for (x, y) in [(a, b), (b, a)] {
                    if let Some(disp) = self.imm(y, Size::S64).and_then(|i| i.checked_add(offset)) {
                        return Amode {
                            base: self.use_val(x),
                            index: None,
                            scale: 0,
                            disp,
                        };
                    }
                }
                return Amode {
                    base: self.use_val(a),
                    index: Some(self.use_val(b)),
                    scale: 0,
                    disp: offset,
                };
            }
        }
        Amode {
            base: self.use_val(addr),
            index: None,
            scale: 0,
            disp: offset,
        }
    }

    fn call(
        &mut self,
        sig: &Signature,
        target: CallTarget,
        args: &[Value],
        results: &[Value],
    ) -> Result<(), String> {
        if sig.results.len() > 1 {
            return Err("x64: calls with multiple results are not supported".into());
        }
        let vals: Vec<VReg> = args.iter().map(|&a| self.use_val(a)).collect();
        let (locs, size) = arg_locs(self.abi, &sig.params);
        self.outgoing = self.outgoing.max(size);
        let mut reg_args = Vec::new();
        let mut stack_args = Vec::new();
        for (v, loc) in vals.into_iter().zip(locs) {
            match loc {
                ArgLoc::Reg(r) => reg_args.push((v, r)),
                ArgLoc::Stack(off) => stack_args.push((v, off)),
            }
        }
        let mut rets = Vec::new();
        let mut post = None;
        if let (Some(&t), Some(&r)) = (sig.results.first(), results.first()) {
            let dst = self.vreg(r);
            if t == Type::I32 {
                let tmp = self.fresh(t);
                rets.push((tmp, ret_reg(t)));
                post = Some(MInst::Mov {
                    size: Size::S32,
                    dst,
                    src: tmp,
                });
            } else {
                rets.push((dst, ret_reg(t)));
            }
        }
        self.push(MInst::Call {
            target,
            reg_args,
            stack_args,
            rets,
            clobbers: self.abi.call_clobbers,
        });
        if let Some(m) = post {
            self.push(m);
        }
        Ok(())
    }

    fn inst(&mut self, i: Inst) -> Result<(), String> {
        let f = self.f;
        let data = f.inst(i);
        let res = f.results(i);
        if let [r] = res {
            if self.sunk[r.index()] {
                return Ok(());
            }
        }
        match data {
            InstData::Iconst { .. } | InstData::F32const { .. } | InstData::F64const { .. } => {}
            InstData::Unary { op, arg } => self.unary(*op, *arg, res[0])?,
            InstData::Binary { op, args } => self.binary(*op, args[0], args[1], res[0])?,
            InstData::IntCmp { cc, args } => {
                let dst = self.vreg(res[0]);
                let cc = self.int_cmp(*cc, args[0], args[1]);
                self.push(MInst::Setcc { cc, dst });
            }
            InstData::FloatCmp { cc, args } => {
                let dst = self.vreg(res[0]);
                let cc = self.float_cmp(*cc, args[0], args[1]);
                self.push(MInst::Setcc { cc, dst });
            }
            InstData::Select {
                cond,
                if_true,
                if_false,
            } => {
                let dst = self.vreg(res[0]);
                let float = self.ty(res[0]).is_float();
                let src = self.use_val(*if_true);
                self.copy_into(dst, *if_false);
                let cc = self.cond(*cond);
                self.push(if float {
                    MInst::XmmCmov {
                        cc,
                        double: true,
                        dst,
                        src,
                    }
                } else {
                    MInst::Cmov {
                        cc,
                        size: Size::S64,
                        dst,
                        src,
                    }
                });
            }
            InstData::Convert { op, to, arg } => self.convert(*op, *to, *arg, res[0])?,
            InstData::Load { kind, addr, offset } => {
                let addr = self.amode(*addr, *offset);
                let dst = self.vreg(res[0]);
                let kind = match kind {
                    MemKind::I32 | MemKind::I64U32 => LoadKind::Plain(Size::S32),
                    MemKind::I64 => LoadKind::Plain(Size::S64),
                    MemKind::F32 => LoadKind::F32,
                    MemKind::F64 => LoadKind::F64,
                    MemKind::I32S8 => LoadKind::Ext(Ext::S { from: 8, to: Size::S32 }),
                    MemKind::I32S16 => LoadKind::Ext(Ext::S { from: 16, to: Size::S32 }),
                    MemKind::I64S8 => LoadKind::Ext(Ext::S { from: 8, to: Size::S64 }),
                    MemKind::I64S16 => LoadKind::Ext(Ext::S { from: 16, to: Size::S64 }),
                    MemKind::I64S32 => LoadKind::Ext(Ext::S { from: 32, to: Size::S64 }),
                    MemKind::I32U8 | MemKind::I64U8 => LoadKind::Ext(Ext::Z { from: 8 }),
                    MemKind::I32U16 | MemKind::I64U16 => LoadKind::Ext(Ext::Z { from: 16 }),
                };
                self.push(MInst::Load { kind, dst, addr });
            }
            InstData::Store {
                kind,
                addr,
                value,
                offset,
            } => {
                let src = self.use_val(*value);
                let addr = self.amode(*addr, *offset);
                let kind = match kind {
                    MemKind::I32 | MemKind::I64S32 | MemKind::I64U32 => StoreKind::B32,
                    MemKind::I64 => StoreKind::B64,
                    MemKind::F32 => StoreKind::F32,
                    MemKind::F64 => StoreKind::F64,
                    MemKind::I32S8 | MemKind::I32U8 | MemKind::I64S8 | MemKind::I64U8 => StoreKind::B8,
                    MemKind::I32S16 | MemKind::I32U16 | MemKind::I64S16 | MemKind::I64U16 => {
                        StoreKind::B16
                    }
                };
                self.push(MInst::Store { kind, src, addr });
            }
            InstData::Call { func, args } => {
                let ext = &f.funcs[func.index()];
                let sig = f.sigs[ext.sig.index()].clone();
                self.call(&sig, CallTarget::Func(ext.id), args, res)?;
            }
            InstData::CallIndirect { sig, callee, args } => {
                let sig = f.sigs[sig.index()].clone();
                let c = self.use_val(*callee);
                self.call(&sig, CallTarget::Reg(c), args, res)?;
            }
            InstData::Trap { code } => {
                self.has_traps = true;
                self.push(MInst::Trap { code: *code });
            }
            InstData::TrapIf { cond, code } => {
                self.has_traps = true;
                let cc = self.cond(*cond);
                self.push(MInst::TrapIf { cc, code: *code });
            }
            InstData::Jump { dest } => {
                let args = dest.args.iter().map(|&a| self.use_val(a)).collect();
                self.push(MInst::Jmp {
                    target: self.bmap[dest.block.index()],
                    args,
                });
            }
            InstData::Brif { cond, then, else_ } => {
                if !then.args.is_empty() || !else_.args.is_empty() {
                    return Err("x64: brif with arguments (critical edges not split)".into());
                }
                let cc = self.cond(*cond);
                self.push(MInst::Jcc {
                    cc,
                    taken: self.bmap[then.block.index()],
                    not_taken: self.bmap[else_.block.index()],
                });
            }
            InstData::BrTable {
                index,
                targets,
                default,
            } => {
                if targets.iter().chain([default]).any(|t| !t.args.is_empty()) {
                    return Err("x64: br_table with arguments (critical edges not split)".into());
                }
                let index = self.use_val(*index);
                self.push(MInst::BrTable {
                    index,
                    targets: targets.iter().map(|t| self.bmap[t.block.index()]).collect(),
                    default: self.bmap[default.block.index()],
                });
            }
            InstData::Return { args } => {
                let vals = args
                    .iter()
                    .map(|&a| {
                        let r = ret_reg(self.ty(a));
                        (self.use_val(a), r)
                    })
                    .collect();
                self.push(MInst::Ret { vals });
            }
        }
        Ok(())
    }

    fn unary(&mut self, op: UnaryOp, arg: Value, r: Value) -> Result<(), String> {
        let ty = self.ty(arg);
        let size = size_of(ty);
        let double = ty == Type::F64;
        let dst = self.vreg(r);
        match op {
            UnaryOp::Clz | UnaryOp::Ctz | UnaryOp::Popcnt => {
                let bop = match op {
                    UnaryOp::Clz if self.feat.lzcnt => BitOp::Lzcnt,
                    UnaryOp::Clz => BitOp::ClzBsr,
                    UnaryOp::Ctz if self.feat.bmi1 => BitOp::Tzcnt,
                    UnaryOp::Ctz => BitOp::CtzBsf,
                    _ if self.feat.popcnt => BitOp::Popcnt,
                    _ => return Err("x64: popcnt needs the POPCNT extension".into()),
                };
                let src = self.use_val(arg);
                self.push(MInst::Bit {
                    op: bop,
                    size,
                    dst,
                    src,
                });
            }
            UnaryOp::Sext8 | UnaryOp::Sext16 | UnaryOp::Sext32 => {
                let from = match op {
                    UnaryOp::Sext8 => 8,
                    UnaryOp::Sext16 => 16,
                    _ => 32,
                };
                let src = self.use_val(arg);
                self.push(MInst::MovX {
                    ext: Ext::S { from, to: size },
                    dst,
                    src,
                });
            }
            UnaryOp::Fneg | UnaryOp::Fabs => {
                let sign = if double { 1u64 << 63 } else { 0x8000_0000_8000_0000 };
                let (op, mask) = if op == UnaryOp::Fneg {
                    (MaskOp::Xor, sign)
                } else {
                    (MaskOp::And, !sign)
                };
                self.copy_into(dst, arg);
                self.push(MInst::XmmMask {
                    op,
                    double,
                    dst,
                    mask,
                });
            }
            UnaryOp::Sqrt => {
                let src = self.use_val(arg);
                self.push(MInst::XmmSqrt { double, dst, src });
            }
            UnaryOp::Ceil | UnaryOp::Floor | UnaryOp::Trunc | UnaryOp::Nearest => {
                if !self.feat.sse41 {
                    return Err("x64: float rounding needs SSE4.1".into());
                }
                // Rounding-control immediates, with bit 3 suppressing the precision exception.
                let mode = match op {
                    UnaryOp::Nearest => 8,
                    UnaryOp::Floor => 9,
                    UnaryOp::Ceil => 10,
                    _ => 11,
                };
                let src = self.use_val(arg);
                self.push(MInst::XmmRound {
                    double,
                    mode,
                    dst,
                    src,
                });
            }
            UnaryOp::Eqz => return Err("x64: eqz reached lowering".into()),
        }
        Ok(())
    }

    fn binary(&mut self, op: BinaryOp, a: Value, b: Value, r: Value) -> Result<(), String> {
        use BinaryOp::*;
        let ty = self.ty(a);
        let size = size_of(ty);
        let double = ty == Type::F64;
        let dst = self.vreg(r);
        match op {
            Iadd | Isub | Band | Bor | Bxor => {
                let (a, b) = if op != Isub && self.imm(b, size).is_none() && self.imm(a, size).is_some() {
                    (b, a)
                } else {
                    (a, b)
                };
                let aop = match op {
                    Iadd => AluOp::Add,
                    Isub => AluOp::Sub,
                    Band => AluOp::And,
                    Bor => AluOp::Or,
                    _ => AluOp::Xor,
                };
                let src = self.reg_imm(b, size);
                self.copy_into(dst, a);
                self.push(MInst::Alu {
                    op: aop,
                    size,
                    dst,
                    src,
                });
            }
            Imul => {
                if let Some(imm) = self.imm(b, size) {
                    let src = self.use_val(a);
                    self.push(MInst::Imul3 { size, dst, src, imm });
                } else if let Some(imm) = self.imm(a, size) {
                    let src = self.use_val(b);
                    self.push(MInst::Imul3 { size, dst, src, imm });
                } else {
                    let src = self.use_val(b);
                    self.copy_into(dst, a);
                    self.push(MInst::Imul { size, dst, src });
                }
            }
            Sdiv | Udiv | Srem | Urem => {
                let dividend = self.use_val(a);
                let divisor = self.use_val(b);
                self.push(MInst::Div {
                    signed: matches!(op, Sdiv | Srem),
                    size,
                    rem: matches!(op, Srem | Urem),
                    dividend,
                    divisor,
                    dst,
                });
            }
            Ishl | Ushr | Sshr | Rotl | Rotr => {
                let sop = match op {
                    Ishl => ShiftOp::Shl,
                    Ushr => ShiftOp::Shr,
                    Sshr => ShiftOp::Sar,
                    Rotl => ShiftOp::Rol,
                    _ => ShiftOp::Ror,
                };
                if let Some(c) = self.iconst(b) {
                    let amt = (c & (ty.bits() as u64 - 1)) as u8;
                    self.copy_into(dst, a);
                    if amt != 0 {
                        self.push(MInst::ShiftImm {
                            op: sop,
                            size,
                            dst,
                            amt,
                        });
                    }
                } else if self.feat.bmi2 && matches!(op, Ishl | Ushr | Sshr) {
                    let src = self.use_val(a);
                    let amt = self.use_val(b);
                    self.push(MInst::ShiftX {
                        op: sop,
                        size,
                        dst,
                        src,
                        amt,
                    });
                } else {
                    let amt = self.use_val(b);
                    self.copy_into(dst, a);
                    self.push(MInst::ShiftCl {
                        op: sop,
                        size,
                        dst,
                        amt,
                    });
                }
            }
            Fadd | Fsub | Fmul | Fdiv => {
                let xop = match op {
                    Fadd => XmmOp::Add,
                    Fsub => XmmOp::Sub,
                    Fmul => XmmOp::Mul,
                    _ => XmmOp::Div,
                };
                let src = self.use_val(b);
                self.copy_into(dst, a);
                self.push(MInst::XmmAlu {
                    op: xop,
                    double,
                    dst,
                    src,
                });
            }
            Fmin | Fmax => {
                let src = self.use_val(b);
                self.copy_into(dst, a);
                self.push(MInst::XmmMinMax {
                    min: op == Fmin,
                    double,
                    dst,
                    src,
                });
            }
            Fcopysign => return Err("x64: fcopysign reached lowering".into()),
        }
        Ok(())
    }

    fn convert(&mut self, op: ConvOp, to: Type, arg: Value, r: Value) -> Result<(), String> {
        let from = self.ty(arg);
        if op == ConvOp::Uext {
            return Ok(());
        }
        let dst = self.vreg(r);
        match op {
            ConvOp::Wrap => {
                if let Some(c) = self.iconst(arg) {
                    self.push(MInst::MovImm {
                        size: Size::S32,
                        dst,
                        imm: c as u32 as u64,
                    });
                } else {
                    let src = self.use_val(arg);
                    self.push(MInst::Mov {
                        size: Size::S32,
                        dst,
                        src,
                    });
                }
            }
            ConvOp::Sext => {
                let src = self.use_val(arg);
                self.push(MInst::MovX {
                    ext: Ext::S { from: 32, to: Size::S64 },
                    dst,
                    src,
                });
            }
            ConvOp::FromSint => {
                let src = self.use_val(arg);
                self.push(MInst::CvtIntToFloat {
                    src_size: size_of(from),
                    double: to == Type::F64,
                    dst,
                    src,
                });
            }
            ConvOp::ToSint => {
                let src = self.use_val(arg);
                self.push(MInst::CvtFloatToInt {
                    dst_size: size_of(to),
                    double: from == Type::F64,
                    dst,
                    src,
                });
            }
            ConvOp::Promote | ConvOp::Demote => {
                let src = self.use_val(arg);
                self.push(MInst::CvtFloatFloat {
                    to_double: op == ConvOp::Promote,
                    dst,
                    src,
                });
            }
            ConvOp::Bitcast => {
                let src = self.use_val(arg);
                let size = size_of(to);
                self.push(if to.is_float() {
                    MInst::GprToXmm { size, dst, src }
                } else {
                    MInst::XmmToGpr { size, dst, src }
                });
            }
            ConvOp::Uext
            | ConvOp::FromUint
            | ConvOp::ToUint
            | ConvOp::ToSintSat
            | ConvOp::ToUintSat => return Err(format!("x64: {op:?} reached lowering")),
        }
        Ok(())
    }
}
