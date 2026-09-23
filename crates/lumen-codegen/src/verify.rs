//! IR well-formedness: every placed instruction is in exactly one block, blocks end in exactly
//! one terminator, operand types match, branch arguments match target parameters, and every use
//! is dominated by its definition.

use crate::cfg::Cfg;
use crate::ir::*;

pub fn verify(func: &Function) -> Result<(), String> {
    let mut errs = Vec::new();
    let mut err = |s: String| errs.push(s);

    if func.layout.is_empty() {
        return Err("function has no blocks".into());
    }
    let entry = func.entry();
    let entry_tys: Vec<Type> = func.blocks[entry.index()]
        .params
        .iter()
        .map(|&p| func.value_type(p))
        .collect();
    if entry_tys != func.sig.params {
        err(format!("entry parameters {entry_tys:?} != signature {:?}", func.sig.params));
    }

    // Placement.
    let mut inst_block: Vec<Option<(Block, usize)>> = vec![None; func.insts.len()];
    let mut seen_block = vec![false; func.blocks.len()];
    for &b in &func.layout {
        if std::mem::replace(&mut seen_block[b.index()], true) {
            err(format!("{b} appears twice in the layout"));
        }
        let insts = &func.blocks[b.index()].insts;
        if insts.is_empty() {
            err(format!("{b} is empty"));
        }
        for (i, &inst) in insts.iter().enumerate() {
            if inst_block[inst.index()].replace((b, i)).is_some() {
                err(format!("{inst} placed twice"));
            }
            let term = func.inst(inst).is_terminator();
            if term != (i + 1 == insts.len()) {
                err(format!("{b}: {inst} terminator placement"));
            }
        }
    }

    let cfg = Cfg::new(func);
    let def_pos = |v: Value| -> Option<(Block, isize)> {
        match func.values[v.index()].def {
            ValueDef::Result(inst, _) => inst_block[inst.index()].map(|(b, i)| (b, i as isize)),
            ValueDef::Param(b, _) => seen_block[b.index()].then_some((b, -1)),
            ValueDef::Alias(_) => None,
        }
    };

    for &b in &func.layout {
        if !cfg.is_reachable(b) {
            continue;
        }
        for (i, &inst) in func.blocks[b.index()].insts.iter().enumerate() {
            let data = func.inst(inst);
            // Dominance of operands.
            data.for_each_arg(|v| {
                if v.index() >= func.values.len() {
                    err(format!("{inst}: unknown {v}"));
                    return;
                }
                match def_pos(v) {
                    None => err(format!("{inst}: {v} is an alias or not placed")),
                    Some((db, di)) => {
                        let ok = if db == b { di < i as isize } else { cfg.dominates(db, b) };
                        if !ok {
                            err(format!("{b}: {inst} uses {v} which does not dominate it"));
                        }
                    }
                }
            });
            if let Err(e) = check_types(func, data) {
                err(format!("{b}: {inst} {data:?}: {e}"));
            }
            for call in data.successors() {
                let params = &func.blocks[call.block.index()].params;
                if !seen_block[call.block.index()] {
                    err(format!("{inst}: branch to {} outside the layout", call.block));
                } else if call.block == entry {
                    err(format!("{inst}: branch to the entry block"));
                }
                if params.len() != call.args.len() {
                    err(format!(
                        "{inst}: {} arguments for {} parameters of {}",
                        call.args.len(),
                        params.len(),
                        call.block
                    ));
                    continue;
                }
                for (&a, &p) in call.args.iter().zip(params) {
                    if func.value_type(a) != func.value_type(p) {
                        err(format!("{inst}: argument {a} type for {p}"));
                    }
                }
            }
        }
    }

    if errs.is_empty() {
        Ok(())
    } else {
        Err(format!("{}\n{func}", errs.join("\n")))
    }
}

fn check_types(func: &Function, data: &InstData) -> Result<(), String> {
    let ty = |v: Value| func.value_type(v);
    let want = |v: Value, t: Type| {
        if ty(v) == t {
            Ok(())
        } else {
            Err(format!("{v} is {:?}, expected {t:?}", ty(v)))
        }
    };
    // Pointer width is the front end's choice: I64 for native targets, I32 for wasm32.
    let want_ptr = |v: Value| {
        if ty(v).is_int() {
            Ok(())
        } else {
            Err(format!("{v} is {:?}, expected an I32 or I64 address", ty(v)))
        }
    };
    match data {
        InstData::Iconst { ty: t, imm } => {
            if !t.is_int() {
                return Err("iconst of a float type".into());
            }
            if *t == Type::I32 && *imm != *imm as i32 as i64 {
                return Err("I32 immediate not sign-extended".into());
            }
            Ok(())
        }
        InstData::F32const { .. } | InstData::F64const { .. } | InstData::Trap { .. } => Ok(()),
        InstData::Unary { op, arg } => {
            use UnaryOp::*;
            let t = ty(*arg);
            let ok = match op {
                Clz | Ctz | Popcnt | Eqz | Sext8 | Sext16 => t.is_int(),
                Sext32 => t == Type::I64,
                _ => t.is_float(),
            };
            if ok { Ok(()) } else { Err(format!("{op:?} on {t:?}")) }
        }
        InstData::Binary { op, args } => {
            use BinaryOp::*;
            want(args[1], ty(args[0]))?;
            let t = ty(args[0]);
            let int = !matches!(op, Fadd | Fsub | Fmul | Fdiv | Fmin | Fmax | Fcopysign);
            if int == t.is_int() { Ok(()) } else { Err(format!("{op:?} on {t:?}")) }
        }
        InstData::IntCmp { args, .. } => {
            want(args[1], ty(args[0]))?;
            if ty(args[0]).is_int() { Ok(()) } else { Err("icmp on floats".into()) }
        }
        InstData::FloatCmp { args, .. } => {
            want(args[1], ty(args[0]))?;
            if ty(args[0]).is_float() { Ok(()) } else { Err("fcmp on ints".into()) }
        }
        InstData::Select {
            cond,
            if_true,
            if_false,
        } => {
            want(*cond, Type::I32)?;
            want(*if_false, ty(*if_true))
        }
        InstData::Convert { op, to, arg } => {
            use ConvOp::*;
            let from = ty(*arg);
            let ok = match op {
                Wrap => from == Type::I64 && *to == Type::I32,
                Sext | Uext => from == Type::I32 && *to == Type::I64,
                FromSint | FromUint => from.is_int() && to.is_float(),
                ToSint | ToUint | ToSintSat | ToUintSat => from.is_float() && to.is_int(),
                Promote => from == Type::F32 && *to == Type::F64,
                Demote => from == Type::F64 && *to == Type::F32,
                Bitcast => from.bits() == to.bits() && from.is_int() != to.is_int(),
            };
            if ok { Ok(()) } else { Err(format!("{op:?} {from:?} -> {to:?}")) }
        }
        InstData::Load { addr, .. } => want_ptr(*addr),
        InstData::Store { kind, addr, value, .. } => {
            want_ptr(*addr)?;
            want(*value, kind.ty())
        }
        InstData::Call { func: f, args } => {
            let sig = &func.sigs[func.funcs[f.index()].sig.index()];
            check_args(func, &sig.params, args)
        }
        InstData::CallIndirect { sig, callee, args } => {
            want_ptr(*callee)?;
            check_args(func, &func.sigs[sig.index()].params, args)
        }
        InstData::TrapIf { cond, .. } => want(*cond, Type::I32),
        InstData::Jump { .. } => Ok(()),
        InstData::Brif { cond, .. } => want(*cond, Type::I32),
        InstData::BrTable { index, .. } => want(*index, Type::I32),
        InstData::Return { args } => check_args(func, &func.sig.results, args),
    }
}

fn check_args(func: &Function, params: &[Type], args: &[Value]) -> Result<(), String> {
    let got: Vec<Type> = args.iter().map(|&a| func.value_type(a)).collect();
    if got == params {
        Ok(())
    } else {
        Err(format!("arguments {got:?}, expected {params:?}"))
    }
}
