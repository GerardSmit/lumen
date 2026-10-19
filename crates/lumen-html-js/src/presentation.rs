//! Host-backed fullscreen and pointer-lock DOM services.
//!
//! Browser state is scoped to one `DomRealm`; the host callbacks only change real presentation
//! state, while host notifications keep the DOM synchronized with Escape, native loss, and
//! detached targets.
use super::*;

#[derive(Clone, Default)]
pub struct PresentationHost {
    /// Apply or restore this realm's actual fullscreen presentation root.
    pub set_fullscreen: Option<Rc<dyn Fn(Option<NodeId>) -> Result<(), String>>>,
    /// Apply or release actual relative pointer input and cursor capture for this realm.
    pub set_pointer_lock: Option<Rc<dyn Fn(Option<NodeId>, bool) -> Result<(), String>>>,
}

#[derive(Default)]
pub(super) struct RealmPresentation {
    host: RefCell<Option<PresentationHost>>,
    fullscreen_element: Cell<Option<NodeId>>,
    pointer_lock_element: Cell<Option<NodeId>>,
    pending_detach_cleanup: Cell<bool>,
    mutation_sink_installed: Cell<bool>,
}

impl DomRealm {
    /// Install real host presentation callbacks.
    pub fn set_presentation_host(self: &Rc<Self>, host: PresentationHost) {
        *self.browser_services.presentation.host.borrow_mut() = Some(host);
        install(self);
    }

    /// Disable presentation APIs after the host has released any active presentation.
    pub fn clear_presentation_host(self: &Rc<Self>) {
        *self.browser_services.presentation.host.borrow_mut() = None;
        install(self);
    }

    pub fn fullscreen_element(&self) -> Option<NodeId> {
        self.browser_services.presentation.fullscreen_element.get()
    }

    pub fn pointer_lock_element(&self) -> Option<NodeId> {
        self.browser_services
            .presentation
            .pointer_lock_element
            .get()
    }

    /// Notify the DOM that the host exited fullscreen, such as after Escape or native loss.
    pub fn notify_fullscreen_exit(self: &Rc<Self>, ctx: &mut Ctx) -> OpResult<()> {
        let callback = self
            .browser_services
            .presentation
            .host
            .borrow()
            .as_ref()
            .and_then(|host| host.set_fullscreen.clone());
        if let Some(callback) = callback {
            callback(None).map_err(|message| {
                OpError::new(
                    "UnknownError",
                    format!("fullscreen host exit failed: {message}"),
                )
            })?;
        }
        notify_fullscreen_change(ctx, self, None)
    }

    /// Synchronize the DOM when the native presentation host reports a fullscreen transition.
    pub fn notify_fullscreen_change(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        element: Option<NodeId>,
    ) -> OpResult<()> {
        notify_fullscreen_change(ctx, self, element)
    }

    /// Notify the DOM that the host released pointer lock, such as after Escape or native loss.
    pub fn notify_pointer_lock_exit(self: &Rc<Self>, ctx: &mut Ctx) -> OpResult<()> {
        let callback = self
            .browser_services
            .presentation
            .host
            .borrow()
            .as_ref()
            .and_then(|host| host.set_pointer_lock.clone());
        if let Some(callback) = callback {
            callback(None, false).map_err(|message| {
                OpError::new(
                    "UnknownError",
                    format!("pointer lock host exit failed: {message}"),
                )
            })?;
        }
        notify_pointer_lock_change(ctx, self, None)
    }

    /// Synchronize the DOM when native pointer lock changes, including Escape and host loss.
    pub fn notify_pointer_lock_change(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        element: Option<NodeId>,
    ) -> OpResult<()> {
        notify_pointer_lock_change(ctx, self, element)
    }

    /// Pump presentation state transitions detected by DOM mutation observers.
    pub fn pump_presentation(self: &Rc<Self>, ctx: &mut Ctx) -> OpResult<()> {
        let pending = &self.browser_services.presentation.pending_detach_cleanup;
        if !pending.get() {
            return Ok(());
        }
        pending.set(false);
        if let Some(node) = self.fullscreen_element() {
            if !is_connected(self, node) {
                let callback = self
                    .browser_services
                    .presentation
                    .host
                    .borrow()
                    .as_ref()
                    .and_then(|host| host.set_fullscreen.clone());
                if let Some(callback) = callback {
                    if let Err(message) = callback(None) {
                        pending.set(true);
                        return Err(OpError::new(
                            "UnknownError",
                            format!("fullscreen cleanup failed: {message}"),
                        ));
                    }
                }
                notify_fullscreen_change(ctx, self, None)?;
            }
        }
        if let Some(node) = self.pointer_lock_element() {
            if !is_connected(self, node) {
                let callback = self
                    .browser_services
                    .presentation
                    .host
                    .borrow()
                    .as_ref()
                    .and_then(|host| host.set_pointer_lock.clone());
                if let Some(callback) = callback {
                    if let Err(message) = callback(None, false) {
                        pending.set(true);
                        return Err(OpError::new(
                            "UnknownError",
                            format!("pointer lock cleanup failed: {message}"),
                        ));
                    }
                }
                notify_pointer_lock_change(ctx, self, None)?;
            }
        }
        Ok(())
    }
}

pub(crate) fn install(realm: &Rc<DomRealm>) {
    if realm
        .browser_services
        .presentation
        .mutation_sink_installed
        .replace(true)
    {
        return;
    }
    let weak = Rc::downgrade(realm);
    realm.add_mutation_sink(Rc::new(move |_, _| {
        let Some(realm) = weak.upgrade() else {
            return;
        };
        let presentation = &realm.browser_services.presentation;
        if presentation.fullscreen_element.get().is_some()
            || presentation.pointer_lock_element.get().is_some()
        {
            presentation.pending_detach_cleanup.set(true);
        }
    }));
}

pub(crate) fn fullscreen_enabled(realm: &DomRealm) -> bool {
    realm.permissions_policy_allows(super::permissions_policy::FULLSCREEN) && realm
        .browser_services
        .presentation
        .host
        .borrow()
        .as_ref()
        .is_some_and(|host| host.set_fullscreen.is_some())
}

pub(crate) fn request_fullscreen(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    node: NodeId,
) -> OpResult<()> {
    if !realm.use_permissions_policy(Some(ctx),super::permissions_policy::FULLSCREEN){
        dispatch_error(ctx,realm,node,"fullscreenerror")?;
        return Err(OpError::type_error("fullscreen is disabled by Permissions Policy"));
    }
    if !is_connected(realm, node) {
        dispatch_error(ctx, realm, node, "fullscreenerror")?;
        return Err(OpError::new(
            "TypeError",
            "fullscreen requires a connected element",
        ));
    }
    if !realm.has_transient_user_activation() {
        dispatch_error(ctx, realm, node, "fullscreenerror")?;
        return Err(OpError::new(
            "NotAllowedError",
            "fullscreen requires trusted user activation",
        ));
    }
    let callback = realm
        .browser_services
        .presentation
        .host
        .borrow()
        .as_ref()
        .and_then(|host| host.set_fullscreen.clone());
    let Some(callback) = callback else {
        dispatch_error(ctx, realm, node, "fullscreenerror")?;
        return Err(OpError::new(
            "NotSupportedError",
            "the host has not supplied fullscreen presentation",
        ));
    };
    if !realm.consume_user_activation() {
        dispatch_error(ctx, realm, node, "fullscreenerror")?;
        return Err(OpError::new(
            "NotAllowedError",
            "fullscreen requires trusted user activation",
        ));
    }
    if let Err(message) = callback(Some(node)) {
        dispatch_error(ctx, realm, node, "fullscreenerror")?;
        return Err(OpError::new(
            "NotAllowedError",
            format!("fullscreen request was denied: {message}"),
        ));
    }
    notify_fullscreen_change(ctx, realm, Some(node))
}

pub(crate) fn request_pointer_lock(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    node: NodeId,
    options: Option<Value>,
) -> OpResult<()> {
    if !is_connected(realm, node) {
        dispatch_error(ctx, realm, node, "pointerlockerror")?;
        return Err(OpError::new(
            "TypeError",
            "pointer lock requires a connected element",
        ));
    }
    if !realm.has_transient_user_activation() {
        dispatch_error(ctx, realm, node, "pointerlockerror")?;
        return Err(OpError::new(
            "NotAllowedError",
            "pointer lock requires trusted user activation",
        ));
    }
    let unadjusted = match options {
        None | Some(Value::Undefined | Value::Null) => false,
        Some(Value::Obj(object)) => {
            let options = Value::Obj(object);
            let value = ctx
                .member_get(&options, "unadjustedMovement")
                .map_err(OpError::thrown)?;
            ctx.to_boolean(&value)
        }
        Some(_) => {
            dispatch_error(ctx, realm, node, "pointerlockerror")?;
            return Err(OpError::new(
                "TypeError",
                "pointer lock options must be a dictionary",
            ));
        }
    };
    // Reading a dictionary member may invoke page code. Recheck the target after conversion so a
    // getter cannot detach it between validation and the host operation.
    if !is_connected(realm, node) {
        dispatch_error(ctx, realm, node, "pointerlockerror")?;
        return Err(OpError::new(
            "TypeError",
            "pointer lock target became disconnected during option conversion",
        ));
    }
    let callback = realm
        .browser_services
        .presentation
        .host
        .borrow()
        .as_ref()
        .and_then(|host| host.set_pointer_lock.clone());
    let Some(callback) = callback else {
        dispatch_error(ctx, realm, node, "pointerlockerror")?;
        return Err(OpError::new(
            "NotSupportedError",
            "the host has not supplied pointer lock",
        ));
    };
    if !realm.consume_user_activation() {
        dispatch_error(ctx, realm, node, "pointerlockerror")?;
        return Err(OpError::new(
            "NotAllowedError",
            "pointer lock requires trusted user activation",
        ));
    }
    if let Err(message) = callback(Some(node), unadjusted) {
        dispatch_error(ctx, realm, node, "pointerlockerror")?;
        return Err(OpError::new(
            "NotAllowedError",
            format!("pointer lock request was denied: {message}"),
        ));
    }
    notify_pointer_lock_change(ctx, realm, Some(node))
}

pub(crate) fn exit_fullscreen(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> OpResult<()> {
    if realm.fullscreen_element().is_none() {
        return Ok(());
    }
    let callback = realm
        .browser_services
        .presentation
        .host
        .borrow()
        .as_ref()
        .and_then(|host| host.set_fullscreen.clone())
        .ok_or_else(|| OpError::new("NotSupportedError", "fullscreen host is unavailable"))?;
    callback(None).map_err(|message| {
        OpError::new("UnknownError", format!("fullscreen exit failed: {message}"))
    })?;
    notify_fullscreen_change(ctx, realm, None)
}

pub(crate) fn exit_pointer_lock(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> OpResult<()> {
    if realm.pointer_lock_element().is_none() {
        return Ok(());
    }
    let callback = realm
        .browser_services
        .presentation
        .host
        .borrow()
        .as_ref()
        .and_then(|host| host.set_pointer_lock.clone())
        .ok_or_else(|| OpError::new("NotSupportedError", "pointer lock host is unavailable"))?;
    callback(None, false).map_err(|message| {
        OpError::new(
            "UnknownError",
            format!("pointer lock exit failed: {message}"),
        )
    })?;
    notify_pointer_lock_change(ctx, realm, None)
}

pub(crate) fn notify_fullscreen_change(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    next: Option<NodeId>,
) -> OpResult<()> {
    if let Some(node) = next {
        if !is_connected(realm, node) {
            return Err(OpError::new(
                "InvalidStateError",
                "fullscreen target is not connected to this document",
            ));
        }
    }
    let previous = realm
        .browser_services
        .presentation
        .fullscreen_element
        .replace(next);
    if previous != next {
        let previous_is_live = previous.filter(|node| is_connected(realm, *node));
        if let Some(previous) = previous_is_live.filter(|previous| Some(*previous) != next) {
            realm.dispatch(ctx, previous, "fullscreenchange", true, false, &[])?;
        }
        if let Some(next) = next {
            realm.dispatch(ctx, next, "fullscreenchange", true, false, &[])?;
        } else if previous_is_live.is_none() {
            realm.dispatch(
                ctx,
                document_root(realm),
                "fullscreenchange",
                true,
                false,
                &[],
            )?;
        }
    }
    Ok(())
}

pub(crate) fn notify_pointer_lock_change(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    next: Option<NodeId>,
) -> OpResult<()> {
    if let Some(node) = next {
        if !is_connected(realm, node) {
            return Err(OpError::new(
                "InvalidStateError",
                "pointer lock target is not connected to this document",
            ));
        }
    }
    let previous = realm
        .browser_services
        .presentation
        .pointer_lock_element
        .replace(next);
    if previous != next {
        realm.dispatch(
            ctx,
            document_root(realm),
            "pointerlockchange",
            true,
            false,
            &[],
        )?;
    }
    Ok(())
}

pub(crate) fn adopt_nodes(
    ctx: &mut Ctx,
    source: &Rc<DomRealm>,
    mapping: &[(NodeId, NodeId)],
) -> OpResult<()> {
    let adopted: Vec<NodeId> = mapping.iter().map(|(old, _)| *old).collect();
    if source
        .fullscreen_element()
        .is_some_and(|node| adopted.contains(&node))
    {
        let callback = source
            .browser_services
            .presentation
            .host
            .borrow()
            .as_ref()
            .and_then(|host| host.set_fullscreen.clone());
        if let Some(callback) = callback {
            callback(None).map_err(|message| {
                OpError::new(
                    "UnknownError",
                    format!("fullscreen adoption cleanup failed: {message}"),
                )
            })?;
        }
        notify_fullscreen_change(ctx, source, None)?;
    }
    if source
        .pointer_lock_element()
        .is_some_and(|node| adopted.contains(&node))
    {
        let callback = source
            .browser_services
            .presentation
            .host
            .borrow()
            .as_ref()
            .and_then(|host| host.set_pointer_lock.clone());
        if let Some(callback) = callback {
            callback(None, false).map_err(|message| {
                OpError::new(
                    "UnknownError",
                    format!("pointer lock adoption cleanup failed: {message}"),
                )
            })?;
        }
        notify_pointer_lock_change(ctx, source, None)?;
    }
    Ok(())
}

fn dispatch_error(ctx: &mut Ctx, realm: &Rc<DomRealm>, node: NodeId, name: &str) -> OpResult<()> {
    if realm.session.borrow().document().kind(node).is_ok() {
        realm.dispatch(ctx, node, name, true, false, &[])?;
    } else {
        realm.dispatch(ctx, document_root(realm), name, true, false, &[])?;
    }
    Ok(())
}

fn document_root(realm: &DomRealm) -> NodeId {
    realm.session.borrow().document().root()
}

fn is_connected(realm: &DomRealm, node: NodeId) -> bool {
    let session = realm.session.borrow();
    let document = session.document();
    let root = document.root();
    let mut cursor = Some(node);
    let mut remaining = document.node_count().saturating_add(1);
    while let Some(id) = cursor {
        if id == root {
            return true;
        }
        if remaining == 0 || document.kind(id).is_err() {
            return false;
        }
        remaining -= 1;
        cursor = document
            .parent(id)
            .ok()
            .flatten()
            .or_else(|| document.shadow_host(id).ok().flatten());
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen::Engine;

    fn eval(engine: &mut Engine, source: &str) -> Value {
        match engine.eval_value(source).unwrap() {
            Ok(value) => value,
            Err(error) => {
                let message = engine
                    .ctx()
                    .get_member(&error, "stack")
                    .ok()
                    .and_then(|value| match value {
                        Value::Str(text) => Some(text.as_str().to_owned()),
                        _ => None,
                    })
                    .unwrap_or_else(|| "JavaScript error without a stack".into());
                panic!("{message}\nSource: {source}");
            }
        }
    }

    fn install_engine() -> (Engine, Rc<DomRealm>) {
        let mut engine = Engine::new();
        let realm = super::super::install(engine.ctx(), "<main></main>", 32).unwrap();
        (engine, realm)
    }

    #[test]
    fn fullscreen_and_pointer_lock_use_host_state_activation_events_and_detach_cleanup() {
        let (mut engine, realm) = install_engine();
        let fullscreen_calls = Rc::new(RefCell::new(Vec::new()));
        let pointer_calls = Rc::new(RefCell::new(Vec::new()));
        let pointer_modes = Rc::new(RefCell::new(Vec::new()));
        let full_log = fullscreen_calls.clone();
        let pointer_log = pointer_calls.clone();
        let mode_log = pointer_modes.clone();
        realm.set_presentation_host(PresentationHost {
            set_fullscreen: Some(Rc::new(move |node| {
                full_log.borrow_mut().push(node);
                Ok(())
            })),
            set_pointer_lock: Some(Rc::new(move |node, unadjusted| {
                pointer_log.borrow_mut().push(node);
                mode_log.borrow_mut().push(unadjusted);
                Ok(())
            })),
        });
        assert!(matches!(
            eval(
                &mut engine,
                "var element=document.createElement('div'); document.body.appendChild(element); var fullChanges=0; var fullErrors=0; var lockChanges=0; var lockErrors=0; document.addEventListener('fullscreenchange',()=>fullChanges++); document.addEventListener('pointerlockchange',()=>lockChanges++); element.addEventListener('fullscreenerror',()=>fullErrors++); element.addEventListener('pointerlockerror',()=>lockErrors++); var fullResult='pending'; element.requestFullscreen().then(()=>fullResult='resolved',e=>fullResult=e.name); void 0"
            ),
            Value::Undefined
        ));
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(
            eval(
                &mut engine,
                "fullResult === 'NotAllowedError' && fullErrors === 1 && document.fullscreenElement === null"
            ),
            Value::Bool(true)
        ));
        assert!(fullscreen_calls.borrow().is_empty());

        realm.mark_user_activation();
        assert!(matches!(
            eval(
                &mut engine,
                "element.requestFullscreen().then(()=>fullResult='resolved'); void 0"
            ),
            Value::Undefined
        ));
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(
            eval(
                &mut engine,
                "fullResult === 'resolved' && document.fullscreenEnabled && document.fullscreenElement === element && fullChanges === 1"
            ),
            Value::Bool(true)
        ));
        assert_eq!(fullscreen_calls.borrow().len(), 1);

        realm.mark_user_activation();
        assert!(matches!(
            eval(
                &mut engine,
                "element.requestPointerLock({unadjustedMovement:true}).then(()=>fullResult='locked'); void 0"
            ),
            Value::Undefined
        ));
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(
            eval(
                &mut engine,
                "fullResult === 'locked' && document.pointerLockElement === element && lockChanges === 1"
            ),
            Value::Bool(true)
        ));
        assert_eq!(pointer_calls.borrow().len(), 1);
        assert_eq!(&*pointer_modes.borrow(), &[true]);

        assert!(matches!(
            eval(&mut engine, "document.body.removeChild(element); void 0"),
            Value::Undefined
        ));
        realm.pump_presentation(engine.ctx()).unwrap();
        engine.ctx().drain_microtasks_for_host();
        let settled = eval(
            &mut engine,
            "document.fullscreenElement === null && document.pointerLockElement === null && fullChanges === 2 && lockChanges === 2",
        );
        if !matches!(settled, Value::Bool(true)) {
            let detail = eval(
                &mut engine,
                "(document.fullscreenElement===null)+'|'+(document.pointerLockElement===null)+'|'+fullChanges+'|'+lockChanges",
            );
            let detail = match detail {
                Value::Str(value) => value.as_str().to_owned(),
                _ => "<invalid diagnostic>".into(),
            };
            panic!("detached presentation cleanup state was {detail}");
        }
        assert_eq!(fullscreen_calls.borrow().len(), 2);
        assert!(fullscreen_calls.borrow()[0].is_some());
        assert_eq!(fullscreen_calls.borrow()[1], None);
        assert_eq!(pointer_calls.borrow().len(), 2);
        assert_eq!(pointer_calls.borrow()[1], None);
    }

    #[test]
    fn host_denials_reject_without_committing_presentation_state() {
        let (mut engine, realm) = install_engine();
        realm.set_presentation_host(PresentationHost {
            set_fullscreen: Some(Rc::new(|_| Err("platform denied".into()))),
            set_pointer_lock: Some(Rc::new(|_, _| Err("platform denied".into()))),
        });
        eval(
            &mut engine,
            "var element=document.createElement('div'); document.body.appendChild(element); var result=[]; element.addEventListener('fullscreenerror',()=>result.push('full-error')); element.addEventListener('pointerlockerror',()=>result.push('lock-error')); void 0",
        );
        realm.mark_user_activation();
        eval(
            &mut engine,
            "element.requestFullscreen().catch(error=>result.push(error.name)); void 0",
        );
        realm.mark_user_activation();
        eval(
            &mut engine,
            "element.requestPointerLock().catch(error=>result.push(error.name)); void 0",
        );
        engine.ctx().drain_microtasks_for_host();
        let settled = eval(
            &mut engine,
            "document.fullscreenElement === null && document.pointerLockElement === null && result.join(',') === 'full-error,lock-error,NotAllowedError,NotAllowedError'",
        );
        if !matches!(settled, Value::Bool(true)) {
            let detail = eval(
                &mut engine,
                "(document.fullscreenElement===null)+'|'+(document.pointerLockElement===null)+'|'+result.join(',')",
            );
            let detail = match detail {
                Value::Str(value) => value.as_str().to_owned(),
                _ => "<invalid diagnostic>".into(),
            };
            panic!("host denial state and event order were {detail}");
        }
    }

    #[test]
    fn pointer_event_movement_defaults_to_zero_and_preserves_host_relative_deltas() {
        let (mut engine, realm) = install_engine();
        assert!(matches!(
            eval(
                &mut engine,
                "new MouseEvent('mousemove').movementX === 0 && new PointerEvent('pointermove').movementY === 0"
            ),
            Value::Bool(true)
        ));
        eval(
            &mut engine,
            "var observedMovement; document.addEventListener('pointermove', event => observedMovement = [event.movementX, event.movementY]); void 0",
        );
        let target = realm.with_session(|session| {
            lumen_html::selector::query_selector(
                session.document(),
                session.document().root(),
                "main",
            )
            .unwrap()
            .unwrap()
        });
        realm
            .dispatch(
                engine.ctx(),
                target,
                "pointermove",
                true,
                true,
                &[
                    ("movementX", Value::Num(-7.5)),
                    ("movementY", Value::Num(2.25)),
                ],
            )
            .unwrap();
        assert!(matches!(
            eval(
                &mut engine,
                "observedMovement[0] === -7.5 && observedMovement[1] === 2.25"
            ),
            Value::Bool(true)
        ));
    }
}
