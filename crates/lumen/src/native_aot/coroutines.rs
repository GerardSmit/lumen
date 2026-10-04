//! Owner-local native continuations, shared with the promise and generator drivers.
use super::{NativeFrame, NativeProgram, STATUS_OK, STATUS_SUSPEND, STATUS_THROW};
use crate::coroutine::{Resume, Suspend};
use crate::interpreter::{Abrupt, Env, Interp};
use crate::value::Value;
use std::rc::Rc;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Parked {
    Start,
    Await,
    Yield,
    Delegate,
}

pub struct NativeCoro {
    state: Option<Box<NativeFrame>>,
    pub(crate) frame: crate::interpreter::stack_trace::ResumeFrame,
    pub(crate) done: bool,
    pub(crate) started: bool,
}

impl NativeCoro {
    pub(crate) fn prologue(&mut self, i: &mut Interp) -> Result<(), Abrupt> {
        match self.step(i) {
            Suspend::Yield(_)
                if self
                    .state
                    .as_ref()
                    .is_some_and(|frame| frame.parked == Parked::Start) =>
            {
                Ok(())
            }
            Suspend::Throw(error) => Err(Abrupt::Throw(error)),
            _ => Err(i.throw(
                "Error",
                "native generator prologue did not reach initial yield",
            )),
        }
    }

    pub(crate) fn resume(&mut self, i: &mut Interp, signal: Resume) -> Suspend {
        if self.done {
            return Suspend::Done(Value::Undefined);
        }
        let frame = self.state.as_mut().expect("live native coroutine frame");
        match signal {
            Resume::Next(value) => match frame.parked {
                Parked::Start if !self.started => {}
                Parked::Yield => frame.stack.extend([value, Value::Bool(false)]),
                Parked::Delegate => frame.stack.extend([value, Value::Num(0.0)]),
                _ => frame.stack.push(value),
            },
            Resume::Throw(value) if self.started && frame.parked == Parked::Delegate => {
                frame.stack.extend([value, Value::Num(1.0)])
            }
            Resume::Throw(value) if self.started => frame.pending_resume = Some(value),
            Resume::Throw(value) => {
                self.done = true;
                self.state = None;
                return Suspend::Throw(value);
            }
            Resume::Return(value) if self.started && frame.parked == Parked::Yield => {
                frame.stack.extend([value, Value::Bool(true)])
            }
            Resume::Return(value) if self.started && frame.parked == Parked::Delegate => {
                frame.stack.extend([value, Value::Num(2.0)])
            }
            Resume::Return(value) => {
                self.done = true;
                self.state = None;
                return Suspend::Done(value);
            }
        }
        self.started = true;
        self.step(i)
    }

    fn step(&mut self, i: &mut Interp) -> Suspend {
        let frame = self.state.as_mut().expect("live native coroutine frame");
        frame.interp = i as *mut Interp;
        frame.tail_allowed = false;
        let flags = frame.program.metadata.functions[frame.function as usize].flags;
        let saved = (
            i.strict,
            i.tco_ok,
            i.in_field_init_code,
            i.in_async_gen_body,
        );
        i.strict = flags & 2 != 0;
        i.tco_ok = false;
        if flags & 1 == 0 {
            i.in_field_init_code = false;
        }
        i.in_async_gen_body = flags & (4 | 8) == (4 | 8);
        let status = super::execute_frame(frame);
        (
            i.strict,
            i.tco_ok,
            i.in_field_init_code,
            i.in_async_gen_body,
        ) = saved;
        let result = std::mem::replace(&mut frame.result, Value::Undefined);
        let outcome = match status {
            STATUS_OK => Suspend::Done(result),
            STATUS_THROW => {
                Suspend::Throw(std::mem::replace(&mut frame.exception, Value::Undefined))
            }
            STATUS_SUSPEND => match frame.parked {
                Parked::Await => Suspend::Await(result),
                _ => Suspend::Yield(result),
            },
            _ => Suspend::Throw(i.make_error("Error", "invalid native coroutine status")),
        };
        if matches!(outcome, Suspend::Done(_) | Suspend::Throw(_)) {
            self.done = true;
            self.state = None;
        }
        outcome
    }
}

pub(crate) fn call(
    i: &mut Interp,
    program: &Rc<NativeProgram>,
    index: u32,
    env: &Env,
    this: Value,
    args: &[Value],
) -> Result<Value, Abrupt> {
    let flags = program.metadata.functions[index as usize].flags;
    let mut state = super::prepare_frame(i, program, index, env, this, args)?;
    state.tail_allowed = false;
    let coroutine = NativeCoro {
        state: Some(state),
        frame: Default::default(),
        done: false,
        started: false,
    };
    i.call_native_coroutine(coroutine, flags)
}
