//! Operand validation at the serialized-bytecode trust boundary.
use super::{CapInit, Chunk, Op, VIRT_REST};

pub(super) fn operands(c: &Chunk) -> Result<(), String> {
    let check = |index: usize, len: usize| {
        if index < len {
            Ok(())
        } else {
            Err("bytecode: operand out of range".to_string())
        }
    };
    let slot = |s: u16| check(s as usize, c.n_slots);
    let name = |n: u32| check(n as usize, c.names.len());
    let constant = |n: u32| check(n as usize, c.consts.len());
    let target = |n: u32| check(n as usize, c.ops.len());
    let cache = |n: u32| check(n as usize, c.caches.len());
    let name_cache = |n: u32| check(n as usize, c.name_caches.len());
    let optional_name = |n| if n == u32::MAX { Ok(()) } else { name(n) };
    let names = |n: u32, len: u16| {
        (n as usize)
            .checked_add(len as usize)
            .filter(|&end| end <= c.names.len())
            .map(|_| ())
            .ok_or_else(|| "bytecode: name span out of range".to_string())
    };
    if c.n_slots > u16::MAX as usize + 1 || c.n_params > c.n_slots {
        return Err("bytecode: invalid parameter/slot layout".into());
    }
    for s in c
        .arguments_slot
        .iter()
        .chain(c.rest_slot.iter())
        .chain(c.virt_base.iter())
        .chain(c.var_force_resets.iter())
    {
        slot(*s)?;
    }
    for init in &c.cap_inits {
        match init {
            CapInit::Param(k, _) => check(*k as usize, c.n_params)?,
            CapInit::Fn(k, _) => check(*k as usize, c.funcs.len())?,
            _ => {}
        }
    }
    let mut switches = vec![false; c.switch_tables.len()];
    for (pc, op) in c.ops.iter().enumerate() {
        if matches!(op, Op::InEnv(_))
            && !matches!(
                c.ops.get(pc + 1),
                Some(Op::MakeClosure(..) | Op::MakeClass(..) | Op::InitMethod(..))
            )
        {
            return Err("bytecode: invalid environment prefix".into());
        }
        match *op {
            Op::CallSpread(0)
            | Op::CallSpreadThis(0)
            | Op::NewSpread(0)
            | Op::TailCallSpread(0, _) => {
                return Err("bytecode: spread call has no spread argument".into());
            }
            Op::Const(k) => constant(k)?,
            Op::LoadLocal(s)
            | Op::StoreLocal(s)
            | Op::UpdateLocal(s, _)
            | Op::Tdz(s)
            | Op::GetElemLocal(s)
            | Op::SetElemLocal(s)
            | Op::SetElemLocalDrop(s)
            | Op::ToPropKeyLocal(s)
            | Op::IterCloseL(s)
            | Op::IterAbortL(s)
            | Op::BlkCopy(s)
            | Op::InEnv(s)
            | Op::AsyncIterResult(s)
            | Op::AsyncCloseCheck(s)
            | Op::AsyncDelegateSpecial(s, _) => slot(s)?,
            Op::LoadCap(n)
            | Op::StoreCap(n)
            | Op::StoreCapInit(n)
            | Op::UpdateCap(n, _)
            | Op::UpdateName(n, _)
            | Op::StoreName(n)
            | Op::GenBin(n)
            | Op::TypeofName(n)
            | Op::GetPrivate(n)
            | Op::SetPrivate(n)
            | Op::GetPrivateMethod(n)
            | Op::PrivateIn(n)
            | Op::UpdatePrivate(n, _)
            | Op::InitProp(n, _)
            | Op::SuperGet(n)
            | Op::SuperMethod(n, _)
            | Op::DefineField(n, _)
            | Op::DeleteProp(n, _) => name(n)?,
            Op::LoadName(n, k)
            | Op::LoadNameForCall(n, k)
            | Op::StoreNameCached(n, k)
            | Op::UpdateNameCached(n, k, _) => {
                name(n)?;
                name_cache(k)?;
            }
            Op::GetProp(n, k)
            | Op::GetPropThis(n, k)
            | Op::SetProp(n, k)
            | Op::SetPropDrop(n, k)
            | Op::SetPropThisDrop(n, k)
            | Op::AppendProp(n, k)
            | Op::GetMethod(n, k)
            | Op::UpdateProp(n, k, _) => {
                name(n)?;
                cache(k)?;
            }
            Op::GetPropLocal(s, n, k) | Op::SetPropLocalDrop(s, n, k) => {
                slot(s)?;
                name(n)?;
                cache(k)?;
            }
            Op::MakeClosure(f, n) => {
                check(f as usize, c.funcs.len())?;
                optional_name(n)?;
            }
            Op::InitMethod(f, n, kind) => {
                check(f as usize, c.funcs.len())?;
                optional_name(n)?;
                check(kind as usize, 3)?;
            }
            Op::MakeClass(k, n) => {
                check(k as usize, c.classes.len())?;
                optional_name(n)?;
            }
            Op::InstanceOf(k) => cache(k)?,
            Op::MakeRegExp(a, b) => {
                name(a)?;
                name(b)?;
            }
            Op::MakeObject(n, count, map) => {
                names(n, count)?;
                if map != u32::MAX {
                    check(map as usize, c.obj_maps.len())?;
                }
            }
            Op::ObjRest(n, count) => names(n, count)?,
            Op::IterStepL(a, b)
            | Op::IterRestL(a, b)
            | Op::AsyncCloseCall(a, b)
            | Op::AsyncDelegateResult(a, b)
            | Op::AsyncDelegateCloseReject(a, b) => {
                slot(a)?;
                slot(b)?;
            }
            Op::ForInStepL(a, b, d)
            | Op::AsyncIterNext(a, b, d)
            | Op::AsyncDelegateCall(a, b, d) => {
                slot(a)?;
                slot(b)?;
                slot(d)?;
            }
            Op::YieldDelegate(s) => {
                slot(s)?;
                check(s as usize + 1, c.n_slots)?;
            }
            Op::BlkNew(s, parent) => {
                slot(s)?;
                if parent != u16::MAX {
                    slot(parent)?;
                }
            }
            Op::BlkDecl(s, n, _)
            | Op::BlkLoad(s, n)
            | Op::BlkStore(s, n)
            | Op::BlkInit(s, n)
            | Op::BlkUpdate(s, n, _) => {
                slot(s)?;
                name(n)?;
            }
            Op::ArgsLen(s, base) | Op::ArgsGet(s, base) | Op::ApplyArgs(s, base) => {
                slot(s)?;
                slot(base & !VIRT_REST)?;
            }
            Op::Jump(t)
            | Op::JumpIfFalse(t)
            | Op::JumpIfFalsePeek(t)
            | Op::JumpIfTruePeek(t)
            | Op::JumpIfNotNullishPeek(t)
            | Op::PushHandler(t)
            | Op::JumpIfNotCmp(_, t) => target(t)?,
            Op::JumpIfNotCmpLL(_, a, b, t) => {
                slot(a)?;
                slot(b)?;
                target(t)?;
            }
            Op::JumpIfNotCmpLK(_, a, k, t) => {
                slot(a)?;
                constant(k)?;
                target(t)?;
            }
            Op::ArithLL(_, a, b, d) => {
                slot(a)?;
                slot(b)?;
                slot(d)?;
            }
            Op::ArithLK(_, a, b, k) => {
                slot(a)?;
                slot(b)?;
                constant(k)?;
            }
            Op::SwitchLK(s, table) => {
                slot(s)?;
                check(table as usize, c.switch_tables.len())?;
                if std::mem::replace(&mut switches[table as usize], true) {
                    return Err("bytecode: shared switch table".into());
                }
            }
            Op::ImportCall(phase, _) => check(phase as usize, 3)?,
            Op::ArrayCbGuard(kind) => {
                if super::inline_callback::CbMethod::from_u8(kind).is_none() {
                    return Err("bytecode: invalid callback kind".into());
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Validate serialized code before publishing it to the execution cache. Unsupported
/// stack effects reject the cached chunk; the stored AST remains the fallback.
pub(super) fn validate(c: &Chunk) -> Result<usize, String> {
    operands(c)?;
    let end = c.ops.len().checked_sub(1).ok_or("bytecode: empty chunk")?;
    let (_, _, max_stack) = stack_region(c, 0, end, true, false)?;
    Ok(max_stack)
}

/// Shared by cached-bytecode validation and JIT region analysis. JIT regions may
/// exit to the VM; serialized chunks must keep every edge inside their code.
pub(crate) fn stack_region(
    chunk: &Chunk,
    header: usize,
    backedge: usize,
    func: bool,
    jit: bool,
) -> Result<(Vec<Option<usize>>, Vec<Vec<(usize, usize)>>, usize), String> {
    let ops = &chunk.ops;
    if header > backedge || backedge >= ops.len() {
        return Err("bytecode: invalid analysis region".into());
    }
    let inr = |pc: usize| pc >= header && pc <= backedge;
    let n = backedge - header + 1;
    let mut depth: Vec<Option<usize>> = vec![None; n];
    let mut hs: Vec<Vec<(usize, usize)>> = vec![Vec::new(); n];
    depth[0] = Some(0);
    let mut work = vec![header];
    let mut max_stack = 0;
    while let Some(pc) = work.pop() {
        let op = &ops[pc];
        match op {
            // Function code leaves it to the interpreter (an exit before it).
            Op::ForInStepL(..) if jit && !func => {
                return Err(format!("iteration op {op:?} at {pc}"));
            }
            _ => {}
        }
        let d = depth[pc - header].expect("queued ops have a depth");
        let effect = if jit {
            None
        } else {
            Some(match op {
                Op::SwitchLK(..) | Op::InitialYield => (0, 0),
                Op::Yield => (1, 2),
                Op::YieldDelegate(_) => (2, 2),
                Op::GetAsyncIter | Op::AsyncDelegateInit => (1, 3),
                Op::AsyncIterNext(..) => (0, 1),
                Op::AsyncIterResult(_)
                | Op::AsyncDelegateCall(..)
                | Op::AsyncDelegateResult(..) => (1, 2),
                Op::AsyncCloseCheck(_)
                | Op::AsyncDelegateCloseReject(..)
                | Op::AsyncDelegateSpecial(_, true) => (1, 0),
                Op::AsyncDelegateSpecial(_, false) => (0, 0),
                // Its two depths are resolved with the following conditional below.
                Op::AsyncCloseCall(..) => (0, 0),
                _ => chunk
                    .jit_stack_effect(pc)
                    .ok_or_else(|| format!("no stack effect for {op:?} at {pc}"))?,
            })
        };
        let (pops, pushes) = effect
            .or_else(|| chunk.jit_stack_effect(pc))
            .ok_or_else(|| format!("no stack effect for {op:?} at {pc}"))?;
        if d < pops {
            return Err(format!("stack underflow at {pc}"));
        }
        let after = d - pops + pushes;
        max_stack = max_stack.max(d).max(after);
        let h = hs[pc - header].clone();
        if !jit
            && !matches!(
                op,
                Op::Await
                    | Op::Yield
                    | Op::Return
                    | Op::ReturnUndef
                    | Op::DerivedReturn
                    | Op::Throw
                    | Op::IterAbortL(_)
                    | Op::AsyncDelegateCloseReject(..)
                    | Op::AsyncDelegateSpecial(_, true)
            )
            && h.last().is_some_and(|&(_, saved)| d - pops < saved)
        {
            return Err(format!("stack drops below handler depth at {pc}"));
        }
        if jit && matches!(op, Op::Await) && (!h.is_empty() || !func) {
            // Under a region handler, the exit's handlers would be finished by a nested driver
            // the suspension leaves. A loop would enter and exit its code once per iteration
            // (until resume entries exist).
            return Err(format!(
                "await at {pc} inside a region handler or loop region"
            ));
        }
        let mut h_after = h.clone();
        // (successor pc, depth, handlers): the op's own edges, plus the throw edge of a region
        // handler to its catch pad.
        let mut edges: Vec<(usize, usize, Vec<(usize, usize)>)> = Vec::new();
        match *op {
            Op::PushHandler(t) => {
                h_after.push((t as usize, d));
                if jit {
                    edges.push((t as usize, d + 1, h.clone()));
                }
            }
            Op::PopHandler => {
                if h_after.pop().is_none() {
                    return Err(format!(
                        "PopHandler at {pc} pops a handler from outside the loop"
                    ));
                }
            }
            _ => {}
        }
        if !jit
            && !matches!(
                op,
                Op::PushHandler(_)
                    | Op::PopHandler
                    | Op::Jump(_)
                    | Op::JumpIfFalse(_)
                    | Op::JumpIfFalsePeek(_)
                    | Op::JumpIfTruePeek(_)
                    | Op::JumpIfNotNullishPeek(_)
                    | Op::Const(_)
                    | Op::Undef
                    | Op::Pop
                    | Op::Dup
                    | Op::Dup2
                    | Op::Nip
                    | Op::Return
                    | Op::ReturnUndef
                    | Op::DerivedReturn
                    | Op::InitialYield
            )
        {
            if let Some(&(catch, saved)) = h.last() {
                // Await/yield can suspend after consuming an operand. A rejected
                // resume truncates what remains; truncate cannot restore that value.
                let mut outer = h.clone();
                outer.pop();
                edges.push((catch, saved.min(d - pops) + 1, outer));
            }
        }
        if !jit && matches!(op, Op::AsyncCloseCall(..)) {
            // The absent-return path pushes only false; the callable-return path
            // pushes an awaitable and true. Consume the flag in the adjacent branch.
            let Some(Op::JumpIfFalse(t)) = ops.get(pc + 1) else {
                return Err("bytecode: async close lacks its conditional".into());
            };
            edges.push((*t as usize, d, h_after.clone()));
            edges.push((pc + 2, d + 1, h_after.clone()));
            max_stack = max_stack.max(d + 2);
        } else {
            let (falls, target) = successors(op);
            let falls = falls
                && (jit
                    || !matches!(
                        op,
                        Op::AsyncDelegateCloseReject(..) | Op::AsyncDelegateSpecial(_, true)
                    ));
            for q in falls.then_some(pc + 1).into_iter().chain(target) {
                edges.push((q, after, h_after.clone()));
            }
        }
        for (q, dq, hq) in edges {
            max_stack = max_stack.max(dq);
            if !inr(q) {
                if !jit {
                    return Err(format!("control flow leaves chunk at {pc}"));
                }
                continue;
            }
            match depth[q - header] {
                None => {
                    depth[q - header] = Some(dq);
                    hs[q - header] = hq;
                    work.push(q);
                }
                Some(e) if e != dq => {
                    return Err(format!("inconsistent stack depth at {q}"));
                }
                Some(_) if hs[q - header] != hq => {
                    return Err(format!("inconsistent handlers at {q}"));
                }
                Some(_) => {}
            }
        }
    }
    if !hs[0].is_empty() {
        return Err("handlers at the loop header".into());
    }

    Ok((depth, hs, max_stack))
}

/// Whether control can continue to `pc + 1`, and the jump target.
pub(crate) fn successors(op: &Op) -> (bool, Option<usize>) {
    let falls = !matches!(
        op,
        Op::Jump(_)
            | Op::Return
            | Op::ReturnUndef
            | Op::Throw
            | Op::IterAbortL(_)
            | Op::DerivedReturn
    );
    // A `PushHandler`'s catch pc is no jump: its throw edge is added separately (with the
    // exception pushed and the handler popped).
    let target = match op {
        Op::PushHandler(_) => None,
        _ => crate::jit_ir::jump_target(op),
    };
    (falls, target)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn chunk() -> Chunk {
        let body = crate::parser::parse_script("function f(x) { return x + 1; }", false).unwrap();
        let crate::ast::Stmt::FuncDecl(f) = &body[0] else {
            panic!("function");
        };
        std::rc::Rc::try_unwrap(super::super::compile(f).unwrap()).unwrap()
    }
    #[test]
    fn rejects_bad_indices_even_in_unreachable_ops() {
        for op in [
            Op::LoadLocal(u16::MAX),
            Op::LoadCap(u32::MAX),
            Op::Const(u32::MAX),
            Op::PushHandler(u32::MAX),
            Op::Jump(u32::MAX),
            Op::MakeClosure(u32::MAX, u32::MAX),
        ] {
            let mut c = chunk();
            assert!(operands(&c).is_ok());
            c.ops.push(op);
            assert!(operands(&c).is_err(), "accepted {op:?}");
        }
    }

    #[test]
    fn rejects_underflow_fallthrough_and_inconsistent_merges() {
        for ops in [
            vec![Op::Pop, Op::ReturnUndef],
            vec![Op::Undef],
            vec![Op::PopHandler, Op::ReturnUndef],
            vec![
                Op::Undef,
                Op::PushHandler(4),
                Op::Pop,
                Op::ReturnUndef,
                Op::Return,
            ],
            vec![Op::Undef, Op::JumpIfFalse(3), Op::Undef, Op::ReturnUndef],
            vec![Op::PushHandler(3), Op::Jump(3), Op::ReturnUndef, Op::Return],
        ] {
            let mut c = chunk();
            c.ops = ops;
            assert!(validate(&c).is_err(), "accepted {:?}", c.ops);
        }
    }

    #[test]
    fn accepts_balanced_branches_and_catch_edges() {
        let mut c = chunk();
        assert!(validate(&c).is_ok());
        c.ops = vec![
            Op::PushHandler(4),
            Op::Undef,
            Op::PopHandler,
            Op::Return,
            Op::Return,
        ];
        assert!(validate(&c).is_ok());
        c.ops = vec![Op::Undef, Op::JumpIfFalse(3), Op::Jump(3), Op::ReturnUndef];
        assert!(validate(&c).is_ok());
    }

    #[test]
    fn validates_compiler_suspensions_switches_and_environment_prefixes() {
        let src = r#"
            function* g() { try { yield* [1, 2]; } finally { yield 3; } }
            async function* ag(a) { yield* a; for await (const x of a) { yield x; break; } }
            function sw(x) { switch(x) { case 1:return 1; case 2:return 2; case 3:return 3; case 4:return 4; default:return 0; } }
            function env(x) { let f; { let y=x; f=()=>y; } return f(...[1]); }
        "#;
        let body = crate::parser::parse_script(src, false).unwrap();
        for stmt in &body {
            let crate::ast::Stmt::FuncDecl(f) = stmt else {
                panic!("function");
            };
            let c = super::super::compile(f).unwrap();
            assert!(validate(&c).is_ok(), "{:?}: {:?}", f.name, validate(&c));
        }
        let mut c = chunk();
        for op in [Op::InEnv(0), Op::CallSpread(0)] {
            c.ops = vec![op, Op::ReturnUndef];
            assert!(validate(&c).is_err());
        }
    }
}
