//! `ServiceWorker`, `ServiceWorkerRegistration`, `ServiceWorkerContainer` and the service worker
//! global scope, over in-memory registries and scope hosts. The realm's loop is the owner loop, so
//! nothing runs until the test pumps it.

use super::*;
use crate::clone_transfer::{CloneAttachment, CloneMessage};
use crate::workers::{
    ClientKind, ClientRecord, FetchOutcome, FetchRequest, FrameType, JobId, LifecycleKind,
    RegisterRequest, RegistrationRecord, ScopeInstall, ScopeKind, ServiceScopeHost,
    ServiceWorkerRegistry, UpdateViaCache, WorkerEvent, WorkerRecord, WorkerScopeHost,
    WorkerState,
};
use lumen_bind::{NativeError, NativeResult};
use lumen_common::cors::{Credentials, Mode, Redirect};
use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn page_extension() -> Extension {
    Extension {
        name: "test-service-workers",
        modules: &[
            lazy_globals::<events::bindings::Module>,
            lazy_globals::<navigator::bindings::Module>,
            lazy_globals::<net::fetch_bindings::Module>,
            lazy_globals::<url::bindings::Module>,
        ],
        state_init: None,
        js_init: None,
        js_init_snapshot: None,
        lazy_globals: &[],
    }
}

/// A bare engine whose only event loop is the owner loop; `notifies` counts host wake-ups.
fn owner_engine() -> (Engine, Arc<AtomicUsize>) {
    let notifies = Arc::new(AtomicUsize::new(0));
    let mut engine = Engine::new();
    let counter = Arc::clone(&notifies);
    owner_loop::install(
        engine.ctx(),
        Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }),
    );
    install(
        &mut engine,
        &[
            page_extension(),
            ports::extension(),
            clone_transfer::extension(),
            messaging::extension(),
        ],
    );
    (engine, notifies)
}

fn eval_str(engine: &mut Engine, src: &str) -> String {
    match engine.eval(src, false).expect("parse") {
        Completion::Value(v) => v,
        Completion::Throw { name, message } => panic!("uncaught {name}: {message}"),
    }
}

fn pump_all(engine: &mut Engine) {
    let thrown = owner_loop::pump(engine, 1024);
    assert!(thrown.is_empty(), "pump reported a throw");
}

fn global_value(engine: &mut Engine, name: &str) -> Value {
    let global = engine.global_this();
    engine
        .ctx()
        .get_member(&global, name)
        .map_err(|_| ())
        .expect("global member")
}

const ORIGIN: &str = "https://example.test";
const SCOPE: &str = "https://example.test/app/";

fn worker(id: u64, state: WorkerState) -> WorkerRecord {
    WorkerRecord {
        id,
        script_url: format!("{SCOPE}sw{id}.js"),
        state,
    }
}

fn registration_in(
    id: u64,
    scope: &str,
    installing: Option<WorkerRecord>,
    waiting: Option<WorkerRecord>,
    active: Option<WorkerRecord>,
) -> RegistrationRecord {
    RegistrationRecord {
        id,
        scope: scope.to_owned(),
        update_via_cache: UpdateViaCache::Imports,
        installing,
        waiting,
        active,
    }
}

fn registration(
    id: u64,
    installing: Option<WorkerRecord>,
    waiting: Option<WorkerRecord>,
    active: Option<WorkerRecord>,
) -> RegistrationRecord {
    registration_in(id, SCOPE, installing, waiting, active)
}

// ---- the in-memory registry ---------------------------------------------------------------------

#[derive(Default)]
struct MemoryRegistry {
    controls: RefCell<Vec<Control>>,
    registrations: RefCell<Vec<RegistrationRecord>>,
    controller: RefCell<Option<WorkerRecord>>,
    registered: RefCell<Vec<(JobId, RegisterRequest)>>,
    updated: RefCell<Vec<(JobId, u64)>>,
    unregistered: RefCell<Vec<u64>>,
    posted: RefCell<Vec<(u64, CloneMessage)>>,
    next_job: Cell<u64>,
}

use crate::workers::Control;

impl MemoryRegistry {
    fn new() -> Rc<Self> {
        Rc::new(Self::default())
    }

    fn with_registrations(records: Vec<RegistrationRecord>) -> Rc<Self> {
        let registry = Self::new();
        *registry.registrations.borrow_mut() = records;
        registry
    }

    fn subscriptions(&self) -> usize {
        self.controls.borrow().len()
    }

    /// Push one freshly built event to every subscribed page.
    fn push(&self, mut event: impl FnMut() -> WorkerEvent) {
        for control in self.controls.borrow().iter() {
            assert!(control.send(event()));
        }
    }

    fn job(&self) -> JobId {
        self.next_job.set(self.next_job.get() + 1);
        self.next_job.get()
    }
}

impl ServiceWorkerRegistry for MemoryRegistry {
    fn register(
        &self,
        client: &workers::ClientInfo,
        request: RegisterRequest,
    ) -> NativeResult<JobId> {
        assert!(client.secure);
        if !request.script_url.starts_with(ORIGIN) {
            return Err(NativeError::named(
                "SecurityError",
                "the script must be same-origin",
            ));
        }
        let job = self.job();
        self.registered.borrow_mut().push((job, request));
        Ok(job)
    }

    fn update(&self, _client: &workers::ClientInfo, registration: u64) -> NativeResult<JobId> {
        let job = self.job();
        self.updated.borrow_mut().push((job, registration));
        Ok(job)
    }

    fn unregister(&self, _client: &workers::ClientInfo, registration: u64) -> NativeResult<bool> {
        let mut records = self.registrations.borrow_mut();
        let before = records.len();
        records.retain(|record| record.id != registration);
        self.unregistered.borrow_mut().push(registration);
        Ok(records.len() != before)
    }

    fn registrations(&self, _client: &workers::ClientInfo) -> Vec<RegistrationRecord> {
        self.registrations.borrow().clone()
    }

    fn controller(&self, _client: &workers::ClientInfo) -> Option<WorkerRecord> {
        self.controller.borrow().clone()
    }

    fn post_to_worker(
        &self,
        _client: &workers::ClientInfo,
        worker: u64,
        message: CloneMessage,
    ) -> NativeResult<()> {
        self.posted.borrow_mut().push((worker, message));
        Ok(())
    }

    fn subscribe(&self, _client: &workers::ClientInfo, control: Control) {
        self.controls.borrow_mut().push(control);
    }
}

fn page_engine(registry: Option<&Rc<MemoryRegistry>>) -> (Engine, Arc<AtomicUsize>) {
    let (mut engine, notifies) = owner_engine();
    eval_str(
        &mut engine,
        &format!(
            "globalThis.location = {{ href: '{SCOPE}index.html', origin: '{ORIGIN}' }}; globalThis.log = []; 0"
        ),
    );
    if let Some(registry) = registry {
        let registry: Rc<dyn ServiceWorkerRegistry> = registry.clone();
        workers::install_service_workers(engine.ctx(), registry).map_err(|_| ()).unwrap();
    }
    (engine, notifies)
}

// ---- page: shape and feature detection -------------------------------------------------------

#[test]
fn service_worker_classes_have_web_idl_shape() {
    let registry = MemoryRegistry::new();
    let (mut engine, _) = page_engine(Some(&registry));
    assert_eq!(
        eval_str(
            &mut engine,
            "['ServiceWorker', 'ServiceWorkerRegistration', 'ServiceWorkerContainer']
               .map((name) => typeof globalThis[name] + (Object.getPrototypeOf(globalThis[name].prototype) === EventTarget.prototype))
               .join()"
        ),
        "functiontrue,functiontrue,functiontrue"
    );
    assert_eq!(
        eval_str(
            &mut engine,
            "(() => {
               const getter = (proto, key) => typeof Object.getOwnPropertyDescriptor(proto, key).get;
               return [
                 getter(ServiceWorker.prototype, 'scriptURL'),
                 getter(ServiceWorker.prototype, 'state'),
                 getter(ServiceWorker.prototype, 'onstatechange'),
                 getter(ServiceWorkerRegistration.prototype, 'installing'),
                 getter(ServiceWorkerRegistration.prototype, 'updateViaCache'),
                 getter(ServiceWorkerRegistration.prototype, 'onupdatefound'),
                 getter(ServiceWorkerContainer.prototype, 'controller'),
                 getter(ServiceWorkerContainer.prototype, 'ready'),
                 getter(ServiceWorkerContainer.prototype, 'oncontrollerchange'),
                 ServiceWorkerContainer.prototype.register.length,
                 ServiceWorker.prototype.postMessage.length,
               ].join();
             })()"
        ),
        "function,function,function,function,function,function,function,function,function,1,1"
    );
    assert_eq!(
        eval_str(
            &mut engine,
            "['new ServiceWorker()', 'new ServiceWorkerRegistration()', 'new ServiceWorkerContainer()']
               .map((code) => { try { eval(code); return 'no'; } catch (e) { return e.constructor.name; } })
               .join()"
        ),
        "TypeError,TypeError,TypeError"
    );
    assert_eq!(
        eval_str(
            &mut engine,
            "(() => { try { ServiceWorker.prototype.postMessage.call({}, 1); return 'no'; } catch (e) { return e.constructor.name; } })()"
        ),
        "TypeError"
    );
    assert_eq!(
        eval_str(
            &mut engine,
            "navigator.serviceWorker instanceof ServiceWorkerContainer && navigator.serviceWorker instanceof EventTarget"
        ),
        "true"
    );
}

#[test]
fn feature_detection_follows_the_registry_not_the_shared_navigator() {
    let (mut without, _) = page_engine(None);
    assert_eq!(eval_str(&mut without, "'serviceWorker' in navigator"), "false");
    assert_eq!(eval_str(&mut without, "typeof navigator.serviceWorker"), "undefined");
    assert_eq!(
        eval_str(&mut without, "typeof ServiceWorker + typeof ServiceWorkerContainer"),
        "undefinedundefined"
    );

    let registry = MemoryRegistry::new();
    let (mut with, _) = page_engine(Some(&registry));
    assert_eq!(eval_str(&mut with, "'serviceWorker' in navigator"), "true");
    assert_eq!(
        eval_str(&mut with, "Object.prototype.hasOwnProperty.call(navigator, 'serviceWorker')"),
        "true"
    );
    assert_eq!(
        eval_str(&mut with, "'serviceWorker' in Navigator.prototype"),
        "false",
        "the shared prototype must stay untouched"
    );
    assert_eq!(
        eval_str(
            &mut with,
            "'serviceWorker' in Object.create(Navigator.prototype)"
        ),
        "false"
    );
    assert_eq!(
        eval_str(
            &mut with,
            "(() => { const d = Object.getOwnPropertyDescriptor(navigator, 'serviceWorker'); return [typeof d.get, d.set === undefined, d.enumerable, d.configurable].join(); })()"
        ),
        "function,true,true,true"
    );
    assert_eq!(
        eval_str(
            &mut with,
            "(() => { try { Object.getOwnPropertyDescriptor(navigator, 'serviceWorker').get.call({}); return 'no'; } catch (e) { return e.constructor.name; } })()"
        ),
        "TypeError"
    );
}

#[test]
fn navigator_service_worker_is_one_object_and_nothing_is_armed_until_read() {
    let registry = MemoryRegistry::new();
    let (mut engine, notifies) = page_engine(Some(&registry));
    assert_eq!(registry.subscriptions(), 0, "installing does not subscribe");
    eval_str(&mut engine, "navigator.serviceWorker; 0");
    assert_eq!(registry.subscriptions(), 1);
    assert_eq!(
        eval_str(&mut engine, "navigator.serviceWorker === navigator.serviceWorker"),
        "true"
    );
    assert_eq!(registry.subscriptions(), 1, "one subscription per realm");
    assert_eq!(notifies.load(Ordering::SeqCst), 0);
    assert!(!owner_loop::has_ready(engine.ctx()));
    pump_all(&mut engine);
    assert!(!owner_loop::has_ready(engine.ctx()));
}

#[test]
fn interface_objects_are_lazy_globals() {
    let registry = MemoryRegistry::new();
    let (mut engine, _) = page_engine(Some(&registry));
    assert_eq!(
        eval_str(
            &mut engine,
            "['ServiceWorker', 'ServiceWorkerRegistration', 'ServiceWorkerContainer'].map((name) => name in globalThis).join()"
        ),
        "true,true,true",
        "`in` does not build a lazy global"
    );
    assert_eq!(eval_str(&mut engine, "typeof ServiceWorkerRegistration"), "function");
    assert_eq!(
        eval_str(
            &mut engine,
            "typeof Object.getOwnPropertyDescriptor(globalThis, 'ServiceWorkerRegistration').value"
        ),
        "function"
    );
}

// ---- page: register, update, unregister --------------------------------------------------------

#[test]
fn register_settles_on_the_pushed_job() {
    let registry = MemoryRegistry::new();
    let (mut engine, notifies) = page_engine(Some(&registry));
    eval_str(
        &mut engine,
        "navigator.serviceWorker.register('sw.js', { scope: '/app/' }).then(
           (r) => { globalThis.reg = r; log.push('ok:' + r.scope); },
           (e) => log.push('no:' + e.name)); 0",
    );
    {
        let registered = registry.registered.borrow();
        assert_eq!(registered.len(), 1);
        let request = &registered[0].1;
        assert_eq!(request.script_url, format!("{SCOPE}sw.js"));
        assert_eq!(request.scope.as_deref(), Some(SCOPE));
        assert!(!request.module);
        assert_eq!(request.update_via_cache, UpdateViaCache::Imports);
    }
    assert_eq!(eval_str(&mut engine, "log.join()"), "", "pending until the registry pushes");
    assert_eq!(notifies.load(Ordering::SeqCst), 0);

    let job = registry.registered.borrow()[0].0;
    let record = registration(1, Some(worker(1, WorkerState::Installing)), None, None);
    registry.push(|| WorkerEvent::Registration(Box::new(record.clone())));
    registry.push(|| WorkerEvent::JobSettled {
        job,
        result: Ok(Box::new(record.clone())),
    });
    assert_eq!(notifies.load(Ordering::SeqCst), 1, "a burst of pushes wakes the host once");
    pump_all(&mut engine);
    assert_eq!(eval_str(&mut engine, "log.join()"), format!("ok:{SCOPE}"));
    assert_eq!(
        eval_str(
            &mut engine,
            "[reg.installing.state, reg.installing.scriptURL, reg.waiting, reg.active, reg.updateViaCache].join()"
        ),
        format!("installing,{SCOPE}sw1.js,,,imports")
    );
    assert!(!owner_loop::has_ready(engine.ctx()));
}

#[test]
fn register_forwards_options_and_rejects_bad_calls() {
    let registry = MemoryRegistry::new();
    let (mut engine, _) = page_engine(Some(&registry));
    eval_str(
        &mut engine,
        "navigator.serviceWorker.register('/sw.mjs', { type: 'module', updateViaCache: 'none' }); 0",
    );
    {
        let registered = registry.registered.borrow();
        let request = &registered[0].1;
        assert_eq!(request.script_url, format!("{ORIGIN}/sw.mjs"));
        assert_eq!(request.scope, None);
        assert!(request.module);
        assert_eq!(request.update_via_cache, UpdateViaCache::None);
    }
    eval_str(
        &mut engine,
        "const sw = navigator.serviceWorker;
         sw.register().catch((e) => log.push('missing:' + e.name));
         sw.register('sw.js', { type: 'bad' }).catch((e) => log.push('type:' + e.name));
         sw.register('sw.js', { updateViaCache: 'bad' }).catch((e) => log.push('cache:' + e.name));
         sw.register('http://[bad').catch((e) => log.push('url:' + e.name));
         sw.register('https://evil.test/sw.js').catch((e) => log.push('registry:' + e.name)); 0",
    );
    assert_eq!(
        eval_str(&mut engine, "log.join()"),
        "missing:TypeError,type:TypeError,cache:TypeError,url:TypeError,registry:SecurityError"
    );
    assert_eq!(registry.registered.borrow().len(), 1, "rejected calls reach no job");
}

#[test]
fn a_failed_job_rejects_with_the_pushed_dom_exception() {
    let registry = MemoryRegistry::new();
    let (mut engine, _) = page_engine(Some(&registry));
    eval_str(
        &mut engine,
        "navigator.serviceWorker.register('sw.js').catch((e) => log.push(e.name + ':' + e.message + ':' + (e instanceof DOMException))); 0",
    );
    let job = registry.registered.borrow()[0].0;
    registry.push(|| WorkerEvent::JobSettled {
        job,
        result: Err(("AbortError".into(), "install failed".into())),
    });
    pump_all(&mut engine);
    assert_eq!(eval_str(&mut engine, "log.join()"), "AbortError:install failed:true");
}

#[test]
fn update_and_unregister_settle_and_reuse_the_wrapper() {
    let registry = MemoryRegistry::with_registrations(vec![registration(
        1,
        None,
        None,
        Some(worker(1, WorkerState::Activated)),
    )]);
    let (mut engine, _) = page_engine(Some(&registry));
    eval_str(
        &mut engine,
        "navigator.serviceWorker.getRegistration().then((r) => { globalThis.reg = r; }); 0",
    );
    assert_eq!(eval_str(&mut engine, "reg.scope"), SCOPE);
    assert_eq!(eval_str(&mut engine, "reg.active.state"), "activated");
    eval_str(
        &mut engine,
        "reg.update().then((r) => log.push('updated:' + (r === reg) + ':' + (reg.waiting && reg.waiting.state))); 0",
    );
    assert_eq!(registry.updated.borrow().as_slice(), &[(1, 1)]);
    let record = registration(
        1,
        None,
        Some(worker(2, WorkerState::Installed)),
        Some(worker(1, WorkerState::Activated)),
    );
    registry.push(|| WorkerEvent::JobSettled {
        job: 1,
        result: Ok(Box::new(record.clone())),
    });
    pump_all(&mut engine);
    assert_eq!(eval_str(&mut engine, "log.join()"), "updated:true:installed");
    assert_eq!(eval_str(&mut engine, "reg.active.state + reg.waiting.state"), "activatedinstalled");

    eval_str(&mut engine, "reg.unregister().then((value) => log.push('unregistered:' + value)); 0");
    assert_eq!(registry.unregistered.borrow().as_slice(), &[1]);
    assert_eq!(
        eval_str(&mut engine, "log.join()"),
        "updated:true:installed,unregistered:true"
    );
}

#[test]
fn get_registration_picks_the_longest_scope_and_checks_the_origin() {
    let registry = MemoryRegistry::with_registrations(vec![
        registration_in(1, &format!("{ORIGIN}/"), None, None, Some(worker(1, WorkerState::Activated))),
        registration_in(2, SCOPE, None, None, Some(worker(2, WorkerState::Activated))),
        registration_in(3, &format!("{ORIGIN}/other/"), None, None, None),
    ]);
    let (mut engine, _) = page_engine(Some(&registry));
    eval_str(
        &mut engine,
        "const sw = navigator.serviceWorker;
         sw.getRegistration().then((r) => log.push('here:' + r.scope));
         sw.getRegistration('/app/deep/page.html').then((r) => log.push('deep:' + r.scope));
         sw.getRegistration('/other/x').then((r) => log.push('other:' + r.scope));
         sw.getRegistration('/elsewhere').then((r) => log.push('root:' + r.scope));
         sw.getRegistration('https://evil.test/').catch((e) => log.push('cross:' + e.name));
         sw.getRegistrations().then((all) => log.push('all:' + all.map((r) => new URL(r.scope).pathname).join('|'))); 0",
    );
    assert_eq!(
        eval_str(&mut engine, "log.join()"),
        format!(
            "here:{SCOPE},deep:{SCOPE},other:{ORIGIN}/other/,root:{ORIGIN}/,cross:SecurityError,all:/|/app/|/other/"
        )
    );
}

// ---- page: ready and controller -----------------------------------------------------------------

#[test]
fn ready_resolves_on_the_push_that_activates_a_worker() {
    let registry = MemoryRegistry::with_registrations(vec![registration(
        1,
        Some(worker(1, WorkerState::Installing)),
        None,
        None,
    )]);
    let (mut engine, notifies) = page_engine(Some(&registry));
    assert_eq!(
        eval_str(
            &mut engine,
            "navigator.serviceWorker.ready.then((r) => log.push('ready:' + r.active.state + ':' + r.scope));
             navigator.serviceWorker.ready === navigator.serviceWorker.ready"
        ),
        "true"
    );
    pump_all(&mut engine);
    assert_eq!(eval_str(&mut engine, "log.join()"), "", "no active worker yet");
    assert_eq!(notifies.load(Ordering::SeqCst), 0, "nothing polls for the registry");

    let elsewhere = registration_in(
        9,
        &format!("{ORIGIN}/other/"),
        None,
        None,
        Some(worker(9, WorkerState::Activated)),
    );
    registry.push(|| WorkerEvent::Registration(Box::new(elsewhere.clone())));
    pump_all(&mut engine);
    assert_eq!(eval_str(&mut engine, "log.join()"), "", "another scope does not resolve ready");

    let active = registration(1, None, None, Some(worker(1, WorkerState::Activated)));
    registry.push(|| WorkerEvent::Registration(Box::new(active.clone())));
    pump_all(&mut engine);
    assert_eq!(eval_str(&mut engine, "log.join()"), format!("ready:activated:{SCOPE}"));
}

#[test]
fn ready_is_already_resolved_for_an_active_registration() {
    let registry = MemoryRegistry::with_registrations(vec![registration(
        4,
        None,
        None,
        Some(worker(4, WorkerState::Activated)),
    )]);
    let (mut engine, _) = page_engine(Some(&registry));
    eval_str(
        &mut engine,
        "navigator.serviceWorker.ready.then((r) => log.push('ready:' + r.active.scriptURL)); 0",
    );
    assert_eq!(eval_str(&mut engine, "log.join()"), format!("ready:{SCOPE}sw4.js"));
}

#[test]
fn controller_is_a_stable_wrapper_and_changes_by_push() {
    let registry = MemoryRegistry::new();
    *registry.controller.borrow_mut() = Some(worker(7, WorkerState::Activated));
    let (mut engine, _) = page_engine(Some(&registry));
    assert_eq!(
        eval_str(&mut engine, "navigator.serviceWorker.controller.scriptURL"),
        format!("{SCOPE}sw7.js")
    );
    assert_eq!(
        eval_str(
            &mut engine,
            "navigator.serviceWorker.controller === navigator.serviceWorker.controller"
        ),
        "true"
    );
    eval_str(
        &mut engine,
        "navigator.serviceWorker.addEventListener('controllerchange', (e) =>
           log.push('change:' + e.isTrusted + ':' + String(navigator.serviceWorker.controller && navigator.serviceWorker.controller.scriptURL))); 0",
    );
    registry.push(|| WorkerEvent::ControllerChange(Some(worker(8, WorkerState::Activated))));
    registry.push(|| WorkerEvent::ControllerChange(None));
    pump_all(&mut engine);
    assert_eq!(
        eval_str(&mut engine, "log.join()"),
        format!("change:true:{SCOPE}sw8.js,change:true:null")
    );
    assert_eq!(eval_str(&mut engine, "navigator.serviceWorker.controller"), "null");
}

#[test]
fn an_uncontrolled_page_has_a_null_controller() {
    let registry = MemoryRegistry::new();
    let (mut engine, _) = page_engine(Some(&registry));
    assert_eq!(eval_str(&mut engine, "navigator.serviceWorker.controller === null"), "true");
}

// ---- page: events ---------------------------------------------------------------------------------

#[test]
fn updatefound_and_statechange_fire_in_push_order() {
    let registry = MemoryRegistry::with_registrations(vec![registration(1, None, None, None)]);
    let (mut engine, _) = page_engine(Some(&registry));
    eval_str(
        &mut engine,
        "navigator.serviceWorker.getRegistration().then((r) => {
           globalThis.reg = r;
           r.onupdatefound = () => {
             const w = r.installing;
             log.push('updatefound:' + w.state);
             w.addEventListener('statechange', (e) => log.push('state:' + w.state + ':' + e.isTrusted));
           };
         }); 0",
    );
    let installing = registration(1, Some(worker(2, WorkerState::Installing)), None, None);
    registry.push(|| WorkerEvent::Registration(Box::new(installing.clone())));
    registry.push(|| WorkerEvent::UpdateFound { registration: 1 });
    for state in [
        WorkerState::Installed,
        WorkerState::Installing,
        WorkerState::Installed,
        WorkerState::Activating,
        WorkerState::Activated,
    ] {
        registry.push(|| WorkerEvent::StateChange { worker: 2, state });
    }
    pump_all(&mut engine);
    assert_eq!(
        eval_str(&mut engine, "log.join()"),
        "updatefound:installing,state:installed:true,state:activating:true,state:activated:true",
        "stale and repeated states fire nothing"
    );
    assert_eq!(eval_str(&mut engine, "reg.installing.state"), "activated");
}

#[test]
fn a_registration_snapshot_moves_the_worker_slots_without_events() {
    let registry = MemoryRegistry::with_registrations(vec![registration(
        1,
        Some(worker(1, WorkerState::Installing)),
        None,
        None,
    )]);
    let (mut engine, _) = page_engine(Some(&registry));
    eval_str(
        &mut engine,
        "navigator.serviceWorker.getRegistration().then((r) => {
           globalThis.reg = r;
           globalThis.first = r.installing;
           first.onstatechange = () => log.push('first:' + first.state);
           r.onupdatefound = () => log.push('updatefound');
         }); 0",
    );
    let moved = registration(
        1,
        None,
        None,
        Some(worker(1, WorkerState::Activated)),
    );
    registry.push(|| WorkerEvent::Registration(Box::new(moved.clone())));
    pump_all(&mut engine);
    assert_eq!(
        eval_str(
            &mut engine,
            "[String(reg.installing), reg.active === first, first.state, log.length].join()"
        ),
        "null,true,installing,0",
        "the snapshot reuses the worker wrapper and fires nothing"
    );
    registry.push(|| WorkerEvent::StateChange {
        worker: 1,
        state: WorkerState::Activated,
    });
    pump_all(&mut engine);
    assert_eq!(eval_str(&mut engine, "log.join()"), "first:activated");
}

#[test]
fn post_message_reaches_the_registry_and_messages_come_back_with_their_source() {
    let registry = MemoryRegistry::with_registrations(vec![registration(
        1,
        None,
        None,
        Some(worker(1, WorkerState::Activated)),
    )]);
    let (mut engine, _) = page_engine(Some(&registry));
    eval_str(
        &mut engine,
        "globalThis.mc = new MessageChannel();
         navigator.serviceWorker.addEventListener('message', (e) =>
           log.push([e.data.n, e.source === reg.active, e.origin, e.ports.length, e.isTrusted, e instanceof MessageEvent].join()));
         navigator.serviceWorker.getRegistration().then((r) => {
           globalThis.reg = r;
           r.active.postMessage({ n: 7 }, [mc.port2]);
         }); 0",
    );
    let (id, message) = registry.posted.borrow_mut().pop().expect("a message for the worker");
    assert_eq!(id, 1);
    assert!(!message.bytes.is_empty());
    assert_eq!(message.attachments.len(), 1);
    assert!(matches!(message.attachments[0], CloneAttachment::Port(_)));
    assert_eq!(
        eval_str(&mut engine, "(() => { try { mc.port2.postMessage(1); return 'quiet'; } catch (e) { return e.name; } })()"),
        "quiet",
        "the transferred port is detached, not an error"
    );
    registry.controls.borrow()[0].send(WorkerEvent::ServiceMessage {
        source: worker(1, WorkerState::Activated),
        message,
    });
    pump_all(&mut engine);
    assert_eq!(
        eval_str(&mut engine, "log.join()"),
        format!("7,true,{ORIGIN},1,true,true")
    );
}

#[test]
fn post_message_to_a_redundant_worker_throws() {
    let registry = MemoryRegistry::with_registrations(vec![registration(
        1,
        None,
        None,
        Some(worker(1, WorkerState::Activated)),
    )]);
    let (mut engine, _) = page_engine(Some(&registry));
    eval_str(
        &mut engine,
        "navigator.serviceWorker.getRegistration().then((r) => { globalThis.sw = r.active; }); 0",
    );
    registry.push(|| WorkerEvent::StateChange {
        worker: 1,
        state: WorkerState::Redundant,
    });
    pump_all(&mut engine);
    assert_eq!(
        eval_str(
            &mut engine,
            "(() => { try { sw.postMessage(1); return 'no'; } catch (e) { return e.name; } })()"
        ),
        "InvalidStateError"
    );
    assert!(registry.posted.borrow().is_empty());
}

// ---- page: wrapper identity and collection --------------------------------------------------------

#[test]
fn one_wrapper_per_registration_and_worker() {
    let registry = MemoryRegistry::with_registrations(vec![registration(
        1,
        None,
        None,
        Some(worker(1, WorkerState::Activated)),
    )]);
    *registry.controller.borrow_mut() = Some(worker(1, WorkerState::Activated));
    let (mut engine, _) = page_engine(Some(&registry));
    eval_str(
        &mut engine,
        "Promise.all([navigator.serviceWorker.getRegistration(), navigator.serviceWorker.getRegistrations(), navigator.serviceWorker.ready])
           .then(([a, all, ready]) => {
             globalThis.same = [a === all[0], a === ready, a.active === navigator.serviceWorker.controller].join();
           }); 0",
    );
    assert_eq!(eval_str(&mut engine, "same"), "true,true,true");
}

#[test]
fn unreferenced_wrappers_are_collected_and_rebuilt() {
    let registry = MemoryRegistry::new();
    *registry.controller.borrow_mut() = Some(worker(3, WorkerState::Activated));
    let (mut engine, _) = page_engine(Some(&registry));
    eval_str(
        &mut engine,
        "globalThis.probe = new WeakRef(navigator.serviceWorker.controller); 0",
    );
    eval_str(&mut engine, "0");
    engine.collect_garbage();
    engine.collect_garbage();
    assert_eq!(eval_str(&mut engine, "probe.deref() === undefined"), "true");
    assert_eq!(
        eval_str(&mut engine, "navigator.serviceWorker.controller.state"),
        "activated",
        "a collected wrapper is rebuilt from the registry's record"
    );
}

#[test]
fn a_worker_with_a_listener_survives_collection_until_it_is_redundant() {
    let registry = MemoryRegistry::new();
    *registry.controller.borrow_mut() = Some(worker(3, WorkerState::Activated));
    let (mut engine, _) = page_engine(Some(&registry));
    eval_str(
        &mut engine,
        "(() => {
           const w = navigator.serviceWorker.controller;
           w.onstatechange = () => log.push('state:' + w.state);
           globalThis.probe = new WeakRef(w);
         })(); 0",
    );
    eval_str(&mut engine, "0");
    engine.collect_garbage();
    engine.collect_garbage();
    assert_eq!(eval_str(&mut engine, "probe.deref() !== undefined"), "true");
    registry.push(|| WorkerEvent::StateChange {
        worker: 3,
        state: WorkerState::Redundant,
    });
    pump_all(&mut engine);
    assert_eq!(eval_str(&mut engine, "log.join()"), "state:redundant");
    eval_str(&mut engine, "0");
    engine.collect_garbage();
    engine.collect_garbage();
    assert_eq!(
        eval_str(&mut engine, "probe.deref() === undefined"),
        "true",
        "a redundant worker is no longer pinned"
    );
}

#[test]
fn a_realm_with_no_events_stays_idle_after_collection() {
    let registry = MemoryRegistry::new();
    let (mut engine, notifies) = page_engine(Some(&registry));
    eval_str(&mut engine, "navigator.serviceWorker.getRegistrations(); 0");
    engine.collect_garbage();
    pump_all(&mut engine);
    assert_eq!(notifies.load(Ordering::SeqCst), 0);
    assert!(!owner_loop::has_ready(engine.ctx()));
}

// ---- the service worker's scope ---------------------------------------------------------------------

#[derive(Default)]
struct ScopeProbe {
    settled: RefCell<Vec<(u64, Result<(), String>)>>,
    responses: RefCell<Vec<(u64, FetchOutcome)>>,
    skips: Cell<u32>,
    claims: Cell<u32>,
    posted: RefCell<Vec<(String, CloneMessage)>>,
    errors: RefCell<Vec<String>>,
    clients: RefCell<Vec<ClientRecord>>,
}

struct MockScope(Rc<ScopeProbe>);
struct MockService(Rc<ScopeProbe>);

impl WorkerScopeHost for MockScope {
    fn kind(&self) -> ScopeKind {
        ScopeKind::Service
    }

    fn location(&self) -> String {
        format!("{SCOPE}sw1.js")
    }

    fn name(&self) -> String {
        String::new()
    }

    fn module(&self) -> bool {
        false
    }

    fn close(&self) {}

    fn load_classic_script(&self, _: &mut Ctx, _: &str) -> NativeResult<(String, String)> {
        Err(NativeError::named("NetworkError", "no importScripts in tests"))
    }

    fn report_error(&self, message: String) {
        self.0.errors.borrow_mut().push(message);
    }

    fn service(&self) -> Option<Rc<dyn ServiceScopeHost>> {
        Some(Rc::new(MockService(self.0.clone())))
    }
}

impl ServiceScopeHost for MockService {
    fn registration(&self) -> RegistrationRecord {
        registration(1, None, None, Some(worker(1, WorkerState::Activated)))
    }

    fn worker(&self) -> WorkerRecord {
        worker(1, WorkerState::Activated)
    }

    fn skip_waiting(&self) -> NativeResult<()> {
        self.0.skips.set(self.0.skips.get() + 1);
        Ok(())
    }

    fn claim(&self) -> NativeResult<()> {
        self.0.claims.set(self.0.claims.get() + 1);
        Ok(())
    }

    fn match_all(&self, _include_uncontrolled: bool) -> Vec<ClientRecord> {
        self.0.clients.borrow().clone()
    }

    fn client(&self, id: &str) -> Option<ClientRecord> {
        self.0.clients.borrow().iter().find(|c| c.id == id).cloned()
    }

    fn post_to_client(&self, client: &str, message: CloneMessage) -> NativeResult<()> {
        self.0.posted.borrow_mut().push((client.to_owned(), message));
        Ok(())
    }

    fn fetch_response(&self, event: u64, outcome: FetchOutcome) {
        self.0.responses.borrow_mut().push((event, outcome));
    }

    fn event_settled(&self, event: u64, result: Result<(), String>) {
        self.0.settled.borrow_mut().push((event, result));
    }
}

fn window_client(id: &str) -> ClientRecord {
    ClientRecord {
        id: id.to_owned(),
        url: format!("{SCOPE}index.html"),
        kind: ClientKind::Window,
        frame_type: FrameType::TopLevel,
    }
}

fn service_engine() -> (Engine, Arc<AtomicUsize>, Rc<ScopeProbe>, Control) {
    let (mut engine, notifies) = owner_engine();
    let probe = Rc::new(ScopeProbe::default());
    let control = Control::new();
    workers::install_scope(
        engine.ctx(),
        ScopeInstall {
            host: Rc::new(MockScope(probe.clone())),
            inside: None,
            control: Some(control.clone()),
        },
    )
    .map_err(|_| ())
    .expect("install the service worker scope");
    eval_str(&mut engine, "globalThis.log = []; 0");
    (engine, notifies, probe, control)
}

fn fetch_request(event: u64, url: &str) -> FetchRequest {
    FetchRequest {
        event,
        url: url.to_owned(),
        method: "POST".into(),
        headers: vec![("content-type".into(), "text/plain".into())],
        body: b"abc".to_vec(),
        mode: Mode::Cors,
        credentials: Credentials::Include,
        redirect: Redirect::Manual,
        client_id: "c1".into(),
    }
}

#[test]
fn the_global_is_a_service_worker_global_scope_with_lazy_classes() {
    let (mut engine, _, _, _) = service_engine();
    assert_eq!(
        eval_str(
            &mut engine,
            "const d = Object.getOwnPropertyDescriptor(globalThis, 'FetchEvent');
             [typeof d.value, d.writable, d.enumerable, d.configurable].join()"
        ),
        "function,true,false,true",
        "classes are built on first use and read as ordinary interface objects"
    );
    assert_eq!(
        eval_str(
            &mut engine,
            "[self === globalThis,
              self instanceof ServiceWorkerGlobalScope,
              self instanceof WorkerGlobalScope,
              self instanceof EventTarget,
              Object.getPrototypeOf(ServiceWorkerGlobalScope.prototype) === WorkerGlobalScope.prototype,
              Object.getPrototypeOf(ExtendableEvent.prototype) === Event.prototype,
              Object.getPrototypeOf(FetchEvent.prototype) === ExtendableEvent.prototype,
              Object.getPrototypeOf(ExtendableMessageEvent.prototype) === ExtendableEvent.prototype,
              Object.getPrototypeOf(WindowClient.prototype) === Client.prototype,
              clients instanceof Clients,
              clients === clients,
              registration instanceof ServiceWorkerRegistration,
              serviceWorker instanceof ServiceWorker,
              typeof skipWaiting,
              typeof DedicatedWorkerGlobalScope].join()"
        ),
        "true,true,true,true,true,true,true,true,true,true,true,true,true,function,undefined"
    );
    assert_eq!(
        eval_str(
            &mut engine,
            "[registration.scope, registration.active.state, serviceWorker.scriptURL, 'serviceWorker' in navigator === undefined].join()"
        )
        .split(',')
        .take(3)
        .collect::<Vec<_>>()
        .join(","),
        format!("{SCOPE},activated,{SCOPE}sw1.js")
    );
}

#[test]
fn install_waits_for_wait_until_promises() {
    let (mut engine, notifies, probe, control) = service_engine();
    eval_str(
        &mut engine,
        "addEventListener('install', (e) => {
           log.push('install:' + [e.isTrusted, e.cancelable, e instanceof ExtendableEvent].join());
           e.waitUntil(new Promise((resolve) => { globalThis.finishInstall = resolve; }));
         });
         oninstall = () => log.push('oninstall'); 0",
    );
    assert!(control.send(WorkerEvent::Lifecycle {
        event: 1,
        kind: LifecycleKind::Install,
    }));
    assert_eq!(notifies.load(Ordering::SeqCst), 1);
    pump_all(&mut engine);
    assert_eq!(eval_str(&mut engine, "log.join()"), "install:true,false,true,oninstall");
    assert!(probe.settled.borrow().is_empty(), "the promise is still pending");
    eval_str(&mut engine, "finishInstall(); 0");
    assert_eq!(probe.settled.borrow().as_slice(), &[(1, Ok(()))]);
}

#[test]
fn a_rejected_wait_until_fails_the_event_and_a_late_one_is_refused() {
    let (mut engine, _, probe, control) = service_engine();
    eval_str(
        &mut engine,
        "addEventListener('activate', (e) => {
           globalThis.activateEvent = e;
           e.waitUntil(Promise.reject(new Error('boom')));
           e.waitUntil(Promise.resolve());
         }); 0",
    );
    control.send(WorkerEvent::Lifecycle {
        event: 2,
        kind: LifecycleKind::Activate,
    });
    pump_all(&mut engine);
    assert_eq!(
        probe.settled.borrow().as_slice(),
        &[(2, Err("Error: boom".to_owned()))]
    );
    assert_eq!(
        eval_str(
            &mut engine,
            "(() => { try { activateEvent.waitUntil(Promise.resolve()); return 'no'; } catch (e) { return e.name; } })()"
        ),
        "InvalidStateError"
    );
    assert_eq!(
        eval_str(
            &mut engine,
            "(() => { try { new ExtendableEvent('x').waitUntil(1); return 'no'; } catch (e) { return e.name; } })()"
        ),
        "InvalidStateError"
    );
}

#[test]
fn an_event_without_listeners_settles_at_once() {
    let (mut engine, _, probe, control) = service_engine();
    control.send(WorkerEvent::Lifecycle {
        event: 5,
        kind: LifecycleKind::Activate,
    });
    pump_all(&mut engine);
    assert_eq!(probe.settled.borrow().as_slice(), &[(5, Ok(()))]);
}

#[test]
fn fetch_event_respond_with_produces_status_headers_and_body() {
    let (mut engine, _, probe, control) = service_engine();
    eval_str(
        &mut engine,
        "addEventListener('fetch', (e) => {
           globalThis.seen = [e.request.url, e.request.method, e.request.mode, e.request.credentials,
             e.request.redirect, e.clientId, e.isTrusted, e.cancelable, e instanceof ExtendableEvent,
             e.request.headers.get('content-type')].join();
           e.respondWith(new Response('hi', { status: 201, statusText: 'Made', headers: { 'x-a': '1' } }));
         }); 0",
    );
    control.send(WorkerEvent::Fetch(Box::new(fetch_request(
        3,
        &format!("{SCOPE}data.json"),
    ))));
    pump_all(&mut engine);
    assert_eq!(
        eval_str(&mut engine, "seen"),
        format!("{SCOPE}data.json,POST,cors,include,manual,c1,true,true,true,text/plain")
    );
    let responses = probe.responses.borrow();
    assert_eq!(responses.len(), 1);
    match &responses[0] {
        (3, FetchOutcome::Response { status, status_text, headers, body }) => {
            assert_eq!(*status, 201);
            assert_eq!(status_text, "Made");
            assert!(headers.iter().any(|(name, value)| name == "x-a" && value == "1"));
            assert_eq!(body.as_slice(), b"hi");
        }
        other => panic!("unexpected outcome {other:?}"),
    }
    assert_eq!(probe.settled.borrow().as_slice(), &[(3, Ok(()))]);
}

#[test]
fn fetch_event_waits_for_a_promised_response_and_for_wait_until() {
    let (mut engine, _, probe, control) = service_engine();
    eval_str(
        &mut engine,
        "addEventListener('fetch', (e) => {
           e.respondWith(new Promise((resolve) => { globalThis.answer = () => resolve(new Response('late')); }));
           e.waitUntil(new Promise((resolve) => { globalThis.finish = resolve; }));
         }); 0",
    );
    control.send(WorkerEvent::Fetch(Box::new(fetch_request(4, &format!("{SCOPE}x")))));
    pump_all(&mut engine);
    assert!(probe.responses.borrow().is_empty());
    eval_str(&mut engine, "answer(); 0");
    match probe.responses.borrow().as_slice() {
        [(4, FetchOutcome::Response { status: 200, body, .. })] => assert_eq!(body.as_slice(), b"late"),
        other => panic!("unexpected outcomes {other:?}"),
    }
    assert!(probe.settled.borrow().is_empty(), "waitUntil is still pending");
    eval_str(&mut engine, "finish(); 0");
    assert_eq!(probe.settled.borrow().as_slice(), &[(4, Ok(()))]);
}

#[test]
fn fetch_event_falls_back_and_reports_bad_responses() {
    let (mut engine, _, probe, control) = service_engine();
    control.send(WorkerEvent::Fetch(Box::new(fetch_request(6, &format!("{SCOPE}a")))));
    pump_all(&mut engine);
    assert!(matches!(
        probe.responses.borrow().as_slice(),
        [(6, FetchOutcome::Fallback)]
    ));

    eval_str(
        &mut engine,
        "addEventListener('fetch', (e) => {
           const url = new URL(e.request.url).pathname.slice(-1);
           if (url === 'b') e.respondWith(5);
           else if (url === 'c') e.respondWith(Promise.reject(new TypeError('nope')));
           else if (url === 'd') { e.respondWith(new Response('')); try { e.respondWith(new Response('')); } catch (x) { globalThis.second = x.name; } }
           else if (url === 'e') globalThis.kept = e;
         }); 0",
    );
    for (event, path) in [(7, "b"), (8, "c"), (9, "d"), (10, "e")] {
        control.send(WorkerEvent::Fetch(Box::new(fetch_request(
            event,
            &format!("{SCOPE}{path}"),
        ))));
    }
    pump_all(&mut engine);
    let responses = probe.responses.borrow();
    // Outcomes settle in completion order: a fallback is immediate, a respondWith waits.
    let outcome = |id: u64| {
        responses
            .iter()
            .find(|(event, _)| *event == id)
            .map(|(_, outcome)| outcome)
            .unwrap_or_else(|| panic!("no outcome for fetch event {id}"))
    };
    assert!(matches!(outcome(7), FetchOutcome::Error(message) if message.contains("must be a Response")));
    assert!(matches!(outcome(8), FetchOutcome::Error(message) if message.contains("nope")));
    assert!(matches!(outcome(9), FetchOutcome::Response { status: 200, .. }));
    assert!(matches!(outcome(10), FetchOutcome::Fallback));
    drop(responses);
    assert_eq!(eval_str(&mut engine, "second"), "InvalidStateError");
    assert_eq!(
        eval_str(
            &mut engine,
            "(() => { try { kept.respondWith(new Response('')); return 'no'; } catch (e) { return e.name; } })()"
        ),
        "InvalidStateError",
        "respondWith after the handlers returned"
    );
}

#[test]
fn client_messages_carry_their_source_and_can_be_answered() {
    let (mut engine, _, probe, control) = service_engine();
    probe.clients.borrow_mut().push(window_client("c1"));
    eval_str(
        &mut engine,
        "globalThis.payload = { n: 41 };
         addEventListener('message', (e) => {
           globalThis.lastSource = e.source;
           log.push([e.data.n, e.source.id, e.source instanceof WindowClient, e.source.type, e.source.frameType,
             e.origin, e instanceof ExtendableMessageEvent, e.ports.length, e.isTrusted].join());
           e.source.postMessage({ reply: e.data.n + 1 });
         }); 0",
    );
    let payload = global_value(&mut engine, "payload");
    let message = crate::messaging::serialize_message(engine.ctx(), payload, Value::Undefined)
        .map_err(|_| ())
        .expect("serialize");
    control.send(WorkerEvent::ClientMessage {
        event: 11,
        source: window_client("c1"),
        message,
    });
    pump_all(&mut engine);
    assert_eq!(
        eval_str(&mut engine, "log.join()"),
        format!("41,c1,true,window,top-level,{ORIGIN},true,0,true")
    );
    assert_eq!(probe.settled.borrow().as_slice(), &[(11, Ok(()))]);
    let posted = probe.posted.borrow();
    assert_eq!(posted.len(), 1);
    assert_eq!(posted[0].0, "c1");
    assert!(!posted[0].1.bytes.is_empty());
    drop(posted);
    eval_str(
        &mut engine,
        "clients.get('c1').then((c) => { globalThis.fromClients = c; }); 0",
    );
    assert_eq!(eval_str(&mut engine, "fromClients === lastSource"), "true");
}

#[test]
fn clients_match_all_get_claim_and_skip_waiting() {
    let (mut engine, _, probe, _) = service_engine();
    probe.clients.borrow_mut().push(window_client("c1"));
    probe.clients.borrow_mut().push(ClientRecord {
        id: "w1".into(),
        url: format!("{SCOPE}worker.js"),
        kind: ClientKind::Worker,
        frame_type: FrameType::None,
    });
    eval_str(
        &mut engine,
        "clients.matchAll().then((all) => log.push('default:' + all.map((c) => c.id + ':' + (c instanceof WindowClient)).join('|')));
         clients.matchAll({ type: 'all', includeUncontrolled: true }).then((all) =>
           log.push('all:' + all.map((c) => c.id + ':' + (c instanceof WindowClient) + ':' + (c instanceof Client)).join('|')));
         clients.get('nope').then((c) => log.push('none:' + c));
         clients.matchAll({ type: 'bad' }).catch((e) => log.push('bad:' + e.name));
         clients.openWindow('/').catch((e) => log.push('open:' + e.name));
         clients.claim().then((v) => log.push('claim:' + v));
         skipWaiting().then((v) => log.push('skip:' + v)); 0",
    );
    assert_eq!(
        eval_str(&mut engine, "log.join()"),
        "default:c1:true,all:c1:true:true|w1:false:true,none:undefined,bad:TypeError,open:NotSupportedError,claim:undefined,skip:undefined"
    );
    assert_eq!(probe.claims.get(), 1);
    assert_eq!(probe.skips.get(), 1);
}

#[test]
fn the_scope_follows_registry_pushes_for_its_own_worker() {
    let (mut engine, _, _, control) = service_engine();
    eval_str(
        &mut engine,
        "globalThis.sw = serviceWorker;
         sw.onstatechange = () => log.push('state:' + sw.state); 0",
    );
    control.send(WorkerEvent::StateChange {
        worker: 1,
        state: WorkerState::Redundant,
    });
    pump_all(&mut engine);
    assert_eq!(eval_str(&mut engine, "log.join()"), "state:redundant");
    assert_eq!(eval_str(&mut engine, "serviceWorker === sw"), "true");
}

#[test]
fn an_idle_service_worker_realm_is_never_woken() {
    let (mut engine, notifies, _, _) = service_engine();
    eval_str(&mut engine, "addEventListener('fetch', () => {}); clients; registration; 0");
    pump_all(&mut engine);
    assert_eq!(notifies.load(Ordering::SeqCst), 0);
    assert!(!owner_loop::has_ready(engine.ctx()));
}
