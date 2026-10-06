//! The data and traits of the service-worker classes: what a host's registry tells a page's
//! `navigator.serviceWorker`, and what a service worker's global scope asks its host.
//!
//! Everything a registry reports is plain `Send` data ([`RegistrationRecord`], [`WorkerRecord`],
//! [`ClientRecord`]); the page and scope classes own the script-visible wrappers and cache them
//! per registration, worker and client id. Changes are pushed as [`super::WorkerEvent`]s to the
//! [`Control`] a page [`subscribe`](ServiceWorkerRegistry::subscribe)s with; nothing is polled.

use super::control::Control;
use crate::clone_transfer::CloneMessage;
use lumen_bind::NativeResult;
use lumen_common::cors::{Credentials, Mode, Redirect};

/// `ServiceWorkerState`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum WorkerState {
    Parsed,
    Installing,
    Installed,
    Activating,
    Activated,
    Redundant,
}

impl WorkerState {
    pub fn as_str(self) -> &'static str {
        match self {
            WorkerState::Parsed => "parsed",
            WorkerState::Installing => "installing",
            WorkerState::Installed => "installed",
            WorkerState::Activating => "activating",
            WorkerState::Activated => "activated",
            WorkerState::Redundant => "redundant",
        }
    }
}

/// `ServiceWorkerUpdateViaCache`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateViaCache {
    Imports,
    All,
    None,
}

impl UpdateViaCache {
    pub fn as_str(self) -> &'static str {
        match self {
            UpdateViaCache::Imports => "imports",
            UpdateViaCache::All => "all",
            UpdateViaCache::None => "none",
        }
    }

    pub(crate) fn parse(text: &str) -> Option<Self> {
        match text {
            "imports" => Some(UpdateViaCache::Imports),
            "all" => Some(UpdateViaCache::All),
            "none" => Some(UpdateViaCache::None),
            _ => None,
        }
    }
}

/// One service worker as a page sees it. `id` identifies the worker (the script and its
/// version) for as long as the registry keeps it; a new version is a new id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerRecord {
    pub id: u64,
    pub script_url: String,
    pub state: WorkerState,
}

/// One registration. `id` is stable for a scope for as long as the registration exists (an
/// update installs a new worker under the same id), because the page caches its
/// `ServiceWorkerRegistration` by it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistrationRecord {
    pub id: u64,
    pub scope: String,
    pub update_via_cache: UpdateViaCache,
    pub installing: Option<WorkerRecord>,
    pub waiting: Option<WorkerRecord>,
    pub active: Option<WorkerRecord>,
}

/// The page a call comes from, read from the realm's `location` at call time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientInfo {
    pub url: String,
    pub origin: String,
    pub secure: bool,
}

/// A registry job (`register()` or `update()`); the registry settles it with
/// [`super::WorkerEvent::JobSettled`].
pub type JobId = u64;

/// The validated arguments of `ServiceWorkerContainer.register`. The URLs are absolute (resolved
/// against the page); the registry applies its own policy (same origin, scheme, scope rules) and
/// fails the job with a `SecurityError` or similar.
#[derive(Clone, Debug)]
pub struct RegisterRequest {
    pub script_url: String,
    pub scope: Option<String>,
    pub module: bool,
    pub update_via_cache: UpdateViaCache,
}

/// What a page's `navigator.serviceWorker` asks its host. All methods run on the page realm's
/// thread; a registry that lives elsewhere keeps the `Control`s it is given and pushes to them
/// from any thread (they are `Send`).
///
/// Ordering contract for pushes (the page applies them in order): a
/// [`super::WorkerEvent::Registration`] snapshot comes before the
/// [`super::WorkerEvent::UpdateFound`] and [`super::WorkerEvent::StateChange`] events it explains,
/// and every state transition of a worker is its own `StateChange`. A page never moves a
/// wrapper's state backwards, so a snapshot that is newer than the events behind it is harmless.
pub trait ServiceWorkerRegistry: 'static {
    /// Start a registration job.
    fn register(&self, client: &ClientInfo, request: RegisterRequest) -> NativeResult<JobId>;

    /// Start an update job for `registration`.
    fn update(&self, client: &ClientInfo, registration: u64) -> NativeResult<JobId>;

    /// `ServiceWorkerRegistration.unregister()`: whether a registration was removed.
    fn unregister(&self, client: &ClientInfo, registration: u64) -> NativeResult<bool>;

    /// Every registration of the client's origin.
    fn registrations(&self, client: &ClientInfo) -> Vec<RegistrationRecord>;

    /// The worker controlling the client.
    fn controller(&self, client: &ClientInfo) -> Option<WorkerRecord>;

    /// `ServiceWorker.postMessage`: queue `message` for `worker`, delivered to its scope as an
    /// `ExtendableMessageEvent` whose `source` is the client.
    fn post_to_worker(
        &self,
        client: &ClientInfo,
        worker: u64,
        message: CloneMessage,
    ) -> NativeResult<()>;

    /// Push registry changes for this client to `control` from now on. Called once per realm,
    /// the first time the page uses `navigator.serviceWorker`. A registry drops a control whose
    /// [`Control::send`] fails.
    fn subscribe(&self, client: &ClientInfo, control: Control);
}

/// `ClientType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientKind {
    Window,
    Worker,
    SharedWorker,
}

impl ClientKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ClientKind::Window => "window",
            ClientKind::Worker => "worker",
            ClientKind::SharedWorker => "sharedworker",
        }
    }
}

/// `FrameType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameType {
    Auxiliary,
    Nested,
    TopLevel,
    None,
}

impl FrameType {
    pub fn as_str(self) -> &'static str {
        match self {
            FrameType::Auxiliary => "auxiliary",
            FrameType::Nested => "nested",
            FrameType::TopLevel => "top-level",
            FrameType::None => "none",
        }
    }
}

/// One client of a service worker (`Client` / `WindowClient`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientRecord {
    pub id: String,
    pub url: String,
    pub kind: ClientKind,
    pub frame_type: FrameType,
}

/// `ExtendableEvent` types the host fires with [`super::WorkerEvent::Lifecycle`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleKind {
    Install,
    Activate,
}

impl LifecycleKind {
    pub(crate) fn event_type(self) -> &'static str {
        match self {
            LifecycleKind::Install => "install",
            LifecycleKind::Activate => "activate",
        }
    }
}

/// One intercepted request, for [`super::WorkerEvent::Fetch`].
#[derive(Clone, Debug)]
pub struct FetchRequest {
    /// Names the event in [`ServiceScopeHost::fetch_response`] and
    /// [`ServiceScopeHost::event_settled`].
    pub event: u64,
    pub url: String,
    pub method: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub mode: Mode,
    pub credentials: Credentials,
    pub redirect: Redirect,
    pub client_id: String,
}

/// How a `FetchEvent` ended, for the request that was intercepted.
#[derive(Clone, Debug)]
pub enum FetchOutcome {
    /// `respondWith` resolved to a `Response`: its head and the whole body.
    Response {
        status: u16,
        status_text: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    /// No listener called `respondWith`: the browser handles the request.
    Fallback,
    /// `respondWith` was given something else than a `Response`, or its promise rejected.
    Error(String),
}

/// What a service worker's global scope asks its host (the registry side that owns the worker).
/// Events come in on the scope's [`Control`]; their outcomes go back through here.
pub trait ServiceScopeHost: 'static {
    /// The registration this worker belongs to (`self.registration`).
    fn registration(&self) -> RegistrationRecord;

    /// This worker (`self.serviceWorker`).
    fn worker(&self) -> WorkerRecord;

    /// `self.skipWaiting()`.
    fn skip_waiting(&self) -> NativeResult<()>;

    /// `clients.claim()`.
    fn claim(&self) -> NativeResult<()>;

    /// `clients.matchAll()`.
    fn match_all(&self, include_uncontrolled: bool) -> Vec<ClientRecord>;

    /// `clients.get(id)`.
    fn client(&self, id: &str) -> Option<ClientRecord>;

    /// `Client.postMessage`: deliver `message` to the client's `navigator.serviceWorker`.
    fn post_to_client(&self, client: &str, message: CloneMessage) -> NativeResult<()>;

    /// `respondWith` produced `outcome` for fetch event `event`.
    fn fetch_response(&self, event: u64, outcome: FetchOutcome);

    /// Event `event` finished: its listeners ran and every `waitUntil` / `respondWith` promise
    /// settled; `Err` carries the first rejection.
    fn event_settled(&self, event: u64, result: Result<(), String>);
}

/// Whether a document at `url` is a secure context (`isSecureContext`): `https:`, `wss:` and
/// `file:` URLs, and `http:` to the loopback hosts.
pub fn is_secure_context(url: &str) -> bool {
    let Ok(parsed) = lumen_common::url::parse(url, None) else {
        return false;
    };
    match parsed.scheme.as_str() {
        "https" | "wss" | "file" => true,
        "http" | "ws" => {
            let host = parsed.hostname();
            let host = host.trim_start_matches('[').trim_end_matches(']');
            host == "localhost"
                || host.ends_with(".localhost")
                || host == "::1"
                || host.starts_with("127.")
        }
        _ => false,
    }
}
