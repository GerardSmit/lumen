//! Document navigation phases. State belongs to a Document, not its reusable Window.
use super::*;
use events::{DomEvent, DomEventTarget};
use lumen_host::events::EventInit;
use lumen::embed::JsHost;
use lumen_bind::Host;

#[derive(Default)]
pub(crate) struct DocumentLifecycle {
    pub(crate) unload_counter: Cell<u32>,
    pub(crate) page_showing: Cell<bool>,
    pub(crate) destroyed: Cell<bool>,
    pub(crate) hidden: Cell<bool>,
    pub(crate) initial_about_blank: Cell<bool>,
    pub(crate) sandboxed_automatic_features: Cell<bool>,
    pub(crate) sandboxed_origin: Cell<bool>,
    pub(crate) revealed: Cell<bool>,
}

/// The no-transition form of the HTML page reveal event. Its dispatch belongs
/// to the first real rendering opportunity, rather than a load task or timer.
#[lumen_bind::class(name = "PageRevealEvent", extends = DomEvent, hint(js(webidl)))]
pub(crate) struct DomPageRevealEvent { base: DomEvent }

#[lumen_bind::methods]
impl DomPageRevealEvent {
    #[constructor(coerce)]
    fn new(ctx: &mut Ctx, kind: &str, options: Option<Value>) -> OpResult<Self> {
        let init = EventInit::read(ctx, &options)?;
        let transition = super::ui_events::dictionary_member(ctx, &options, "viewTransition")?;
        // No ViewTransition implementation is registered yet. Web IDL requires
        // an actual native ViewTransition brand, not an arbitrary author object.
        if transition.is_some_and(|value| !matches!(value, Value::Null | Value::Undefined)) {
            return Err(OpError::new("TypeError", "viewTransition must be a ViewTransition or null"));
        }
        Ok(Self { base: DomEvent::from_init(kind, init) })
    }

    #[getter(name = "viewTransition")]
    fn view_transition(&self) -> Value { Value::Null }
}

pub(crate) fn reveal_pending(ctx: &mut Ctx) -> bool {
    super::window_globals::current_dom_realm(ctx).is_some_and(|document| {
        document.ready_state.get() >= DocumentReadyState::Interactive && can_reveal(&document)
    })
}

fn can_reveal(document: &DomRealm) -> bool {
    document.has_browsing_context && !document.lifecycle.revealed.get()
        && !document.lifecycle.destroyed.get() && !document.lifecycle.hidden.get()
        && document.browsing_context().is_none_or(|context|
            super::browsing_context::is_active_document(&context, document))
}

pub(crate) fn reveal(ctx: &mut Ctx) -> OpResult<()> {
    let Some(document) = super::window_globals::current_dom_realm(ctx) else { return Ok(()); };
    // A host may present partial content before parsing finishes. That genuine
    // opportunity must reveal the document too; readiness only asks the host
    // for a frame when no other producer has requested one.
    if !can_reveal(&document) { return Ok(()); }
    // Set before dispatch because listeners may reenter a rendering opportunity.
    document.lifecycle.revealed.set(true);
    let event = ctx.new_instance(DomPageRevealEvent { base: DomEvent::from_init("pagereveal", EventInit::default()) });
    let window = ctx.global_object();
    DomEventTarget::dispatch_trusted(ctx, &window, &event)?;
    Ok(())
}

#[lumen_bind::class(name = "BeforeUnloadEvent", extends = DomEvent, hint(js(webidl)))]
pub(crate) struct DomBeforeUnloadEvent {
    base: DomEvent,
    return_value: RefCell<String>,
}

#[lumen_bind::methods]
impl DomBeforeUnloadEvent {
    #[getter(name = "returnValue")]
    fn return_value(&self) -> String { self.return_value.borrow().clone() }

    #[setter(name = "returnValue", coerce)]
    fn set_return_value(&self, value: String) { *self.return_value.borrow_mut() = value; }
}

#[lumen_bind::class(name = "PageTransitionEvent", extends = DomEvent, hint(js(webidl)))]
pub(crate) struct DomPageTransitionEvent {
    base: DomEvent,
    persisted: bool,
}

#[lumen_bind::methods]
impl DomPageTransitionEvent {
    #[constructor(coerce)]
    fn new(ctx: &mut Ctx, kind: &str, options: Option<Value>) -> OpResult<Self> {
        let init = EventInit::read(ctx, &options)?;
        let persisted = super::ui_events::dictionary_member(ctx, &options, "persisted")?
            .is_some_and(|value| ctx.to_boolean(&value));
        Ok(Self { base: DomEvent::from_init(kind, init), persisted })
    }

    #[getter]
    fn persisted(&self) -> bool { self.persisted }
}

#[lumen_bind::class(name = "PopStateEvent", extends = DomEvent, hint(js(webidl)))]
pub(crate) struct DomPopStateEvent { base: DomEvent, state: Value, has_ua_visual_transition: bool }
impl lumen::embed::NativeIdentityOwner for DomPopStateEvent {
    const TRACES_NATIVE_VALUES: bool = true;
    fn trace_native_identities(&self, id: u64, visit: &mut dyn FnMut(&Value)) {
        lumen::embed::NativeIdentityOwner::trace_native_identities(&self.base, id, visit);
    }
    fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
        lumen::embed::NativeIdentityOwner::trace_native_values(&self.base, visit);
        visit(&self.state);
    }
}
struct PopStateConstructor(DomPopStateEvent);
impl lumen_bind::CtorRet<JsHost, DomPopStateEvent> for PopStateConstructor {
    fn into_ctor(self, cx: &<JsHost as Host>::Cx<'_>) -> Result<Value, Value> {
        let value = <JsHost as Host>::construct(cx, self.0)?;
        <JsHost as Host>::with_ctx(cx, |ctx| ctx.set_native_identity_owner::<DomPopStateEvent>(&value)
            .expect("PopStateEvent native brand"));
        Ok(value)
    }
}

#[lumen_bind::methods]
impl DomPopStateEvent {
    #[constructor(coerce)]
    fn new(ctx: &mut Ctx, kind: &str, options: Option<Value>) -> OpResult<PopStateConstructor> {
        let init = EventInit::read(ctx, &options)?;
        let has_ua_visual_transition = super::ui_events::dictionary_member(ctx, &options, "hasUAVisualTransition")?.is_some_and(|value| ctx.to_boolean(&value));
        let state = super::ui_events::dictionary_member(ctx, &options, "state")?.unwrap_or(Value::Null);
        Ok(PopStateConstructor(Self { base: DomEvent::from_init(kind, init), state, has_ua_visual_transition }))
    }
    #[getter]
    fn state(&self) -> Value { self.state.clone() }
    #[getter(name = "hasUAVisualTransition")]
    fn has_ua_visual_transition(&self) -> bool { self.has_ua_visual_transition }
}

#[lumen_bind::class(name = "HashChangeEvent", extends = DomEvent, hint(js(webidl)))]
pub(crate) struct DomHashChangeEvent { base: DomEvent, old_url: String, new_url: String }
#[lumen_bind::methods]
impl DomHashChangeEvent {
    #[constructor(coerce)]
    fn new(ctx: &mut Ctx, kind: &str, options: Option<Value>) -> OpResult<Self> {
        let init = EventInit::read(ctx, &options)?;
        let new_url = match super::ui_events::dictionary_member(ctx, &options, "newURL")? { Some(value) => ctx.coerce_string(&value).map_err(OpError::thrown)?.to_string(), None => String::new() };
        let old_url = match super::ui_events::dictionary_member(ctx, &options, "oldURL")? { Some(value) => ctx.coerce_string(&value).map_err(OpError::thrown)?.to_string(), None => String::new() };
        Ok(Self { base: DomEvent::from_init(kind, init), old_url, new_url })
    }
    #[getter(name = "oldURL")]
    fn old_url(&self) -> String { self.old_url.clone() }
    #[getter(name = "newURL")]
    fn new_url(&self) -> String { self.new_url.clone() }
}

pub(crate) fn history_events(ctx: &mut Ctx, state: Value, old_url: String, new_url: String) -> OpResult<()> {
    let event = ctx.new_instance(DomPopStateEvent { base: DomEvent::from_init("popstate", EventInit::default()), state, has_ua_visual_transition: false });
    ctx.set_native_identity_owner::<DomPopStateEvent>(&event)?;
    let window = ctx.global_object();
    DomEventTarget::dispatch_trusted(ctx, &window, &event)?;
    let old_fragment = lumen_common::url::parse(&old_url, None).ok().and_then(|url| url.fragment);
    let new_fragment = lumen_common::url::parse(&new_url, None).ok().and_then(|url| url.fragment);
    if old_fragment != new_fragment {
        super::scheduling::queue_task(ctx, move |ctx| {
            let event = ctx.new_instance(DomHashChangeEvent { base: DomEvent::from_init("hashchange", EventInit::default()), old_url, new_url });
            let window = ctx.global_object();
            DomEventTarget::dispatch_trusted(ctx, &window, &event)?;
            Ok(())
        })?;
    }
    Ok(())
}

pub(crate) fn apply_beforeunload_return(ctx: &mut Ctx, event: &Value, result: &Value) -> OpResult<()> {
    if matches!(result, Value::Null | Value::Undefined) { return Ok(()); }
    // Web IDL DOMString conversion can invoke author code: do it outside native projection.
    let result = ctx.coerce_string(result).map_err(OpError::thrown)?.to_string();
    let _ = ctx.with_instance::<DomBeforeUnloadEvent, _>(event, |event| {
        event.base.prevent_default();
        if event.return_value.borrow().is_empty() { *event.return_value.borrow_mut() = result; }
    });
    Ok(())
}

struct UnloadScope<'a>(&'a Cell<u32>);
impl<'a> UnloadScope<'a> {
    fn enter(counter: &'a Cell<u32>) -> Self { counter.set(counter.get().saturating_add(1)); Self(counter) }
}
impl Drop for UnloadScope<'_> { fn drop(&mut self) { self.0.set(self.0.get().saturating_sub(1)); } }

/// A host prompt policy is optional; absence means the UA declines to show a prompt.
/// `true` confirms leaving. It is consulted only after sticky activation and sandbox checks.
#[derive(Clone)]
struct PromptPolicy(Option<Rc<dyn Fn(&DomRealm) -> bool>>);
pub fn set_beforeunload_prompt_policy(ctx: &mut Ctx, policy: Option<Rc<dyn Fn(&DomRealm) -> bool>>) {
    ctx.op_state().put(PromptPolicy(policy));
}

pub(crate) fn fire_beforeunload(ctx: &mut Ctx, document: &Rc<DomRealm>, prompt_shown: &Cell<bool>) -> OpResult<bool> {
    if document.lifecycle.destroyed.get() { return Ok(true); }
    let _scope = UnloadScope::enter(&document.lifecycle.unload_counter);
    let event = ctx.new_instance(DomBeforeUnloadEvent {
        base: DomEvent::from_init("beforeunload", EventInit { cancelable: true, ..EventInit::default() }),
        return_value: RefCell::new(String::new()),
    });
    let window = ctx.global_object();
    let accepted = DomEventTarget::dispatch_trusted(ctx, &window, &event)?;
    let nonempty = ctx.with_instance::<DomBeforeUnloadEvent, _>(&event, |event| !event.return_value.borrow().is_empty())?;
    if (!accepted || nonempty) && document.has_been_active() && !prompt_shown.get()
        && document.browsing_context().is_some_and(|context| context.allows_modals()) {
        let policy = ctx.op_state().get::<PromptPolicy>().cloned();
        if let Some(policy) = policy.and_then(|policy| policy.0) {
            prompt_shown.set(true);
            return Ok(policy(document));
        }
    }
    Ok(true)
}

pub(crate) fn page_transition(ctx: &mut Ctx, document: &Rc<DomRealm>, kind: &str) -> OpResult<()> {
    let event = ctx.new_instance(DomPageTransitionEvent {
        base: DomEvent::from_init(kind, EventInit::default()), persisted: false,
    });
    let window = ctx.global_object();
    let target = document.document_value(ctx);
    DomEventTarget::dispatch_trusted_with_target(ctx, &window, &event, target)?;
    Ok(())
}

pub(crate) fn unload(ctx: &mut Ctx, document: &Rc<DomRealm>) -> OpResult<()> {
    if document.lifecycle.destroyed.get() { return Ok(()); }
    if document.lifecycle.page_showing.replace(false) {
        page_transition(ctx, document, "pagehide")?;
        document.lifecycle.hidden.set(true);
        if let Err(error) = crate::view_transition::visibility_changed(ctx, document) {
            let error = error.to_value(ctx);
            DomRealm::report_exception(ctx, error);
        }
        let root = document.session.borrow().document().root();
        document.dispatch_user_agent(ctx, root, "visibilitychange", true, false, &[])?;
    }
    document.dispatch_window_user_agent(ctx, "unload", false, false)?;
    let realm = ctx.current_host_realm();
    if let Some(timers) = ctx.host_mut::<lumen_timers::Timers>() { timers.cancel_realm(&realm); }
    Ok(())
}

pub(crate) fn destroy(ctx: &mut Ctx, document: &Rc<DomRealm>) {
    if document.lifecycle.destroyed.replace(true) { return; }
    if let Err(error) = crate::view_transition::retire(ctx, document) {
        let error = error.to_value(ctx);
        DomRealm::report_exception(ctx, error);
    }
    document.stylesheet_links.retire();
    document.object_resources.retire();
    super::history::document_destroyed(document);
    crate::object_urls::retire_document(ctx, document);
    document.document_parser.borrow_mut().take();
    document.xml_document_parser.borrow_mut().take();
    document.pending_parser_source.borrow_mut().take();
    document.document_parser_retention.borrow_mut().clear();
    document.module_activations.borrow_mut().clear();
    *document.autofocus.borrow_mut()=super::focus::AutofocusState::default();
    document.layout_flusher.borrow_mut().take();
    document.browsing_context.borrow_mut().clone_from(&std::rc::Weak::new());
    super::scheduling::cancel_tasks_for_document(ctx, document);
}

/// Completion of real navigation tasks; embedders drive their existing task loop.
#[derive(Clone)]
pub struct NavigationPhase {
    remaining: Rc<Cell<usize>>,
    accepted: Rc<Cell<bool>>,
}
impl NavigationPhase {
    pub fn finished(&self) -> bool { self.remaining.get() == 0 }
    pub fn accepted(&self) -> bool { self.finished() && self.accepted.get() }
}

struct PhaseLease { phase: NavigationPhase, ran: bool }
impl PhaseLease {
    fn complete(&mut self, accepted: bool) {
        self.phase.accepted.set(self.phase.accepted.get() && accepted);
        self.ran = true;
    }
}
impl Drop for PhaseLease {
    fn drop(&mut self) {
        if !self.ran { self.phase.accepted.set(false); }
        self.phase.remaining.set(self.phase.remaining.get().saturating_sub(1));
    }
}

pub(crate) fn queue_phase(ctx: &mut Ctx, documents: Vec<(Rc<DomRealm>, lumen::embed::RealmHandle)>, unloading: bool) -> OpResult<NavigationPhase> {
    let phase = NavigationPhase { remaining: Rc::new(Cell::new(documents.len())), accepted: Rc::new(Cell::new(true)) };
    let prompt_shown = Rc::new(Cell::new(false));
    for (document, handle) in documents {
        let mut lease = PhaseLease { phase: phase.clone(), ran: false };
        let prompt_shown = prompt_shown.clone();
        ctx.with_host_realm(&handle, |ctx| super::scheduling::queue_navigation_task(ctx, move |ctx| {
            let result = if unloading {
                let _scope = UnloadScope::enter(&document.lifecycle.unload_counter);
                unload(ctx, &document).map(|_| { destroy(ctx, &document); true })
            } else { fire_beforeunload(ctx, &document, &prompt_shown) };
            match result {
                Ok(accepted) => { lease.complete(accepted); Ok(()) }
                Err(error) => Err(error),
            }
        })).map_err(|_| OpError::new("InvalidStateError", "navigation task realm unavailable"))??;
    }
    Ok(phase)
}

#[cfg(test)]
mod reveal_tests {
    use super::*;

    #[test]
    fn page_reveal_can_present_partial_document_before_load() {
        let mut engine = lumen::Engine::new();
        let _document = crate::install(engine.ctx(), "<main></main>", 64).unwrap();
        engine.eval_value("var revealedBeforeLoad = false; onpagereveal = () => { revealedBeforeLoad = document.readyState === 'loading'; }; requestAnimationFrame(() => {});").unwrap().unwrap_or_else(|_| panic!("page reveal test setup threw"));
        assert!(!reveal_pending(engine.ctx()));
        assert!(crate::scheduling::run_animation_frame(&mut engine).is_empty());
        assert!(engine.eval_value("revealedBeforeLoad").unwrap().is_ok_and(|value| matches!(value, Value::Bool(true))));
    }

    #[test]
    fn page_reveal_uses_first_real_frame_and_precedes_animation_callbacks() {
        let mut engine = lumen::Engine::new();
        let document = crate::install(engine.ctx(), "<!doctype html><main></main>", 64).unwrap();
        engine.eval_value(r#"
            var revealOrder = [];
            addEventListener('pagereveal', e => {
                if (!(e instanceof PageRevealEvent) || !(e instanceof Event) ||
                    e.viewTransition !== null || !e.isTrusted || e.target !== window)
                    throw new Error('incorrect native reveal event');
                revealOrder.push('reveal');
            });
            requestAnimationFrame(() => revealOrder.push('raf'));
        "#).unwrap().unwrap_or_else(|_| panic!("page reveal test setup threw"));
        document.set_document_ready_state(engine.ctx(), DocumentReadyState::Interactive).unwrap();
        assert!(reveal_pending(engine.ctx()));
        assert!(engine.eval_value("revealOrder.length === 0").unwrap().is_ok_and(|value| matches!(value, Value::Bool(true))));
        assert!(crate::scheduling::run_animation_frame(&mut engine).is_empty());
        assert!(engine.eval_value("revealOrder.join(',') === 'reveal,raf'").unwrap().is_ok_and(|value| matches!(value, Value::Bool(true))));
        assert!(!reveal_pending(engine.ctx()));
        assert!(crate::scheduling::run_animation_frame(&mut engine).is_empty());
        assert!(engine.eval_value("revealOrder.length === 2").unwrap().is_ok_and(|value| matches!(value, Value::Bool(true))));
    }

    #[test]
    fn page_reveal_native_constructor_defaults_and_brand_checks() {
        let mut engine = lumen::Engine::new();
        let _document = crate::install(engine.ctx(), "<main></main>", 64).unwrap();
        assert!(engine.eval_value(r#"
            (() => {
                const e = new PageRevealEvent('test', { bubbles: true });
                if (!(e instanceof Event) || e.viewTransition !== null || !e.bubbles || e.isTrusted)
                    return false;
                if (new PageRevealEvent('test', {viewTransition: null}).viewTransition !== null)
                    return false;
                try { new PageRevealEvent('test', {viewTransition: {}}); return false; }
                catch (error) { return error instanceof TypeError; }
            })()
        "#).unwrap().is_ok_and(|value| matches!(value, Value::Bool(true))));
    }
}
