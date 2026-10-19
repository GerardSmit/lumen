//! What a host supplies to run workers: the traits the page classes and the worker global scopes
//! call, and where a realm finds its implementation.

use super::control::Control;
use super::registry::ServiceScopeHost;
use crate::ports::PortTransfer;
use crate::Ctx;
use lumen_bind::{NativeError, NativeResult};
use lumen_common::cors::Credentials;
use lumen_common::worker::SharedWorkerKey;
use std::rc::Rc;

/// Everything a backend needs to start a dedicated worker.
pub struct DedicatedSpec {
    /// The script: the resolved URL when the page has a location, the path the page gave otherwise.
    pub url: String,
    pub module: bool,
    pub name: String,
    pub credentials: Credentials,
    /// The page's origin when it has a location (the backend enforces the same-origin rule and
    /// fetches HTTP(S) entries); `None` for a page without a location.
    pub owner_origin: Option<String>,
    /// The worker-side end of the implicit port pair.
    pub inside: PortTransfer,
    /// Where the backend reports `Error` and `Exit`.
    pub control: Control,
}

/// One connection of a page to a shared worker.
pub struct SharedSpec {
    pub key: SharedWorkerKey,
    /// What the worker runs: a file path or an absolute URL.
    pub entry: String,
    /// `entry` is a URL to fetch, not a file path.
    pub remote: bool,
    /// The page's end of the connection's port. The backend hooks its close to count clients.
    pub page_side: PortTransfer,
    /// The worker's end, delivered with [`super::WorkerEvent::Connect`].
    pub worker_side: PortTransfer,
    /// Where the backend reports `Error` and `Close` for this client.
    pub control: Control,
}

/// The page side of the worker machinery: starts, stops and counts workers. Scheduling is not
/// part of it; the realm's completion channel and task registry carry every wake.
pub trait WorkerBackend: 'static {
    /// Start a dedicated worker and return its id.
    fn spawn_dedicated(&self, _ctx: &mut Ctx, _spec: DedicatedSpec) -> NativeResult<u64> {
        Err(unsupported("Worker"))
    }

    /// `Worker.terminate()`: stop the worker as soon as possible.
    fn terminate(&self, _ctx: &mut Ctx, _id: u64) {}

    /// The worker reported `Exit`: release what the backend keeps for `id`.
    fn exited(&self, _ctx: &mut Ctx, _id: u64) {}

    /// Connect to (starting it when needed) the shared worker `spec.key` and return the
    /// connection's id.
    fn connect_shared(&self, _ctx: &mut Ctx, _spec: SharedSpec) -> NativeResult<u64> {
        Err(unsupported("SharedWorker"))
    }

    /// The page's end of connection `id` closed.
    fn disconnect_shared(&self, _ctx: &mut Ctx, _id: u64) {}
}

/// `NotSupportedError` for a class the backend does not provide.
pub fn unsupported(class: &str) -> NativeError {
    NativeError::named(
        "NotSupportedError",
        format!("{class} is not supported in this environment"),
    )
}

/// Which worker global scope a realm is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeKind {
    Dedicated,
    Shared,
    /// A service worker: [`WorkerScopeHost::service`] must be implemented.
    Service,
}

/// The worker side: what a global scope asks its host.
pub trait WorkerScopeHost: 'static {
    fn kind(&self) -> ScopeKind;

    /// The script's final response URL (`WorkerLocation`).
    fn location(&self) -> String;

    fn name(&self) -> String;

    /// A module worker (`importScripts` is unavailable).
    fn module(&self) -> bool;

    /// `self.close()`: stop the worker's loop.
    fn close(&self);

    /// One `importScripts` resource: its final URL and source text.
    fn load_classic_script(&self, ctx: &mut Ctx, url: &str) -> NativeResult<(String, String)>;

    /// An uncaught error in the worker, for the page's `error` event.
    fn report_error(&self, message: String);

    /// The service-worker half of the host, for [`ScopeKind::Service`].
    fn service(&self) -> Option<Rc<dyn ServiceScopeHost>> {
        None
    }
}

/// The document embedding checks its policy before allocating ports or spawning a worker.
pub trait WorkerRequestPolicy: 'static {
    fn check(&self, ctx: &mut Ctx, url: &str, shared: bool) -> lumen::embed::OpResult<bool>;
}
struct RequestPolicySlot(Rc<dyn WorkerRequestPolicy>);
pub fn set_request_policy(ctx: &mut Ctx, policy: Rc<dyn WorkerRequestPolicy>) {
    ctx.op_state().put(RequestPolicySlot(policy));
}
pub(crate) fn check_request_policy(ctx: &mut Ctx, url: &str, shared: bool) -> lumen::embed::OpResult<bool> {
    let policy = ctx.op_state().get::<RequestPolicySlot>().map(|slot|slot.0.clone());
    if let Some(policy) = policy { return policy.check(ctx, url, shared); }
    Ok(true)
}

struct BackendSlot(Rc<dyn WorkerBackend>);

/// Make `backend` the implementation behind this realm's `Worker` and `SharedWorker`.
pub fn set_backend(ctx: &mut Ctx, backend: Rc<dyn WorkerBackend>) {
    ctx.op_state().put(BackendSlot(backend));
}

pub(crate) fn backend(ctx: &mut Ctx) -> Option<Rc<dyn WorkerBackend>> {
    ctx.op_state().get::<BackendSlot>().map(|slot| slot.0.clone())
}
