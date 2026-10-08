//! Animation Worklet module realms and animator-driven keyframe sampling.
//! Resource routing and module evaluation belong to the engine/host loader;
//! this adapter owns animator definitions and document rendering opportunities.
use super::*;
use crate::animations::{self, WorkletEffectBinding};
use crate::realm_services::{capture_realm_value, RealmServices};
use lumen::embed::{Deferred, JsHost, Nullable, RealmHandle};
use lumen_bind::{Host, This};
use std::collections::HashMap;
use std::collections::HashSet;
use std::rc::Weak;
use lumen_common::limits::{ByteBudget, ByteLease};

const MAX_DEFINITIONS: usize = 1024;
const MAX_ANIMATIONS: usize = 4096;
const MAX_EFFECTS: usize = 256;
const MAX_MODULE_BYTES: usize = 8 * 1024 * 1024;

struct WorkletState {
    owner: Weak<DomRealm>,
    scope: Option<RealmHandle>,
    definitions: HashMap<String, AnimatorDefinition>,
    modules: HashSet<String>,
    records: Vec<Weak<RefCell<AnimationState>>>,
    options_budget: std::sync::Arc<ByteBudget>,
}

#[derive(Clone)]
struct AnimatorDefinition {
    constructor: WeakValue,
    animate: WeakValue,
}

struct EffectState {
    binding: WorkletEffectBinding,
    local_time: Cell<Option<f64>>,
}

struct AnimationState {
    worklet: Rc<RefCell<WorkletState>>,
    name: String,
    effect_value: Value,
    timeline: Value,
    options: Rc<[u8]>,
    _options_reservation: ByteLease,
    instance: Option<WeakValue>,
    wrapper: Option<WeakValue>,
    effects: Vec<Rc<EffectState>>,
    active_slot: String,
    instance_slot: String,
    start_time: Option<f64>,
    hold_time: Option<f64>,
    rate: f64,
    state: &'static str,
    pending: bool,
}

fn service(ctx: &mut Ctx) -> OpResult<Rc<RefCell<WorkletState>>> {
    RealmServices::<RefCell<WorkletState>>::current(ctx)
        .ok_or_else(|| OpError::new("InvalidStateError", "Animation Worklet is unavailable"))
}

fn member(ctx: &mut Ctx, value: &Value, key: &str) -> OpResult<Value> {
    ctx.get_member(value, key).map_err(|error| OpError::thrown(lumen::embed::abrupt_value(error)))
}

#[lumen_bind::module(name = "animation_worklet_scope")]
mod scope_bindings {
    use super::*;
    #[op(rename(js = "registerAnimator"))]
    pub fn register_animator(ctx: &mut Ctx, name: &str, constructor: Value) -> OpResult<()> {
        if name.is_empty() { return Err(OpError::type_error("animator name must not be empty")); }
        if !ctx.value_is_constructor(&constructor) {
            return Err(OpError::type_error("animator definition must be a constructor"));
        }
        let state = service(ctx)?;
        if state.borrow().definitions.contains_key(name) {
            return Err(OpError::new("NotSupportedError", "animator name is already registered"));
        }
        if state.borrow().definitions.len() >= MAX_DEFINITIONS {
            return Err(OpError::new("QuotaExceededError", "animator definition budget exhausted"));
        }
        let prototype = member(ctx, &constructor, "prototype")?;
        let animate = member(ctx, &prototype, "animate")?;
        if !animate.is_callable() {
            return Err(OpError::type_error("animator prototype must have an animate method"));
        }
        let state_method = member(ctx, &prototype, "state")?;
        if !matches!(state_method, Value::Undefined) && !state_method.is_callable() {
            return Err(OpError::type_error("animator state must be callable"));
        }
        let constructor = capture_realm_value(ctx, constructor)?;
        let animate = capture_realm_value(ctx, animate)?;
        state.borrow_mut().definitions.insert(name.to_owned(), AnimatorDefinition { constructor, animate });
        Ok(())
    }
}

fn ensure_scope(ctx: &mut Ctx, state: &Rc<RefCell<WorkletState>>) -> OpResult<RealmHandle> {
    if let Some(scope) = state.borrow().scope.clone() { return Ok(scope); }
    let scope = ctx.create_host_realm();
    ctx.with_host_realm(&scope, |ctx| {
        RealmServices::replace_shared_current(ctx, state.clone());
        let global = ctx.global_object();
        ctx.install_module::<scope_bindings::Module>(&global).map_err(OpError::thrown)?;
        let constructor = ctx.class_constructor::<DomWorkletEffect>();
        crate::install_interface(ctx, &global, "WorkletAnimationEffect", constructor)?;
        let constructor = ctx.class_constructor::<DomWorkletGroupEffect>();
        crate::install_interface(ctx, &global, "WorkletGroupEffect", constructor)?;
        Ok::<_, OpError>(())
    }).map_err(crate::browsing_context::host_realm_error)??;
    state.borrow_mut().scope = Some(scope.clone());
    Ok(scope)
}

#[lumen_bind::class(name = "Worklet", hint(js(webidl)))]
pub(crate) struct DomAnimationWorklet { loader: WorkletLoader }
type ModuleLoader = Rc<dyn Fn(&mut Ctx, &str, &str) -> OpResult<Value>>;
enum WorkletLoader { Animation(Rc<RefCell<WorkletState>>), Other(ModuleLoader) }

pub(crate) fn new_worklet(ctx: &mut Ctx, loader: ModuleLoader) -> Value {
    ctx.new_instance(DomAnimationWorklet { loader: WorkletLoader::Other(loader) })
}

#[lumen_bind::methods]
impl DomAnimationWorklet {
    fn add_module(&self, ctx: &mut Ctx, url: &str, options: Option<Value>) -> OpResult<Value> {
        let credentials = crate::ui_events::dictionary_string(ctx, &options, "credentials", "same-origin", false)?;
        if !matches!(credentials.as_str(), "omit" | "same-origin" | "include") {
            return Err(OpError::type_error("invalid Worklet credentials mode"));
        }
        let (deferred, promise) = Deferred::new_registered(ctx, |_, deferred| {
            let promise = deferred.promise(); (deferred, promise)
        });
        match self.load_module(ctx, url, &credentials) {
            // Adopt the real ECMAScript evaluation (including dependencies and
            // top-level await) into the calling Window's Promise intrinsics.
            Ok(evaluation) => deferred.resolve(ctx, evaluation),
            Err(error) => {
                let reason = error.to_value(ctx);
                deferred.reject(ctx, reason);
            }
        }
        Ok(promise)
    }
}

impl DomAnimationWorklet {
    fn load_module(&self, ctx: &mut Ctx, input: &str, credentials: &str) -> OpResult<Value> {
        let state = match &self.loader {
            WorkletLoader::Animation(state) => state,
            WorkletLoader::Other(loader) => return loader(ctx, input, credentials),
        };
        let owner = state.borrow().owner.upgrade()
            .ok_or_else(|| OpError::new("InvalidStateError", "worklet document was retired"))?;
        let url = lumen_common::url::parse(input, Some(&owner.base_url()))
            .map_err(|_| OpError::new("SyntaxError", "invalid worklet module URL"))?.href();
        if state.borrow().modules.len() >= MAX_DEFINITIONS && !state.borrow().modules.contains(&url) {
            return Err(OpError::new("QuotaExceededError", "worklet module budget exhausted"));
        }
        let scope = ensure_scope(ctx, state)?;
        let evaluation = load_module_in_scope(ctx, &owner, &scope, &url, credentials)?;
        state.borrow_mut().modules.insert(url);
        Ok(evaluation)
    }
}

/// Shared isolated worklet ESM fetch/evaluation. Only the captured browser fetch
/// service is inherited; globals, module maps and import maps remain separate.
pub(crate) fn load_module_in_scope(ctx: &mut Ctx, owner: &Rc<DomRealm>, scope: &RealmHandle,
    url: &str, credentials: &str) -> OpResult<Value> {
        let base = owner.document_url().unwrap_or_else(|| "about:blank".into());
        let loader = ctx.module_fetch_loader_for_host();
        let context = Rc::new(lumen::ClassicScriptContext { base_url: url.into(), nonce: String::new(),
            credentials_mode: credentials.into(), referrer_policy: owner.script_fetch_referrer().policy.name().into() });
        let resource = if url.starts_with("blob:") {
            let policies = owner.module_fetch_policy_snapshot()?;
            let decision = policies.check_script_request(&url, &base, "", "", false)
                .map_err(|_| OpError::new("NetworkError", "worklet policy check failed"))?;
            owner.report_module_fetch_violations(ctx, decision.violations)?;
            if decision.blocked { return Err(OpError::new("NetworkError", "worklet module blocked by CSP")); }
            let resource = lumen_host::blob::object_url_resource(ctx, &url)
                .ok_or_else(|| OpError::new("NetworkError", "worklet object URL is unavailable"))?;
            if !lumen_common::mime::is_javascript_module_mime(Some(&resource.content_type)) {
                return Err(OpError::new("NetworkError", "worklet module has a non-JavaScript MIME type"));
            }
            if resource.bytes.len() > MAX_MODULE_BYTES {
                return Err(OpError::new("QuotaExceededError", "worklet module budget exhausted"));
            }
            let text = String::from_utf8_lossy(&resource.bytes);
            lumen::ModuleFetchResult { key: url.into(), source: text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned(), script_context: Some(context.clone()) }
        } else {
            let loader = loader.as_ref().ok_or_else(|| OpError::new("NetworkError", "browser module loader is unavailable"))?;
            loader(lumen::ModuleFetchRequest { settings_key: ctx.global_object().object_identity().unwrap_or(0),
                resolution: None, specifier: url.into(), referrer: base,
                attribute_type: None, script_context: Some(context.clone()) })
                .ok_or_else(|| OpError::new("NetworkError", "worklet module fetch failed"))?
        };
        if resource.source.len() > MAX_MODULE_BYTES {
            return Err(OpError::new("QuotaExceededError", "worklet module budget exhausted"));
        }
        ctx.with_host_realm(scope, |ctx| {
            // Only the fetch service is inherited. The worklet gets its own
            // ECMAScript module map, global, and lexical environment.
            if let Some(loader) = loader {
                ctx.install_module_fetch_loader(Rc::new(move |mut request| {
                    // Worklets do not inherit a Window import map. Use the
                    // shared URL/module resolver before the captured fetch.
                    request.resolution = Some(lumen_common::import_maps::resolve_without_map(&request.specifier, &request.referrer).ok()?);
                    loader(request)
                }));
            }
            let resolution_url = resource.script_context.as_ref().map_or(url, |context| context.base_url.as_str());
            ctx.run_prepared_module_for_host(&resource.source, &resource.key, &url, resolution_url, resource.script_context.clone())
                .map(|evaluation| evaluation.promise().clone())
                .map_err(|error| OpError::new("SyntaxError", error.message))
        }).map_err(crate::browsing_context::host_realm_error)?
}

#[lumen_bind::class(name = "WorkletAnimation", hint(js(webidl)))]
pub(crate) struct DomWorkletAnimation { record: Rc<RefCell<AnimationState>> }

struct WorkletConstructor(DomWorkletAnimation);
impl lumen_bind::CtorRet<JsHost, DomWorkletAnimation> for WorkletConstructor {
    fn into_ctor(self, cx: &<JsHost as Host>::Cx<'_>) -> Result<Value, Value> {
        let record = self.0.record.clone();
        let value = <JsHost as Host>::construct(cx, self.0)?;
        <JsHost as Host>::with_ctx(cx, |ctx| {
            let mut state = record.borrow_mut();
            state.wrapper = ctx.weak_value(&value);
            for retained in [&state.effect_value, &state.timeline] {
                let slot = ctx.allocate_native_private_slot_name();
                ctx.define_native_private_value_slot(&value, &slot, retained.clone())?;
            }
            Ok::<_, Value>(())
        })?;
        Ok(value)
    }
}

#[lumen_bind::methods]
impl DomWorkletAnimation {
    #[constructor]
    fn new(ctx: &mut Ctx, name: &str, effects: Value, timeline: Option<Value>, options: Option<Value>) -> OpResult<WorkletConstructor> {
        let worklet = service(ctx)?;
        if !worklet.borrow().definitions.contains_key(name) {
            return Err(OpError::new("InvalidStateError", "animator name is not registered"));
        }
        worklet.borrow_mut().records.retain(|record| record.strong_count() != 0);
        if worklet.borrow().records.len() >= MAX_ANIMATIONS {
            return Err(OpError::new("QuotaExceededError", "worklet animation budget exhausted"));
        }
        let owner = worklet.borrow().owner.upgrade().ok_or_else(|| OpError::new("InvalidStateError", "worklet document was retired"))?;
        let options = lumen_host::structured_clone::serialize_for_storage(ctx, &options.unwrap_or(Value::Undefined), MAX_MODULE_BYTES)?;
        let options_reservation = worklet.borrow().options_budget.reserve(options.len())
            .ok_or_else(|| OpError::new("QuotaExceededError", "worklet animator options budget exhausted"))?;
        let timeline = match timeline { Some(value) if !matches!(value, Value::Null | Value::Undefined) => value,
            _ => animations::document_timeline(ctx, &owner)? };
        let values = if ctx.with_instance::<animations::DomKeyframeEffect, _>(&effects, |_| ()).is_ok() {
            vec![effects.clone()]
        } else {
            ctx.iterable_to_list(&effects, MAX_EFFECTS)?
        };
        if values.is_empty() { return Err(OpError::new("NotSupportedError", "WorkletAnimation requires at least one effect")); }
        for value in &values { ctx.with_instance::<animations::DomKeyframeEffect, _>(value, |_| ())?; }
        let mut bindings = Vec::new();
        for value in values {
            bindings.push(Rc::new(EffectState { binding: animations::worklet_bind_effect(ctx, &value, &timeline)?, local_time: Cell::new(None) }));
        }
        let record = Rc::new(RefCell::new(AnimationState { worklet: worklet.clone(), name: name.to_owned(),
            effect_value: effects, timeline, options: options.into(), _options_reservation: options_reservation, instance: None, wrapper: None,
            effects: bindings, active_slot: ctx.allocate_native_private_slot_name(), instance_slot: ctx.allocate_native_private_slot_name(), start_time: None, hold_time: None,
            rate: 1.0, state: "idle", pending: false }));
        worklet.borrow_mut().records.push(Rc::downgrade(&record));
        Ok(WorkletConstructor(Self { record }))
    }
    #[getter]
    fn animator_name(&self) -> String { self.record.borrow().name.clone() }
    #[getter]
    fn effect(&self) -> Value { self.record.borrow().effect_value.clone() }
    #[getter]
    fn timeline(&self) -> Value { self.record.borrow().timeline.clone() }
    #[getter]
    fn play_state(&self) -> String { self.record.borrow().state.into() }
    #[getter]
    fn start_time(&self) -> Nullable<f64> { Nullable(self.record.borrow().start_time) }
    #[getter]
    fn current_time(&self, ctx: &mut Ctx) -> OpResult<Nullable<f64>> { Ok(Nullable(current_time(ctx, &self.record)?)) }
    #[getter]
    fn playback_rate(&self) -> f64 { self.record.borrow().rate }
    #[setter]
    fn set_playback_rate(&self, ctx: &mut Ctx, rate: f64) -> OpResult<()> {
        if !rate.is_finite() { return Err(OpError::type_error("playbackRate must be finite")); }
        let current = current_time(ctx, &self.record)?;
        let timeline_value = self.record.borrow().timeline.clone();
        let (timeline, progress) = animations::worklet_timeline_sample(ctx, &timeline_value)?;
        let mut state = self.record.borrow_mut();
        state.rate = rate;
        if state.state == "running" && !progress {
            state.start_time = if rate == 0.0 { None } else { timeline.zip(current).map(|(timeline, time)| timeline - time / rate) };
            state.hold_time = if rate == 0.0 { current } else { None };
        }
        Ok(())
    }
    fn play(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<()> {
        let timeline_value = self.record.borrow().timeline.clone();
        let (now, progress) = animations::worklet_timeline_sample(ctx, &timeline_value)?;
        let mut state = self.record.borrow_mut();
        if state.worklet.borrow().owner.upgrade().is_none() {
            return Err(OpError::new("InvalidStateError", "worklet document was retired"));
        }
        if state.effects.iter().any(|effect| !effect.binding.has_keyframes()) {
            return Err(OpError::new("InvalidStateError", "worklet animation has an empty keyframe effect"));
        }
        if state.state == "running" || state.state == "pending" { return Ok(()); }
        state.hold_time = if progress { now.map(|now| now * state.rate) } else { Some(state.hold_time.unwrap_or(0.0)) };
        state.start_time = None;
        state.pending = true;
        state.state = "pending";
        let slot = state.active_slot.clone();
        drop(state);
        let global = ctx.global_object();
        ctx.set_native_internal_value_slot(&global, &slot, this.0).map_err(OpError::thrown)?;
        Ok(())
    }
    fn pause(&self, ctx: &mut Ctx) -> OpResult<()> {
        let current = current_time(ctx, &self.record)?;
        let mut state = self.record.borrow_mut();
        state.hold_time = current;
        state.start_time = None;
        state.state = "paused";
        // A first-frame animator sample still belongs to immediately paused
        // animations; subsequent frames keep the frozen result.
        state.pending = state.instance.is_none();
        Ok(())
    }
    fn cancel(&self, ctx: &mut Ctx) -> OpResult<()> {
        let mut state = self.record.borrow_mut();
        state.state = "idle";
        state.pending = false;
        state.start_time = None;
        state.hold_time = None;
        state.instance = None;
        for effect in &state.effects { effect.local_time.set(None); effect.binding.clear()?; }
        let slot = state.active_slot.clone();
        let instance_slot = state.instance_slot.clone();
        let wrapper = state.wrapper.as_ref().and_then(WeakValue::upgrade);
        drop(state);
        let global = ctx.global_object();
        ctx.set_native_internal_value_slot(&global, &slot, Value::Null).map_err(OpError::thrown)?;
        if let Some(wrapper) = wrapper { ctx.set_native_internal_value_slot(&wrapper, &instance_slot, Value::Null).map_err(OpError::thrown)?; }
        Ok(())
    }
}

fn current_time(ctx: &mut Ctx, record: &Rc<RefCell<AnimationState>>) -> OpResult<Option<f64>> {
    let (state, hold, start, rate, timeline) = { let state = record.borrow();
        (state.state, state.hold_time, state.start_time, state.rate, state.timeline.clone()) };
    if state == "idle" { return Ok(None); }
    let (now, progress) = animations::worklet_timeline_sample(ctx, &timeline)?;
    if progress && state != "paused" { return Ok(now.map(|now| now * rate)); }
    if hold.is_some() { return Ok(hold); }
    Ok(now.zip(start).map(|(now, start)| (now - start) * rate))
}

#[lumen_bind::class(name = "WorkletAnimationEffect", hint(js(webidl)))]
struct DomWorkletEffect { effect: Rc<EffectState> }
#[lumen_bind::methods]
impl DomWorkletEffect {
    #[getter]
    fn local_time(&self) -> Nullable<f64> { Nullable(self.effect.local_time.get()) }
    #[setter]
    fn set_local_time(&self, time: Option<f64>) -> OpResult<()> {
        if time.is_some_and(|time| !time.is_finite()) { return Err(OpError::type_error("localTime must be finite")); }
        self.effect.local_time.set(time); Ok(())
    }
    fn get_timing(&self, ctx: &mut Ctx) -> OpResult<Value> {
        animations::worklet_timing_object(ctx, &self.effect.binding, self.effect.local_time.get(), false)
    }
    fn get_computed_timing(&self, ctx: &mut Ctx) -> OpResult<Value> {
        animations::worklet_timing_object(ctx, &self.effect.binding, self.effect.local_time.get(), true)
    }
}

#[lumen_bind::class(name = "WorkletGroupEffect", hint(js(webidl)))]
struct DomWorkletGroupEffect { children: Vec<Value> }
#[lumen_bind::methods]
impl DomWorkletGroupEffect {
    fn get_children(&self) -> Vec<Value> { self.children.clone() }
}

pub(crate) fn pending(ctx: &mut Ctx) -> bool {
    RealmServices::<RefCell<WorkletState>>::current(ctx).is_some_and(|service| service.borrow().records.iter()
        .filter_map(Weak::upgrade).any(|record| { let record = record.borrow(); record.pending || record.state == "running" || record.state == "pending" }))
}

pub(crate) fn advance(ctx: &mut Ctx) -> OpResult<()> {
    let Some(service) = RealmServices::<RefCell<WorkletState>>::current(ctx) else { return Ok(()); };
    let records = service.borrow().records.iter().filter_map(Weak::upgrade).collect::<Vec<_>>();
    for record in records {
        let (state, pending, timeline) = { let state = record.borrow(); (state.state, state.pending, state.timeline.clone()) };
        if state == "idle" || (state == "paused" && !pending) { continue; }
        let (now, progress) = animations::worklet_timeline_sample(ctx, &timeline)?;
        if pending {
            let mut state = record.borrow_mut();
            if state.state != "paused" {
                state.state = "running";
                state.start_time = if progress { now.map(|_| 0.0) } else {
                    now.filter(|_| state.rate != 0.0).map(|now| now - state.hold_time.unwrap_or(0.0) / state.rate)
                };
                if state.start_time.is_some() { state.hold_time = None; }
            }
            state.pending = false;
        }
        if progress && record.borrow().state == "running" {
            record.borrow_mut().start_time = now.map(|_| 0.0);
            if now.is_none() {
                for effect in &record.borrow().effects { effect.binding.publish(None); }
                continue;
            }
        }
        let current = current_time(ctx, &record)?;
        let (worklet, name, options, instance, wrapper, effects) = { let state = record.borrow();
            (state.worklet.clone(), state.name.clone(), state.options.clone(), state.instance.as_ref().and_then(WeakValue::upgrade),
                state.wrapper.as_ref().and_then(WeakValue::upgrade), state.effects.clone()) };
        let scope = ensure_scope(ctx, &worklet)?;
        let result = ctx.with_host_realm(&scope, |ctx| {
            let definition = worklet.borrow().definitions.get(&name).cloned()
                .ok_or_else(|| OpError::new("InvalidStateError", "animator definition was retired"))?;
            let instance = match instance {
                Some(instance) => instance,
                None => {
                    let constructor = definition.constructor.upgrade()
                        .ok_or_else(|| OpError::new("InvalidStateError", "animator definition was retired"))?;
                    let options = lumen_host::structured_clone::deserialize_for_storage(ctx, &options)?;
                    ctx.construct_value(constructor, &[options]).map_err(OpError::thrown)?
                }
            };
            if let Some(wrapper) = wrapper {
                let slot = record.borrow().instance_slot.clone();
                ctx.set_native_internal_value_slot(&wrapper, &slot, instance.clone()).map_err(OpError::thrown)?;
            }
            record.borrow_mut().instance = ctx.weak_value(&instance);
            let mut values = Vec::new();
            for effect in &effects { values.push(ctx.new_instance(DomWorkletEffect { effect: effect.clone() })); }
            let effect = if values.len() == 1 { values.pop().unwrap() } else {
                let group = ctx.new_instance(DomWorkletGroupEffect { children: values.clone() });
                let slot = ctx.allocate_native_private_slot_name();
                ctx.define_native_private_array_slot(&group, &slot, values).map_err(OpError::thrown)?;
                group
            };
            let animate = definition.animate.upgrade()
                .ok_or_else(|| OpError::new("InvalidStateError", "animator method was retired"))?;
            ctx.invoke(animate, instance, &[current.map(Value::Num).unwrap_or(Value::Null), effect]).map_err(OpError::thrown)?;
            Ok::<_, OpError>(())
        }).map_err(crate::browsing_context::host_realm_error)?;
        match result {
            Ok(()) => for effect in effects { effect.binding.publish(effect.local_time.get()); },
            Err(error) => {
                // One broken animator does not suppress other animations or
                // requestAnimationFrame callbacks at this rendering opportunity.
                DomWorkletAnimation { record: record.clone() }.cancel(ctx)?;
                let exception = error.to_value(ctx); DomRealm::report_exception(ctx, exception);
            }
        }
    }
    Ok(())
}

pub(crate) fn install(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> OpResult<()> {
    if let Some(previous) = RealmServices::<RefCell<WorkletState>>::current(ctx) {
        let previous_owner = previous.borrow().owner.upgrade();
        if previous_owner.as_ref().is_some_and(|owner| Rc::ptr_eq(owner, realm)) { return Ok(()); }
        if let Some(previous_owner) = previous_owner { retire_document(ctx, &previous_owner)?; }
    }
    let state = RealmServices::replace_current(ctx, RefCell::new(WorkletState {
        owner: Rc::downgrade(realm), scope: None, definitions: HashMap::new(), modules: HashSet::new(), records: Vec::new(), options_budget: ByteBudget::new(16 * 1024 * 1024),
    }));
    let global = ctx.global_object();
    let constructor = ctx.class_constructor::<DomWorkletAnimation>();
    crate::install_interface(ctx, &global, "WorkletAnimation", constructor)?;
    let constructor = ctx.class_constructor::<DomAnimationWorklet>();
    crate::install_interface(ctx, &global, "Worklet", constructor)?;
    let css = member(ctx, &global, "CSS")?;
    let worklet = ctx.new_instance(DomAnimationWorklet { loader: WorkletLoader::Animation(state) });
    ctx.set_member(&css, "animationWorklet", worklet).map_err(|error| OpError::thrown(lumen::embed::abrupt_value(error)))?;
    Ok(())
}

pub(crate) fn retire_document(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> OpResult<()> {
    let Some(state) = RealmServices::<RefCell<WorkletState>>::current(ctx) else { return Ok(()); };
    if !state.borrow().owner.upgrade().is_some_and(|owner| Rc::ptr_eq(&owner, realm)) { return Ok(()); }
    let records = state.borrow().records.iter().filter_map(Weak::upgrade).collect::<Vec<_>>();
    for record in records { DomWorkletAnimation { record }.cancel(ctx)?; }
    let scope = state.borrow_mut().scope.take();
    state.borrow_mut().definitions.clear();
    state.borrow_mut().modules.clear();
    state.borrow_mut().records.clear();
    state.borrow_mut().owner = Weak::new();
    if let Some(scope) = scope {
        ctx.cancel_async_module_imports_for_realm(&scope);
        ctx.dispose_host_realm(&scope).map_err(|_| OpError::new("InvalidStateError", "worklet realm could not be retired"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen::Engine;

    fn eval(engine: &mut Engine, source: &str) -> Value {
        match engine.eval_value(source).expect("parse test script") {
            Ok(value) => value,
            Err(error) => panic!("script exception: {}", engine.ctx().coerce_string(&error).unwrap_or_else(|_| "<unprintable>".into())),
        }
    }
    fn setup(module: &'static str) -> (Engine, Rc<DomRealm>) {
        let mut engine = Engine::new();
        let realm = crate::install(engine.ctx(), "<div id='target' style='opacity:.1;width:10px'></div><div id='other'></div>", 128).unwrap();
        realm.set_document_url("https://example.test/document");
        install(engine.ctx(), &realm).unwrap();
        engine.ctx().install_module_fetch_loader(Rc::new(move |request| {
            let url = lumen_common::url::parse(&request.specifier, Some(&request.referrer)).ok()?.href();
            let source = if url == "https://example.test/animator.js" { module }
                else if url == "https://example.test/dependency.js" { "export const localTime=250;" }
                else { return None };
            Some(lumen::ModuleFetchResult { key: url, source: source.into(), script_context: None })
        }));
        eval(&mut engine, "globalThis.loaded=false;globalThis.rejected=false;globalThis.modulePromise=CSS.animationWorklet.addModule('animator.js');modulePromise.then(value=>loaded=value===undefined,()=>rejected=true)");
        assert!(matches!(eval(&mut engine, "modulePromise instanceof Promise"), Value::Bool(true)));
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(eval(&mut engine, "loaded && !rejected"), Value::Bool(true)));
        (engine, realm)
    }
    fn frame(engine: &mut Engine, timestamp: f64) {
        let realm = engine.ctx().current_host_realm();
        assert!(crate::scheduling::run_animation_frame_in_realm_at(engine, &realm, timestamp).is_empty(),
            "actual host rendering opportunity samples the animator and shared effect");
    }

    #[test]
    fn animation_worklet_module_isolation_imports_and_real_effect_sampling() {
        let (mut engine, realm) = setup("import {localTime} from './dependency.js'; if(typeof document!=='undefined')throw Error('Window leaked');registerAnimator('constant',class {animate(now,effect){effect.localTime=localTime}});");
        assert!(matches!(eval(&mut engine, "typeof registerAnimator==='undefined'"), Value::Bool(true)));
        eval(&mut engine, "globalThis.effect=new KeyframeEffect(document.getElementById('target'),{opacity:[0,1],width:['10px','50px']},{duration:1000,fill:'both'});globalThis.animation=new WorkletAnimation('constant',effect);animation.play()");
        frame(&mut engine, 1000.0);
        assert!(matches!(eval(&mut engine, "animation.playState==='running' && effect.getComputedTiming().localTime===250"), Value::Bool(true)));
        realm.with_session(|session| {
            let document = session.document();
            let target = selector::query_selector(document, document.root(), "#target").unwrap().unwrap();
            let style = session.computed_style(target).unwrap();
            assert!((style.opacity - 0.25).abs() < 0.001);
            assert_eq!(style.width, Some(20.0));
        });
        eval(&mut engine, "animation.cancel()");
        assert!(matches!(eval(&mut engine, "animation.playState==='idle' && animation.currentTime===null && effect.getComputedTiming().localTime===null"), Value::Bool(true)));
        realm.with_session(|session| {
            let document = session.document();
            let target = selector::query_selector(document, document.root(), "#target").unwrap().unwrap();
            assert!((session.computed_style(target).unwrap().opacity - 0.1).abs() < 0.001);
        });
    }

    #[test]
    fn animation_worklet_retains_instance_options_and_fresh_effect_wrappers() {
        let (mut engine, _) = setup("registerAnimator('stateful',class {constructor(options){this.time=options.time} animate(now,effect){if(effect.authorProperty)throw Error('stale wrapper');effect.authorProperty=true;effect.localTime=this.time++;}});");
        eval(&mut engine, "globalThis.effect=new KeyframeEffect(document.getElementById('target'),{opacity:[0,1]},{duration:1000,fill:'both'});globalThis.options={time:100};globalThis.animation=new WorkletAnimation('stateful',effect,document.timeline,options);options.time=900;animation.play()");
        frame(&mut engine, 1000.0); frame(&mut engine, 1010.0);
        assert!(matches!(eval(&mut engine, "effect.getComputedTiming().localTime===101 && animation.currentTime===10"), Value::Bool(true)));
        eval(&mut engine, "animation.pause()"); frame(&mut engine, 1200.0);
        assert!(matches!(eval(&mut engine, "animation.playState==='paused' && animation.currentTime===10 && effect.getComputedTiming().localTime===101"), Value::Bool(true)));
        eval(&mut engine, "animation.play()"); frame(&mut engine, 1500.0);
        assert!(matches!(eval(&mut engine, "effect.getComputedTiming().localTime===102 && animation.currentTime===10"), Value::Bool(true)));
        eval(&mut engine, "animation.cancel();animation.play()"); frame(&mut engine, 1600.0);
        assert!(matches!(eval(&mut engine, "effect.getComputedTiming().localTime===100"), Value::Bool(true)));
    }

    #[test]
    fn animation_worklet_timing_recomputes_and_null_removes_effect() {
        let (mut engine, _) = setup("registerAnimator('timing',class {constructor(){this.count=0} animate(now,effect){effect.localTime=350;if(effect.getComputedTiming().progress!==0.5)throw Error('timing');effect.localTime=this.count++===0?effect.getTiming().duration/2:null}});");
        eval(&mut engine, "globalThis.effect=new KeyframeEffect(document.getElementById('target'),{opacity:[0,1]},{duration:100,iterations:5,fill:'both'});globalThis.animation=new WorkletAnimation('timing',effect);animation.play()");
        frame(&mut engine, 1000.0);
        assert!(matches!(eval(&mut engine, "effect.getComputedTiming().localTime===50"), Value::Bool(true)));
        frame(&mut engine, 1010.0);
        assert!(matches!(eval(&mut engine, "effect.getComputedTiming().localTime===null && getComputedStyle(document.getElementById('target')).opacity==='0.1'"), Value::Bool(true)));
    }

    #[test]
    fn animation_worklet_blob_module_mime_revocation_and_csp() {
        let mut runtime = lumen_runtime::Runtime::new_browser();
        let realm = crate::install(runtime.engine().ctx(), "<div id='target'></div>", 128).unwrap();
        realm.set_document_url("https://example.test/document");
        install(runtime.engine().ctx(), &realm).unwrap();
        eval(runtime.engine(), r#"
            globalThis.loaded=false;globalThis.denied=0;
            const source="registerAnimator('blob',class {animate(now,effect){effect.localTime=400}});";
            const url=URL.createObjectURL(new Blob([source],{type:'text/javascript'}));
            CSS.animationWorklet.addModule(url).then(()=>loaded=true);
            const wrong=URL.createObjectURL(new Blob([source],{type:'text/plain'}));
            CSS.animationWorklet.addModule(wrong).catch(()=>denied++);
            const revoked=URL.createObjectURL(new Blob([source],{type:'text/javascript'}));
            URL.revokeObjectURL(revoked);
            CSS.animationWorklet.addModule(revoked).catch(()=>denied++);
        "#);
        runtime.engine().ctx().drain_microtasks_for_host();
        assert!(matches!(eval(runtime.engine(), "loaded && denied===2"), Value::Bool(true)));
        eval(runtime.engine(), "globalThis.effect=new KeyframeEffect(document.getElementById('target'),{opacity:[0,1]},{duration:1000});globalThis.animation=new WorkletAnimation('blob',effect);animation.play()");
        frame(runtime.engine(), 1000.0);
        assert!(matches!(eval(runtime.engine(), "effect.getComputedTiming().localTime===400"), Value::Bool(true)));
        realm.set_content_security_policy_headers(&[("Content-Security-Policy".into(), "script-src 'none'".into())]).unwrap();
        eval(runtime.engine(), "CSS.animationWorklet.addModule(URL.createObjectURL(new Blob(['registerAnimator(\"blocked\",class{animate(){}})'],{type:'text/javascript'}))).catch(()=>denied++)");
        runtime.engine().ctx().drain_microtasks_for_host();
        assert!(matches!(eval(runtime.engine(), "denied===3"), Value::Bool(true)));
    }

    #[test]
    fn animation_worklet_top_level_await_settles_real_module_promise() {
        let (mut engine, _) = setup("await Promise.resolve();registerAnimator('after-await',class {animate(now,effect){effect.localTime=300}});");
        eval(&mut engine, "globalThis.effect=new KeyframeEffect(document.getElementById('target'),{opacity:[0,1]},{duration:1000});globalThis.animation=new WorkletAnimation('after-await',effect);animation.play()");
        frame(&mut engine, 1000.0);
        assert!(matches!(eval(&mut engine, "effect.getComputedTiming().localTime===300"), Value::Bool(true)));
        let owner = service(engine.ctx()).unwrap().borrow().owner.upgrade().unwrap();
        retire_document(engine.ctx(), &owner).unwrap();
        assert!(matches!(eval(&mut engine, "animation.playState==='idle' && effect.getComputedTiming().localTime===null"), Value::Bool(true)));
    }

    #[test]
    fn animation_worklet_group_children_sample_distinct_real_effects() {
        let (mut engine, _) = setup("registerAnimator('group',class {animate(now,group){const children=group.getChildren();if(children[0]!==group.getChildren()[0])throw Error('identity');children[0].localTime=250;children[1].localTime=750}});");
        eval(&mut engine, "globalThis.a=new KeyframeEffect(document.getElementById('target'),{opacity:[0,1]},{duration:1000,fill:'both'});globalThis.b=new KeyframeEffect(document.getElementById('other'),{opacity:[0,1]},{duration:1000,fill:'both'});globalThis.animation=new WorkletAnimation('group',[a,b]);animation.play()");
        engine.ctx().collect_garbage_for_host();
        frame(&mut engine, 1000.0);
        assert!(matches!(eval(&mut engine, "a.getComputedTiming().localTime===250 && b.getComputedTiming().localTime===750 && getComputedStyle(document.getElementById('target')).opacity==='0.25' && getComputedStyle(document.getElementById('other')).opacity==='0.75'"), Value::Bool(true)));
    }

    #[test]
    fn animation_worklet_fetch_credentials_referrer_and_module_base_are_captured() {
        let mut engine = Engine::new();
        let realm = crate::install(engine.ctx(), "<base href='https://example.test/modules/'><div></div>", 128).unwrap();
        realm.set_document_url("https://example.test/document");
        let requests = Rc::new(RefCell::new(Vec::new()));
        let observed = requests.clone();
        engine.ctx().install_module_fetch_loader(Rc::new(move |request| {
            let context = request.script_context.as_ref()?;
            observed.borrow_mut().push((request.specifier.clone(), request.referrer.clone(), context.credentials_mode.clone()));
            let source = if request.specifier.ends_with("animator.js") {
                "import './dependency.js';registerAnimator('fetch',class{animate(){}});"
            } else { "export const value=1;" };
            let key = request.resolution.map_or_else(||request.specifier.clone(), |resolution| resolution.url);
            Some(lumen::ModuleFetchResult { key, source: source.into(), script_context: request.script_context })
        }));
        eval(&mut engine, "CSS.animationWorklet.addModule('animator.js',{credentials:'include'})");
        engine.ctx().drain_microtasks_for_host();
        let requests = requests.borrow();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0], ("https://example.test/modules/animator.js".into(), "https://example.test/document".into(), "include".into()));
        assert_eq!(requests[1].2, "include");
    }

    #[test]
    fn animation_worklet_scroll_geometry_and_inactive_source_drive_shared_sampling() {
        let (mut engine, realm) = setup("registerAnimator('scroll',class{animate(time,effect){effect.localTime=time}});");
        realm.set_layout_flusher(Rc::new(|session| {
            session.display_list(500, 500, crate::canvas::canvas_fallback_fonts())
                .map(|_| ()).map_err(|error| format!("{error:?}"))
        }));
        eval(&mut engine, "document.body.innerHTML='<div id=target></div><div id=scroller style=\"overflow:hidden;width:100px;height:100px\"><div style=\"height:1000px\"></div></div>';globalThis.scroller=document.getElementById('scroller');globalThis.effect=new KeyframeEffect(document.getElementById('target'),{opacity:[0,1]},{duration:1000,fill:'both'});globalThis.timeline=new ScrollTimeline({scrollSource:scroller,orientation:'block'});globalThis.animation=new WorkletAnimation('scroll',effect,timeline);animation.play();scroller.scrollTop=(scroller.scrollHeight-scroller.clientHeight)/2");
        frame(&mut engine, 1000.0);
        assert!(matches!(eval(&mut engine, "Math.abs(animation.currentTime-500)<.01 && effect.getComputedTiming().localTime===animation.currentTime && animation.startTime===0 && timeline.source===scroller && timeline.scrollSource===scroller && timeline.currentTime instanceof CSSUnitValue && Math.abs(timeline.currentTime.value-50)<.001"), Value::Bool(true)));
        realm.with_session(|session| {
            let document = session.document();
            let target = selector::query_selector(document, document.root(), "#target").unwrap().unwrap();
            assert!((session.computed_style(target).unwrap().opacity - 0.5).abs() < 0.001);
        });
        eval(&mut engine, "scroller.style.display='none'");
        frame(&mut engine, 1010.0);
        assert!(matches!(eval(&mut engine, "animation.currentTime===null && animation.startTime===null && effect.getComputedTiming().localTime===null"), Value::Bool(true)));
        eval(&mut engine, "scroller.style.display='block';scroller.scrollTop=(scroller.scrollHeight-scroller.clientHeight)*.75");
        frame(&mut engine, 1020.0);
        assert!(matches!(eval(&mut engine, "Math.abs(animation.currentTime-750)<.01 && effect.getComputedTiming().localTime===animation.currentTime"), Value::Bool(true)));
    }

    #[test]
    fn animation_worklet_registration_captures_method_and_rejects_uncloneable_options() {
        let (mut engine, _) = setup("class Animator{animate(time,effect){effect.localTime=200}}registerAnimator('captured',Animator);Animator.prototype.animate=()=>{throw Error('changed prototype')};");
        eval(&mut engine, "globalThis.effect=new KeyframeEffect(document.getElementById('target'),{opacity:[0,1]},{duration:1000});globalThis.animation=new WorkletAnimation('captured',effect);animation.play()");
        frame(&mut engine, 1000.0);
        assert!(matches!(eval(&mut engine, "effect.getComputedTiming().localTime===200"), Value::Bool(true)));
        assert!(matches!(eval(&mut engine, "(()=>{try{new WorkletAnimation('captured',effect,document.timeline,{callback(){}})}catch(error){return error.name==='DataCloneError'}return false})()"), Value::Bool(true)));
        assert!(matches!(eval(&mut engine, "(()=>{try{new WorkletAnimation('captured',new KeyframeEffect(document.getElementById('target'),[],{duration:1000})).play()}catch(error){return error.name==='InvalidStateError'}return false})()"), Value::Bool(true)));
    }
}
