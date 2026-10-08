//! `Worker`, `SharedWorker` and the worker global scopes as native classes over a host-supplied
//! [`WorkerBackend`]. Design: `docs/native-workers.md`.
//!
//! - [`control`]: [`Control`], the `Send` event queue between a worker and the object that
//!   represents it in another realm.
//! - [`backend`]: [`WorkerBackend`] (page side) and [`WorkerScopeHost`] (worker side), and where a
//!   realm finds its backend.
//! - [`page_bindings`]: `Worker` and `SharedWorker`.
//! - [`install_scope`]: turns a worker realm's global object into a dedicated, shared or service
//!   worker global scope.
//! - [`registry`]: the data and traits behind the service-worker classes ([`ServiceWorkerRegistry`]
//!   for a page, [`ServiceScopeHost`] for a service worker's scope).
//! - [`install_service_workers`]: `navigator.serviceWorker`, `ServiceWorker`,
//!   `ServiceWorkerRegistration` and `ServiceWorkerContainer` in one realm.
//!
//! A realm needs [`crate::ports::extension`], [`crate::clone_transfer::extension`], the messaging
//! classes ([`crate::messaging::install`]) and an event loop (a runtime's, or
//! [`crate::owner_loop`]).

mod backend;
mod control;
mod page;
mod registry;
mod scope;
mod service;
mod service_scope;

pub use backend::{
    set_backend, set_request_policy, WorkerRequestPolicy, unsupported, DedicatedSpec, ScopeKind, SharedSpec, WorkerBackend, WorkerScopeHost,
};
pub use control::{Control, WorkerEvent};
pub use page::bindings as page_bindings;
pub use page::{SharedWorker, Worker};
pub use scope::{dispatch_rejection, install_scope, ScopeInstall};
pub use scope::{dedicated, shared};
pub use scope::{WorkerGlobalScope, WorkerLocation};
pub use registry::{
    is_secure_context, ClientInfo, ClientKind, ClientRecord, FetchOutcome, FetchRequest,
    FrameType, JobId, LifecycleKind, RegisterRequest, RegistrationRecord, ServiceScopeHost,
    ServiceWorkerRegistry, UpdateViaCache, WorkerRecord, WorkerState,
};
pub use service::{
    install_service_workers, ServiceWorker, ServiceWorkerContainer, ServiceWorkerRegistration,
};
pub use service_scope::{
    Client, Clients, ExtendableEvent, ExtendableMessageEvent, FetchEvent, ServiceWorkerGlobalScope,
    WindowClient,
};

use crate::lazy_globals;
use lumen::embed::{Ctx, Value};

/// Publish `Worker` and `SharedWorker` as lazy globals of the realm.
pub fn install_page_classes(ctx: &mut Ctx) -> Result<(), Value> {
    lazy_globals::<page_bindings::Module>(ctx)
}
