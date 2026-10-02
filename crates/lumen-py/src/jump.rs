//! Assigning `frame.f_lineno` from a trace function: moves the running frame to the first
//! instruction of another line, after checking that the stack and the block stack there are
//! compatible with the ones here. The check follows the VM's own instructions with an abstract
//! stack (what kind of item sits in each slot), starting from the first instruction.

use crate::bytecode::{Code, Op, MF_CLOSURE, MF_DEFAULTS, MF_KWDEFAULTS, MF_ANNOTATIONS};
use crate::object::*;
use crate::trace::ev;
use crate::vm::{Block, Interp};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Slot {
    Value,
    Iter,
    PrevExc,
    Exc,
    Exit,
}

#[derive(Clone, PartialEq)]
struct State {
    stack: Vec<Slot>,
    blocks: Vec<(u32, u32)>,
}

impl State {
    fn pop(&mut self, n: usize) -> Option<()> {
        let len = self.stack.len().checked_sub(n)?;
        self.stack.truncate(len);
        Some(())
    }

    fn push(&mut self, n: usize) {
        self.stack.extend(std::iter::repeat_n(Slot::Value, n));
    }

    fn effect(mut self, pops: usize, pushes: usize) -> Option<State> {
        self.pop(pops)?;
        self.push(pushes);
        Some(self)
    }
}

/// The states the instruction at `pc` leads to when it runs in state `st`: the next instruction
/// and/or its jump target, and the handler a block set up here would enter.
fn flow(op: Op, pc: usize, st: &State) -> Option<Vec<(usize, State)>> {
    let next = pc + 1;
    let mut s = st.clone();
    let one = |s: State| Some(vec![(next, s)]);
    let eff = |s: &State, pops: usize, pushes: usize| s.clone().effect(pops, pushes).map(|s| vec![(next, s)]);
    match op {
        Op::Nop | Op::SetupAnnotations | Op::LoadMethod(_) | Op::CallMethod(_) => one(s),
        Op::Pop => eff(&s, 1, 0),
        Op::Dup => {
            let k = *s.stack.last()?;
            s.stack.push(k);
            one(s)
        }
        Op::DupTwo => {
            let n = s.stack.len();
            let (a, b) = (*s.stack.get(n.checked_sub(2)?)?, s.stack[n - 1]);
            s.stack.push(a);
            s.stack.push(b);
            one(s)
        }
        Op::Swap(n) => {
            let l = s.stack.len();
            let n = n as usize;
            if n == 0 || n > l {
                return None;
            }
            s.stack.swap(l - 1, l - n);
            one(s)
        }
        Op::Rot3 | Op::Rot4 => {
            let depth = if op == Op::Rot3 { 2 } else { 3 };
            let top = s.stack.pop()?;
            let l = s.stack.len();
            s.stack.insert(l.checked_sub(depth)?, top);
            one(s)
        }
        Op::LoadConst(_)
        | Op::LoadFast(_)
        | Op::LoadName(_)
        | Op::LoadGlobal(_)
        | Op::LoadDeref(_)
        | Op::LoadClosure(_)
        | Op::LoadClassDeref(_)
        | Op::LoadLocals
        | Op::LoadBuildClass
        | Op::LoadAssertionError
        | Op::WithExceptStart
        | Op::ImportFrom(_)
        | Op::GetANext
        | Op::MatchSequence
        | Op::MatchMapping
        | Op::MatchLen(..) => eff(&s, 0, 1),
        Op::StoreFast(_) | Op::StoreName(_) | Op::StoreGlobal(_) | Op::StoreDeref(_) | Op::DelAttr(_) | Op::ImportStar => eff(&s, 1, 0),
        Op::DelFast(_) | Op::DelName(_) | Op::DelGlobal(_) | Op::DelDeref(_) => one(s),
        Op::LoadAttr(_)
        | Op::Unary(_)
        | Op::ListToTuple
        | Op::CheckExcMatch
        | Op::ExcStarWrap
        | Op::ExcStarEnd
        | Op::WithCallExit
        | Op::YieldValue
        | Op::GetAwaitable
        | Op::GetYieldFromIter
        | Op::GetAIter
        | Op::AsyncGenWrap
        | Op::LoadFromDictOrDeref(_)
        | Op::LoadFromDictOrGlobals(_)
        | Op::CallIntrinsic1(_)
        | Op::MatchSeqItem(_)
        | Op::MatchStarSlice(..) => eff(&s, 1, 1),
        Op::StoreAttr(_) | Op::DelSubscr | Op::MapAdd(_) => eff(&s, 2, 0),
        Op::StoreSubscr => eff(&s, 3, 0),
        Op::Subscr | Op::Binary(_) | Op::Inplace(_) | Op::Compare(_) | Op::YieldFrom | Op::ImportName(_) | Op::CallIntrinsic2(_) | Op::MatchClass(..) => eff(&s, 2, 1),
        Op::ListAppend(_) | Op::SetAdd(_) | Op::ListExtend(_) | Op::SetUpdate(_) | Op::DictUpdate(_) | Op::KwMerge => eff(&s, 1, 0),
        Op::ExcStarSplit => eff(&s, 2, 2),
        Op::EndFinally => eff(&s, 2, 0),
        Op::BuildTuple(n) | Op::BuildList(n) | Op::BuildSet(n) | Op::BuildSetConst(n) | Op::BuildSlice(n) | Op::BuildString(n) | Op::MatchKeys(n) | Op::MatchRest(n) => {
            eff(&s, n as usize, 1)
        }
        Op::BuildMap(n) => eff(&s, 2 * n as usize, 1),
        Op::UnpackSequence(n) => eff(&s, 1, n as usize),
        Op::UnpackEx(before, after) => eff(&s, 1, (before + after + 1) as usize),
        Op::FormatValue(_, spec) => eff(&s, 1 + spec as usize, 1),
        Op::MakeFunction(flags) => {
            let extra = [MF_CLOSURE, MF_ANNOTATIONS, MF_KWDEFAULTS, MF_DEFAULTS].iter().filter(|f| flags & **f != 0).count();
            eff(&s, 1 + extra, 1)
        }
        Op::Call(n) => eff(&s, n as usize + 1, 1),
        Op::CallKw(n) => eff(&s, n as usize + 2, 1),
        Op::CallEx(flags) => eff(&s, 2 + (flags & 1) as usize, 1),
        Op::Jump(t) => Some(vec![(t as usize, s)]),
        Op::JumpIfFalse(t) | Op::JumpIfTrue(t) => {
            s.pop(1)?;
            Some(vec![(next, s.clone()), (t as usize, s)])
        }
        Op::JumpIfFalseKeep(t) | Op::JumpIfTrueKeep(t) => {
            let taken = s.clone();
            s.pop(1)?;
            Some(vec![(next, s), (t as usize, taken)])
        }
        Op::GetIter => {
            s.pop(1)?;
            s.stack.push(Slot::Iter);
            one(s)
        }
        Op::ForIter(t) => {
            let mut body = s.clone();
            body.push(1);
            s.pop(1)?;
            Some(vec![(next, body), (t as usize, s)])
        }
        Op::SetupBlock(t) => {
            let depth = s.stack.len() as u32;
            let mut handler = s.clone();
            handler.stack.push(Slot::Exc);
            s.blocks.push((t, depth));
            Some(vec![(next, s), (t as usize, handler)])
        }
        Op::SetupWith(t) => {
            s.pop(1)?;
            s.stack.push(Slot::Exit);
            let depth = s.stack.len() as u32;
            let mut handler = s.clone();
            handler.stack.push(Slot::Exc);
            s.blocks.push((t, depth));
            s.push(1);
            Some(vec![(next, s), (t as usize, handler)])
        }
        Op::PopBlock => {
            s.blocks.pop()?;
            one(s)
        }
        Op::PushExcInfo => {
            s.pop(1)?;
            s.stack.push(Slot::PrevExc);
            s.stack.push(Slot::Exc);
            one(s)
        }
        Op::PopExcInfo(keep) => {
            let idx = s.stack.len().checked_sub(1 + keep as usize)?;
            s.stack.remove(idx);
            one(s)
        }
        Op::UnwindExc(keep) => {
            let idx = s.stack.len().checked_sub(2 + keep as usize)?;
            s.stack.remove(idx);
            s.stack.remove(idx);
            one(s)
        }
        Op::BeforeAsyncWith => {
            s.pop(1)?;
            s.stack.push(Slot::Exit);
            s.push(1);
            one(s)
        }
        Op::WithExceptEnd(t) => {
            s.pop(3)?;
            Some(vec![(t as usize, s)])
        }
        Op::EndAsyncFor(t) => {
            s.pop(2)?;
            Some(vec![(t as usize, s)])
        }
        Op::ReturnValue | Op::Raise(_) | Op::Reraise | Op::CleanupReraise => Some(Vec::new()),
    }
}

/// The state before each instruction reachable from the start of `code`.
fn analyze(code: &Code) -> Vec<Option<State>> {
    let n = code.ops.len();
    let mut states: Vec<Option<State>> = vec![None; n];
    let mut work = vec![0usize];
    states[0] = Some(State { stack: Vec::new(), blocks: Vec::new() });
    while let Some(pc) = work.pop() {
        let Some(st) = states[pc].clone() else { continue };
        for (to, next) in flow(code.ops[pc], pc, &st).unwrap_or_default() {
            if to < n && states[to].is_none() {
                states[to] = Some(next);
                work.push(to);
            }
        }
    }
    states
}

/// Checks that `target` can replace `source` as the next instruction and returns the number of
/// stack items to keep.
fn check_move(source: &State, target: &State) -> Result<usize, &'static str> {
    let common = source.stack.iter().zip(&target.stack).take_while(|(a, b)| a == b).count();
    if common < target.stack.len() {
        return Err(match target.stack[common] {
            Slot::Iter if common == source.stack.len() => "can't jump into the body of a for loop",
            Slot::Exc | Slot::PrevExc if common == source.stack.len() => "can't jump into an 'except' block as there's no exception",
            _ if common == source.stack.len() => "can't jump into the middle of a block: incompatible stack",
            _ => "can't jump: the stack at the target line does not match the stack here",
        });
    }
    Ok(common)
}

/// `frame.f_lineno = line`: `at` is the position of the frame on the interpreter stack when it
/// is running.
pub fn set_lineno(it: &mut Interp, at: Option<usize>, line: i64) -> R<()> {
    let event = it.mon.cur_event;
    let running = at.filter(|&a| a + 1 == it.frames.len() && event != 0);
    let Some(at) = running else {
        return Err(it.value_error("f_lineno can only be set by a trace function"));
    };
    let (code, pc) = {
        let f = &it.frames[at];
        (f.code.clone(), f.pc)
    };
    if pc == 0 || event & (ev::PY_START | ev::PY_RESUME | ev::PY_THROW) != 0 {
        return Err(it.value_error("can't jump from the 'call' trace event of a new frame"));
    }
    if event != ev::LINE {
        return Err(it.value_error("can only jump from a 'line' trace event"));
    }
    let first = code.lines.iter().copied().filter(|&l| l != 0).min().map_or(code.first_line, |m| m.min(code.first_line));
    let last = code.lines.iter().copied().max().unwrap_or(code.first_line);
    if line < first as i64 {
        return Err(it.value_error(&format!("line {line} comes before the current code block")));
    }
    if line > last as i64 {
        return Err(it.value_error(&format!("line {line} comes after the current code block")));
    }
    let Some(target) = lumen_common::lineno::first_instruction_at_or_after(&code.lines, line as u32) else {
        return Err(it.value_error(&format!("line {line} comes after the current code block")));
    };
    let states = analyze(&code);
    let (Some(source), Some(dest)) = (states.get(pc - 1).and_then(|s| s.as_ref()), states.get(target).and_then(|s| s.as_ref())) else {
        return Err(it.value_error("cannot find bytecode for specified line"));
    };
    let keep = match check_move(source, dest) {
        Ok(keep) => keep,
        Err(msg) => return Err(it.value_error(msg)),
    };
    let mut restored = None;
    let frame = &mut it.frames[at];
    for slot in source.stack[keep..].iter().rev() {
        let item = frame.stack.pop();
        if *slot == Slot::PrevExc {
            restored = Some(item.unwrap_or(Value::None));
        }
    }
    frame.stack.truncate(dest.stack.len());
    frame.blocks = dest.blocks.iter().map(|&(handler, depth)| Block { handler, depth }).collect();
    frame.pc = target;
    frame.jump_back = target < pc;
    if let Some(prev) = restored {
        it.handled = prev.as_obj().cloned();
    }
    Ok(())
}
