//! Readiness for [`Source::Host`] sources that the embedding host delivers itself (Bitnest's
//! kernel sockets and pipes). The host provides three hooks; there is no loop here, the host calls
//! the [`Wake`] it was given when a source is ready.

use super::{Interest, IoState, Ready, Reactor, Registration, RegistrationOps, Source, Wake};
use crate::sched::SchedError;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

/// The embedder's side. `register` returns an id for `rearm` and `deregister`; its error is an
/// errno. A host wake carries no detail, so the registration reports the armed interest as ready.
#[derive(Clone, Copy)]
pub struct HostHooks {
    pub register: fn(token: u64, interest: u8, wake: Arc<dyn Wake>) -> Result<u64, i32>,
    pub rearm: fn(id: u64, interest: u8) -> Result<(), i32>,
    pub deregister: fn(id: u64),
}

pub struct HostedReactor {
    hooks: HostHooks,
}

impl HostedReactor {
    pub const fn new(hooks: HostHooks) -> Self {
        Self { hooks }
    }
}

fn host_error(errno: i32) -> SchedError {
    SchedError::Exhausted(format!("host reactor error {errno}"))
}

struct HostOps {
    hooks: HostHooks,
    id: u64,
    state: Arc<IoState>,
    armed: Arc<AtomicU8>,
}

impl RegistrationOps for HostOps {
    fn rearm(&self, interest: Interest) -> Result<(), SchedError> {
        self.armed.store(interest.bits(), Ordering::Release);
        (self.hooks.rearm)(self.id, interest.bits()).map_err(host_error)
    }
}

impl Drop for HostOps {
    fn drop(&mut self) {
        self.state.mark_closed();
        (self.hooks.deregister)(self.id);
    }
}

struct HostWake {
    state: Arc<IoState>,
    armed: Arc<AtomicU8>,
    wake: Arc<dyn Wake>,
}

impl Wake for HostWake {
    fn wake(&self) {
        if self.state.is_closed() {
            return;
        }
        let bits = self.armed.load(Ordering::Acquire);
        self.state.set(Ready::from_bits(bits));
        self.wake.wake();
    }
}

impl Reactor for HostedReactor {
    fn register(
        &self,
        src: Source,
        interest: Interest,
        wake: Arc<dyn Wake>,
    ) -> Result<Registration, SchedError> {
        #[allow(irrefutable_let_patterns)]
        let Source::Host(token) = src
        else {
            return Err(SchedError::Unsupported("only host sources on a hosted reactor"));
        };
        let state = Arc::new(IoState::default());
        let armed = Arc::new(AtomicU8::new(interest.bits()));
        let hook_wake = Arc::new(HostWake { state: state.clone(), armed: armed.clone(), wake });
        let id = (self.hooks.register)(token, interest.bits(), hook_wake).map_err(host_error)?;
        let ops = HostOps { hooks: self.hooks, id, state: state.clone(), armed };
        Ok(Registration::new(state, Box::new(ops)))
    }
}
