//! The readiness reactor of a loop that blocks in a [`Poller`].
//!
//! A runtime creates a [`LoopReactor`] and keeps a clone in its [`OpState`](crate::OpState), so a
//! native op reaches the loop's reactor from its `Ctx` and registers a descriptor with a wake that
//! runs on the loop thread, inside the loop's own turn. Nothing exists until the first use: the
//! poller (its kernel queue and wake descriptor) is created by the first [`LoopReactor::poller`]
//! call, which is either the first registration or the loop's first block.
//!
//! A wake must be short and must not block; typically it reads what is ready and sends a
//! [`TaskCompletion`](crate::TaskCompletion), or records the readiness for the next completion.
//! Hosts that drive the loop with `run_until_idle` instead of blocking get their wakes from a
//! non-blocking [`LoopReactor::poll`].

use crate::CompletionTx;
use lumen_os::reactor::{Poller, Reactor};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

struct Inner {
    tx: CompletionTx,
    poller: OnceLock<Option<Arc<Poller>>>,
}

/// A loop's lazily created [`Poller`]. Clones share it.
#[derive(Clone)]
pub struct LoopReactor {
    inner: Arc<Inner>,
}

impl LoopReactor {
    /// The reactor of the loop whose completions arrive through `tx`.
    pub fn new(tx: CompletionTx) -> LoopReactor {
        LoopReactor {
            inner: Arc::new(Inner {
                tx,
                poller: OnceLock::new(),
            }),
        }
    }

    /// The loop's poller, created on first use; `None` where the platform has no reactor backend
    /// (wasm32, Windows until its backend lands), in which case the loop blocks on its channel.
    /// Creating it routes every later completion through the poller's coalesced waker.
    pub fn poller(&self) -> Option<&Arc<Poller>> {
        self.inner
            .poller
            .get_or_init(|| {
                let poller = Poller::new().ok()?;
                self.inner.tx.set_loop_waker(poller.waker());
                Some(Arc::new(poller))
            })
            .as_ref()
    }

    /// Whether [`LoopReactor::poller`] has been called, successfully or not.
    pub fn is_started(&self) -> bool {
        self.inner.poller.get().is_some()
    }

    /// The reactor to register sources with; wakes run on the loop thread.
    pub fn reactor(&self) -> Option<Arc<dyn Reactor>> {
        self.poller().map(|poller| Arc::clone(poller) as Arc<dyn Reactor>)
    }

    /// Runs the wakes of registrations that are ready now, without blocking. A no-op until the
    /// poller exists.
    pub fn poll(&self) -> usize {
        match self.inner.poller.get() {
            Some(Some(poller)) => poller.turn(Some(Duration::ZERO)).unwrap_or(0),
            _ => 0,
        }
    }
}
