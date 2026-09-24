//! `for await (… of …)` on the bytecode VM.
//!
//! The loop keeps three hidden slots: the iterator, its `next` method and a *state* number:
//! `0` for an async iterator (each `next()` result is awaited, then unpacked), or, for a sync
//! iterator driven async-from-sync, `1` while a step is in flight, `2` when the step produced a
//! value and `3` when the step reported `done` (a rejection of a not-done step's value closes
//! the sync iterator inside the wrapper's reaction job — closeOnRejection). The tree-walker's tick structure is reproduced exactly:
//! an async-from-sync value settles through a `then` of its resolved promise and the loop's
//! `Await` adds the outer tick.
//!
//! Emitted shape (the for-of arm of `Compiler::stmt`, plus the `Compiler` helpers below):
//!
//! ```text
//! head: AsyncIterNext(it, next, st) Await AsyncIterResult(st) JumpIfFalse(exit)
//!       <bind> PushHandler(body_pad) <body> PopHandler Jump(head)
//! body_pad: <throw-mode AsyncIteratorClose, errors swallowed> rethrow
//! exit: Pop
//! ```
//!
//! A normal-mode close (`break`/`return`) is `AsyncCloseCall(it, st) JumpIfFalse(L) Await
//! AsyncCloseCheck(st) L:` — no `return` method means no await at all.
use super::{Compiler, Op, PushValue};
use crate::interpreter::{Abrupt, Interp};
use crate::value::Value;

const ASYNC: f64 = 0.0;
const SYNC_STEP: f64 = 1.0;
const SYNC_VALUE: f64 = 2.0;
const SYNC_DONE: f64 = 3.0;

fn state(v: &Value) -> f64 {
    match v {
        Value::Num(n) => *n,
        _ => ASYNC,
    }
}

/// `GetAsyncIter`: pops the iterable, pushes the iterator, its `next` and the initial state.
pub(super) fn get_async_iter(i: &mut Interp, stack: &mut impl PushValue, rhs: Value) -> Result<(), Abrupt> {
    let akey = crate::builtins::async_iterator_key(i);
    let method = match &akey {
        Some(k) => i.get_member(&rhs, k)?,
        None => Value::Undefined,
    };
    // GetMethod: a non-callable, non-nullish @@asyncIterator is a TypeError.
    if !matches!(method, Value::Undefined | Value::Null) && !method.is_callable() {
        return Err(i.throw("TypeError", "@@asyncIterator is not callable"));
    }
    let (iter, next, st) = if method.is_callable() {
        let it = i.call(method, rhs, &[])?;
        if !matches!(it, Value::Obj(_)) {
            return Err(i.throw("TypeError", "@@asyncIterator did not return an object"));
        }
        let next = i.get_member(&it, "next")?;
        (it, next, ASYNC)
    } else {
        let (it, _) = i.get_iterator(&rhs)?;
        // Same read as the tree-walker (which re-reads `next` off the sync iterator).
        let next = i.get_member(&it, "next")?;
        (it, next, SYNC_STEP)
    };
    stack.push_value(iter);
    stack.push_value(next);
    stack.push_value(Value::Num(st));
    Ok(())
}

/// `AsyncIterNext(it, next, st)`: call `next()`; pushes the value the loop awaits next.
pub(super) fn next(i: &mut Interp, slots: &mut [Value], it: u16, nx: u16, st: u16) -> Result<Value, Abrupt> {
    let sync = state(&slots[st as usize]) != ASYNC;
    if sync {
        slots[st as usize] = Value::Num(SYNC_STEP);
    }
    let iter = slots[it as usize].clone();
    let next = slots[nx as usize].clone();
    let res = i.call(next, iter, &[])?;
    if !sync {
        return Ok(res);
    }
    if !matches!(res, Value::Obj(_)) {
        return Err(i.throw("TypeError", "iterator result is not an object"));
    }
    let done = i.get_member(&res, "done")?;
    let done = i.to_boolean(&done);
    let raw = i.get_member(&res, "value")?;
    let close = (!done).then_some(&slots[it as usize]);
    let px = continuation(i, raw, close)?;
    slots[st as usize] = Value::Num(if done { SYNC_DONE } else { SYNC_VALUE });
    Ok(px)
}

/// AsyncFromSyncIteratorContinuation's promise for a sync result's `value`: the wrapper
/// settles via its own reaction job (one tick; the loop's `Await` adds another). With
/// `close_on_rejection` (a `next` step that is not done), a rejection closes the sync iterator
/// inside that reaction job, and an abrupt PromiseResolve closes it at once.
fn continuation(i: &mut Interp, raw: Value, close_on_rejection: Option<&Value>) -> Result<Value, Abrupt> {
    match i.promise_resolve_checked(raw) {
        Ok(p) => {
            let on_r = match close_on_rejection {
                Some(iter) => {
                    let iter = iter.clone();
                    let f: std::rc::Rc<crate::value::NativeClosure> =
                        std::rc::Rc::new(move |i: &mut Interp, _this: Value, args: &[Value]| {
                            let reason = args.first().cloned().unwrap_or(Value::Undefined);
                            i.iterator_close(&iter);
                            Err(reason)
                        });
                    i.new_native_fn("", 1, f)
                }
                None => Value::Undefined,
            };
            Ok(i.promise_then(&p, Value::Undefined, on_r))
        }
        Err(e) => {
            if let Some(iter) = close_on_rejection {
                let iter = iter.clone();
                i.iterator_close(&iter);
            }
            let p = i.new_promise();
            i.reject_promise(&p, e);
            Ok(p)
        }
    }
}

/// `AsyncIterResult(st)`: unpack the awaited step into (value, has-value), like `IterStepL`.
pub(super) fn result(i: &mut Interp, stack: &mut impl PushValue, slots: &[Value], st: u16, v: Value) -> Result<(), Abrupt> {
    let s = state(&slots[st as usize]);
    let (value, more) = if s == ASYNC {
        if !matches!(v, Value::Obj(_)) {
            return Err(i.throw("TypeError", "iterator result is not an object"));
        }
        let done = i.get_member(&v, "done")?;
        if i.to_boolean(&done) {
            (Value::Undefined, false)
        } else {
            (i.get_member(&v, "value")?, true)
        }
    } else if s == SYNC_DONE {
        (Value::Undefined, false)
    } else {
        (v, true)
    };
    stack.push_value(value);
    stack.push_value(Value::Bool(more));
    Ok(())
}

/// `AsyncCloseCall(it, st)`: AsyncIteratorClose's call half. Pushes `false` when there is no
/// `return` method, else the value to await and `true`.
pub(super) fn close_call(i: &mut Interp, stack: &mut impl PushValue, slots: &[Value], it: u16, st: u16) -> Result<(), Abrupt> {
    let iter = slots[it as usize].clone();
    let ret = i.get_member(&iter, "return")?;
    if matches!(ret, Value::Undefined | Value::Null) {
        stack.push_value(Value::Bool(false));
        return Ok(());
    }
    if !ret.is_callable() {
        return Err(i.throw("TypeError", "iterator 'return' is not callable"));
    }
    let r = i.call(ret, iter, &[])?;
    if state(&slots[st as usize]) == ASYNC {
        stack.push_value(r);
    } else {
        if !matches!(r, Value::Obj(_)) {
            return Err(i.throw("TypeError", "iterator result is not an object"));
        }
        // %AsyncFromSyncIteratorPrototype%.return settles through the same continuation as
        // `next` (two ticks before the loop resumes, as in V8).
        let raw = i.get_member(&r, "value")?;
        let px = continuation(i, raw, None)?;
        stack.push_value(px);
    }
    stack.push_value(Value::Bool(true));
    Ok(())
}

/// `AsyncCloseCheck(st)`: the awaited `return()` result of an async iterator must be an object.
pub(super) fn close_check(i: &mut Interp, slots: &[Value], st: u16, v: Value) -> Result<(), Abrupt> {
    if state(&slots[st as usize]) == ASYNC && !matches!(v, Value::Obj(_)) {
        return Err(i.throw("TypeError", "iterator result is not an object"));
    }
    Ok(())
}

impl Compiler {
    /// A normal-mode AsyncIteratorClose (a `break`/`return` leaving a `for await`).
    pub(super) fn emit_async_close(&mut self, it: u16, st: u16) {
        self.emit(Op::AsyncCloseCall(it, st));
        let jf = self.emit(Op::JumpIfFalse(0));
        self.emit(Op::Await);
        self.emit(Op::AsyncCloseCheck(st));
        self.patch(jf);
    }

    /// Close the for-of iterator in slot `it` in normal mode (sync or async loop).
    pub(super) fn emit_iter_close(&mut self, it: u16) {
        let st = self
            .loops
            .iter()
            .rev()
            .find(|c| c.foreach_iter == Some(it))
            .and_then(|c| c.foreach_async);
        match st {
            Some(st) => self.emit_async_close(it, st),
            None => {
                self.emit(Op::IterCloseL(it));
            }
        }
    }

    /// The body's throw pad of a `for await`: the exception is on the stack. Close in throw
    /// mode (the close's own errors are swallowed, but its awaits still happen) and rethrow.
    pub(super) fn emit_async_abort(&mut self, it: u16, st: u16) {
        let exc = self.fresh_slot("%exc%");
        self.emit(Op::StoreLocal(exc));
        let push = self.emit(Op::PushHandler(0));
        self.emit_async_close(it, st);
        self.emit(Op::PopHandler);
        self.emit(Op::LoadLocal(exc));
        self.emit(Op::Throw);
        let pad = self.ops.len() as u32;
        if let Op::PushHandler(t) = &mut self.ops[push] {
            *t = pad;
        }
        self.emit(Op::Pop);
        self.emit(Op::LoadLocal(exc));
        self.emit(Op::Throw);
    }
}
