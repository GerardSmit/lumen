//! Complete boxed native lowering. Bytecode is compiler input only: operands become
//! immediate call arguments and control flow becomes native branches.
#[path = "aot_ops.rs"]
mod aot_ops;

use crate::bytecode::{verify, Chunk, Op};
use lumen_codegen::{Block, FuncRef, Function, FunctionBuilder, IntCC, MemKind, Signature, Type};
use std::collections::BTreeMap;

pub use aot_ops::OP_NAMES;
pub use crate::native_ops::{ENTER, LAND, RESUME, SAFEPOINT};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildMode { Jit, Aot }

pub struct Built {
    pub function: Function,
    pub max_stack: usize,
    pub resumes: Vec<(usize, usize)>,
}

/// Native semantic helpers receive `(frame, a, b, c, d, depth, resume)`.
/// Status is 0 for success, 1 for throw, 2 for suspension, 4 for a taken branch.
pub(crate) fn build(chunk: &Chunk, pointer_width: u8) -> Result<Built, String> {
    build_with_profile(chunk, pointer_width, None)
}

pub(crate) fn build_with_profile(chunk: &Chunk, pointer_width: u8, profile: Option<&crate::feedback::Profile>) -> Result<Built, String> {
    let context = |message: String| format!("{}: {message}", chunk.debug_name);
    verify::operands(chunk).map_err(&context)?;
    for (pc, op) in chunk.ops.iter().enumerate() {
        if matches!(op, Op::ImportCall(phase, options) if *phase != 0 || *options) {
            return Err(context(format!("unsupported native dynamic import phase or options at operation {pc}")));
        }
    }
    let (depths, handlers, max_stack) = verify::native_stack_region(chunk).map_err(&context)?;
    if max_stack > u32::MAX as usize || chunk.ops.len() > u32::MAX as usize {
        return Err(context("native frame exceeds 32-bit limits".into()));
    }
    let p = match pointer_width {
        32 => Type::I32,
        64 => Type::I64,
        _ => return Err(context("unsupported native pointer width".into())),
    };
    let i = Type::I32;
    let mut function = Function::new(&chunk.debug_name, Signature::new(vec![p], vec![i]));
    let enter = function.import_function(Signature::new(vec![p], vec![i]), ENTER);
    let safepoint = function.import_function(Signature::new(vec![p], vec![i]), SAFEPOINT);
    let land = function.import_function(Signature::new(vec![p, i], vec![i]), LAND);
    let resume_check = function.import_function(Signature::new(vec![p], vec![i]), RESUME);
    let numeric_sites: Vec<bool> = chunk.ops.iter().enumerate().map(|(pc, op)| {
        matches!(op, Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Mod | Op::Lt | Op::Le | Op::Gt | Op::Ge | Op::EqEq | Op::NotEq | Op::StrictEq | Op::StrictNotEq)
            && profile.is_some_and(|profile| profile.numeric(chunk.feedback_key(), pc))
    }).collect();
    let numeric_helpers = numeric_sites.iter().any(|&site| site).then(|| (
        function.import_function(Signature::new(vec![p, i, i], vec![i]), crate::native_ops::TYPE_GUARD),
        function.import_function(Signature::new(vec![p, i, i, i], vec![i]), crate::native_ops::NUM_BINARY),
    ));
    let shape_sites: Vec<Option<u64>> = chunk.ops.iter().enumerate().map(|(pc, op)| {
        if matches!(op, Op::GetProp(..)) { profile.and_then(|profile| profile.shape(chunk.feedback_key(), pc)) } else { None }
    }).collect();
    let shape_helpers = shape_sites.iter().any(Option::is_some).then(|| (
        function.import_function(Signature::new(vec![p, i, i], vec![i]), crate::native_ops::SHAPE_GUARD),
        function.import_function(Signature::new(vec![p, i, i, i, i], vec![i]), crate::native_ops::SHAPE_GET_PROP),
    ));
    let signature = Signature::new(vec![p, i, i, i, i, i, i], vec![i]);
    let mut imports = BTreeMap::<u32, FuncRef>::new();
    for (pc, &op) in chunk.ops.iter().enumerate() {
        if depths[pc].is_none() || matches!(op, Op::Jump(_) | Op::PushHandler(_) | Op::PopHandler | Op::SwitchLK(..)) {
            continue;
        }
        let (id, _) = aot_ops::operands(op);
        imports.entry(id).or_insert_with(|| function.import_function(signature.clone(), id));
    }
    let mut fb = FunctionBuilder::new(&mut function);
    let entry = fb.create_entry_block();
    let frame = fb.block_params(entry)[0];
    let blocks: Vec<Block> = chunk.ops.iter().map(|_| fb.create_block()).collect();
    let thrown = fb.create_block();
    let returned = fb.create_block();
    let suspended = fb.create_block();
    let dispatch = fb.create_block();
    let zero = fb.iconst(i, 0);
    let one = fb.iconst(i, 1);
    let two = fb.iconst(i, 2);
    let four = fb.iconst(i, 4);
    let status = fb.call_fn(enter, &[frame])[0];
    let ok = fb.icmp(IntCC::Eq, status, zero);
    fb.brif(ok, dispatch, &[], thrown, &[]);
    fb.switch_to_block(dispatch);
    let resume = fb.load(MemKind::I32, frame, 0);
    let mut resumes = vec![(0, 0)];
    let mut resume_sites = BTreeMap::new();
    let mut landings = BTreeMap::new();
    for (pc, op) in chunk.ops.iter().enumerate() {
        if depths[pc].is_none() { continue; }
        let target = match op {
            Op::Await | Op::Yield | Op::InitialYield => Some(pc + 1),
            Op::YieldDelegate(_) => Some(pc),
            _ => None,
        };
        if let Some(target) = target {
            let depth = depths.get(target).and_then(|d| *d).ok_or_else(|| context(format!("invalid resume at source position {}", chunk.call_site_pos(pc))))?;
            resumes.push((target, depth));
            resume_sites.insert(target, pc);
        }
    }
    resumes.sort_unstable();
    resumes.dedup();
    for &(pc, _) in &resumes {
        let next = fb.create_block();
        let check = fb.create_block();
        let state = fb.iconst(i, pc as i64);
        let matches = fb.icmp(IntCC::Eq, resume, state);
        fb.brif(matches, check, &[], next, &[]);
        fb.switch_to_block(check);
        let site = resume_sites.get(&pc).copied().unwrap_or(pc);
        let catch = landing(&mut fb, frame, land, &handlers[site], &blocks, thrown, zero, i, &mut landings);
        let status = fb.call_fn(resume_check, &[frame])[0];
        let rejected = fb.icmp(IntCC::Eq, status, one);
        fb.brif(rejected, catch, &[], blocks[pc], &[]);
        fb.switch_to_block(next);
    }
    fb.jump(thrown, &[]);
    for (pc, &op) in chunk.ops.iter().enumerate() {
        let Some(depth) = depths[pc] else { continue; };
        fb.switch_to_block(blocks[pc]);
        let catch = landing(&mut fb, frame, land, &handlers[pc], &blocks, thrown, zero, i, &mut landings);
        if verify::successors(&op).1.is_some_and(|target| target <= pc) {
            let proceed = fb.create_block();
            let status = fb.call_fn(safepoint, &[frame])[0];
            let ok = fb.icmp(IntCC::Eq, status, zero);
            fb.brif(ok, proceed, &[], catch, &[]);
            fb.switch_to_block(proceed);
        }
        if let Op::Jump(target) = op {
            fb.jump(blocks[target as usize], &[]);
            continue;
        }
        if matches!(op, Op::PushHandler(_) | Op::PopHandler | Op::SwitchLK(..)) {
            fb.jump(blocks[pc + 1], &[]);
            continue;
        }
        let (id, immediates) = aot_ops::operands(op);
        let mut args = vec![frame];
        args.extend(immediates.map(|v| fb.iconst(i, v as i64)));
        args.push(fb.iconst(i, depth as i64));
        args.push(fb.iconst(i, if matches!(op, Op::YieldDelegate(_)) { pc as i64 } else { (pc + 1) as i64 }));
        let status = if numeric_sites[pc] {
            let (guard, binary) = numeric_helpers.unwrap();
            let fast = fb.create_block();
            let slow = fb.create_block();
            let join = fb.create_block();
            let status = fb.append_block_param(join, i);
            let number = fb.iconst(i, 4);
            let matched = fb.call_fn(guard, &[frame, number, number])[0];
            let matched = fb.icmp(IntCC::Eq, matched, one);
            fb.brif(matched, fast, &[], slow, &[]);
            fb.switch_to_block(fast);
            let operation = fb.iconst(i, id as i64);
            let result = fb.call_fn(binary, &[frame, operation, args[5], args[6]])[0];
            fb.jump(join, &[result]);
            fb.switch_to_block(slow);
            let result = fb.call_fn(imports[&id], &args)[0];
            fb.jump(join, &[result]);
            fb.switch_to_block(join);
            status
        } else if let Some(shape) = shape_sites[pc] {
            let (guard, property) = shape_helpers.unwrap();
            let fast = fb.create_block();
            let slow = fb.create_block();
            let join = fb.create_block();
            let status = fb.append_block_param(join, i);
            let low = fb.iconst(i, shape as u32 as i64);
            let high = fb.iconst(i, (shape >> 32) as i64);
            let matched = fb.call_fn(guard, &[frame, low, high])[0];
            let matched = fb.icmp(IntCC::Eq, matched, one);
            fb.brif(matched, fast, &[], slow, &[]);
            fb.switch_to_block(fast);
            let result = fb.call_fn(property, &[frame, args[1], args[2], args[5], args[6]])[0];
            fb.jump(join, &[result]);
            fb.switch_to_block(slow);
            let result = fb.call_fn(imports[&id], &args)[0];
            fb.jump(join, &[result]);
            fb.switch_to_block(join);
            status
        } else {
            fb.call_fn(imports[&id], &args)[0]
        };
        let success = fb.create_block();
        let failed = fb.icmp(IntCC::Eq, status, one);
        let catch = if matches!(op, Op::DerivedReturn) { thrown } else { catch };
        fb.brif(failed, catch, &[], success, &[]);
        fb.switch_to_block(success);
        if matches!(op, Op::Return | Op::ReturnUndef | Op::DerivedReturn | Op::TailCall(..) | Op::TailCallSpread(..)) {
            fb.jump(returned, &[]);
        } else if matches!(op, Op::Throw | Op::IterAbortL(_) | Op::AsyncDelegateCloseReject(..) | Op::AsyncDelegateSpecial(_, true)) {
            fb.jump(catch, &[]);
        } else if matches!(op, Op::Await | Op::Yield | Op::InitialYield | Op::YieldDelegate(_)) {
            let parked = fb.icmp(IntCC::Eq, status, two);
            fb.brif(parked, suspended, &[], blocks[pc + 1], &[]);
        } else if matches!(op, Op::AsyncCloseCall(..)) {
            let Op::JumpIfFalse(target) = chunk.ops[pc + 1] else { unreachable!("verified async close"); };
            let taken = fb.icmp(IntCC::Eq, status, four);
            fb.brif(taken, blocks[pc + 2], &[], blocks[target as usize], &[]);
        } else if let Some(target) = verify::successors(&op).1 {
            let taken = fb.icmp(IntCC::Eq, status, four);
            fb.brif(taken, blocks[target], &[], blocks[pc + 1], &[]);
        } else {
            fb.jump(blocks[pc + 1], &[]);
        }
    }
    fb.switch_to_block(thrown);
    fb.ret(&[one]);
    fb.switch_to_block(returned);
    fb.ret(&[zero]);
    fb.switch_to_block(suspended);
    fb.ret(&[two]);
    fb.seal_all_blocks();
    drop(fb);
    Ok(Built { function, max_stack, resumes })
}

fn landing(fb: &mut FunctionBuilder<'_>, frame: lumen_codegen::Value, land: FuncRef,
    handlers: &[(usize, usize)], blocks: &[Block], thrown: Block,
    zero: lumen_codegen::Value, ty: Type, cache: &mut BTreeMap<(usize, usize), Block>) -> Block {
    let Some(&(target, saved)) = handlers.last() else { return thrown; };
    if let Some(&block) = cache.get(&(target, saved)) { return block; }
    let origin = fb.current_block().expect("native block");
    let catch = fb.create_block();
    fb.switch_to_block(catch);
    let saved_value = fb.iconst(ty, saved as i64);
    let status = fb.call_fn(land, &[frame, saved_value])[0];
    let ok = fb.icmp(IntCC::Eq, status, zero);
    fb.brif(ok, blocks[target], &[], thrown, &[]);
    fb.switch_to_block(origin);
    cache.insert((target, saved), catch);
    catch
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lower(source: &str) -> Built {
        let body = crate::parser::parse_script(source, false).unwrap();
        let crate::ast::Stmt::FuncDecl(function) = &body[0] else { panic!("function"); };
        let chunk = crate::bytecode::compile(function).expect("bytecode input");
        build(&chunk, 64).unwrap()
    }

    #[test]
    fn exceptions_and_loops_have_only_native_helpers() {
        let built = lower("function f(x) { try { while (x > 0) { if (x === 2) throw x; x--; } return x; } catch (e) { return e; } }");
        lumen_codegen::verify::verify(&built.function).unwrap();
        assert!(built.function.funcs.iter().all(|helper| (ENTER..=RESUME).contains(&helper.id) || (0x1000..0x1000 + OP_NAMES.len() as u32).contains(&helper.id)));
        assert!(built.function.funcs.iter().any(|helper| helper.id == LAND));
        assert!(built.function.funcs.iter().any(|helper| helper.id == SAFEPOINT));
        assert_eq!(built.resumes, vec![(0, 0)]);
    }

    #[test]
    fn await_resume_is_a_native_entry_state() {
        let built = lower("async function f(x) { try { return await x; } catch (e) { return e; } }");
        lumen_codegen::verify::verify(&built.function).unwrap();
        assert_eq!(built.resumes.len(), 2);
        assert!(built.function.funcs.iter().any(|helper| helper.id == RESUME));
    }

    #[test]
    fn native_handler_roots_include_non_throwing_source_bodies() {
        let built = lower("function f() { try { return 1; } catch (e) { return e; } }");
        lumen_codegen::verify::verify(&built.function).unwrap();
    }
}
