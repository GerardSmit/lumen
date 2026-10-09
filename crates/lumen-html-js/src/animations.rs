//! Web Animations adapter backed by shared HTML timing, CSS parsing and the
//! retained style/cascade pipeline.
use super::*;
use crate::realm_services::RealmServices;
use lumen::embed::{Deferred, JsFunction, JsObject, JsHost};
use lumen_bind::{Class, FromArg, Host, Slot, This};
use lumen_html::animation::{
    self, CompositeMode, Direction, FillMode, Keyframe, PlaybackState, Timing,
};
use lumen_html::Document;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::{Rc, Weak};
use std::sync::Arc;

#[path = "progress_timelines.rs"]
mod progress_timelines;

/// Preserve the actual JS identity alongside the native record when a native class
/// is passed as an argument. Native constructors return a host-wrapped instance after
/// their Rust constructor returns, so the constructor's provisional `this` is not the
/// object script code receives.
struct NativeClassArgument<T> {
    native: T,
    value: Value,
}

type EffectArgument = NativeClassArgument<DomKeyframeEffect>;
type TimelineArgument = NativeClassArgument<DomDocumentTimeline>;

/// The Animation constructor tells an omitted timeline (the default document
/// timeline) apart from an explicit null (no timeline).
enum TimelineChoice {
    Default,
    Null,
    Timeline(TimelineArgument),
}

impl<'a> FromArg<'a, lumen::embed::JsHost> for TimelineChoice {
    fn from_arg(
        cx: &'a lumen::embed::ArgCx<'_>,
        value: &'a Value,
        at: Slot,
    ) -> Result<Self, Value> {
        match value {
            Value::Undefined => Ok(Self::Default),
            Value::Null => Ok(Self::Null),
            _ => TimelineArgument::from_arg(cx, value, at).map(Self::Timeline),
        }
    }

    fn from_missing(_: &'a lumen::embed::ArgCx<'_>, _: Slot) -> Result<Self, Value> {
        Ok(Self::Default)
    }
}

impl<'a, T: Class + Clone> FromArg<'a, lumen::embed::JsHost> for NativeClassArgument<T> {
    fn from_arg(
        cx: &'a lumen::embed::ArgCx<'_>,
        value: &'a Value,
        at: Slot,
    ) -> Result<Self, Value> {
        let native = <lumen::embed::JsHost as Host>::class_ref::<T>(cx, value, at)?.clone();
        Ok(Self {
            native,
            value: value.clone(),
        })
    }
}

use lumen_html::animation::progress_timelines::ProgressBinding;

#[derive(Clone)]
struct Record {

    id: u32,
    public_id: String,
    effect_id: Option<u32>,
    css_name: Option<String>,
    css_order: Option<usize>,
    realm: Weak<DomRealm>,
    timeline_realm: Weak<DomRealm>,
    timeline_id: u32,
    timeline_origin_ms: f64,
    progress_timeline: Option<ProgressBinding>,
    timeline_value: Value,
    node: Option<NodeId>,
    keyframes: Rc<[Keyframe]>,
    timing: Timing,
    easing: Rc<str>,
    composite: CompositeMode,
    start_ms: f64,
    start_time_ms: f64,
    hold_time_ms: Option<f64>,
    playback_rate: f64,
    paused_at_ms: Option<f64>,
    pending_task: Option<PlaybackState>,
    pending_task_revision: u64,
    start_resolved: bool,
    cancelled: bool,
    finish_event_fired: bool,
    retired: bool,
    event_target: DomEventTarget,
}

// Immutable payloads remain alive across the layout flush between paint passes.
// The short registry borrow ends before layout or any reentrant host callback.
struct PaintSnapshot {
    iteration_accumulate: bool,
    id: u32,
    pseudo:Option<lumen_html::css::PseudoElement>,
    transition_origin:bool,
    transition_class:bool,
    endpoint_styles:Option<(Arc<lumen_html::css::Style>,Arc<lumen_html::css::Style>)>,
    css_order: Option<usize>,
    node: Option<NodeId>,
    keyframes: Rc<[Keyframe]>,
    easing: Rc<str>,
    composite: CompositeMode,
    sample: animation::Sample,
}

#[derive(Clone)]
struct EffectRecord {
    iteration_accumulate: bool,
    id: u32,
    realm: Weak<DomRealm>,
    node: Option<NodeId>,
    pseudo:Option<lumen_html::css::PseudoElement>,
    capture_name:Option<Arc<str>>,
    keyframes: Rc<[Keyframe]>,
    timing: Timing,
    easing: Rc<str>,
    duration_auto: bool,
    fill_auto: bool,
    composite: CompositeMode,
    animation_id: Option<u32>,
}

struct CssEventState {
    owner: Weak<DomRealm>,
    node: NodeId,
    pseudo:Option<lumen_html::css::PseudoElement>,
    order: usize,
    _retention: NodeRetention,
    previous: animation::CssEventSample,
    sampled: bool,
    associated: bool,
    css_paused: bool,
}

type TransitionTarget=(usize,NodeId,Option<lumen_html::css::PseudoElement>,String);
struct TransitionRecord {
    owner:Weak<DomRealm>,
    node:NodeId,
    pseudo:Option<lumen_html::css::PseudoElement>,
    property:String,
    state:animation::TransitionState<String>,
    previous:animation::TransitionPhase,
    associated:bool,
    generation:u64,
    retention:Option<NodeRetention>,
    cancellation_elapsed:Option<f64>,
    endpoint_styles:Option<(Arc<lumen_html::css::Style>,Arc<lumen_html::css::Style>)>,
}

struct AnimationHub {
    default_realm: Weak<DomRealm>,
    next_id: u32,
    next_effect_id: u32,
    next_timeline_id: u32,
    records: BTreeMap<u32, Record>,
    effect_records: HashMap<u32, EffectRecord>,
    wrappers: HashMap<u32, WeakValue>,
    finished: HashMap<u32, FinishedPromise>,
    ready: HashMap<u32, ReadyPromise>,
    effects: HashMap<u32, WeakValue>,
    timelines: HashMap<usize, WeakValue>,
    default_timeline_ids: HashMap<usize, u32>,
    timeline_values: HashMap<u32, WeakValue>,
    overlaid_nodes: HashMap<usize, HashSet<(NodeId,Option<lumen_html::css::PseudoElement>,bool)>>,
    css_records: HashMap<(usize, NodeId, Option<lumen_html::css::PseudoElement>, usize), (String, String, u32)>,
    css_generations: HashMap<usize, (Weak<DomRealm>, u64)>,
    css_events: HashMap<u32, CssEventState>,
    transitions:HashMap<u32,TransitionRecord>,
    running_transitions:HashMap<TransitionTarget,u32>,
    completed_transitions:HashMap<TransitionTarget,String>,
    transition_snapshots:HashMap<usize,(Weak<DomRealm>,Arc<lumen_html::session::TransitionSnapshot>)>,
    transition_mutation_realms:Vec<Weak<DomRealm>>,
    transition_generation:u64,
    refreshing_transitions:bool,
    rendered_revisions: HashMap<usize, (Weak<DomRealm>, u64, u64)>,
    dispatching_css: HashMap<u32, usize>,
    css_cancellations: Vec<Weak<CssCancellation>>,
}

impl Default for AnimationHub {
    fn default() -> Self {
        Self {
            next_id: 0,
            next_effect_id: 0,
            next_timeline_id: 0,
            records: BTreeMap::new(),
            effect_records: HashMap::new(),
            wrappers: HashMap::new(),
            finished: HashMap::new(),
            ready: HashMap::new(),
            effects: HashMap::new(),
            timelines: HashMap::new(),
            default_timeline_ids: HashMap::new(),
            timeline_values: HashMap::new(),
            overlaid_nodes: HashMap::new(),
            css_records: HashMap::new(),
            css_generations: HashMap::new(),
            css_events: HashMap::new(),
            transitions:HashMap::new(),running_transitions:HashMap::new(),
            completed_transitions:HashMap::new(),transition_snapshots:HashMap::new(),
            transition_mutation_realms:Vec::new(),
            transition_generation:0,refreshing_transitions:false,
            rendered_revisions: HashMap::new(),
            dispatching_css: HashMap::new(),
            css_cancellations: Vec::new(),
            default_realm: Weak::new(),
        }
    }
}

struct FinishedPromise {
    promise: WeakValue,
    deferred: Option<Deferred>,
}

struct ReadyPromise {
    promise: WeakValue,
    deferred: Option<Deferred>,
}

fn replace_finished_promise(ctx: &mut Ctx, hub: &Rc<RefCell<AnimationHub>>, id: u32) -> (Value, Option<FinishedPromise>) {
    Deferred::new_registered(ctx, |ctx, deferred| {
        let value = deferred.promise();
        let promise = ctx.weak_value(&value).expect("Promise is a GC object");
        let previous = hub.borrow_mut().finished.insert(id, FinishedPromise { promise, deferred: Some(deferred) });
        (value, previous)
    })
}

fn replace_resolved_ready(ctx: &mut Ctx, hub: &Rc<RefCell<AnimationHub>>, id: u32, value: Value) -> Value {
    let (deferred, promise) = Deferred::new_registered(ctx, |ctx, deferred| {
        let promise = deferred.promise();
        hub.borrow_mut().ready.insert(id, ReadyPromise {
            promise: ctx.weak_value(&promise).expect("Promise is a GC object"),
            deferred: None,
        });
        (deferred, promise)
    });
    deferred.resolve(ctx, value);
    promise
}

fn hub(ctx: &mut Ctx) -> OpResult<Rc<RefCell<AnimationHub>>> {
    RealmServices::<RefCell<AnimationHub>>::current(ctx)
        .ok_or_else(|| OpError::new("Error", "Web Animations are not installed"))
}

pub fn install(ctx: &mut Ctx, realm: &Rc<DomRealm>) {
    if let Some(hub)=RealmServices::<RefCell<AnimationHub>>::current(ctx) {
        let previous=hub.borrow().default_realm.upgrade();
        if let Some(previous)=previous.filter(|previous|!Rc::ptr_eq(previous,realm)) {
            if let Err(error)=retire_document(ctx,&previous) {
                let exception=error.to_value(ctx);DomRealm::report_exception(ctx,exception);
            }
        }
        hub.borrow_mut().default_realm=Rc::downgrade(realm);
    }else {
        let mut hub=AnimationHub::default();hub.default_realm=Rc::downgrade(realm);
        RealmServices::replace_current(ctx,RefCell::new(hub));
    }
    ctx.class_constructor::<DomAnimation>();
    ctx.class_constructor::<DomCssAnimation>();
    ctx.class_constructor::<DomCssTransition>();
    ctx.class_constructor::<DomKeyframeEffect>();
    ctx.class_constructor::<DomDocumentTimeline>();
    let global = ctx.global_object();
    let constructor = ctx.class_constructor::<progress_timelines::DomScrollTimeline>();
    assert!(crate::install_interface(ctx, &global, "ScrollTimeline", constructor).is_ok(), "ScrollTimeline install");
    let constructor = ctx.class_constructor::<progress_timelines::DomViewTimeline>();
    assert!(crate::install_interface(ctx, &global, "ViewTimeline", constructor).is_ok(), "ViewTimeline install");
    let constructor = ctx.class_constructor::<DomAnimationEvent>();
    let global = ctx.global_object();
    assert!(crate::install_interface(ctx, &global, "AnimationEvent", constructor).is_ok(),
        "AnimationEvent interface install");
    let constructor=ctx.class_constructor::<DomTransitionEvent>();
    assert!(crate::install_interface(ctx,&global,"TransitionEvent",constructor).is_ok(),"TransitionEvent interface install");
}

/// Retirement removes actual CSS document ownership, while author-retained
/// Animation/KeyframeEffect/Timeline identities remain in their original hub.
pub(crate) fn retire_document(ctx:&mut Ctx,realm:&Rc<DomRealm>)->OpResult<()> {
    crate::paint_worklet::retire_document(ctx,realm)?;
    super::animation_worklet::retire_document(ctx, realm)?;
    let Some(hub)=RealmServices::<RefCell<AnimationHub>>::current(ctx) else{return Ok(());};
    let key=Rc::as_ptr(realm) as usize;
    let ids=hub.borrow().records.values().filter(|record|
        record.realm.upgrade().is_some_and(|owner|Rc::ptr_eq(&owner,realm))
            && (record.css_name.is_some() || hub.borrow().transitions.contains_key(&record.id)))
        .map(|record|record.id).collect::<Vec<_>>();
    for id in ids {
        reset_pending_task(ctx,&hub,id)?;
        let mut state=hub.borrow_mut();
        if let Some(record)=state.records.get_mut(&id) {
            record.cancelled=true;record.start_resolved=false;record.hold_time_ms=None;record.paused_at_ms=None;
        }
        if let Some(event)=state.css_events.get_mut(&id) {event.associated=false;event.previous=animation::CssEventSample::IDLE;}
        if let Some(event)=state.transitions.get_mut(&id) {
            event.associated=false;event.previous=animation::TransitionPhase::Idle;
            event.cancellation_elapsed=None;event.retention=None;
        }
        drop(state);reject_finished(ctx,&hub,id);
    }
    let mut state=hub.borrow_mut();
    state.css_records.retain(|target,_|target.0!=key);
    state.running_transitions.retain(|target,_|target.0!=key);
    state.completed_transitions.retain(|target,_|target.0!=key);
    state.transition_snapshots.remove(&key);state.css_generations.remove(&key);state.rendered_revisions.remove(&key);
    // Frozen cancellation batches from a real retired document cannot retarget
    // a successor Document merely because its Window/global was reused.
    state.css_cancellations.retain(|snapshot|snapshot.upgrade().is_some_and(|snapshot|!Rc::ptr_eq(&snapshot.owner,realm)));
    Ok(())
}

fn normalize_effect_options(ctx: &mut Ctx, options: Option<Value>) -> OpResult<Option<Value>> {
    let Some(options) = options.filter(|value| !matches!(value, Value::Null | Value::Undefined))
    else {
        return Ok(None);
    };
    if matches!(options, Value::Obj(_)) {
        return Ok(Some(options));
    }
    let duration = ctx.coerce_number(&options).map_err(OpError::thrown)?;
    let object = ctx.new_object_with_proto(&Value::Null);
    ctx.set_member(&object, "duration", Value::Num(duration))
        .map_err(|error| OpError::thrown(lumen::embed::abrupt_value(error)))?;
    Ok(Some(object))
}

fn parse_options(ctx: &mut Ctx, options: Option<&Value>) -> OpResult<(Timing, String, bool, bool)> {
    let Some(options) = options.filter(|value| !matches!(value, Value::Null | Value::Undefined))
    else {
        return Ok((Timing::default(), "linear".into(), true, true));
    };
    let number = |ctx: &mut Ctx, name: &str, default: f64| -> OpResult<f64> {
        let value = ctx
            .get_member(options, name)
            .map_err(|error| OpError::thrown(lumen::embed::abrupt_value(error)))?;
        if matches!(value, Value::Undefined) {
            Ok(default)
        } else {
            ctx.coerce_number(&value).map_err(OpError::thrown)
        }
    };
    let string = |ctx: &mut Ctx, name: &str, default: &str| -> OpResult<String> {
        let value = ctx
            .get_member(options, name)
            .map_err(|error| OpError::thrown(lumen::embed::abrupt_value(error)))?;
        if matches!(value, Value::Undefined) {
            Ok(default.to_owned())
        } else {
            Ok(ctx
                .coerce_string(&value)
                .map_err(OpError::thrown)?
                .to_string())
        }
    };
    let duration_value = ctx
        .get_member(options, "duration")
        .map_err(|error| OpError::thrown(lumen::embed::abrupt_value(error)))?;
    // Web IDL selects the numeric union member only for a Number. All other
    // values use DOMString conversion, whose sole accepted value is "auto".
    let (duration_ms, duration_auto) = match duration_value {
        Value::Undefined => (0.0, true),
        Value::Num(value) => (value, false),
        value => {
            let value = ctx.coerce_string(&value).map_err(OpError::thrown)?;
            if value.as_ref() != "auto" {
                return Err(OpError::new(
                    "TypeError",
                    "animation duration string must be auto",
                ));
            }
            (0.0, true)
        }
    };
    if duration_ms.is_nan() || duration_ms < 0.0 {
        return Err(OpError::new(
            "TypeError",
            "animation duration must be non-negative",
        ));
    }
    let iterations = number(ctx, "iterations", 1.0)?;
    if iterations.is_nan() || iterations < 0.0 {
        return Err(OpError::new(
            "TypeError",
            "animation iterations must be non-negative",
        ));
    }
    let iteration_start = number(ctx, "iterationStart", 0.0)?;
    let delay_ms = number(ctx, "delay", 0.0)?;
    let end_delay_ms = number(ctx, "endDelay", 0.0)?;
    if !iteration_start.is_finite()
        || iteration_start < 0.0
        || !delay_ms.is_finite()
        || !end_delay_ms.is_finite()
    {
        return Err(OpError::new("TypeError", "invalid animation timing value"));
    }
    let specified_fill = string(ctx, "fill", "auto")?;
    let fill_auto = specified_fill == "auto";
    let fill = match specified_fill.as_str() {
        "auto" | "none" => FillMode::None,
        "forwards" => FillMode::Forwards,
        "backwards" => FillMode::Backwards,
        "both" => FillMode::Both,
        _ => return Err(OpError::new("TypeError", "invalid animation fill mode")),
    };
    let direction = match string(ctx, "direction", "normal")?.as_str() {
        "normal" => Direction::Normal,
        "reverse" => Direction::Reverse,
        "alternate" => Direction::Alternate,
        "alternate-reverse" => Direction::AlternateReverse,
        _ => return Err(OpError::new("TypeError", "invalid animation direction")),
    };
    let easing = string(ctx, "easing", "linear")?;
    if animation::ease(&easing, 0.5).is_none() {
        return Err(OpError::new(
            "TypeError",
            "unsupported animation easing function",
        ));
    }
    let timing = Timing {
        delay_ms,
        end_delay_ms,
        duration_ms,
        iteration_start,
        iterations,
        fill,
        direction,
    };
    Ok((timing, easing, duration_auto, fill_auto))
}

fn css_property_name(property: &str) -> String {
    if property.starts_with("--") {return property.to_owned();}
    if property=="cssFloat" {return String::from("float");}
    if property=="cssOffset" {return String::from("offset");}
    let mut name = String::with_capacity(property.len());
    for ch in property.chars() {
        if ch.is_ascii_uppercase() {
            name.push('-');
            name.push(ch.to_ascii_lowercase());
        } else {
            name.push(ch);
        }
    }
    name
}

fn parse_keyframe_composite(value: &str) -> OpResult<Option<CompositeMode>> {
    match value {
        "auto" => Ok(None),
        "replace" => Ok(Some(CompositeMode::Replace)),
        "add" => Ok(Some(CompositeMode::Add)),
        "accumulate" => Ok(Some(CompositeMode::Accumulate)),
        _ => Err(OpError::new("TypeError", "invalid keyframe composite mode")),
    }
}

fn keyframe_iterator_method(ctx: &mut Ctx, value: &Value) -> OpResult<Option<Value>> {
    if !matches!(value, Value::Obj(_)) { return Ok(None); }
    let key = ctx.well_known_symbol("iterator").expect("installed iterator symbol");
    let method = ctx.reflect_get(value, &key, value).map_err(OpError::thrown)?;
    Ok((!matches!(method, Value::Undefined | Value::Null)).then_some(method))
}

fn animatable_keyframe_property(name: &str) -> Option<String> {
    if !name.starts_with("--") && (name.contains('-') || matches!(name,"float"|"offset")) { return None; }
    let property = css_property_name(name);
    let longhands = lumen_html::css::declaration_block::longhands(&property);
    let animatable = if longhands.is_empty() {
        lumen_html::css::transition_value_kind(&property) != lumen_html::css::TransitionValueKind::NotAnimatable
    } else {
        longhands.iter().any(|name| lumen_html::css::transition_value_kind(name) != lumen_html::css::TransitionValueKind::NotAnimatable)
    };
    animatable.then_some(property)
}

fn canonical_keyframe_easing(value: &str) -> OpResult<String> {
    let value=lumen_html::css::serialize_cssom_property_value("animation-timing-function",value)
        .filter(|value|animation::ease(value,0.5).is_some())
        .ok_or_else(||OpError::new("TypeError","unsupported keyframe easing function"))?;
    Ok(value)
}

fn parse_frame(ctx: &mut Ctx, object: &Value, offset: f64) -> OpResult<Keyframe> {
    // BaseKeyframe dictionary conversion precedes enumerable property reads.
    let composite = ctx.member_get(object,"composite").map_err(OpError::thrown)?;
    let composite = if matches!(composite,Value::Undefined) {None} else {
        parse_keyframe_composite(&ctx.coerce_string(&composite).map_err(OpError::thrown)?)?
    };
    let easing = ctx.member_get(object,"easing").map_err(OpError::thrown)?;
    let easing = if matches!(easing,Value::Undefined) {None} else {
        Some(ctx.coerce_string(&easing).map_err(OpError::thrown)?.to_string())
    };
    let authored = ctx.member_get(object,"offset").map_err(OpError::thrown)?;
    let explicit_offset = if matches!(authored,Value::Null|Value::Undefined) {None} else {
        let offset=ctx.coerce_number(&authored).map_err(OpError::thrown)?;
        if !offset.is_finite() {return Err(OpError::new("TypeError","keyframe offset must be finite"));}
        Some(offset)
    };
    let mut declarations=Vec::new();
    for key in object_keys(ctx,object)? {
        let Some(property)=animatable_keyframe_property(&key) else {continue;};
        let value=ctx.member_get(object,&key).map_err(OpError::thrown)?;
        let value=ctx.coerce_string(&value).map_err(OpError::thrown)?.to_string();
        declarations.push((property,value));
    }
    Ok(Keyframe{offset:explicit_offset.unwrap_or(offset),offset_is_specified:explicit_offset.is_some(),declarations,easing,composite})
}

fn object_keys(ctx: &mut Ctx, object: &Value) -> OpResult<Vec<String>> {
    let mut names=Vec::new();
    for key in ctx.reflect_own_keys(object).map_err(OpError::thrown)? {
        let Value::Str(name)=&key else {continue;};
        let descriptor=ctx.reflect_get_own_property_descriptor(object,&key).map_err(OpError::thrown)?;
        if matches!(descriptor,Value::Undefined) {continue;}
        if !matches!(ctx.member_get(&descriptor,"enumerable").map_err(OpError::thrown)?,Value::Bool(true)) {continue;}
        if names.len()==512 {return Err(OpError::new("RangeError","keyframe property count exceeds limit"));}
        names.push(name.to_string());
    }
    names.sort();
    Ok(names)
}

fn values(ctx: &mut Ctx, value: &Value) -> OpResult<Vec<Value>> {
    if let Some(method) = keyframe_iterator_method(ctx,value)? {
        return ctx.convert_iterable_with_method(value,method,512,|_,value|Ok(value));
    }
    Ok(vec![value.clone()])
}

fn property_indexed_keyframes(ctx: &mut Ctx, object: &Value) -> OpResult<Vec<Keyframe>> {
    let mut properties = Vec::<(String, Vec<Value>)>::new();
    let mut offset_values = Vec::new();
    let mut easing_values = Vec::new();
    let mut composite_values = Vec::new();
    for key in ["composite", "easing", "offset"] {
        let value=ctx.member_get(object,key).map_err(OpError::thrown)?;
        if matches!(value,Value::Undefined) {continue;}
        let converted=values(ctx,&value)?.into_iter().map(|value| {
            if key=="offset" {
                if matches!(value,Value::Null|Value::Undefined) {return Ok(Value::Null);}
                let value=ctx.coerce_number(&value).map_err(OpError::thrown)?;
                if !value.is_finite(){return Err(OpError::new("TypeError","keyframe offset must be finite"));}
                Ok(Value::Num(value))
            } else {
                let value=ctx.coerce_string(&value).map_err(OpError::thrown)?;
                if key=="composite" {parse_keyframe_composite(&value)?;}
                Ok(Value::Str(value.into()))
            }
        }).collect::<OpResult<Vec<_>>>()?;
        match key {"offset"=>offset_values=converted,"easing"=>easing_values=converted,_=>composite_values=converted}
    }
    for key in object_keys(ctx, object)? {
        if matches!(key.as_str(),"offset"|"easing"|"composite") {continue;}
        let Some(property)=animatable_keyframe_property(&key) else {continue;};
        let value = ctx
            .member_get(object, &key)
            .map_err(OpError::thrown)?;
        let values=values(ctx,&value)?.into_iter().map(|value|ctx.coerce_string(&value).map(|value|Value::Str(value.into())).map_err(OpError::thrown)).collect::<OpResult<Vec<_>>>()?;
        properties.push((property,values));
    }
    // Web Animations §6.6.3: distribute each property's own sequence,
    // then merge equal offsets. Metadata cannot manufacture keyframes.
    let mut frames = Vec::<Keyframe>::new();
    for (property, property_values) in properties {
        let count = property_values.len();
        for (index, value) in property_values.into_iter().enumerate() {
            let value = ctx
                .coerce_string(&value)
                .map_err(OpError::thrown)?
                .to_string();
            let offset = if count == 1 { 1.0 } else { index as f64 / (count - 1) as f64 };
            let at = match frames.binary_search_by(|frame| frame.offset.total_cmp(&offset)) {
                Ok(at) => at,
                Err(at) => {
                    if frames.len() >= 512 {
                        return Err(OpError::new("RangeError", "keyframe count exceeds limit"));
                    }
                    frames.try_reserve(1).map_err(|_| OpError::new("RangeError", "keyframe storage exceeds limit"))?;
                    frames.insert(at, Keyframe { offset_is_specified: false, offset, declarations: Vec::new(), easing: None, composite: None });
                    at
                }
            };
            frames[at].declarations.push((property.clone(), value));
        }
    }
    let count = frames.len();
    // Authored offsets replace the preliminary offsets used to merge tracks;
    // missing offsets are computed again on the resulting frame sequence.
    for frame in &mut frames { frame.offset = f64::NAN; }
    for (index, value) in offset_values.into_iter().take(count).enumerate() {
        if !matches!(value, Value::Undefined | Value::Null) {
            let offset = ctx.coerce_number(&value).map_err(OpError::thrown)?;
            if !offset.is_finite() || !(0.0..=1.0).contains(&offset) {
                return Err(OpError::new(
                    "TypeError",
                    "keyframe offset must be between zero and one",
                ));
            }
            frames[index].offset = offset;
            frames[index].offset_is_specified = true;
        }
    }
    let easings = easing_values.into_iter().map(|value| {
        let easing = ctx
                .coerce_string(&value)
                .map_err(OpError::thrown)?
                .to_string();
        canonical_keyframe_easing(&easing)
    }).collect::<OpResult<Vec<_>>>()?;
    if !easings.is_empty() {
        for (index, frame) in frames.iter_mut().enumerate() {
            frame.easing = Some(easings[index % easings.len()].clone());
        }
    }
    let composites = composite_values.into_iter().map(|value| {
        let value = ctx
                .coerce_string(&value)
                .map_err(OpError::thrown)?
                .to_string();
        parse_keyframe_composite(&value)
    }).collect::<OpResult<Vec<_>>>()?;
    if !composites.is_empty() {
        for (index, frame) in frames.iter_mut().enumerate() {
            frame.composite = composites[index % composites.len()];
        }
    }
    normalize_offsets(&mut frames)?;
    Ok(frames)
}

fn normalize_offsets(frames: &mut [Keyframe]) -> OpResult<()> {
    let mut previous = None;
    for frame in frames.iter() {
        if frame.offset.is_finite() {
            if !(0.0..=1.0).contains(&frame.offset) {return Err(OpError::new("TypeError","keyframe offset must be between zero and one"));}
            if previous.is_some_and(|previous| frame.offset < previous) {
                return Err(OpError::new(
                    "TypeError",
                    "keyframe offsets must be increasing",
                ));
            }
            previous = Some(frame.offset);
        }
    }
    if frames.is_empty() {
        return Ok(());
    }
    if frames.len() > 1 && frames[0].offset.is_nan() {
        frames[0].offset = 0.0;
    }
    let last = frames.len() - 1;
    if frames[last].offset.is_nan() {
        frames[last].offset = 1.0;
    }
    let mut left = 0;
    while left < last {
        let Some(right) = (left + 1..frames.len()).find(|index| frames[*index].offset.is_finite())
        else {
            break;
        };
        let start = frames[left].offset;
        let end = frames[right].offset;
        let span = (right - left) as f64;
        for index in left + 1..right {
            frames[index].offset = start + (end - start) * ((index - left) as f64 / span);
        }
        left = right;
    }
    frames.sort_by(|a, b| a.offset.total_cmp(&b.offset));
    Ok(())
}

fn parse_keyframes(ctx: &mut Ctx, value: &Value) -> OpResult<Vec<Keyframe>> {
    if matches!(value, Value::Null | Value::Undefined) {
        return Ok(Vec::new());
    }
    if let Some(method) = keyframe_iterator_method(ctx,value)? {
        let mut frames = ctx.convert_iterable_with_method(value,method,512,|ctx,object| {
            if !matches!(object, Value::Obj(_) | Value::Null | Value::Undefined) {
                return Err(OpError::new("TypeError", "keyframe must be an object"));
            }
            if matches!(object,Value::Null | Value::Undefined) {
                return Ok(Keyframe{offset:f64::NAN,offset_is_specified:false,declarations:Vec::new(),easing:None,composite:None});
            }
            parse_frame(ctx,&object,f64::NAN)
        })?;
        normalize_offsets(&mut frames)?;
        for frame in &mut frames {
            if let Some(value)=frame.easing.as_deref() {frame.easing=Some(canonical_keyframe_easing(value)?);}
        }
        discard_invalid_keyframe_values(&mut frames);
        Ok(frames)
    } else if matches!(value, Value::Obj(_)) {
        let mut frames = property_indexed_keyframes(ctx, value)?;
        discard_invalid_keyframe_values(&mut frames);
        Ok(frames)
    } else {
        Err(OpError::new(
            "TypeError",
            "keyframes must be an object or sequence",
        ))
    }
}

fn discard_invalid_keyframe_values(frames: &mut [Keyframe]) {
    for frame in frames {
        frame.declarations.retain_mut(|(property,value)| {
            if property.starts_with("--") {return lumen_html::css::supports_property_value(property,value);}
            let mut block=lumen_html::css::declaration_block::DeclarationBlock::default();
            if block.set(property,value,false).is_err() {return false;}
            let Some((canonical,_))=block.value(property) else {return false;};
            *value=lumen_html::css::serialize_cssom_property_value(property,&canonical).unwrap_or(canonical);
            true
        });
    }
}

fn base_css_value(style:&lumen_html::css::Style,name:&str)->Option<String> {
    style.computed_css_value(name,lumen_html::css::computed_values::ComputedValueContext::default())
}

fn interpolate_animation_value(from: &str, to: &str, progress: f64) -> Option<String> {
    animation::interpolate_numeric(from, to, progress).or_else(|| {
        let current=lumen_common::color::Color::rgba8([0,0,0,255]);
        let from=lumen_html::css::parse_animation_unclipped_color(from,current)?;
        let to=lumen_html::css::parse_animation_unclipped_color(to,current)?;
        lumen_html::css::interpolate_source_colors(&from,&to,progress as f32).serialize()
    })
}

fn composite_value(
    _property: &str,
    base: &str,
    value: &str,
    iteration_delta: Option<(&str, u64)>,
) -> Option<String> {
    // CSS Values defines nonadditive values, including colors and shadows,
    // to compose as V_B. Numeric values keep their specified addition.
    animation::add_numeric_values(base, value, iteration_delta)
        .or_else(|| Some(value.to_owned()))
}

// Iteration accumulation has the final keyframe as V_A and the composed
// interval endpoint as V_B. Unsupported/nonadditive animation types return V_B.
// Binary accumulation keeps even large iteration counts logarithmic.
fn accumulate_iteration_endpoint(property:&str,final_value:&str,value:&str,iterations:f64,
    context:Option<&(lumen_html::css::Style,lumen_html::paint::Rect,lumen_html::css::MediaEnvironment)>,
    text:Option<&dyn lumen_html::paint::TextShaper>,query:lumen_html::css::ContainerUnitContext)->Option<String> {
    if matches!(property,"box-shadow"|"text-shadow") || lumen_html::css::animation_color_property(property) {
        return Some(value.to_owned());
    }
    if matches!(property,"translate"|"rotate"|"scale"){
        let(style,_,viewport)=context?;
        return lumen_html::css::combine_animation_individual_transform(property,final_value,value,lumen_html::css::registered_properties::ComputedValueOperation::Accumulate(iterations),style,*viewport,text,query);
    }
    if let Some(value)=animation::combine_numeric_values(&[(final_value,iterations),(value,1.0)]) {return Some(value);}
    if !matches!(property,"transform"|"border-image-slice"|"border-image-width"|"border-image-outset") {
        return Some(value.to_owned());
    }
    if !iterations.is_finite(){return None;}
    let(style,rect,viewport)=context?;
    if property=="transform"&&(lumen_html::css::animation_transform_has_deferred_source(final_value)||lumen_html::css::animation_transform_has_deferred_source(value)) {
        return lumen_html::css::combine_animation_source_transforms(final_value,value,lumen_html::css::registered_properties::ComputedValueOperation::Accumulate(iterations),style,*viewport,text,[rect.width as f64,rect.height as f64]);
    }
    let combine=|a:&str,b:&str| {
        if property=="transform" {
            let a=lumen_html::css::parse_animation_transforms(a,style,*viewport,text)?;
            let b=lumen_html::css::parse_animation_transforms(b,style,*viewport,text)?;
            animation::accumulate_transforms(&a,&b,rect.width,rect.height)
        }else{
            let a=lumen_html::css::parse_animation_border_image_with_query(property,a,style,*viewport,text,query)?;
            let b=lumen_html::css::parse_animation_border_image_with_query(property,b,style,*viewport,text,query)?;
            animation::transition_values::compose_border_image(property,&a,&b)
        }
    };
    let mut result=value.to_owned();let mut power=final_value.to_owned();let mut count=iterations.floor();
    while count>=1.0 {
        if count%2.0>=1.0 {result=combine(&power,&result)?;}
        count=(count/2.0).floor();
        if count>=1.0 {power=combine(&power,&power)?;}
    }
    Some(result)
}

// Parsed declarations retain their authored computed-value dependencies.
// This allocation-free check only admits the live color source when sampling
// needs it; no style or per-property base map is acquired at effect creation.
fn keyframes_need_color_context(frames:&[Keyframe])->bool {
    frames.iter().any(|frame|frame.declarations.iter().any(|(property,value)|
        lumen_html::css::animation_color_property(property)
            && lumen_html::css::animation_color_needs_context(value)))
}

fn parse_composite(ctx: &mut Ctx, options: Option<&Value>) -> OpResult<CompositeMode> {
    let Some(options) = options.filter(|value| !matches!(value, Value::Null | Value::Undefined))
    else {
        return Ok(CompositeMode::Replace);
    };
    let value = ctx
        .get_member(options, "composite")
        .map_err(|_| OpError::new("TypeError", "composite getter failed"))?;
    if matches!(value, Value::Undefined) {
        return Ok(CompositeMode::Replace);
    }
    match ctx.coerce_string(&value).map_err(OpError::thrown)?.as_ref() {
        "replace" => Ok(CompositeMode::Replace),
        "add" => Ok(CompositeMode::Add),
        "accumulate" => Ok(CompositeMode::Accumulate),
        _ => Err(OpError::new(
            "TypeError",
            "invalid animation composite mode",
        )),
    }
}

fn parse_iteration_composite(ctx:&mut Ctx,options:Option<&Value>)->OpResult<bool> {
    let Some(options)=options.filter(|value|!matches!(value,Value::Null|Value::Undefined)) else{return Ok(false);};
    let value=ctx.member_get(options,"iterationComposite").map_err(OpError::thrown)?;
    if matches!(value,Value::Undefined){return Ok(false);}
    iteration_composite_value(&ctx.coerce_string(&value).map_err(OpError::thrown)?)
}
fn iteration_composite_value(value:&str)->OpResult<bool> {
    match value {"replace"=>Ok(false),"accumulate"=>Ok(true),_=>Err(OpError::new("TypeError","invalid iteration composite operation"))}
}

fn parse_effect_pseudo(value:&str)->OpResult<Option<lumen_html::css::PseudoElement>> {
    use lumen_html::css::PseudoElement::*;
    Ok(match value.trim().to_ascii_lowercase().as_str() {
        ""=>None,"::before" | ":before"=>Some(Before),"::after" | ":after"=>Some(After),"::marker"=>Some(Marker),
        _=>return Err(OpError::new("SyntaxError","invalid animation pseudo-element")),
    })
}

fn create_effect(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    node: Option<NodeId>,
    keyframes: Value,
    options: Option<Value>,
) -> OpResult<DomKeyframeEffect> {
    let options = normalize_effect_options(ctx, options)?;
    if node.is_some_and(|node| {
        !matches!(
            realm.session.borrow().document().kind(node),
            Ok(NodeKind::Element { .. })
        )
    }) {
        return Err(OpError::new(
            "TypeError",
            "KeyframeEffect target must be an element",
        ));
    }
    let pseudo=match crate::ui_events::dictionary_member(ctx,&options,"pseudoElement")? {
        None | Some(Value::Null) | Some(Value::Undefined)=>None,
        Some(value)=>parse_effect_pseudo(&ctx.coerce_string(&value).map_err(OpError::thrown)?)?,
    };
    let frames = parse_keyframes(ctx, &keyframes)?;
    let composite = parse_composite(ctx, options.as_ref())?;
    let iteration_accumulate=parse_iteration_composite(ctx,options.as_ref())?;
    let (timing, easing, duration_auto, fill_auto) = parse_options(ctx, options.as_ref())?;
    let hub = hub(ctx)?;
    let effect_id = {
        let mut state = hub.borrow_mut();
        state.next_effect_id = state.next_effect_id.wrapping_add(1).max(1);
        let id = state.next_effect_id;
        state.effect_records.insert(
            id,
            EffectRecord {
                iteration_accumulate,
                id,
                animation_id: None,
                realm: Rc::downgrade(realm),
                node,pseudo,capture_name:None,
                keyframes: frames.into(),
                timing,
                easing: easing.into(),
                duration_auto,
                fill_auto,
                composite,
            },
        );
        id
    };
    Ok(DomKeyframeEffect { id: effect_id })
}

fn attach_effect(
    ctx: &mut Ctx,
    effect_id: u32,
    start_playing: bool,
    timeline_id: u32,
    timeline_realm: &Rc<DomRealm>,
    timeline_origin_ms: f64,
    timeline_value: Value,
) -> OpResult<(u32, DomEventTarget)> {
    attach_effect_with_css_identity(ctx,effect_id,start_playing,timeline_id,
        timeline_realm,timeline_origin_ms,timeline_value,None)
}

fn attach_effect_with_css_identity(
    ctx:&mut Ctx,effect_id:u32,start_playing:bool,timeline_id:u32,
    timeline_realm:&Rc<DomRealm>,timeline_origin_ms:f64,timeline_value:Value,
    css_identity:Option<(String,usize)>,
) ->OpResult<(u32,DomEventTarget)> {
    let hub = hub(ctx)?;
    let now = document_time(timeline_realm).unwrap_or(0.0);
    let displaced=hub.borrow().effect_records.get(&effect_id).and_then(|effect|effect.animation_id);
    let (id, event_target, realms) = {
        let mut state = hub.borrow_mut();
        let effect = state
            .effect_records
            .get(&effect_id)
            .cloned()
            .ok_or_else(|| {
                OpError::new(
                    "InvalidStateError",
                    "keyframe effect is no longer available",
                )
            })?;
        let mut realms = Vec::new();
        if let Some(previous_id) = effect.animation_id {
            if let Some(previous) = state.records.get_mut(&previous_id) {
                previous.effect_id = None;
                if let Some(realm) = previous.realm.upgrade() {
                    realms.push(realm);
                }
            }
        }
        state.next_id = state.next_id.wrapping_add(1).max(1);
        let id = state.next_id;
        let event_target =
            DomEventTarget::independent(&effect.realm.upgrade().ok_or_else(|| {
                OpError::new("InvalidStateError", "effect target document was destroyed")
            })?);
        state.records.insert(
            id,
            Record {
                id,
                public_id: String::new(),
                effect_id: Some(effect_id),
                css_name: css_identity.as_ref().map(|(name,_)|name.clone()),
                css_order: css_identity.as_ref().map(|(_,order)|*order),
                realm: effect.realm.clone(),
                timeline_realm: Rc::downgrade(timeline_realm),
                timeline_id,
                timeline_origin_ms,
                progress_timeline: None,
                timeline_value,
                node: effect.node,
                keyframes: effect.keyframes.clone(),
                timing: effect.timing,
                easing: effect.easing.clone(),
                composite: effect.composite,
                start_ms: now,
                start_time_ms: 0.0,
                hold_time_ms: start_playing.then_some(0.0),
                playback_rate: 1.0,
                paused_at_ms: None,
                pending_task: start_playing.then_some(PlaybackState::Running),
                pending_task_revision: 0,
                start_resolved: false,
                cancelled: !start_playing,
                finish_event_fired: false,
                retired: false,
                event_target: event_target.clone(),
            },
        );
        state
            .effect_records
            .get_mut(&effect_id)
            .ok_or_else(|| OpError::new("InvalidStateError", "keyframe effect is unavailable"))?
            .animation_id = Some(id);
        if let Some(realm) = effect.realm.upgrade() {
            realms.push(realm);
        }
        (id, event_target, realms)
    };
    if let Some(displaced)=displaced {update_live_animation_effect_owner(ctx,&hub,displaced)?;}
    mark_ready_pending(ctx, &hub, id);
    ensure_pending_finished(ctx, &hub, id);
    let _ = start_playing;
    for realm in realms {
        apply_record_change(&hub, &realm, now, id)?;
    }
    Ok((id, event_target))
}

pub fn animate(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    node: NodeId,
    keyframes: Value,
    options: Option<Value>,
) -> OpResult<Value> {
    if !matches!(
        realm.session.borrow().document().kind(node),
        Ok(NodeKind::Element { .. })
    ) {
        return Err(OpError::new(
            "TypeError",
            "Element.animate requires an element",
        ));
    }
    let mut public_id = String::new();
    let mut selected_timeline: Option<Option<TimelineArgument>> = None;
    if let Some(options) = options
        .as_ref()
        .filter(|value| matches!(value, Value::Obj(_)))
    {
        let id = ctx
            .get_member(options, "id")
            .map_err(|error| OpError::thrown(lumen::embed::abrupt_value(error)))?;
        if !matches!(id, Value::Undefined) {
            public_id = ctx.coerce_string(&id).map_err(OpError::thrown)?.to_string();
        }
        let timeline = ctx
            .get_member(options, "timeline")
            .map_err(|error| OpError::thrown(lumen::embed::abrupt_value(error)))?;
        selected_timeline = match timeline {
            Value::Undefined => None,
            Value::Null => Some(None),
            value => {
                let native = ctx
                    .with_instance::<DomDocumentTimeline, _>(&value, |timeline| timeline.clone())?;
                Some(Some(TimelineArgument { native, value }))
            }
        };
    }
    let effect = create_effect(ctx, realm, Some(node), keyframes, options)?;
    let effect_id = effect.id;
    let timeline_value = document_timeline(ctx, realm)?;
    let timeline_id = hub(ctx)?
        .borrow()
        .default_timeline_ids
        .get(&(Rc::as_ptr(realm) as usize))
        .copied()
        .ok_or_else(|| OpError::new("InvalidStateError", "default timeline was not registered"))?;
    let (id, event_target) = attach_effect(
        ctx,
        effect_id,
        true,
        timeline_id,
        realm,
        0.0,
        timeline_value,
    )?;
    let hub = hub(ctx)?;
    hub.borrow_mut().records.get_mut(&id).unwrap().public_id = public_id;
    let animation = DomAnimation {
        id,
        base: event_target,
    };
    if let Some(timeline) = selected_timeline {
        animation.set_timeline(ctx, timeline)?;
    }
    let wrapper = ctx.new_instance(animation);
    if let Some(weak) = ctx.weak_value(&wrapper) {
        hub.borrow_mut().wrappers.insert(id, weak);
    }
    publish_animation_effect_owner(ctx,&hub,id,&wrapper)?;
    // Attaching the effect publishes its sample; an explicit timeline change
    // also republishes in its setter. Wrapper ownership adds no style change.
    Ok(wrapper)
}

pub fn for_element(ctx: &mut Ctx, realm: &Rc<DomRealm>, node: NodeId, subtree: bool) -> OpResult<Value> {
    let tree_order = if subtree {
        let session = realm.session.borrow();
        let document = session.document();
        let mut order = HashMap::new();
        order.insert(node, 0usize);
        let mut cursor = lumen_html::selector::next_shadow_including_descendant(document, node, node).map_err(dom_error)?;
        while let Some(id) = cursor {
            order.insert(id, order.len());
            cursor = lumen_html::selector::next_shadow_including_descendant(document, node, id).map_err(dom_error)?;
        }
        order
    } else {
        HashMap::from([(node, 0usize)])
    };
    let hub = hub(ctx)?;
    let now = document_time(realm).unwrap_or(0.0);
    refresh_css_transitions(ctx,&hub,realm,now)?;
    refresh_css_animations(ctx, &hub, realm, now)?;
    apply_realm(&hub, realm, now)?;
    collect_retired_css_animations(ctx, &hub);
    let ids=ordered_relevant_animations(&hub,realm,now,&tree_order)?;
    animation_array(ctx, &hub, &ids)
}

/// Return relevant animations whose targets are descendants of a Document or
/// ShadowRoot. The ordinary node walk deliberately stops at shadow boundaries;
/// each shadow root is queried through its own root instead.
pub fn for_tree_root(ctx: &mut Ctx, realm: &Rc<DomRealm>, root: NodeId) -> OpResult<Value> {
    let targets = {
        let session = realm.session.borrow();
        let document = session.document();
        let mut targets = HashMap::new();
        let mut node = lumen_html::selector::next_shadow_including_descendant(document, root, root).map_err(dom_error)?;
        while let Some(id) = node {
            targets.insert(id,targets.len());
            node = lumen_html::selector::next_shadow_including_descendant(document, root, id).map_err(dom_error)?;
        }
        targets
    };
    let hub = hub(ctx)?;
    let now = document_time(realm).unwrap_or(0.0);
    refresh_css_transitions(ctx,&hub,realm,now)?;
    refresh_css_animations(ctx, &hub, realm, now)?;
    apply_realm(&hub, realm, now)?;
    collect_retired_css_animations(ctx, &hub);
    let ids=ordered_relevant_animations(&hub,realm,now,&targets)?;
    animation_array(ctx, &hub, &ids)
}

fn ordered_relevant_animations(hub:&Rc<RefCell<AnimationHub>>,realm:&Rc<DomRealm>,now:f64,
    tree_order:&HashMap<NodeId,usize>)->OpResult<Vec<u32>> {
    let state=hub.borrow();
    let mut records=state.records.values().filter(|record|record.node.is_some_and(|node|tree_order.contains_key(&node))
        && record_is_relevant(record,realm,now)).collect::<Vec<_>>();
    // Effect retargeting does not change the CSS owning element. Only that
    // rare case needs an ordering walk outside the queried target subtree.
    let foreign_owner=records.iter().any(|record|state.transitions.get(&record.id).filter(|event|event.associated).map(|event|event.node)
        .or_else(||state.css_events.get(&record.id).filter(|event|event.associated).map(|event|event.node))
        .is_some_and(|node|!tree_order.contains_key(&node)));
    let full_order=if foreign_owner {Some(css_tree_order(core::iter::once(realm))?)}else{None};
    let owner_order=full_order.as_ref().and_then(|orders|orders.get(&(Rc::as_ptr(realm) as usize))).unwrap_or(tree_order);
    let order=|record:&Record| {
        let transition=state.transitions.get(&record.id);
        let css=state.css_events.get(&record.id).filter(|event|event.associated);
        let owning=transition.filter(|transition|transition.associated).map(|transition|(transition.node,transition.pseudo))
            .or_else(||css.map(|event|(event.node,event.pseudo)));
        let kind=if transition.is_some(){0u8}else if css.is_some(){1}else{2};
        (kind,owning.is_none(),owning.and_then(|(node,_)|owner_order.get(&node)).copied().unwrap_or(usize::MAX),
            pseudo_order(owning.and_then(|(_,pseudo)|pseudo)),
            transition.filter(|transition|transition.associated).map_or(0,|transition|transition.generation),
            transition.filter(|transition|transition.associated).map_or("",|transition|transition.property.as_str()),
            css.map_or(0,|event|event.order),record.id)
    };
    records.sort_by(|a,b|order(a).cmp(&order(b)));
    Ok(records.into_iter().map(|record|record.id).collect())
}

fn record_is_relevant(record: &Record, realm: &Rc<DomRealm>, now: f64) -> bool {
    record.effect_id.is_some()
        && !record.cancelled
        && record
            .realm
            .upgrade()
            .is_some_and(|candidate| Rc::ptr_eq(&candidate, realm))
        && animation::effect_is_relevant(
            record_sampling_timing(record),
            sample_record(record, now),
            record.playback_rate,
        )
}

fn css_list_value(style: &[Option<Arc<str>>; 19], property: usize, index: usize, default: &str) -> String {
    let values = style[property]
        .as_deref()
        .map(lumen_html::css::css_list_items)
        .unwrap_or_default();
    values.get(index % values.len().max(1)).cloned().unwrap_or_else(|| default.to_owned())
}

fn css_time_value(input: &str) -> Option<f64> {
    let input = input.trim();
    let (number, scale) = if let Some(number) = input.strip_suffix("ms") { (number, 1.0) }
    else if let Some(number) = input.strip_suffix('s') { (number, 1000.0) }
    else { return None; };
    let value = number.trim().parse::<f64>().ok()? * scale;
    value.is_finite().then_some(value)
}

/// Generated transition captures use the ordinary animation owner, timeline,
/// pending play/pause tasks, promises and public CSSAnimation wrappers.
pub(crate) fn create_capture_animation(ctx:&mut Ctx,realm:&Rc<DomRealm>,node:NodeId,
    pseudo:lumen_html::css::PseudoElement,capture_name:Arc<str>,name:&str,
    controls:&[Option<Arc<str>>;19],index:usize,frames:Vec<Keyframe>)
    ->OpResult<(u32,Value)> {
    let state=hub(ctx)?;
    let timeline_value=document_timeline(ctx,realm)?;
    let timeline_id=state.borrow().default_timeline_ids[&(Rc::as_ptr(realm) as usize)];
    let direction=match css_list_value(controls,5,index,"normal").as_str() {
        "reverse"=>Direction::Reverse,"alternate"=>Direction::Alternate,
        "alternate-reverse"=>Direction::AlternateReverse,_=>Direction::Normal,
    };
    let fill=match css_list_value(controls,6,index,"none").as_str() {
        "both"=>FillMode::Both,"forwards"=>FillMode::Forwards,
        "backwards"=>FillMode::Backwards,_=>FillMode::None,
    };
    let iterations=match css_list_value(controls,4,index,"1").as_str() {
        "infinite"=>f64::INFINITY,value=>value.parse::<f64>().unwrap_or(1.0).max(0.0),
    };
    let effect_id={
        let mut state=state.borrow_mut();
        state.next_effect_id=state.next_effect_id.wrapping_add(1).max(1);
        let id=state.next_effect_id;
        state.effect_records.insert(id,EffectRecord{
            id,realm:Rc::downgrade(realm),node:Some(node),pseudo:Some(pseudo),
            capture_name:Some(capture_name),keyframes:frames.into(),
            timing:Timing{delay_ms:css_time_value(&css_list_value(controls,2,index,"0s")).unwrap_or(0.0),
                end_delay_ms:0.0,duration_ms:css_time_value(&css_list_value(controls,1,index,"0s")).unwrap_or(0.0).max(0.0),
                iteration_start:0.0,iterations,fill,direction},
            easing:css_list_value(controls,3,index,"ease").into(),
            duration_auto:false,fill_auto:false,composite:CompositeMode::Replace,iteration_accumulate:false,animation_id:None,
        });
        id
    };
    let (id,_)=attach_effect_with_css_identity(ctx,effect_id,true,timeline_id,realm,0.0,
        timeline_value,Some((name.to_owned(),index)))?;
    if css_list_value(controls,7,index,"running")=="paused" {
        request_pending_task(ctx,&state,id,PlaybackState::Paused,false)?;
    }
    Ok((id,existing_wrapper(ctx,&state,id)?))
}

pub(crate) fn capture_animation_sample(ctx:&mut Ctx,id:u32)->OpResult<(bool,Vec<(String,String)>)> {
    let (_,record)=record_state(ctx,id)?;
    let sample=sample_record(&record,0.0);
    let complete=record.cancelled || (record.pending_task.is_none()
        && sample.state==PlaybackState::Finished);
    let values=sample.progress.map(|progress| {
        let progress=animation::ease_with_before(&record.easing,progress,sample.before).unwrap_or(progress);
        animation::sample_keyframes_with_before(&record.keyframes,progress,sample.before,interpolate_animation_value)
    }).unwrap_or_default();
    Ok((complete,values))
}

pub(crate) fn cancel_capture_animation(ctx:&mut Ctx,id:u32)->OpResult<()> {
    let state=hub(ctx)?;
    let wrapper=existing_wrapper(ctx,&state,id)?;
    let (_,record)=record_state(ctx,id)?;
    DomAnimation{id,base:record.event_target}.cancel(ctx,This(wrapper))
}

fn css_keyframe_effect(
    keyframes: &lumen_html::css::KeyframesRuleText,
    direction: animation::Direction,
    composite: CompositeMode,
) -> Option<Vec<Keyframe>> {
    let mut frames = Vec::new();
    for rule in &keyframes.rules {
        let offsets = lumen_html::css::keyframe_offsets(&rule.key_text)?;
        let mut declarations = lumen_html::css::parse_keyframe_declarations(&rule.style).ok()?;
        let easing = declarations
            .iter()
            .position(|(name, _)| name == "animation-timing-function")
            .map(|index| declarations.remove(index).1);
        // Composition is keyframe metadata, like easing. It overrides the
        // effect default without becoming an animated property track.
        let frame_composite = declarations.iter()
            .position(|(name, _)| name == "animation-composition")
            .map(|index| declarations.remove(index).1);
        let frame_composite = match frame_composite {
            Some(value) => {
                let values = lumen_html::css::css_list_items(&value);
                parse_keyframe_composite(values.first()?.as_str()).ok()?.unwrap_or(composite)
            }
            None => composite,
        };
        for offset in offsets {
            frames.push(Keyframe { offset_is_specified: true,
                offset,
                declarations: declarations.clone(),
                easing: easing.clone(),
                composite: Some(frame_composite),
            });
        }
    }
    // A valid empty CSS rule is a timed effect with no property tracks,
    // just like Element.animate([]). It still participates in cancellation
    // and finished/ready promises through the shared timeline sampler.
    // CSS keyframe selectors are normalized by the shared parser; preserve
    // source order among duplicate offsets for the shared sampler.
    frames.sort_by(|left, right| left.offset.total_cmp(&right.offset));
    let _ = direction;
    Some(frames)
}

/// An animation-name list edit keeps each surviving name's Animation and moves
/// it to its new list position; only names that left the list are cancelled.
fn reposition_css_records(
    hub: &Rc<RefCell<AnimationHub>>,
    realm_key: usize,
    node: NodeId,
    pseudo:Option<lumen_html::css::PseudoElement>,
    names: &[String],
    snapshot: &lumen_html::session::AnimationSnapshot,
    scope: Option<NodeId>,
    environment: lumen_html::css::MediaEnvironment,
) {
    let wanted = names.iter().map(|authored| {
        lumen_html::css::normalize_animation_name(authored).filter(|_name| !authored.trim().eq_ignore_ascii_case("none")) .filter(|name|
            snapshot.keyframes.iter().any(|rule| rule.name == *name && rule.scope == scope
                && rule.media.iter().all(|query| lumen_html::css::media_query_matches(query, environment))))
    }).collect::<Vec<_>>();
    let mut state = hub.borrow_mut();
    let displaced = wanted.iter().enumerate().any(|(index, name)| name.as_ref().is_some_and(|name|
        state.css_records.get(&(realm_key, node, pseudo, index)).is_some_and(|(old, _, _)| old != name)));
    if !displaced { return; }
    let mut old = state.css_records.iter()
        .filter(|(key, _)| key.0 == realm_key && key.1 == node && key.2==pseudo)
        .map(|(key, entry)| (key.3, entry.clone()))
        .collect::<Vec<_>>();
    old.sort_by_key(|(index, _)| *index);
    let mut claimed = vec![false; old.len()];
    let mut moves = Vec::new();
    for (index, name) in wanted.iter().enumerate() {
        let Some(name) = name else { continue };
        if old.iter().any(|(old_index, (old_name, _, _))| *old_index == index && old_name == name) {
            if let Some(slot) = old.iter().position(|(old_index, _)| *old_index == index) { claimed[slot] = true; }
        }
    }
    for (index, name) in wanted.iter().enumerate() {
        let Some(name) = name else { continue };
        if old.iter().any(|(old_index, (old_name, _, _))| *old_index == index && old_name == name) { continue; }
        if let Some(slot) = old.iter().enumerate().position(|(slot, (_, (old_name, _, _)))| !claimed[slot] && old_name == name) {
            claimed[slot] = true;
            moves.push((old[slot].0, index, old[slot].1.clone()));
        }
    }
    for (from, _, _) in &moves { state.css_records.remove(&(realm_key, node, pseudo, *from)); }
    for (_, to, entry) in moves {
        let id = entry.2;
        state.css_records.insert((realm_key, node, pseudo, to), entry);
        if let Some(record) = state.records.get_mut(&id) { record.css_order = Some(to); }
        if let Some(event) = state.css_events.get_mut(&id) { event.order = to; }
    }
}

fn progress_axis_horizontal(session:&mut lumen_html::session::RenderSession,node:NodeId,axis:&str)->OpResult<bool>{
    animation::progress_timelines::axis_horizontal(session,node,axis).map_err(|_|OpError::error("timeline axis resolution failed"))
}
fn nearest_progress_scroll_source(session:&mut lumen_html::session::RenderSession,node:NodeId,axis:&str)->OpResult<(Option<NodeId>,bool)>{
    animation::progress_timelines::nearest_scroll_source(session,node,axis).map_err(|_|OpError::error("timeline scroll source resolution failed"))
}
fn resolve_named_progress_timeline(realm:&DomRealm,snapshot:&lumen_html::session::AnimationSnapshot,
    node:NodeId,scope:Option<NodeId>,name:&str,range:animation::ProgressRange)->OpResult<ProgressBinding>{
    animation::progress_timelines::resolve(&mut realm.session.borrow_mut(),snapshot,node,scope,name,range)
        .map_err(|_|OpError::error("CSS progress timeline resolution failed"))
}
fn progress_time(realm:&DomRealm,binding:&ProgressBinding)->Option<f64>{
    animation::progress_timelines::sample(&mut realm.session.borrow_mut(),binding).map(|sample|sample.progress*1000.0)
}

fn transition_parameters(style:&lumen_html::css::Style,index:usize)->(animation::TransitionParameters,Rc<str>,bool) {
    let time=|values:Option<&[lumen_html::css::typed_numeric::NumericValue]>| {
        values.filter(|values|!values.is_empty()).map_or(0.0,|values| {
            let value=values[index%values.len()];
            match value.unit {lumen_html::css::typed_numeric::NumericUnit::S=>value.value*1000.0,
                lumen_html::css::typed_numeric::NumericUnit::Ms=>value.value,_=>0.0}
        })
    };
    let easing=style.transition_timing_function().filter(|values|!values.is_empty())
        .map_or("ease",|values|values[index%values.len()].as_ref());
    let discrete=style.transition_behavior().filter(|values|!values.is_empty())
        .is_some_and(|values|values[index%values.len()]==lumen_html::css::TransitionBehavior::AllowDiscrete);
    (animation::TransitionParameters {duration_ms:time(style.transition_duration.as_deref()),
        delay_ms:time(style.transition_delay.as_deref())},Rc::from(easing),discrete)
}

fn interpolate_transition_value(property:&str,from:&str,to:&str,progress:f64)->Option<String> {
    use lumen_html::css::TransitionValueKind::*;
    match lumen_html::css::transition_value_kind(property) {
        NotAnimatable | Discrete | Display | Visibility | Custom => None,
        Integer => {
            use lumen_html::css::typed_numeric::{parse_numeric_value,NumericUnit};
            let a=parse_numeric_value(from)?; let b=parse_numeric_value(to)?;
            if a.unit!=NumericUnit::Number || b.unit!=NumericUnit::Number {return None;}
            lumen_html::css::computed_integer_css_value(property,a.value+(b.value-a.value)*progress)
        }
        _=>interpolate_animation_value(from,to,progress),
    }
}

fn transition_pair_eligible(property:&str,from:&str,to:&str,allow_discrete:bool)->bool {
    use lumen_html::css::TransitionValueKind::*;
    match lumen_html::css::transition_value_kind(property) {
        NotAnimatable=>false,
        Transform=>true,
        Visibility=>(from=="visible" || to=="visible") || allow_discrete,
        Discrete | Display | Custom=>allow_discrete,
        _=>interpolate_transition_value(property,from,to,0.5).is_some() || allow_discrete,
    }
}

fn interpolate_transform_animation(from:&str,to:&str,progress:f64,
    context:&(lumen_html::css::Style,lumen_html::paint::Rect,lumen_html::css::MediaEnvironment))->Option<String> {
    let (style,rect,viewport)=context;
    if lumen_html::css::animation_transform_has_deferred_source(from)||lumen_html::css::animation_transform_has_deferred_source(to) {
        return lumen_html::css::combine_animation_source_transforms(from,to,lumen_html::css::registered_properties::ComputedValueOperation::Interpolate(progress),style,*viewport,None,[rect.width as f64,rect.height as f64]);
    }
    let from=lumen_html::css::parse_animation_transforms(from,style,*viewport,None)?;
    let to=lumen_html::css::parse_animation_transforms(to,style,*viewport,None)?;
    animation::interpolate_transforms(&from,&to,progress,rect.width,rect.height)
}

fn sampled_transition_progress(record:&Record,now:f64)->f64 {
    let sample=sample_record(record,now);
    let progress=sample.progress.unwrap_or(0.0);
    animation::ease_with_before(&record.easing,progress,sample.before).unwrap_or(progress)
}

fn cancel_owned_transition(ctx:&mut Ctx,hub:&Rc<RefCell<AnimationHub>>,id:u32)->OpResult<()> {
    reset_pending_task(ctx,hub,id)?;
    let mut state=hub.borrow_mut();
    let elapsed=state.transitions.get(&id).and_then(|transition|transition.cancellation_elapsed).unwrap_or_else(||state.records.get(&id).map(|record| {
        let sample=sample_record(record,0.0);
        (sample.current_time_ms-record.timing.delay_ms).clamp(0.0,record_sampling_timing(record).active_duration())
    }).unwrap_or(0.0));
    if let Some(record)=state.records.get_mut(&id) {record.cancelled=true;record.pending_task=None;record.hold_time_ms=None;}
    if let Some(transition)=state.transitions.get_mut(&id) {
        transition.associated=false;transition.cancellation_elapsed=Some(elapsed);
    }
    drop(state);
    reject_finished(ctx,hub,id);
    Ok(())
}

fn create_owned_transition(ctx:&mut Ctx,hub:&Rc<RefCell<AnimationHub>>,realm:&Rc<DomRealm>,
    node:NodeId,pseudo:Option<lumen_html::css::PseudoElement>,property:&str,
    endpoints:animation::TransitionState<String>,parameters:animation::TransitionParameters,easing:Rc<str>,now:f64,
    endpoint_styles:Option<(Arc<lumen_html::css::Style>,Arc<lumen_html::css::Style>)>)->OpResult<u32> {
    let timeline_value=document_timeline(ctx,realm)?;
    let realm_key=Rc::as_ptr(realm) as usize;
    let timeline_id=hub.borrow().default_timeline_ids[&realm_key];
    let timing=Timing {duration_ms:parameters.duration_ms,delay_ms:parameters.delay_ms,
        fill:FillMode::Backwards,..Timing::default()};
    let frames:Rc<[Keyframe]>=Rc::from(vec![
        Keyframe { offset_is_specified: true,offset:0.0,declarations:vec![(property.to_owned(),endpoints.start_value.clone())],easing:None,composite:None},
        Keyframe { offset_is_specified: true,offset:1.0,declarations:vec![(property.to_owned(),endpoints.end_value.clone())],easing:None,composite:None},
    ]);
    let mut state=hub.borrow_mut();
    state.next_id=state.next_id.wrapping_add(1).max(1);let id=state.next_id;
    state.next_effect_id=state.next_effect_id.wrapping_add(1).max(1);let effect_id=state.next_effect_id;
    state.effect_records.insert(effect_id,EffectRecord {iteration_accumulate:false,id:effect_id,realm:Rc::downgrade(realm),node:Some(node),pseudo,capture_name:None,
        keyframes:frames.clone(),timing,easing:easing.clone(),duration_auto:false,fill_auto:false,
        composite:CompositeMode::Replace,animation_id:Some(id)});
    state.records.insert(id,Record {id,public_id:String::new(),effect_id:Some(effect_id),css_name:None,css_order:None,
        realm:Rc::downgrade(realm),timeline_realm:Rc::downgrade(realm),timeline_id,timeline_origin_ms:0.0,
        progress_timeline:None,timeline_value,node:Some(node),keyframes:frames,timing,easing,
        composite:CompositeMode::Replace,start_ms:now,start_time_ms:0.0,hold_time_ms:Some(0.0),playback_rate:1.0,
        paused_at_ms:None,pending_task:Some(PlaybackState::Running),pending_task_revision:0,start_resolved:false,
        cancelled:false,finish_event_fired:false,retired:false,event_target:DomEventTarget::independent(realm)});
    let generation=state.transition_generation;
    state.transitions.insert(id,TransitionRecord {owner:Rc::downgrade(realm),node,pseudo,property:property.to_owned(),
        state:endpoints,previous:animation::TransitionPhase::Idle,associated:true,generation,
        retention:Some(NodeRetention::new(realm,node)),cancellation_elapsed:None,endpoint_styles});
    drop(state);
    mark_ready_pending(ctx,hub,id); ensure_pending_finished(ctx,hub,id);
    Ok(id)
}

fn before_change_style<'a>(target:(NodeId,Option<lumen_html::css::PseudoElement>),
    inputs:&HashMap<(NodeId,Option<lumen_html::css::PseudoElement>),&'a lumen_html::session::TransitionStyleInput>,
    samples:&HashMap<(NodeId,Option<lumen_html::css::PseudoElement>),Vec<(String,String)>>,
    transition_samples:&HashMap<(NodeId,Option<lumen_html::css::PseudoElement>),Vec<(String,String)>>,
    document:&Document,environment:lumen_html::css::MediaEnvironment,text:Option<&dyn lumen_html::paint::TextShaper>,
    prepared:&mut HashMap<(NodeId,Option<lumen_html::css::PseudoElement>),Arc<lumen_html::css::Style>>,budget:&mut EffectSamplingBudget,depth:usize,
)->OpResult<Arc<lumen_html::css::Style>> {
    if let Some(style)=prepared.get(&target) {return Ok(style.clone());}
    if depth>=512 {return Err(OpError::new("InvalidStateError","before-change style depth exceeded"));}
    let input=inputs.get(&target).ok_or_else(||OpError::new("InvalidStateError","before-change target unavailable"))?;
    let parent=input.parent.and_then(|node|inputs.get(&(node,None)).copied());
    let resolved_parent=parent.map(|parent|before_change_style((parent.node,None),inputs,samples,transition_samples,document,environment,text,prepared,budget,depth+1)).transpose()?;
    let changed_parent=parent.zip(resolved_parent.as_ref()).is_some_and(|(old,new)|!Arc::ptr_eq(&old.style,new));
    let pairs=samples.get(&target).map_or(&[][..],Vec::as_slice);
    let mut style=input.style.clone();
    if (changed_parent || !pairs.is_empty()) && document.kind(input.node).is_ok() {
        Arc::make_mut(&mut style).apply_animation_snapshot(document,input.node,resolved_parent.as_deref(),input.pseudo,text,
            environment,pairs,&input.eligibility).map_err(|error|OpError::new("SyntaxError",format!("before-change animation sample failed: {error:?}")))?;
    }
    if let Some(pairs)=transition_samples.get(&target).filter(|_|document.kind(input.node).is_ok()) {
        Arc::make_mut(&mut style).apply_animation_snapshot(document,input.node,resolved_parent.as_deref(),input.pseudo,text,
            environment,pairs,&lumen_html::css::EffectEligibility::all())
            .map_err(|error|OpError::new("SyntaxError",format!("before-change transition sample failed: {error:?}")))?;
    }
    budget.reserve(prepared,1)?;budget.style(&style,true)?;
    prepared.insert(target,style.clone());Ok(style)
}

fn observe_transition_removals(hub:&Rc<RefCell<AnimationHub>>,realm:&Rc<DomRealm>)->OpResult<()> {
    {
        let mut state=hub.borrow_mut();
        state.transition_mutation_realms.retain(|owner|owner.strong_count()!=0);
        if state.transition_mutation_realms.iter().any(|owner|owner.upgrade().is_some_and(|owner|Rc::ptr_eq(&owner,realm))) {return Ok(());}
        if state.transition_mutation_realms.len()>=crate::scheduling::MAX_PENDING_HTML_TASKS {
            return Err(OpError::new("QuotaExceededError","transition mutation realm limit"));
        }
        state.transition_mutation_realms.try_reserve(1).map_err(|_|OpError::new("QuotaExceededError","transition mutation realm allocation"))?;
        state.transition_mutation_realms.push(Rc::downgrade(realm));
    }
    let weak_hub=Rc::downgrade(hub);let weak_realm=Rc::downgrade(realm);
    realm.add_mutation_sink(Rc::new(move|document,mutation| {
        if mutation.kind.removed_nodes().next().is_none() {return;}
        let (Some(hub),Some(realm))=(weak_hub.upgrade(),weak_realm.upgrade()) else{return;};
        let key=Rc::as_ptr(&realm) as usize;
        let mut state=hub.borrow_mut();
        if let Some((_,snapshot))=state.transition_snapshots.get(&key) {
            for input in &snapshot.nodes {
                if input.was_rendered() && !document.is_connected_element(input.node) {input.set_rendered(false);}
            }
        }
        let AnimationHub {records,transitions,completed_transitions,..}=&mut *state;
        for (id,transition) in transitions.iter_mut() {
            if !transition.associated || !transition.owner.upgrade().is_some_and(|owner|Rc::ptr_eq(&owner,&realm))
                || document.is_connected_element(transition.node) {continue;}
            let Some(record)=records.get_mut(id) else{continue;};
            let sample=sample_record(record,0.0);
            let elapsed=(sample.current_time_ms-record.timing.delay_ms).clamp(0.0,record_sampling_timing(record).active_duration());
            record.cancelled=true;
            transition.associated=false;transition.cancellation_elapsed=Some(elapsed);
        }
        completed_transitions.retain(|target,_|target.0!=key || document.is_connected_element(target.1));
    }));
    Ok(())
}

fn refresh_css_transitions(ctx:&mut Ctx,hub:&Rc<RefCell<AnimationHub>>,realm:&Rc<DomRealm>,now:f64)->OpResult<()> {
    let result=refresh_css_transitions_inner(ctx,hub,realm,now);
    // AFTER sampling is an internal cascade projection, never an observable
    // partial publication when snapshot/transition admission fails.
    if result.is_err(){let _=apply_realm(hub,realm,now);}
    result
}

fn refresh_css_transitions_inner(ctx:&mut Ctx,hub:&Rc<RefCell<AnimationHub>>,realm:&Rc<DomRealm>,now:f64)->OpResult<()> {
    let realm_key=Rc::as_ptr(realm) as usize;
    observe_transition_removals(hub,realm)?;
    // Internal removal hooks run with borrowed Documents and cannot invoke JS.
    // Complete their promise/ready lifecycle here before sampling new styles.
    let removed=hub.borrow().running_transitions.iter().filter(|(target,id)|target.0==realm_key
        && hub.borrow().records.get(id).is_some_and(|record|record.cancelled))
        .map(|(target,id)|(target.clone(),*id)).collect::<Vec<_>>();
    for (target,id) in removed {hub.borrow_mut().running_transitions.remove(&target);cancel_owned_transition(ctx,hub,id)?;}
    // Style changes can occur after the active interval but before the next
    // rendering event phase. Reconcile completion without consuming end events.
    let completed= {
        let state=hub.borrow();
        state.running_transitions.iter().filter(|(key,_)|key.0==realm_key).filter_map(|(key,id)| {
            let record=state.records.get(id)?;
            let transition=state.transitions.get(id)?;
            let sample=sample_record(record,now);
            (record.pending_task.is_none() && record.playback_rate>0.0 && sample.state==PlaybackState::Finished)
                .then(||(key.clone(),transition.state.end_value.clone()))
        }).collect::<Vec<_>>()
    };
    let completed_display=completed.iter().any(|(key,_)|key.3=="display");
    {let mut state=hub.borrow_mut();for (key,value) in completed {state.running_transitions.remove(&key);state.completed_transitions.insert(key,value);}}
    let epoch=realm.session.borrow_mut().transition_input_epoch()
        .map_err(|error|OpError::new("InvalidStateError",format!("transition style inputs failed: {error:?}")))?;
    if !completed_display && hub.borrow().transition_snapshots.get(&realm_key).is_some_and(|(owner,previous)|
        owner.upgrade().is_some_and(|owner|Rc::ptr_eq(&owner,realm)) && previous.epoch==epoch) {return Ok(());}
    // An unchanged cascade input epoch needs lifecycle reconciliation above,
    // but no BEFORE stack publication. The caller samples its actual demand.
    // Genuine style changes still advance old effects against their captured
    // source before projecting AFTER declarations and new CSS timing.
    apply_realm_with_underlying(hub,realm,now,true)?;
    // Preserve the complete old composited stack before calculating AFTER.
    // Merely suppressing the Transition cascade origin is insufficient: an
    // additive Animation-origin result may already contain a lower transition.
    let sampled_animations=realm.session.borrow().sampled_effect_declarations(lumen_html::css::EffectOrigin::Animation)
        .map(|(node,pseudo,pairs)|((node,pseudo),pairs.to_vec())).collect::<HashMap<_,_>>();
    let sampled_transitions=realm.session.borrow().sampled_effect_declarations(lumen_html::css::EffectOrigin::Transition)
        .map(|(node,pseudo,pairs)|((node,pseudo),pairs.to_vec())).collect::<HashMap<_,_>>();
    apply_realm_in_phase(hub,realm,now,EffectStylePhase::AfterChange)?;
    let fonts=crate::canvas::initialized_realm_font_source(realm)?;
    let text=fonts.as_ref().map(|fonts|&**fonts as &dyn lumen_html::paint::TextShaper);
    let mut snapshot=realm.session.borrow_mut().transition_snapshot_with_text(text)
        .map_err(|error|OpError::new("InvalidStateError",format!("transition style snapshot failed: {error:?}")))?;
    let registrations=realm.session.borrow().registered_custom_property_snapshot();
    let previous=hub.borrow().transition_snapshots.get(&realm_key).map(|(_,snapshot)|snapshot.clone());
    let generation=realm.browsing_context().ok_or_else(||OpError::new("InvalidStateError","transition document browsing context unavailable"))?.next_transition_generation()?;
    hub.borrow_mut().transition_generation=generation;
    let previous_nodes=previous.as_ref().map(|snapshot|snapshot.nodes.iter().map(|input|((input.node,input.pseudo),input)).collect::<HashMap<_,_>>()).unwrap_or_default();
    let mut before_styles=HashMap::new();
    let mut before_budget=EffectSamplingBudget{bytes:0,style_bytes:0};
    let after_nodes=snapshot.nodes.iter().map(|input|((input.node,input.pseudo),input)).collect::<HashMap<_,_>>();
    let mut running_by_target=HashMap::<(NodeId,Option<lumen_html::css::PseudoElement>),Vec<String>>::new();
    for (owner,node,pseudo,property) in hub.borrow().running_transitions.keys() {
        if *owner==realm_key {running_by_target.entry((*node,*pseudo)).or_default().push(property.clone());}
    }
    let mut active=HashSet::new();
    let mut effective_rendered=HashMap::new();
    for input in &snapshot.nodes {
        let running=running_by_target.get(&(input.node,input.pseudo));
        let running_display=running.is_some_and(|properties|properties.iter().any(|property|property=="display"));
        let old=previous_nodes.get(&(input.node,input.pseudo)).copied().filter(|old|old.was_rendered() || running_display);
        let display_index=input.style.transition_property().map_or(Some(0),|properties|properties.iter().enumerate()
            .filter(|(_,name)|lumen_html::css::transition_property_longhands(name).iter().any(|name|name.as_ref()=="display"))
            .map(|(index,_)|index).last());
        let parent_rendered=input.parent.and_then(|parent|effective_rendered.get(&(parent,None)).copied())
            .or_else(||input.parent.and_then(|parent|after_nodes.get(&(parent,None)).map(|input|input.was_rendered()))).unwrap_or(true);
        let display_completed=hub.borrow().completed_transitions.contains_key(&(realm_key,input.node,input.pseudo,String::from("display")));
        let display_exit=!display_completed && old.is_some() && input.style.display==lumen_html::css::Display::None
            && display_index.is_some_and(|index| {let (parameters,_,discrete)=transition_parameters(&input.style,index);
                discrete && (running_display || parameters.combined_duration()>0.0)})
            && parent_rendered
            && realm.session.borrow().document().is_connected_element(input.node);
        let rendered=parent_rendered && realm.session.borrow().document().is_connected_element(input.node)
            && (input.style.display!=lumen_html::css::Display::None || display_exit);
        effective_rendered.insert((input.node,input.pseudo),rendered);
        if !rendered {continue;}
        let times=input.style.transition_property().map_or(1,|properties|properties.len());
        if running.is_none() && !(0..times).any(|index|transition_parameters(&input.style,index).0.combined_duration()>0.0) {continue;} 
        let starting=if old.is_none() {
            let parent_id=input.parent;
            let parent=parent_id.and_then(|id|after_nodes.get(&(id,None)).copied());
            Some(realm.session.borrow_mut().transition_starting_style_with_text(input.node,input.pseudo,parent.map(|input|input.style.as_ref()),text)
                .map_err(|error|OpError::new("InvalidStateError",format!("starting style failed: {error:?}")))?)
        }else{None};
        // Only eligible transition targets consume the sampled BEFORE style.
        // The canonical recursive memo still samples their exact old lineage;
        // unrelated snapshot nodes do not allocate reconstructed style entries.
        let before=if let Some(old)=old {
            let session=realm.session.borrow();
            before_change_style((old.node,old.pseudo),&previous_nodes,&sampled_animations,&sampled_transitions,
                session.document(),session.media_environment(),text,&mut before_styles,&mut before_budget,0)?
        }else{starting.expect("before or starting style")};

        let properties=input.style.transition_property();
        let defaults=[Arc::<str>::from("all")];
        let mut matched=BTreeMap::<String,usize>::new();
        for (index,property) in properties.unwrap_or(&defaults).iter().enumerate() {
            if property.as_ref()=="all" {
                for registration in registrations.as_deref().unwrap_or(&[]) {
                    matched.insert(registration.name.clone(),index);
                }
            }
            for longhand in lumen_html::css::transition_property_longhands(property) {
                let physical=lumen_html::css::transition_property_physical_name(&input.style,&longhand);
                matched.insert(physical.to_string(),index);
            }
        }
        // Running entries also need processing when transition-property stops matching.
        if let Some(running)=running { for property in running {matched.entry(property.clone()).or_insert(usize::MAX);} }
        for (property,index) in matched {
            let key=(realm_key,input.node,input.pseudo,property.clone());active.insert(key.clone());
            let after=input.style.computed_css_value(&property,lumen_html::css::computed_values::ComputedValueContext::default());
            let before_value=before.computed_css_value(&property,lumen_html::css::computed_values::ComputedValueContext::default());
            let (Some(after),Some(before_value))=(after,before_value) else {continue;};
            let id=hub.borrow().running_transitions.get(&key).copied();
            let completed=hub.borrow().completed_transitions.get(&key).cloned();
            let old=hub.borrow();
            let running=id.and_then(|id|old.transitions.get(&id).zip(old.records.get(&id)));
            // Before-change style includes modified effects and the complete old effect stack.
            let current=before_value.clone();
            let progress=running.map_or(0.0,|(_,record)|sampled_transition_progress(record,now));
            let running_state=running.map(|(transition,_)|transition.state.clone());
            drop(old);
            if index!=usize::MAX && running_state.as_ref().is_some_and(|state|state.end_value==after) {
                if completed.as_ref().is_some_and(|value|value!=&after) {hub.borrow_mut().completed_transitions.remove(&key);}
                continue;
            }
            let (parameters,easing,discrete)=transition_parameters(&input.style,index.min(usize::MAX-1));
            let parameters=(index!=usize::MAX).then_some(parameters);
            let mut compound=false;
            let update=animation::transition_update_with(&before_value,&after,&current,running_state.as_ref(),completed.as_ref(),parameters,
                |retarget| {
                    // Current is the sampled BEFORE stack; both lifecycle branches
                    // therefore share one endpoint-style projection and probe.
                    compound=animation::transition_values::interpolate_compound_transition(&property,&before,&input.style,0.5,lumen_html_image::interpolate_color).is_some();
                    let from=if retarget{&current}else{&before_value};
                    if let Some(registration)=registrations.as_deref().unwrap_or(&[]).iter().find(|registration|registration.name==property) {
                        if registration.syntax!="*" {
                            return lumen_html::css::registered_properties::combine_computed_values(&registration.syntax,from,&after,
                                lumen_html::css::registered_properties::ComputedValueOperation::Interpolate(0.5)).is_some() || discrete;
                        }
                    }
                    compound || transition_pair_eligible(&property,from,&after,discrete)
                },progress);
            if update.remove_completed {hub.borrow_mut().completed_transitions.remove(&key);}
            match update.action {
                animation::TransitionAction::Keep=>{},
                animation::TransitionAction::Cancel=>if let Some(id)=id {
                    hub.borrow_mut().running_transitions.remove(&key);cancel_owned_transition(ctx,hub,id)?;
                },
                animation::TransitionAction::Start {state,parameters}=>{
                    if let Some(id)=id {cancel_owned_transition(ctx,hub,id)?;}
                    let endpoint_styles=compound.then(||(before.clone(),input.style.clone()));
                    let id=create_owned_transition(ctx,hub,realm,input.node,input.pseudo,&property,state,parameters,easing,now,endpoint_styles)?;
                    hub.borrow_mut().running_transitions.insert(key,id);
                }
            }
        }
    }
    let stale=hub.borrow().running_transitions.iter().filter(|(key,_)|key.0==realm_key && !active.contains(*key))
        .map(|(key,id)|(key.clone(),*id)).collect::<Vec<_>>();
    for (key,id) in stale {hub.borrow_mut().running_transitions.remove(&key);cancel_owned_transition(ctx,hub,id)?;}
    hub.borrow_mut().completed_transitions.retain(|key,_|key.0!=realm_key || active.contains(key));
    drop(after_nodes);
    if let Some(snapshot)=Arc::get_mut(&mut snapshot) {
        for input in &mut snapshot.nodes {input.set_rendered(effective_rendered.get(&(input.node,input.pseudo)).copied().unwrap_or(false));}
    }
    hub.borrow_mut().transition_snapshots.insert(realm_key,(Rc::downgrade(realm),snapshot));
    Ok(())
}

/// Synchronous CSSOM reads admit real pending style changes and publish samples,
/// without delivering animation events outside the rendering-opportunity phase.
pub(crate) fn flush_css_transitions_for(ctx:&mut Ctx,realm:&Rc<DomRealm>,target:Option<NodeId>)->OpResult<()> {
    flush_css_transitions_for_property(ctx,realm,target,None)
}
pub(crate) fn flush_css_transitions_for_property(ctx:&mut Ctx,realm:&Rc<DomRealm>,target:Option<NodeId>,property:Option<&str>)->OpResult<()> {
    realm.flush_inline_stylesheet_updates(ctx)?;
    if document_time(realm).is_none() {return Ok(());}
    let Some(hub)=RealmServices::<RefCell<AnimationHub>>::current(ctx) else{return Ok(());};
    if hub.borrow().refreshing_transitions {return Ok(());}
    hub.borrow_mut().refreshing_transitions=true;
    let result=(||{
        let now=realm.timeline_sample.get();
        refresh_css_transitions(ctx,&hub,realm,now)?;
        refresh_css_animations(ctx,&hub,realm,now)?;
        let demand=match property{Some(property)=>EffectDemand::for_property(&hub,realm,target,property)?,None=>EffectDemand::new(realm,target)?};
        apply_realm_in_phase_with_demand(&hub,realm,now,EffectStylePhase::Current,&demand)
    })();
    hub.borrow_mut().refreshing_transitions=false;
    result
}

fn refresh_css_animations(
    ctx: &mut Ctx,
    hub: &Rc<RefCell<AnimationHub>>,
    realm: &Rc<DomRealm>,
    now: f64,
) -> OpResult<()> {
    let source=hub.borrow().transition_snapshots.get(&(Rc::as_ptr(realm) as usize)).map(|(_,snapshot)|snapshot.clone());
    let snapshot = {let mut session=realm.session.borrow_mut();match source {
        Some(source)=>session.animation_snapshot_from_transition(&source),None=>session.animation_snapshot(),
    }}.map_err(|error|OpError::new("SyntaxError",format!("CSS animation style resolution failed: {error:?}")))?;
    let environment = realm.session.borrow().media_environment();
    let realm_key = Rc::as_ptr(realm) as usize;
    if hub.borrow().css_generations.get(&realm_key).is_some_and(|(owner, generation)| {
        *generation == snapshot.generation && owner.upgrade().is_some_and(|owner| Rc::ptr_eq(&owner, realm))
    }) {
        return Ok(());
    }
    let mut progress_payload_bytes=hub.borrow().records.values().try_fold(0usize,|bytes,record|{
        bytes.checked_add(match &record.progress_timeline{Some(binding)=>binding.checked_retained_bytes()?,None=>0})
    }).ok_or_else(||OpError::new("QuotaExceededError","CSS animation progress metadata exceeded"))?;
    let mut active = HashSet::new();
    let mut created = Vec::new();
    let mut cancelled = Vec::new();
    let mut playback_changes = Vec::new();
    for (node, pseudo, scope, style) in &snapshot.nodes {
        let (node,pseudo,scope) = (*node,*pseudo,*scope);
        let names = style[0].as_deref().map(lumen_html::css::css_list_items).unwrap_or_default();
        reposition_css_records(hub, realm_key, node, pseudo, &names, &snapshot, scope, environment);
        for (index, authored_name) in names.iter().enumerate() {
            let Some(name) = lumen_html::css::normalize_animation_name(authored_name) else { continue; };
            if authored_name.trim().eq_ignore_ascii_case("none") { continue; }
            let selected = snapshot.keyframes.iter().filter(|rule| {
                rule.name == name && rule.scope == scope
                    && rule.media.iter().all(|query| lumen_html::css::media_query_matches(query, environment))
            }).last();
            let Some(selected) = selected else { continue };
            let key = (realm_key,node,pseudo,index);
            active.insert(key);
            let signature = format!("{:?}|{}", style, selected.css_text);
            let duration_value = css_list_value(style,1,index,"auto");
            let duration_auto=duration_value=="auto";
            let timeline_name = css_list_value(style,9,index,"auto");
            let range = animation::ProgressRange::parse(&css_list_value(style,10,index,"normal"))
                .unwrap_or_else(|| animation::ProgressRange::parse("normal").unwrap());
            let mut progress_timeline = if timeline_name == "auto" {None} else {
                Some(resolve_named_progress_timeline(realm,&snapshot,node,scope,&timeline_name,range)?)
            };
            if let Some(binding)=&mut progress_timeline{binding.duration_auto=duration_auto;}
            let old_entry = hub.borrow().css_records.get(&key).cloned();
            let old_payload=old_entry.as_ref().and_then(|(_,_,id)|hub.borrow().records.get(id).and_then(|record|record.progress_timeline.as_ref().map(ProgressBinding::checked_retained_bytes))).flatten().unwrap_or(0);
            let new_payload=progress_timeline.as_ref().map(ProgressBinding::checked_retained_bytes).unwrap_or(Some(0)).ok_or_else(||OpError::new("QuotaExceededError","CSS animation progress metadata exceeded"))?;
            progress_payload_bytes=progress_payload_bytes.checked_sub(old_payload).and_then(|bytes|bytes.checked_add(new_payload)).filter(|bytes|*bytes<=lumen_html::html::MAX_HTML_BYTES).ok_or_else(||OpError::new("QuotaExceededError","CSS animation progress metadata exceeded"))?;

            if old_entry.as_ref().is_some_and(|(old_name, old_signature, _)| old_name == &name && old_signature == &signature) {
                if let Some((_,_,id)) = &old_entry { if let Some(record) = hub.borrow_mut().records.get_mut(id) {record.progress_timeline=progress_timeline;} }
                continue;
            }

            let delay = css_time_value(&css_list_value(&style, 2, index, "0s")).unwrap_or(0.0);
            let easing = css_list_value(&style, 3, index, "ease");
            let iterations = match css_list_value(&style, 4, index, "1").as_str() {
                "infinite" => f64::INFINITY,
                value => value.parse::<f64>().unwrap_or(1.0).max(0.0),
            };
            let duration=css_time_value(&duration_value).unwrap_or(0.0).max(0.0);
            let direction = match css_list_value(&style, 5, index, "normal").as_str() {
                "reverse" => Direction::Reverse,
                "alternate" => Direction::Alternate,
                "alternate-reverse" => Direction::AlternateReverse,
                _ => Direction::Normal,
            };
            let fill = match css_list_value(&style, 6, index, "none").as_str() {
                "forwards" => FillMode::Forwards,
                "backwards" => FillMode::Backwards,
                "both" => FillMode::Both,
                _ => FillMode::None,
            };
            let play_state = css_list_value(&style, 7, index, "running");
            let composite = match css_list_value(&style, 8, index, "replace").as_str() {
                "add" => CompositeMode::Add,
                "accumulate" => CompositeMode::Accumulate,
                _ => CompositeMode::Replace,
            };
            let frames = css_keyframe_effect(selected, direction, composite)
                .ok_or_else(|| OpError::new("SyntaxError", "invalid CSS keyframe block"))?;
            let timing = Timing {
                delay_ms: delay,
                end_delay_ms: css_time_value(&css_list_value(style,17,index,"0s")).unwrap_or(0.0),
                duration_ms: duration,
                iteration_start: 0.0,
                iterations,
                fill,
                direction,
            };
            let frames: Rc<[Keyframe]> = frames.into();
            let easing: Rc<str> = easing.into();
            if let Some((old_name, _old_signature, old_id)) = old_entry {
                if old_name == name {
                    let mut state = hub.borrow_mut();
                    let Some(record_snapshot) = state.records.get(&old_id).cloned() else { continue; };
                    let desired_pause=play_state=="paused";
                    let play_state_changed=state.css_events.get(&old_id).is_some_and(|event|event.css_paused!=desired_pause);
                    if let Some(event)=state.css_events.get_mut(&old_id){event.css_paused=desired_pause;}
                    if let Some(record) = state.records.get_mut(&old_id) {
                        record.progress_timeline = progress_timeline.clone();
                        record.timing = timing;
                        record.keyframes = frames.clone();
                        record.easing = easing.clone();
                        record.composite = composite;
                    }
                    if let Some(effect_id) = record_snapshot.effect_id {
                        if let Some(effect) = state.effect_records.get_mut(&effect_id) {
                            effect.timing = timing;
                            effect.duration_auto = duration_auto;
                            effect.keyframes = frames;
                            effect.easing = easing;
                            effect.composite = composite;
                        }
                    }
                    state.css_records.insert(key, (name, signature, old_id));
                    drop(state);
                    if play_state_changed {
                        playback_changes.push((old_id, if desired_pause {PlaybackState::Paused}else{PlaybackState::Running}));
                    }
                    continue;
                }
            }
            let old_entry = { hub.borrow_mut().css_records.remove(&key) };
            if let Some((_, _, old_id)) = old_entry {
                retire_css_snapshot_record(ctx, hub, old_id, now)?;
                cancelled.push(old_id);
            }
            let timeline_value=document_timeline(ctx,realm)?;
            let timeline_id=hub.borrow().default_timeline_ids[&realm_key];
            let mut state = hub.borrow_mut();
            state.next_effect_id = state.next_effect_id.wrapping_add(1).max(1);
            let effect_id = state.next_effect_id;
            state.next_id = state.next_id.wrapping_add(1).max(1);
            let id = state.next_id;
            let event_target = DomEventTarget::independent(realm);
            state.effect_records.insert(effect_id, EffectRecord {
                iteration_accumulate:false,
                id: effect_id,
                realm: Rc::downgrade(realm),
                node: Some(node),pseudo,capture_name:None,
                keyframes: frames.clone(),
                timing,
                easing: easing.clone(),
                duration_auto,
                fill_auto: false,
                composite,
                animation_id: Some(id),
            });
            state.records.insert(id, Record {
                id,
                public_id: String::new(),
                effect_id: Some(effect_id),
                css_name: Some(name.clone()),
                css_order: Some(index),
                realm: Rc::downgrade(realm),
                timeline_realm: Rc::downgrade(realm),
                timeline_id,
                timeline_origin_ms: 0.0,
                progress_timeline: progress_timeline.clone(),
                timeline_value,
                node: Some(node),
                keyframes: frames,
                timing,
                easing,
                composite,
                start_ms: now,
                start_time_ms: 0.0,
                hold_time_ms: Some(0.0),
                playback_rate: 1.0,
                paused_at_ms: None,
                pending_task: Some(if play_state=="paused" { PlaybackState::Paused } else { PlaybackState::Running }),
                pending_task_revision: 0,
                start_resolved: false,
                cancelled: false,
                finish_event_fired: false,
                retired: false,
                event_target,
            });
            state.css_events.insert(id, CssEventState {
                owner: Rc::downgrade(realm), node, pseudo, order: index,
                _retention: NodeRetention::new(realm, node),
                previous: animation::CssEventSample::IDLE, sampled: false,
                associated: true, css_paused: play_state=="paused",
            });
            state.css_records.insert(key, (name, signature, id));
            created.push(id);
        }
    }
    let stale = hub.borrow().css_records.keys()
        .filter(|key| key.0 == realm_key && !active.contains(*key))
        .copied().collect::<Vec<_>>();
    for key in stale {
        let old_entry = { hub.borrow_mut().css_records.remove(&key) };
        if let Some((_, _, id)) = old_entry {
            retire_css_snapshot_record(ctx, hub, id, now)?;
            cancelled.push(id);
        }
    }
    hub.borrow_mut().css_generations.insert(realm_key, (Rc::downgrade(realm), snapshot.generation));
    // Commit the complete CSS snapshot before publishing Promise hooks. A hook
    // may enumerate animations, mutate styles, or replace a pending task.
    let mut publication = DispatchRetention { hub: hub.clone(), ids: Vec::new() };
    for id in cancelled.iter().chain(created.iter()).chain(playback_changes.iter().map(|(id, _)| id)) {
        publication.retain(*id);
    }
    for id in cancelled { reset_pending_task(ctx, hub, id)?; }
    for (id, playback) in playback_changes {
        if hub.borrow().css_generations.get(&realm_key).is_some_and(|(_, generation)| *generation == snapshot.generation) {
            request_pending_task(ctx, hub, id, playback, false)?;
        }
    }
    for id in created {
        let needs_ready = {
            let state = hub.borrow();
            state.records.get(&id).is_some_and(|record| !record.cancelled)
                && !state.ready.contains_key(&id)
        };
        if needs_ready { mark_ready_pending(ctx, hub, id); }
    }
    Ok(())
}

fn animation_array(ctx: &mut Ctx, hub: &Rc<RefCell<AnimationHub>>, ids: &[u32]) -> OpResult<Value> {
    let global = ctx.global_object();
    let array_ctor = ctx
        .get_member(&global, "Array")
        .map_err(|_| OpError::new("Error", "Array constructor is unavailable"))?;
    let constructor = JsFunction::from_value(array_ctor)
        .ok_or_else(|| OpError::new("Error", "Array is not callable"))?;
    let array = constructor.call(ctx, Value::Undefined, &[Value::Num(ids.len() as f64)])?;
    for (index, id) in ids.iter().enumerate() {
        let value = existing_wrapper(ctx, hub, *id)?;
        ctx.set_member(&array, &index.to_string(), value)
            .map_err(|_| OpError::new("TypeError", "animation array assignment failed"))?;
    }
    Ok(array)
}

// Actual JS graph ownership lives on wrappers, never on the per-global hub.
// Internal slots are traced Object edges and are updated through the shared host API.
const ANIMATION_EFFECT_OWNER:&str="#lumen.html.animation.effect.owner\u{1}effect";
const EFFECT_TARGET_OWNER:&str="#lumen.html.animation.target.owner\u{1}target";
fn publish_effect_target_owner(ctx:&mut Ctx,hub:&Rc<RefCell<AnimationHub>>,id:u32,wrapper:&Value)->OpResult<()> {
    let target={let state=hub.borrow();state.effect_records.get(&id).and_then(|effect|
        effect.realm.upgrade().zip(effect.node))};
    let value=target.map_or(Value::Null,|(realm,node)|realm.wrap(ctx,node));
    ctx.set_native_internal_value_slot(wrapper,EFFECT_TARGET_OWNER,value).map_err(OpError::thrown)
}
fn publish_animation_effect_owner(ctx:&mut Ctx,hub:&Rc<RefCell<AnimationHub>>,id:u32,wrapper:&Value)->OpResult<()> {
    let effect=hub.borrow().records.get(&id).and_then(|record|record.effect_id);
    let value=match effect {Some(effect)=>effect_wrapper(ctx,hub,effect)?,None=>Value::Null};
    ctx.set_native_internal_value_slot(wrapper,ANIMATION_EFFECT_OWNER,value).map_err(OpError::thrown)
}
fn update_live_animation_effect_owner(ctx:&mut Ctx,hub:&Rc<RefCell<AnimationHub>>,id:u32)->OpResult<()> {
    let wrapper=hub.borrow().wrappers.get(&id).and_then(WeakValue::upgrade);
    if let Some(wrapper)=wrapper {publish_animation_effect_owner(ctx,hub,id,&wrapper)?;}
    Ok(())
}

fn existing_wrapper(ctx: &mut Ctx, hub: &Rc<RefCell<AnimationHub>>, id: u32) -> OpResult<Value> {
    if let Some(value) = hub.borrow().wrappers.get(&id).and_then(WeakValue::upgrade) {
        return Ok(value);
    }
    let (event_target, css_name) = hub
        .borrow()
        .records
        .get(&id)
        .map(|record| (record.event_target.clone(), record.css_name.clone()))
        .ok_or_else(|| OpError::new("InvalidStateError", "animation is no longer available"))?;
    let animation = DomAnimation {
        id,
        base: event_target,
    };
    let value = if hub.borrow().transitions.contains_key(&id) {
        ctx.new_instance(DomCssTransition {base:animation})
    } else if css_name.is_some() {
        ctx.new_instance(DomCssAnimation { base: animation })
    } else {
        ctx.new_instance(animation)
    };
    if let Some(weak) = ctx.weak_value(&value) {
        hub.borrow_mut().wrappers.insert(id, weak);
    }
    publish_animation_effect_owner(ctx,hub,id,&value)?;
    Ok(value)
}

fn effect_wrapper(ctx: &mut Ctx, hub: &Rc<RefCell<AnimationHub>>, id: u32) -> OpResult<Value> {
    if let Some(value) = hub.borrow().effects.get(&id).and_then(WeakValue::upgrade) {
        return Ok(value);
    }
    if !hub.borrow().effect_records.contains_key(&id) {
        return Ok(Value::Null);
    }
    let value = ctx.new_instance(DomKeyframeEffect { id });
    publish_effect_target_owner(ctx,hub,id,&value)?;
    if let Some(weak) = ctx.weak_value(&value) {
        hub.borrow_mut().effects.insert(id, weak);
    }
    Ok(value)
}

fn keyframe_object(ctx: &mut Ctx, frame: &Keyframe) -> OpResult<Value> {
    let object = Value::Obj(ctx.new_object());
    ctx.set_member(&object, "offset", if frame.offset_is_specified {Value::Num(frame.offset)} else {Value::Null})
        .map_err(|_| OpError::new("Error", "keyframe offset assignment failed"))?;
    ctx.set_member(&object,"computedOffset",Value::Num(frame.offset)).map_err(|error|OpError::thrown(lumen::embed::abrupt_value(error)))?;
    ctx.set_member(&object,"easing",Value::Str(frame.easing.as_deref().unwrap_or("linear").into()))
        .map_err(|error|OpError::thrown(lumen::embed::abrupt_value(error)))?;
    let composite = match frame.composite {
        None => "auto",
        Some(CompositeMode::Replace) => "replace",
        Some(CompositeMode::Add) => "add",
        Some(CompositeMode::Accumulate) => "accumulate",
    };
    ctx.set_member(&object, "composite", Value::Str(composite.into()))
        .map_err(|_| OpError::new("Error", "keyframe composite assignment failed"))?;
    for (name, value) in &frame.declarations {
        let property=if name.starts_with("--") {name.clone()}
            else if name=="float" {String::from("cssFloat")}
            else if name=="offset" {String::from("cssOffset")}
            else {
                let mut property=String::new();let mut uppercase=false;
                for character in name.chars() {
                    if character=='-' {uppercase=true;}
                    else if uppercase {property.push(character.to_ascii_uppercase());uppercase=false;}
                    else {property.push(character);}
                }
                property
            };
        ctx.set_member(&object, &property, Value::Str(value.clone().into()))
            .map_err(|_| OpError::new("Error", "keyframe declaration assignment failed"))?;
    }
    Ok(object)
}

fn frames_array(ctx: &mut Ctx, frames: &[Keyframe]) -> OpResult<Value> {
    let global = ctx.global_object();
    let constructor = ctx
        .get_member(&global, "Array")
        .map_err(|_| OpError::new("Error", "Array constructor is unavailable"))?;
    let array = ctx
        .construct_value(constructor, &[Value::Num(frames.len() as f64)])
        .map_err(OpError::thrown)?;
    for (index, frame) in frames.iter().enumerate() {
        let value = keyframe_object(ctx, frame)?;
        ctx.set_member(&array, &index.to_string(), value)
            .map_err(|_| OpError::new("Error", "keyframe array assignment failed"))?;
    }
    Ok(array)
}

fn timing_object(
    ctx: &mut Ctx,
    timing: Timing,
    easing: &str,
    duration_auto: bool,
    fill_auto: bool,
) -> OpResult<Value> {
    let object = Value::Obj(ctx.new_object());
    for (name, value) in [
        ("delay", timing.delay_ms),
        ("endDelay", timing.end_delay_ms),
        ("iterationStart", timing.iteration_start),
        ("iterations", timing.iterations),
    ] {
        ctx.set_member(&object, name, Value::Num(value))
            .map_err(|_| OpError::new("Error", "timing assignment failed"))?;
    }
    ctx.set_member(
        &object,
        "duration",
        if duration_auto {
            Value::Str("auto".into())
        } else {
            Value::Num(timing.duration_ms)
        },
    )
    .map_err(|error| OpError::thrown(lumen::embed::abrupt_value(error)))?;
    let fill = if fill_auto {
        "auto"
    } else {
        match timing.fill {
            FillMode::None => "none",
            FillMode::Forwards => "forwards",
            FillMode::Backwards => "backwards",
            FillMode::Both => "both",
        }
    };
    let direction = match timing.direction {
        Direction::Normal => "normal",
        Direction::Reverse => "reverse",
        Direction::Alternate => "alternate",
        Direction::AlternateReverse => "alternate-reverse",
    };
    ctx.set_member(&object, "fill", Value::Str(fill.into()))
        .map_err(|_| OpError::new("Error", "timing assignment failed"))?;
    ctx.set_member(&object, "direction", Value::Str(direction.into()))
        .map_err(|_| OpError::new("Error", "timing assignment failed"))?;
    ctx.set_member(&object, "easing", Value::Str(easing.to_owned().into()))
        .map_err(|_| OpError::new("Error", "timing assignment failed"))?;
    Ok(object)
}

/// Return the realm's live document timeline. Parent integration exposes this
/// from `Document.timeline` and installs the public constructor name.
pub fn document_timeline(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> OpResult<Value> {
    let hub = hub(ctx)?;
    let key = Rc::as_ptr(realm) as usize;
    if let Some(value) = hub
        .borrow()
        .timelines
        .get(&key)
        .and_then(WeakValue::upgrade)
    {
        return Ok(value);
    }
    let id = {
        let mut state = hub.borrow_mut();
        state.next_timeline_id = state.next_timeline_id.wrapping_add(1).max(1);
        let id = state.next_timeline_id;
        state.default_timeline_ids.insert(key, id);
        id
    };
    let value = ctx.new_instance(DomDocumentTimeline {
        id,
        realm: Rc::downgrade(realm),
        origin_time_ms: 0.0,
    });
    if let Some(weak) = ctx.weak_value(&value) {
        let mut state = hub.borrow_mut();
        state.timelines.insert(key, weak.clone());
        state.timeline_values.insert(id, weak);
    }
    Ok(value)
}

fn refresh_ready(ctx: &mut Ctx, hub: &Rc<RefCell<AnimationHub>>, id: u32) -> OpResult<()> {
    let animation = existing_wrapper(ctx, hub, id)?;
    replace_resolved_ready(ctx, hub, id, animation);
    Ok(())
}

fn mark_ready_pending(ctx: &mut Ctx, hub: &Rc<RefCell<AnimationHub>>, id: u32) -> Value {
    Deferred::new_registered(ctx, |ctx, deferred| {
        let value = deferred.promise();
        let promise = ctx.weak_value(&value).expect("Promise is a GC object");
        hub.borrow_mut().ready.insert(id, ReadyPromise { promise, deferred: Some(deferred) });
        value
    })
}

fn resolve_ready(ctx: &mut Ctx, hub: &Rc<RefCell<AnimationHub>>, id: u32) -> OpResult<()> {
    let animation=existing_wrapper(ctx,hub,id)?;
    let deferred=hub.borrow_mut().ready.get_mut(&id).and_then(|ready|ready.deferred.take());
    if let Some(deferred)=deferred {deferred.resolve(ctx,animation);}
    Ok(())
}

fn reset_pending_task(ctx: &mut Ctx, hub: &Rc<RefCell<AnimationHub>>, id: u32) -> OpResult<()> {
    let pending=hub.borrow_mut().records.get_mut(&id).and_then(|record|record.pending_task.take());
    if pending.is_some() {
        let deferred=hub.borrow_mut().ready.get_mut(&id).and_then(|ready|ready.deferred.take());
        if let Some(deferred)=deferred {
            let reason=OpError::new("AbortError","animation playback was canceled").to_value(ctx);
            deferred.reject_handled(ctx,reason);
        }
        refresh_ready(ctx,hub,id)?;
    }
    Ok(())
}

fn request_pending_task(ctx: &mut Ctx, hub: &Rc<RefCell<AnimationHub>>, id: u32,
    kind: PlaybackState, auto_rewind: bool) -> OpResult<()> {
    let now=hub_time(hub);
    let had_pending={
        let mut state=hub.borrow_mut();
        let record=state.records.get_mut(&id).ok_or_else(||OpError::new("InvalidStateError","animation is no longer available"))?;
        if kind==PlaybackState::Paused && (record.pending_task==Some(kind)
            || (record.pending_task.is_none() && record.paused_at_ms.is_some())) {return Ok(());}
        let current=(!record.cancelled).then(||sample_record(record,now).current_time_ms);
        let rewind=auto_rewind && current.is_none_or(|time| if record.playback_rate>=0.0 {
            time<0.0 || time>=record_sampling_timing(record).end_time()
        } else {time<=0.0 || time>record_sampling_timing(record).end_time()});
        if kind==PlaybackState::Running && record.pending_task.is_none()
            && record.start_resolved && record.paused_at_ms.is_none() && !rewind {return Ok(());}
        let had_pending=record.pending_task.is_some();
        let revision = if record.pending_task == Some(kind) {
            record.pending_task_revision
        } else {
            record.pending_task_revision.checked_add(1)
                .ok_or_else(|| OpError::new("QuotaExceededError", "animation task revision exhausted"))?
        };
        if current.is_none() || rewind {
            let seek=if record.playback_rate<0.0 {record_sampling_timing(record).end_time()}else{0.0};
            if !seek.is_finite(){return Err(OpError::new("InvalidStateError","cannot rewind an infinite reverse animation"));}
            record.hold_time_ms=Some(seek);
        } else if kind==PlaybackState::Running && record.paused_at_ms.is_some() {
            record.hold_time_ms=current;
        }
        if record.hold_time_ms.is_some(){record.start_resolved=false;}
        record.pending_task=Some(kind);
        record.pending_task_revision = revision;
        record.cancelled=false;
        record.retired=false;
        record.finish_event_fired=false;
        had_pending
    };
    if !had_pending {mark_ready_pending(ctx,hub,id);}
    Ok(())
}

fn refresh_css_animation_for_accessor(ctx: &mut Ctx, hub: &Rc<RefCell<AnimationHub>>, id: u32) -> OpResult<()> {
    let owner={let state=hub.borrow();state.records.get(&id).filter(|record|record.css_name.is_some() || state.transitions.contains_key(&id)).and_then(|record|record.realm.upgrade())};
    if let Some(owner)=owner {refresh_css_transitions(ctx,hub,&owner,owner.timeline_sample.get())?;refresh_css_animations(ctx,hub,&owner,owner.timeline_sample.get())?;}
    Ok(())
}

fn settle_ready(ctx: &mut Ctx, hub: &Rc<RefCell<AnimationHub>>, timestamp_ms: f64) -> OpResult<()> {
    let pending=hub.borrow().records.iter().filter_map(|(id,record)|
        (record.pending_task.is_some() && !record.cancelled && record_time(record).is_some())
            .then_some((*id, record.pending_task_revision))).collect::<Vec<_>>();
    let mut publication = DispatchRetention { hub: hub.clone(), ids: Vec::new() };
    for (id, _) in &pending { publication.retain(*id); }
    for (id, revision) in pending {
        {
            let mut state=hub.borrow_mut();
            let Some(record)=state.records.get_mut(&id) else { continue; };
            if record.pending_task_revision != revision || record.cancelled
                || record.pending_task.is_none() || record_time(record).is_none() {
                continue;
            }
            match record.pending_task.take() {
                Some(PlaybackState::Running)=>{
                    if let Some(hold)=record.hold_time_ms {
                        record.start_time_ms=hold;record.start_ms=record_time(record).unwrap();
                        if record.playback_rate!=0.0 {record.hold_time_ms=None;}
                    }
                    record.start_resolved=true;record.paused_at_ms=None;
                }
                Some(PlaybackState::Paused)=>{
                    record.hold_time_ms=Some(sample_record(record,timestamp_ms).current_time_ms);
                    record.start_resolved=false;record.paused_at_ms=record_time(record);
                }
                _=>{}
            }
        }
        resolve_ready(ctx,hub,id)?;
    }
    Ok(())
}

fn keyframes_need_context(frames:&[Keyframe])->bool {
    frames.iter().any(|frame|frame.declarations.iter().any(|(property,value)|lumen_html::css::animation_value_needs_context(property,value)))
}

fn keyframes_need_underlying(frames:&[Keyframe],mode:CompositeMode)->bool {
    let (Some(first),Some(last))=(frames.first(),frames.last()) else{return false;};
    keyframes_need_color_context(frames)||mode!=CompositeMode::Replace||first.offset>0.0||last.offset<1.0
        ||frames.iter().any(|frame|frame.composite.is_some_and(|mode|mode!=CompositeMode::Replace)
            ||frame.declarations.iter().any(|(property,_)|
                !first.declarations.iter().any(|(name,_)|name==property)
                ||!last.declarations.iter().any(|(name,_)|name==property)))
}

// Rebase the bounded source snapshot from freshly sampled parent effects,
// without capturing this effect's previous output as its own underlying value.
const MAX_EFFECT_UNDERLYING_BYTES: usize = 16 * 1024 * 1024;
struct EffectSamplingBudget {bytes:usize,style_bytes:usize}
impl EffectSamplingBudget {
    fn add(&mut self,bytes:usize)->OpResult<()> {
        let bytes=self.bytes.checked_add(bytes).filter(|bytes|*bytes<=MAX_EFFECT_UNDERLYING_BYTES)
            .ok_or_else(||OpError::new("QuotaExceededError","effect underlying working-set limit"))?;
        self.bytes=bytes;Ok(())
    }
    fn style(&mut self,style:&lumen_html::css::Style,retain:bool)->OpResult<()> {
        let bytes=lumen_html::layout::checked_style_retained_bytes(style)
            .ok_or_else(||OpError::new("QuotaExceededError","effect underlying style limit"))?;
        let before=self.bytes;self.add(bytes)?;if retain {self.style_bytes=self.style_bytes.checked_add(bytes).ok_or_else(||OpError::new("QuotaExceededError","effect underlying style limit"))?;}else{self.bytes=before;}Ok(())
    }
fn temporary_style(&mut self,style:&lumen_html::css::Style,copies:usize)->OpResult<()> {
    let bytes=lumen_html::layout::checked_style_retained_bytes(style).and_then(|bytes|bytes.checked_mul(copies))
        .ok_or_else(||OpError::new("QuotaExceededError","computed endpoint style limit"))?;
    let checkpoint=self.bytes;self.add(bytes)?;self.bytes=checkpoint;Ok(())
}
    fn release_styles(&mut self) {
        self.bytes-=self.style_bytes;
        self.style_bytes=0;
    }
    fn pairs(&mut self,values:&BTreeMap<String,String>)->OpResult<usize> {
        let bytes=values.len().max(4).checked_mul(std::mem::size_of::<(String,String)>())
            .and_then(|bytes|values.iter().try_fold(bytes,|bytes,(name,value)|bytes.checked_add(name.len())?.checked_add(value.len())))
            .ok_or_else(||OpError::new("QuotaExceededError","effect underlying declaration limit"))?;
        let checkpoint=self.bytes;self.add(bytes)?;Ok(checkpoint)
    }
    fn reserve<K:Eq+std::hash::Hash,V>(&mut self,map:&mut HashMap<K,V>,additional:usize)->OpResult<()> {
        fn allocation_bytes<K,V>(capacity:usize)->Option<usize>{
            if capacity==0{return Some(0);}
            // HashMap::capacity counts usable entries, not allocated buckets.
            // Round a conservative extra slot to a power of two and include
            // each bucket's control byte plus the trailing control group.
            let buckets=capacity.checked_add(1)?.checked_next_power_of_two()?;
            buckets.checked_mul(std::mem::size_of::<(K,V)>().checked_add(1)?)?.checked_add(16)
        }
        let failure=||OpError::new("QuotaExceededError","effect underlying map limit");
        let count=map.len().checked_add(additional).ok_or_else(failure)?;
        let old=map.capacity();if count<=old{return Ok(());}
        let upper=count.checked_mul(2).and_then(|value|value.checked_add(1)).ok_or_else(failure)?;
        let old_bytes=allocation_bytes::<K,V>(old).ok_or_else(failure)?;
        let before=self.bytes;
        self.add(allocation_bytes::<K,V>(upper).and_then(|bytes|bytes.checked_sub(old_bytes)).ok_or_else(failure)?)?;
        map.try_reserve(additional).map_err(|_|OpError::new("QuotaExceededError","effect underlying map allocation"))?;
        self.bytes=before;
        self.add(allocation_bytes::<K,V>(map.capacity()).and_then(|bytes|bytes.checked_sub(old_bytes)).ok_or_else(failure)?)
    }


}

struct EffectUnderlyingSource<'a> {order:usize,input:&'a lumen_html::session::TransitionStyleInput}

fn effect_underlying_style<'a>(target:(NodeId,Option<lumen_html::css::PseudoElement>),own:bool,
    inputs:&HashMap<(NodeId,Option<lumen_html::css::PseudoElement>),EffectUnderlyingSource<'a>>,
    declarations:&HashMap<(NodeId,Option<lumen_html::css::PseudoElement>,bool),BTreeMap<String,String>>,
    document:&Document,environment:lumen_html::css::MediaEnvironment,text:Option<&dyn lumen_html::paint::TextShaper>,
    prepared:&mut HashMap<(NodeId,Option<lumen_html::css::PseudoElement>),Arc<lumen_html::css::Style>>,budget:&mut EffectSamplingBudget,depth:usize,
)->OpResult<Arc<lumen_html::css::Style>> {
    if own {if let Some(style)=prepared.get(&target){return Ok(style.clone());}}
    if depth>=512{return Err(OpError::new("InvalidStateError","effect inheritance depth exceeded"));}
    let input=inputs.get(&target).map(|source|source.input).ok_or_else(||OpError::new("InvalidStateError","effect underlying source unavailable"))?;
    let parent=input.parent.and_then(|node|inputs.get(&(node,None)).map(|source|source.input));
    let resolved_parent=parent.map(|parent|effect_underlying_style((parent.node,None),true,inputs,declarations,document,environment,text,prepared,budget,depth+1)).transpose()?;
    if own{budget.reserve(prepared,1)?;}
    let checkpoint=budget.bytes;
    let source_bytes=lumen_html::layout::checked_style_retained_bytes(&input.style)
        .ok_or_else(||OpError::new("QuotaExceededError","effect underlying source limit"))?;
    budget.add(source_bytes)?;
    let mut style=input.style.clone();
    Arc::make_mut(&mut style).apply_animation_snapshot(document,input.node,resolved_parent.as_deref(),input.pseudo,text,
        environment,&[],&input.eligibility).map_err(|error|OpError::new("SyntaxError",format!("effect underlying inheritance failed: {error:?}")))?;
    if own {
        for transition in [false,true] {
            if let Some(values)=declarations.get(&(input.node,input.pseudo,transition)).filter(|values|!values.is_empty()){
                let checkpoint=budget.pairs(values)?;
                let pairs=values.iter().map(|(name,value)|(name.clone(),value.clone())).collect::<Vec<_>>();
                let unrestricted=transition.then(lumen_html::css::EffectEligibility::all);
                let eligibility=unrestricted.as_ref().unwrap_or(&input.eligibility);
                Arc::make_mut(&mut style).apply_animation_snapshot(document,input.node,resolved_parent.as_deref(),input.pseudo,text,
                    environment,&pairs,eligibility).map_err(|error|OpError::new("SyntaxError",format!("effect parent sample failed: {error:?}")))?;
                drop(pairs);budget.bytes=checkpoint;
            }
        }

        budget.bytes=checkpoint;
        budget.style(&style,true)?;
        prepared.insert(target,style.clone());

    }
    if !own{budget.bytes=checkpoint;budget.style(&style,false)?;}
    Ok(style)
}

fn apply_realm_target(hub:&Rc<RefCell<AnimationHub>>,realm:&Rc<DomRealm>,now:f64,target:Option<NodeId>)->OpResult<()> {
    let demand=EffectDemand::new(realm,target)?;
    apply_realm_in_phase_with_demand(hub,realm,now,EffectStylePhase::Current,&demand)
}
fn apply_record_change(hub:&Rc<RefCell<AnimationHub>>,realm:&Rc<DomRealm>,now:f64,id:u32)->OpResult<()> {
    let state=hub.borrow();
    let record=state.records.get(&id).filter(|record|record.realm.upgrade().is_some_and(|owner|Rc::ptr_eq(&owner,realm)));
    let target=record.and_then(|record|record.node);
    let property=record.filter(|record|record.keyframes.iter().flat_map(|frame|frame.declarations.iter())
        .all(|(name,_)|!lumen_html::css::computed_values::needs_layout(name)))
        .and_then(|record|record.keyframes.iter().flat_map(|frame|frame.declarations.iter()).next().map(|(name,_)|name.as_str()));
    let demand=match property{Some(property)=>EffectDemand::for_property(hub,realm,target,property)?,None=>EffectDemand::new(realm,target)?};
    drop(state);
    apply_realm_in_phase_with_demand(hub,realm,now,EffectStylePhase::Current,&demand)
}
fn apply_effect_change(hub:&Rc<RefCell<AnimationHub>>,realm:&Rc<DomRealm>,now:f64,id:u32)->OpResult<()> {
    let target=hub.borrow().effect_records.get(&id).filter(|effect|effect.realm.upgrade().is_some_and(|owner|Rc::ptr_eq(&owner,realm))).and_then(|effect|effect.node);
    apply_realm_target(hub,realm,now,target)
}

fn apply_realm(hub: &Rc<RefCell<AnimationHub>>, realm: &Rc<DomRealm>, now: f64) -> OpResult<()> {
    apply_realm_with_underlying(hub,realm,now,false)
}

#[derive(Clone,Copy,PartialEq,Eq)]
enum EffectStylePhase {Current,BeforeChange,AfterChange}

fn apply_realm_with_underlying(hub:&Rc<RefCell<AnimationHub>>,realm:&Rc<DomRealm>,now:f64,before_change:bool)->OpResult<()> {
    apply_realm_in_phase(hub,realm,now,if before_change{EffectStylePhase::BeforeChange}else{EffectStylePhase::Current})
}

// Rendering consumes connected effects. An explicit style read additionally
// consumes its detached target and actual composed ancestor lineage. Retained
// detached Animation objects remain live and keep their timing/keyframes; they
// do not force unrelated expensive underlying-style/font acquisition.
#[derive(Default)]
struct EffectDemand { detached: Vec<u128>, scoped_connected:bool }
impl EffectDemand {
    fn new(realm:&DomRealm,target:Option<NodeId>)->OpResult<Self> {Self::new_with_scope(realm,target,false)}
    fn new_with_scope(realm:&DomRealm,target:Option<NodeId>,scoped_connected:bool)->OpResult<Self> {
        let mut result=Self{scoped_connected,..Self::default()};
        let session=realm.session.borrow();let document=session.document();
        if let Some(mut node)=target.filter(|node|scoped_connected||!document.is_connected_element(*node)) {
            // The existing ancestry limit bounds this operation-local selection.
            // Connected render opportunities keep the allocation-free default.
            for depth in 0..512 {
                if matches!(document.kind(node),Ok(NodeKind::Element{..})) {
                    result.detached.try_reserve(1).map_err(|_|OpError::new("QuotaExceededError","effect demand ancestry admission failed"))?;
                    if result.detached.capacity().checked_mul(core::mem::size_of::<u128>())
                        .is_none_or(|bytes|bytes>MAX_EFFECT_UNDERLYING_BYTES) {
                        return Err(OpError::new("QuotaExceededError","effect demand ancestry exceeds budget"));
                    }
                    result.detached.push(node.key());
                }
                let parent=document.composed_parent(node).map_err(|_|OpError::new("InvalidStateError","effect demand target unavailable"))?;
                let Some(parent)=parent else{result.detached.sort_unstable();result.detached.dedup();return Ok(result);};
                node=parent;
                if depth==511{return Err(OpError::new("InvalidStateError","effect demand ancestry exceeds limit"));}
            }
        }
        Ok(result)
    }
    fn for_property(hub:&Rc<RefCell<AnimationHub>>,realm:&DomRealm,target:Option<NodeId>,property:&str)->OpResult<Self>{
        let name=if !property.starts_with("--")&&property.bytes().any(|byte|byte.is_ascii_uppercase()){std::borrow::Cow::Owned(property.to_ascii_lowercase())}else{std::borrow::Cow::Borrowed(property)};
        if target.is_none()||name.starts_with("--")||lumen_html::css::computed_values::needs_layout(&name){return Self::new(realm,target);}
        let demand=Self::new_with_scope(realm,target,true)?;
        // Query geometry and source transform reference boxes can depend on
        // effects outside this owner chain. Keep the full rendering demand.
        let mut session=realm.session.borrow_mut();
        if let Some(node)=target {
            if session.computed_style(node).map_err(|_|OpError::error("effect demand owner style resolution failed"))?.has_query_container_dependencies(){drop(session);return Self::new(realm,target);}
        }
        let geometry=hub.borrow().records.values().filter(|record|demand.includes(session.document(),record.node))
            .any(|record|record.keyframes.iter().any(|frame|frame.declarations.iter().any(|(property,value)|
                property=="transform"||property.starts_with("--")||lumen_html::css::animation_shadow_query_dependent(value))));
        drop(session);
        if geometry{Self::new(realm,target)}else{Ok(demand)}
    }
    fn includes(&self,document:&Document,node:Option<NodeId>)->bool {
        node.is_some_and(|node|self.detached.binary_search(&node.key()).is_ok()||!self.scoped_connected&&document.is_connected_element(node))
    }
}

fn apply_realm_in_phase(hub:&Rc<RefCell<AnimationHub>>,realm:&Rc<DomRealm>,now:f64,phase:EffectStylePhase)->OpResult<()> {
    apply_realm_in_phase_with_demand(hub,realm,now,phase,&EffectDemand::default())
}

fn apply_realm_in_phase_with_demand(hub:&Rc<RefCell<AnimationHub>>,realm:&Rc<DomRealm>,now:f64,
    phase:EffectStylePhase,demand:&EffectDemand)->OpResult<()> {

    let before_change=phase==EffectStylePhase::BeforeChange;
    // Timeline authority belongs to every retained same-realm record,
    // independently of whether its target paint/style is demanded.
    let has_progress=hub.borrow().records.values().any(|record|record.progress_timeline.is_some()
        && record.realm.upgrade().is_some_and(|owner|Rc::ptr_eq(&owner,realm)) && !record.cancelled);
    if has_progress {
        realm.flush_layout()?;
        let samples=hub.borrow().records.values().filter(|record|!record.cancelled
            && record.realm.upgrade().is_some_and(|owner|Rc::ptr_eq(&owner,realm)))
            .filter_map(|record|record.progress_timeline.as_ref().map(|binding|(record.id,progress_time(realm,binding))))
            .collect::<Vec<_>>();
        for (id,time) in samples {
            if let Some(binding)=hub.borrow_mut().records.get_mut(&id).and_then(|record|record.progress_timeline.as_mut()) {binding.sampled_time=time;}
        }
    }
    let key = Rc::as_ptr(realm) as usize;
    let mut snapshot = hub
        .borrow()
        .records
        .values()
        .filter(|record| {
            !record.cancelled && record.effect_id.is_some()
                && demand.includes(realm.session.borrow().document(),record.node)
                && (phase!=EffectStylePhase::AfterChange || !hub.borrow().transitions.contains_key(&record.id))
                && record.effect_id.is_none_or(|id|hub.borrow().effect_records.get(&id).is_none_or(|effect|effect.capture_name.is_none()))
                && record
                    .realm
                    .upgrade()
                    .is_some_and(|candidate| Rc::ptr_eq(&candidate, realm))
        })
        .map(|record| PaintSnapshot {
            iteration_accumulate:record.effect_id.and_then(|id|hub.borrow().effect_records.get(&id).map(|effect|effect.iteration_accumulate)).unwrap_or(false),
            id: record.id, css_order: record.css_order, node: record.node,
            pseudo:record.effect_id.and_then(|id|hub.borrow().effect_records.get(&id).and_then(|effect|effect.pseudo)),
            transition_origin:hub.borrow().transitions.get(&record.id).is_some_and(|transition|transition.associated),
            transition_class:hub.borrow().transitions.contains_key(&record.id),
            endpoint_styles:hub.borrow().transitions.get(&record.id).and_then(|transition|transition.endpoint_styles.clone()),
            keyframes: record.keyframes.clone(),
            easing: record.easing.clone(), composite: record.composite,
            sample: sample_record(record, now),
        })
        .collect::<Vec<_>>();
    let needs_source=|record:&PaintSnapshot|record.sample.progress.is_some()
        && (keyframes_need_underlying(&record.keyframes,record.composite)||keyframes_need_context(&record.keyframes));
    let needs_underlying=snapshot.iter().any(needs_source);
    // The retained transition before-change source remains authoritative. Fresh
    // neutral/additive sources only need their target and inherited lineage.
    let underlying_snapshot=needs_underlying.then(||hub.borrow().transition_snapshots.get(&key).map(|(_,snapshot)|snapshot.clone())).flatten();
    let mut fresh_sources=None;
    let mut underlying_budget=EffectSamplingBudget{bytes:0,style_bytes:0};
    underlying_budget.add(demand.detached.capacity().checked_mul(core::mem::size_of::<u128>()).ok_or_else(||OpError::new("QuotaExceededError","effect demand working-set limit"))?)?;
    let mut underlying_inputs=HashMap::new();
    let mut wanted=HashMap::new();
    for record in snapshot.iter().filter(|record|needs_source(record)) {
        if let Some(node)=record.node {
            let target=(node,record.pseudo);
            if !wanted.contains_key(&target){underlying_budget.reserve(&mut wanted,1)?;wanted.insert(target,true);}
        }
    }
    if let Some(source)=underlying_snapshot.as_ref(){
        // Inputs are captured parent-before-child. One reverse pass finds the
        // exact old lineage even when current DOM parentage has changed. Borrow
        // only selected entries; do not clone or index every document style.
        for (order,input) in source.nodes.iter().enumerate().rev(){
            let target=(input.node,input.pseudo);
            if !wanted.contains_key(&target){continue;}
            underlying_budget.reserve(&mut underlying_inputs,1)?;
            underlying_inputs.insert(target,EffectUnderlyingSource{order,input});
            if let Some(parent)=input.parent {
                let target=(parent,None);
                if !wanted.contains_key(&target){underlying_budget.reserve(&mut wanted,1)?;wanted.insert(target,false);}
            }
        }
    }
    let missing_source=needs_underlying&&snapshot.iter().filter(|record|needs_source(record))
        .filter_map(|record|record.node.map(|node|(node,record.pseudo))).any(|target|!underlying_inputs.contains_key(&target));
    let current_source_needed=!before_change&&snapshot.iter().any(|record|!record.transition_origin&&needs_source(record));
    if missing_source || current_source_needed {
        let count=wanted.values().filter(|explicit|**explicit).count();
        let target_bytes=count.checked_mul(core::mem::size_of::<(NodeId,Option<lumen_html::css::PseudoElement>)>()).ok_or_else(||OpError::new("QuotaExceededError","effect underlying target limit"))?;
        underlying_budget.add(target_bytes)?;
        let mut targets=Vec::new();targets.try_reserve_exact(count).map_err(|_|OpError::new("QuotaExceededError","effect underlying target limit"))?;
        let capacity_bytes=targets.capacity().checked_mul(core::mem::size_of::<(NodeId,Option<lumen_html::css::PseudoElement>)>()).ok_or_else(||OpError::new("QuotaExceededError","effect underlying target limit"))?;
        underlying_budget.add(capacity_bytes.checked_sub(target_bytes).ok_or_else(||OpError::new("QuotaExceededError","effect underlying target limit"))?)?;
        // Each target is captured once even when many effects share its stack.
        // Old-lineage ancestors discovered above are not fresh capture targets.
        for (&target,&explicit) in &wanted {if explicit {targets.push(target);}}
        fresh_sources=Some(realm.session.borrow_mut().effect_underlying_snapshot_with_text(&targets,None).map_err(|error|OpError::new("InvalidStateError",format!("new effect underlying source failed: {error:?}")))?);
    }
    if let Some(source)=fresh_sources.as_ref(){
        let additional=source.nodes.iter().filter(|input|!underlying_inputs.contains_key(&(input.node,input.pseudo))).count();
        underlying_budget.reserve(&mut underlying_inputs,additional)?;
        for (order,input) in source.nodes.iter().enumerate(){
            underlying_inputs.entry((input.node,input.pseudo)).or_insert(EffectUnderlyingSource{order,input});
        }
    }
    let mut current_underlying_inputs=HashMap::new();
    if current_source_needed {
        if let Some(source)=fresh_sources.as_ref() {
            underlying_budget.reserve(&mut current_underlying_inputs,source.nodes.len())?;
            for (order,input) in source.nodes.iter().enumerate(){current_underlying_inputs.insert((input.node,input.pseudo),EffectUnderlyingSource{order,input});}
        }
    }
    let mut current_underlying_parents=HashMap::new();
    let mut underlying_parents=HashMap::new();
    let current_registrations=realm.session.borrow().registered_custom_property_snapshot();
    // CSS animations precede script-created Web Animations in the effect
    // stack; later CSS list entries win among CSS animations.
    snapshot.sort_by_key(|record| if record.transition_class {(0u8,record.id as u64)} else {match record.css_order {
        Some(order) => (1u8, order as u64),
        None => (2u8, record.id as u64),
    }});
    if needs_underlying {snapshot.sort_by_key(|record|record.node.and_then(|node|{
        let target=(node,record.pseudo);
        let source=if before_change{underlying_inputs.get(&target)}else{current_underlying_inputs.get(&target).or_else(||underlying_inputs.get(&target))};
        source.map(|source|source.order)
    }).unwrap_or(usize::MAX));}
    let mut declarations = HashMap::<(NodeId,Option<lumen_html::css::PseudoElement>,bool), BTreeMap<String, String>>::new();

    let registered_transform=|name:&str|name.starts_with("--") && current_registrations.as_deref().unwrap_or(&[]).iter().find(|registration|registration.name==name)
        .is_some_and(|registration|registration.syntax.contains("<transform-"));
    let has_transforms = snapshot.iter().filter(|record|record.sample.progress.is_some()).any(|record| {
        record.keyframes.iter().any(|frame| {
            frame
                .declarations
                .iter()
                .any(|(name, _)| matches!(name.as_str(),"transform"|"translate"|"rotate"|"scale") || registered_transform(name))
        })
    });
    // Resolve layout-affecting animation values first. Matrix fallback for a
    // percentage translation must use the same frame's animated reference box.
    let has_shadows=snapshot.iter().filter(|record|record.sample.progress.is_some()).any(|record|record.keyframes.iter().any(|frame|frame.declarations.iter().any(|(name,_)|matches!(name.as_str(),"box-shadow"|"text-shadow"|"border-image-slice"|"border-image-width"|"border-image-outset"))));
    let context_dependencies=snapshot.iter().filter(|record|record.sample.progress.is_some()).any(|record|keyframes_need_context(&record.keyframes));
    let shadow_fonts=if has_shadows || context_dependencies {crate::canvas::initialized_realm_font_source(realm)?}else{None};
    let shadow_text=shadow_fonts.as_ref().map(|fonts|&**fonts as &dyn lumen_html::paint::TextShaper);
    // A currentColor endpoint consumes the target's sampled color, including
    // another property on this same effect. Establish that dependency first;
    // ordinary effects retain the existing three-pass fast path.
    let color_dependencies=snapshot.iter().filter(|record|record.sample.progress.is_some()).any(|record|keyframes_need_color_context(&record.keyframes));
let property_pass=|name:&str| {
    if matches!(name,"transform"|"translate"|"rotate"|"scale") {3}
    else if matches!(name,"box-shadow"|"text-shadow"|"border-image-slice"|"border-image-width"|"border-image-outset"){2}
    else if registered_transform(name){3}
    else if context_dependencies && name.starts_with("--"){6}
    else if context_dependencies && name=="line-height"{5}
    else if color_dependencies && name=="color" || context_dependencies && (name.starts_with("font-")||matches!(name,"direction"|"writing-mode")){0}
    else{1}
};
    let font_endpoints=snapshot.iter().filter(|record|record.sample.progress.is_some()).any(|record|record.keyframes.iter().any(|frame|frame.declarations.iter()
        .any(|(name,value)|name.starts_with("--")&&lumen_html::css::animation_value_font_dependent(value))));
    let owner_font_changes=font_endpoints&&(snapshot.iter().filter(|record|record.sample.progress.is_some()).any(|record|record.keyframes.iter().any(|frame|frame.declarations.iter()
        .any(|(name,_)|name.starts_with("font-")||name=="font"||name=="line-height")))
        || underlying_inputs.values().chain(current_underlying_inputs.values()).any(|source|source.input.eligibility.font_context_uses_custom_values()));
    // Borrow authored names for this sampling operation only. Charge the
    // existing bounded working set; no per-target retained cache is added.
    let mut font_endpoints=HashMap::<(NodeId,Option<lumen_html::css::PseudoElement>,&str),()>::new();
    if owner_font_changes {
        for record in snapshot.iter().filter(|record|record.sample.progress.is_some()) {
            let Some(node)=record.node else{continue;};
            for frame in record.keyframes.iter() {for (property,value) in &frame.declarations {
                if property.starts_with("--") && lumen_html::css::animation_value_font_dependent(value) {
                    let key=(node,record.pseudo,property.as_str());
                    if !font_endpoints.contains_key(&key){underlying_budget.reserve(&mut font_endpoints,1)?;font_endpoints.insert(key,());}
                }
            }}
        }
    }
    let font_endpoint=|node:NodeId,pseudo:Option<lumen_html::css::PseudoElement>,property:&str|font_endpoints.contains_key(&(node,pseudo,property));
    let mut query_geometry_ready=false;
    for &phase in if color_dependencies || context_dependencies {&[6u8,0,5,7,1,2,3][..]}else{&[1u8,2,3][..]} {
        // The first custom pass supplies variables consumed by font properties.
        // After font and line-height sampling, compute custom endpoints against
        // that same owner context as ordinary lengths. Discard earlier custom
        // outputs before replay so each additive effect composes exactly once.
        if phase==7 && !owner_font_changes {continue;}
        if phase==7 {
            for (&(node,pseudo,_),values) in declarations.iter_mut(){values.retain(|property,_|!font_endpoint(node,pseudo,property));}
        }
        let pass=if phase==7{6}else{phase};
        // Parent samples depend on the preceding pass's published properties.
        underlying_parents.clear();
        current_underlying_parents.clear();
        underlying_budget.release_styles();
        let transform_pass=pass==3;
        for record in &snapshot {
            if !record.keyframes.iter().any(|frame| frame.declarations.iter()
                .any(|(name, _)| property_pass(name) == pass && (phase!=7 || record.node.is_some_and(|node|font_endpoint(node,record.pseudo,name))))) {
                continue;
            }
            let sample = record.sample;
            if let Some(progress) = sample.progress {
                let eased = animation::ease_with_before(&record.easing, progress, sample.before)
                    .unwrap_or(progress);
                if let Some(node) = record.node {
                    let registrations=if record.transition_origin {
                        underlying_inputs.get(&(node,record.pseudo)).and_then(|source|source.input.eligibility.registered_custom_property_snapshot())
                            .or_else(||current_registrations.clone())
                    }else{current_registrations.clone()};
                    let registered_syntax=|property:&str|registrations.as_deref().unwrap_or(&[]).iter().find(|registration|registration.name==property).map(|registration|registration.syntax.as_str());
                    let base_style=if needs_source(record) {
                        let session=realm.session.borrow();
                        let (inputs,parents)=if before_change||record.transition_origin{(&underlying_inputs,&mut underlying_parents)}else{(&current_underlying_inputs,&mut current_underlying_parents)};
                        Some(effect_underlying_style((node,record.pseudo),false,inputs,&declarations,session.document(),
                            session.media_environment(),shadow_text,parents,&mut underlying_budget,0)?)
                    }else{None};
                    let color_style=if pass==1&&keyframes_need_color_context(&record.keyframes) {
                        let session=realm.session.borrow();
                        let (inputs,parents)=if before_change||record.transition_origin{(&underlying_inputs,&mut underlying_parents)}else{(&current_underlying_inputs,&mut current_underlying_parents)};
                        Some(effect_underlying_style((node,record.pseudo),true,inputs,&declarations,session.document(),
                            session.media_environment(),shadow_text,parents,&mut underlying_budget,0)?)
                    }else{None};
                    let context_style=if keyframes_need_context(&record.keyframes) {
                        let session=realm.session.borrow();
                        let (inputs,parents)=if before_change||record.transition_origin{(&underlying_inputs,&mut underlying_parents)}else{(&current_underlying_inputs,&mut current_underlying_parents)};
                        Some(effect_underlying_style((node,record.pseudo),true,inputs,&declarations,session.document(),
                            session.media_environment(),shadow_text,parents,&mut underlying_budget,0)?)
                    }else{None};
                    let context_parent=if context_style.is_some() {
                        let session=realm.session.borrow();
                        let (inputs,parents)=if before_change||record.transition_origin{(&underlying_inputs,&mut underlying_parents)}else{(&current_underlying_inputs,&mut current_underlying_parents)};
                        inputs.get(&(node,record.pseudo)).and_then(|source|source.input.parent)
                            .map(|parent|effect_underlying_style((parent,None),true,inputs,&declarations,session.document(),
                                session.media_environment(),shadow_text,parents,&mut underlying_budget,0)).transpose()?
                    }else{None};
                    let mut endpoint_query=if context_style.is_some() {
                        let inputs=if before_change||record.transition_origin{&underlying_inputs}else{&current_underlying_inputs};
                        inputs.get(&(node,record.pseudo)).and_then(|source|source.input.eligibility.query_context())
                            .unwrap_or_else(||lumen_html::css::ContainerUnitContext::no_container(realm.session.borrow().media_environment()))
                    }else{lumen_html::css::ContainerUnitContext::default()};
                    // Query-dependent computed endpoints consume actual container
                    // geometry, through the existing layout/context authority.
                    if context_style.is_some() && record.keyframes.iter().any(|frame|frame.declarations.iter()
                        .any(|(_,value)|lumen_html::css::animation_shadow_query_dependent(value))) {
                        if !before_change && !record.transition_origin {
                            if !query_geometry_ready {realm.flush_layout()?;query_geometry_ready=true;}
                            endpoint_query=realm.session.borrow_mut().query_container_context(node)
                                .map_err(|_|OpError::new("InvalidStateError","animation endpoint query context unavailable"))?;
                        }
                    }
                    let mut endpoint_failure=None;
                    let mut endpoint_budget_failure=None;
                    let mut shadow_query=lumen_html::css::ContainerUnitContext::default();
                    let color_context=pass==1 && keyframes_need_color_context(&record.keyframes);
                    let transform_context = if transform_pass || pass==2 || color_context {
                        let mut session = realm.session.borrow_mut();
                        let mut style = match record.pseudo {
                            Some(pseudo)=>session.computed_pseudo_style_with_text(node,pseudo,shadow_text),
                            None=>session.computed_style_with_text(node,shadow_text),
                        }.map_err(|_|OpError::new("Error","could not resolve animation computed context"))?;
                        let viewport=session.media_environment();
                        shadow_query=lumen_html::css::ContainerUnitContext::no_container(viewport);
                        let query_dependent=color_context || pass==2 && record.keyframes.iter().any(|frame|frame.declarations.iter()
                            .any(|(property,value)|matches!(property.as_str(),"box-shadow"|"text-shadow"|"border-image-slice"|"border-image-width"|"border-image-outset")
                                && lumen_html::css::animation_shadow_query_dependent(value)));
                        if (pass==2||color_context) && (query_dependent || style.font_query_context_pending()) {
                            shadow_query=session.query_container_context(node).map_err(|_|OpError::new("Error","could not resolve animation query context"))?;
                            let needs_geometry=style.font_query_context_pending() || color_context && record.keyframes.iter().any(|frame|frame.declarations.iter().any(|(property,value)|
                                lumen_html::css::animation_color_property(property) && lumen_html::css::animation_color_needs_context(value)
                                    && lumen_html::css::parse_animation_source_color(value,color_style.as_deref().unwrap_or(&style),viewport,shadow_text,shadow_query).is_none())) || record.keyframes.iter().any(|frame|frame.declarations.iter()
                                .any(|(property,value)|matches!(property.as_str(),"box-shadow"|"text-shadow"|"border-image-slice"|"border-image-width"|"border-image-outset")
                                    && lumen_html::css::animation_shadow_query_dependent(value)
                                    && if matches!(property.as_str(),"border-image-slice"|"border-image-width"|"border-image-outset") {
                                        lumen_html::css::parse_animation_border_image_with_query(property,value,&style,viewport,shadow_text,shadow_query).is_none()
                                    }else{lumen_html::css::parse_animation_shadows_with_query(property,value,&style,viewport,shadow_text,shadow_query).is_none()}));
                            if needs_geometry && !query_geometry_ready {
                                drop(session);
                                realm.flush_layout()?;
                                query_geometry_ready=true;
                                session=realm.session.borrow_mut();
                                style=match record.pseudo {
                                    Some(pseudo)=>session.computed_pseudo_style_with_text(node,pseudo,shadow_text),
                                    None=>session.computed_style_with_text(node,shadow_text),
                                }.map_err(|_|OpError::new("Error","could not resolve animation computed query context"))?;
                                shadow_query=session.query_container_context(node).map_err(|_|OpError::new("Error","could not resolve animation query context"))?;
                            }
                        }
                        let registered_transform_reference=transform_pass && record.keyframes.iter().any(|frame|frame.declarations.iter().any(|(property,_)|property.starts_with("--") && registered_syntax(property).is_some_and(|syntax|syntax.contains("<transform-"))));
                        let needs_reference=registered_transform_reference || transform_pass && record.keyframes.iter().flat_map(|frame|frame.declarations.iter().map(|(property,value)|(property,value)))
                            .any(|(property,value)|property=="transform"
                                && lumen_html::css::parse_animation_transforms(value,&style,viewport,shadow_text)
                                    .is_some_and(|transforms|transforms.iter().any(|transform|matches!(transform,lumen_html::css::Transform::Translate(x,y)if x.percent!=0.0||y.percent!=0.0))))
                            || transform_pass&&base_style.as_ref().is_some_and(|style|style.transforms.as_deref().is_some_and(|transforms|transforms.iter().any(|transform|matches!(transform,lumen_html::css::Transform::Translate(x,y)if x.percent!=0.0||y.percent!=0.0))));
                        if transform_pass && (needs_reference || style.font_query_context_pending()) && !query_geometry_ready {
                            drop(session);realm.flush_layout()?;query_geometry_ready=true;session=realm.session.borrow_mut();
                            style=match record.pseudo {
                                Some(pseudo)=>session.computed_pseudo_style_with_text(node,pseudo,shadow_text),
                                None=>session.computed_style_with_text(node,shadow_text),
                            }.map_err(|_|OpError::new("Error","could not resolve transform reference context"))?;
                        }
                        // Shadow and border-image lengths consume computed font/color/query context;
                        // only transforms consume an actual layout reference box.
                        let rect=if needs_reference {
                            record.pseudo.map_or_else(||session.layout_rect(node),|pseudo|session.pseudo_layout_rect(node,pseudo))
                                .unwrap_or(lumen_html::paint::Rect{x:0.0,y:0.0,width:0.0,height:0.0})
                        }else{lumen_html::paint::Rect{x:0.0,y:0.0,width:0.0,height:0.0}};
                        Some((style,rect,viewport))
                    }else{None};
                    if !record.transition_origin {
                        // The highest effect decides the cascade origin for
                        // this property's one composited stack result. Move a
                        // lower transition result into that stack, retaining
                        // authored property identities and owned string payloads.
                        for frame in record.keyframes.iter() {
                            for(property,_)in &frame.declarations {
                                if property_pass(property)!=pass || phase==7 && !font_endpoint(node,record.pseudo,property){continue;}
                                let lower=declarations.get_mut(&(node,record.pseudo,true)).and_then(|values|values.remove_entry(property));
                                if let Some((property,value))=lower {
                                    declarations.entry((node,record.pseudo,false)).or_default().entry(property).or_insert(value);
                                }
                            }
                        }
                    }
                    let properties = declarations.entry((node,record.pseudo,record.transition_origin)).or_default();

                    let mut composition_failed = false;
                    let sampled = animation::sample_keyframes_composed_with_neutral_property_filter(
                        &record.keyframes,
                        eased,
                        sample.before,
                        |property| property_pass(property) == pass && (phase!=7 || font_endpoint(node,record.pseudo,property)),
                        |property, frame, value| {
                            let registered_numeric=property.starts_with("--") && registered_syntax(property).is_some_and(|syntax|syntax!="*");
                            let mut live_base=properties.get(property).cloned()
                                .or_else(||base_style.as_ref().and_then(|style|base_css_value(style,property)));
                            let neutral=value.is_none();
                            let mut compute_endpoint=|value:&str| {
                            let needs_context=lumen_html::css::animation_value_needs_context(property,value);
                            let color=if lumen_html::css::animation_color_property(property)
                                && lumen_html::css::animation_color_needs_context(value) {
                                color_style.as_ref().or(base_style.as_ref()).and_then(|style| {
                                    let viewport=realm.session.borrow().media_environment();
                                    lumen_html::css::parse_animation_source_color(value,style,viewport,shadow_text,shadow_query).and_then(|source|source.serialize())
                                })
                            }else{None};
                            let value=color.as_deref().unwrap_or(value);
                            let computed=if needs_context {
                                let style=context_style.as_ref()?;
                                if let Err(error)=underlying_budget.temporary_style(style,2) {endpoint_budget_failure=Some(error);return None;}
                                let session=realm.session.borrow();
                                match style.computed_animation_endpoint_with_registrations(session.document(),node,context_parent.as_deref(),record.pseudo,
                                    property,value,shadow_text,session.media_environment(),endpoint_query,registrations.clone()) {
                                    Ok(Some(value))=>Some(value),
                                    Ok(None)=>return None,
                                    Err(error)=>{endpoint_failure=Some(error);return None;},
                                }
                            }else{None};
                            let value=computed.as_deref().unwrap_or(value);
                            Some(value.to_owned())
                            };
                            // A newly registered property can be absent from the old
                            // before-change input. Resolve its current initial value
                            // through the same endpoint cascade, not a string default.
                            if live_base.is_none() && registered_numeric {live_base=compute_endpoint("initial");}
                            let value=value.or(live_base.as_deref())?;
                            let mode=if neutral{CompositeMode::Replace}else{frame.and_then(|frame|frame.composite).unwrap_or(record.composite)};
                            let resolved=if neutral {value.to_owned()}else{compute_endpoint(value)?};
                            let value=resolved.as_str();

                            let base=live_base.as_deref();
                            let composed = if mode==CompositeMode::Replace || property.starts_with("--") && !registered_numeric {Some(value.to_owned())} else {base.and_then(|base| {
                                if registered_numeric {
                                    let syntax=registered_syntax(property)?;
                                    return lumen_html::css::registered_properties::combine_computed_values_with_reference(syntax,base,value,
                                        if mode==CompositeMode::Accumulate {lumen_html::css::registered_properties::ComputedValueOperation::Accumulate(1.0)}else{lumen_html::css::registered_properties::ComputedValueOperation::Add},
                                        None).or_else(||Some(value.to_owned()));
                                }
                                if matches!(property,"translate"|"rotate"|"scale"){
                                    let(style,_,viewport)=transform_context.as_ref()?;
                                    return lumen_html::css::combine_animation_individual_transform(property,base,value,
                                        if mode==CompositeMode::Add{lumen_html::css::registered_properties::ComputedValueOperation::Add}else{lumen_html::css::registered_properties::ComputedValueOperation::Accumulate(1.0)},style,*viewport,shadow_text,shadow_query);
                                }
                                if property == "transform" {
                                    let (style, rect, viewport) = transform_context.as_ref()?;
                                    if lumen_html::css::animation_transform_has_deferred_source(base)||lumen_html::css::animation_transform_has_deferred_source(value) {
                                        return lumen_html::css::combine_animation_source_transforms(base,value,
                                            if mode==CompositeMode::Add{lumen_html::css::registered_properties::ComputedValueOperation::Add}else{lumen_html::css::registered_properties::ComputedValueOperation::Accumulate(1.0)},
                                            style,*viewport,None,[rect.width as f64,rect.height as f64]);
                                    }
                                    let mut from = lumen_html::css::parse_animation_transforms(
                                        base, style, *viewport, None,
                                    )?
                                    .to_vec();
                                    let to = lumen_html::css::parse_animation_transforms(
                                        value, style, *viewport, None,
                                    )?;
                                    if mode == CompositeMode::Add {
                                        from.extend_from_slice(&to);
                                        Some(animation::serialize_transforms(&from))
                                    } else {
                                        animation::accumulate_transforms(
                                            &from,
                                            &to,
                                            rect.width,
                                            rect.height,
                                        )
                                    }
                                } else if matches!(property,"border-image-slice"|"border-image-width"|"border-image-outset") {
                                    let(style,_,viewport)=transform_context.as_ref()?;
                                    let a=lumen_html::css::parse_animation_border_image_with_query(property,base,style,*viewport,shadow_text,shadow_query)?;
                                    let b=lumen_html::css::parse_animation_border_image_with_query(property,value,style,*viewport,shadow_text,shadow_query)?;
                                    Some(animation::transition_values::compose_border_image(property,&a,&b).unwrap_or_else(||value.to_owned()))
                                } else if matches!(property,"box-shadow"|"text-shadow") {
                                    let(style,_,viewport)=transform_context.as_ref()?;
                                    let b=lumen_html::css::parse_animation_source_shadows_with_query(property,value,style,*viewport,shadow_text,shadow_query)?;
                                    animation::transition_values::serialize_source_shadow_list(property,&b)
                                } else if lumen_html::css::animation_color_property(property) {
                                    Some(value.to_owned())
                                } else {
                                    composite_value(property, base, value, None)

                                }
                            })};
                            let composed=if record.iteration_accumulate && (!property.starts_with("--") || registered_numeric) && sample.current_iteration.is_some_and(|iteration|iteration>0.0) {
                                let final_value=record.keyframes.iter().rev().find(|frame|frame.declarations.iter().any(|(name,_)|name==property));
                                // A missing terminal property keyframe is the neutral accumulation
                                // identity; it adds no value to successive iterations.
                                if let Some(final_value)=final_value.filter(|frame|frame.offset==1.0)
                                    .and_then(|frame|frame.declarations.iter().find(|(name,_)|name==property).map(|(_,value)|value.as_str())) {
                                    let final_value=compute_endpoint(final_value)?;
                                    composed.and_then(|value|{
                                        if registered_numeric {
                                            let syntax=registered_syntax(property)?;
                                            lumen_html::css::registered_properties::combine_computed_values_with_reference(syntax,&final_value,&value,
                                                lumen_html::css::registered_properties::ComputedValueOperation::Accumulate(sample.current_iteration.unwrap()),
                                                None)
                                                .or(Some(value))
                                        }else{accumulate_iteration_endpoint(property,&final_value,&value,sample.current_iteration.unwrap(),
                                            transform_context.as_ref(),shadow_text,shadow_query)}
                                    })
                                }else{composed}
                            }else{composed};
                            composition_failed |= composed.is_none();
                            composed
                        },
                        |property, from, to, progress| {
                            if property.starts_with("--") && registered_syntax(property)
                                .is_some_and(|syntax|syntax!="*" && lumen_html::css::registered_properties::accepts_value(syntax,from)
                                    && lumen_html::css::registered_properties::accepts_value(syntax,to)) {
                                let syntax=registered_syntax(property)?;
                                lumen_html::css::registered_properties::combine_computed_values_with_reference(syntax,from,to,
                                    lumen_html::css::registered_properties::ComputedValueOperation::Interpolate(progress),
                                    None)
                            } else if matches!(property,"translate"|"rotate"|"scale"){
                                let(style,_,viewport)=transform_context.as_ref()?;
                                lumen_html::css::combine_animation_individual_transform(property,from,to,lumen_html::css::registered_properties::ComputedValueOperation::Interpolate(progress),style,*viewport,shadow_text,shadow_query)
                            } else if property == "transform" {
                                interpolate_transform_animation(from,to,progress,transform_context.as_ref()?)
                            } else {
                                record.endpoint_styles.as_ref().and_then(|(from_style,to_style)|
                                    animation::transition_values::interpolate_compound_transition(property,from_style,to_style,progress,lumen_html_image::interpolate_color))
                                    .or_else(||{
                                        if matches!(property,"border-image-slice"|"border-image-width"|"border-image-outset") {
                                            let(style,_,viewport)=transform_context.as_ref()?;
                                            let a=lumen_html::css::parse_animation_border_image_with_query(property,from,style,*viewport,shadow_text,shadow_query)?;
                                            let b=lumen_html::css::parse_animation_border_image_with_query(property,to,style,*viewport,shadow_text,shadow_query)?;
                                            animation::transition_values::interpolate_border_image(property,&a,&b,progress)
                                        } else if matches!(property,"box-shadow"|"text-shadow") {
                                            let(style,_,viewport)=transform_context.as_ref()?;
                                            let a=lumen_html::css::parse_animation_source_shadows_with_query(property,from,style,*viewport,shadow_text,shadow_query)?;
                                            let b=lumen_html::css::parse_animation_source_shadows_with_query(property,to,style,*viewport,shadow_text,shadow_query)?;
                                            animation::transition_values::interpolate_source_shadow_lists(property,&a,&b,progress)
                                        } else {interpolate_transition_value(property,from,to,progress)}
                                    })
                            }
                        },
                    );
                    if let Some(error)=endpoint_budget_failure {return Err(error);}
                    if let Some(error)=endpoint_failure {return Err(OpError::new("SyntaxError",format!("animation computed endpoint failed: {error:?}")));}
                    if composition_failed {
                        return Err(OpError::new(
                            "NotSupportedError",
                            "the animated CSS value does not support additive composition",
                        ));
                    }
                    for (property, value) in sampled {
                        properties.insert(property, value);
                    }
                }
            }
        }
        if pass==1 && (has_transforms||has_shadows) && (!declarations.is_empty()||has_transforms) || pass==2 && has_transforms {
            publish_animation_declarations(hub, realm, key, &declarations,demand)?;
            query_geometry_ready=false;
        }
    }
    publish_animation_declarations(hub, realm, key, &declarations,demand)?;
    let active = snapshot_active_nodes(hub, realm, now, demand);
    if demand.scoped_connected {
        let session=realm.session.borrow();let document=session.document();
        let mut state=hub.borrow_mut();let retained=state.overlaid_nodes.entry(key).or_default();
        retained.retain(|(node,_,_)|!demand.includes(document,Some(*node)));retained.extend(active);
    }else{hub.borrow_mut().overlaid_nodes.insert(key, active);}
    Ok(())
}

fn publish_animation_declarations(hub:&Rc<RefCell<AnimationHub>>,realm:&DomRealm,key:usize,
    declarations:&HashMap<(NodeId,Option<lumen_html::css::PseudoElement>,bool),BTreeMap<String,String>>,demand:&EffectDemand)->OpResult<()> {
    let mut targets:HashSet<(NodeId,Option<lumen_html::css::PseudoElement>,bool)>=if demand.scoped_connected {
        let session=realm.session.borrow();hub.borrow().overlaid_nodes.get(&key).map(|targets|targets.iter().filter(|(node,_,_)|demand.includes(session.document(),Some(*node))).copied().collect()).unwrap_or_default()
    }else{hub.borrow().overlaid_nodes.get(&key).cloned().unwrap_or_default()};
    targets.extend(declarations.keys().copied());
    for (node,pseudo,transition) in targets {
        if realm.session.borrow().document().kind(node).is_err() {continue;}
        let pairs=declarations.get(&(node,pseudo,transition)).map(|values|values.iter()
            .map(|(name,value)|(name.clone(),value.clone())).collect()).unwrap_or_default();
        let origin=if transition {lumen_html::css::EffectOrigin::Transition}else{lumen_html::css::EffectOrigin::Animation};
        realm.session.borrow_mut().set_pseudo_effect_declarations(node,pseudo,origin,pairs)
            .map_err(|error|OpError::new("SyntaxError",format!("animation style application failed: {error:?}")))?;
    }
    Ok(())
}

fn snapshot_active_nodes(
    hub: &Rc<RefCell<AnimationHub>>,
    realm: &Rc<DomRealm>,
    now: f64,
    demand: &EffectDemand,
) -> HashSet<(NodeId,Option<lumen_html::css::PseudoElement>,bool)> {
    hub.borrow()
        .records
        .values()
        .filter(|record| {
            record.effect_id.is_some()
                && demand.includes(realm.session.borrow().document(),record.node)
                && !record.cancelled
                && record
                    .realm
                    .upgrade()
                    .is_some_and(|candidate| Rc::ptr_eq(&candidate, realm))
                && sample_record(record, now).progress.is_some()
        })
        .filter_map(|record| record.node.map(|node| {
            let state=hub.borrow();let transition=state.transitions.get(&record.id);
            (node,record.effect_id.and_then(|id|state.effect_records.get(&id).and_then(|effect|effect.pseudo)),transition.is_some_and(|transition|transition.associated))
        }))
        .collect()
}

// Document timelines remain fixed between real rendering opportunities. An
// adopted target does not change the document that owns its animation timeline.
fn document_time(realm: &DomRealm) -> Option<f64> {
    (realm.has_browsing_context && realm.browsing_context()
        .is_some_and(|context| browsing_context::is_active_document(&context, realm)))
        .then(|| realm.timeline_sample.get())
}

fn record_time(record: &Record) -> Option<f64> {
    if let Some(binding)=&record.progress_timeline {return binding.sampled_time;}
    record.timeline_realm.upgrade().and_then(|realm| document_time(&realm))
}

fn hub_time(hub: &Rc<RefCell<AnimationHub>>) -> f64 {
    hub.borrow().default_realm.upgrade().and_then(|realm| document_time(&realm)).unwrap_or(0.0)
}

fn record_sampling_timing(record:&Record)->Timing{
    // An absent associated effect has zero end time; detached effect timing stays unchanged.
    if record.effect_id.is_none() {return Timing::default();}
    record.progress_timeline.as_ref().map_or(record.timing,|binding|record.timing.normalized_for_progress(binding.duration_auto,1000.0))
}

fn sample_record(record: &Record, _now: f64) -> animation::Sample {

    let timeline_time = record_time(record);

    if record.cancelled || (record.hold_time_ms.is_none()
        && (!record.start_resolved || timeline_time.is_none())) {
        return animation::Sample {
            current_time_ms: record.hold_time_ms.unwrap_or(0.0),
            progress: None,
            state: PlaybackState::Idle,
            before: false,
            current_iteration: None,
        };
    }
    let mut current_time = record
        .hold_time_ms
        .unwrap_or(record.start_time_ms + (timeline_time.unwrap_or(record.start_ms) - record.start_ms) * record.playback_rate);
    if record.progress_timeline.is_some() && record.hold_time_ms.is_none() && record.paused_at_ms.is_none() {
        let progress=timeline_time.unwrap_or(0.0)/1000.0;
        current_time=if record.playback_rate<0.0{1.0-progress}else{progress}*1000.0;
    }
    if record.hold_time_ms.is_none() && record.paused_at_ms.is_none() {
        let end_time = record_sampling_timing(&record).end_time();
        if record.playback_rate > 0.0
            && record.start_time_ms <= end_time
            && current_time >= end_time
        {
            current_time = end_time;
        } else if record.playback_rate < 0.0 && record.start_time_ms >= 0.0 && current_time <= 0.0 {
            current_time = 0.0;
        }
    }
    animation::sample_current_time(
        record_sampling_timing(record),
        current_time,
        record.playback_rate,
        record.paused_at_ms.is_some(),
    )
}

/// Worklet effects enter the same retained cascade and timing sampler as
/// ordinary keyframe effects. The worklet owns local time; it never advances
/// through the document timeline or clamps at the Animation's finish boundary.
#[derive(Clone)]
pub(crate) struct WorkletEffectBinding {
    hub: Rc<RefCell<AnimationHub>>,
    id: u32,
    effect_id: u32,
    lease: Rc<()>,
}
impl Drop for WorkletEffectBinding {
    fn drop(&mut self) {
        if Rc::strong_count(&self.lease) != 1 { return; }
        let mut state = self.hub.borrow_mut();
        state.records.remove(&self.id);
        state.ready.remove(&self.id);
        state.finished.remove(&self.id);
        state.wrappers.remove(&self.id);
        if let Some(effect) = state.effect_records.get_mut(&self.effect_id) {
            if effect.animation_id == Some(self.id) { effect.animation_id = None; }
        }
    }
}
impl WorkletEffectBinding {
    pub(crate) fn has_keyframes(&self) -> bool {
        self.hub.borrow().effect_records.get(&self.effect_id)
            .is_some_and(|effect| !effect.keyframes.is_empty())
    }
    pub(crate) fn timing(&self) -> OpResult<(Timing, Rc<str>, bool, bool)> {
        let state = self.hub.borrow();
        let effect = state.effect_records.get(&self.effect_id)
            .ok_or_else(|| OpError::new("InvalidStateError", "worklet effect was retired"))?;
        Ok((effect.timing, effect.easing.clone(), effect.duration_auto, effect.fill_auto))
    }
    pub(crate) fn publish(&self, time: Option<f64>) {
        if let Some(record) = self.hub.borrow_mut().records.get_mut(&self.id) {
            record.cancelled = time.is_none();
            record.hold_time_ms = time;
            record.start_resolved = false;
            record.pending_task = None;
            record.paused_at_ms = Some(0.0);
        }
    }
    pub(crate) fn clear(&self) -> OpResult<()> {
        self.publish(None);
        let realm = self.hub.borrow().records.get(&self.id).and_then(|record| record.realm.upgrade());
        if let Some(realm) = realm { apply_realm(&self.hub, &realm, realm.timeline_sample.get())?; }
        Ok(())
    }
}
pub(crate) fn worklet_bind_effect(ctx: &mut Ctx, value: &Value, timeline: &Value) -> OpResult<WorkletEffectBinding> {
    let effect_id = ctx.with_instance::<DomKeyframeEffect, _>(value, |effect| effect.id)?;
    let (timeline_id, owner, origin) = if let Ok(native) = ctx.with_instance::<DomDocumentTimeline, _>(timeline, Clone::clone) {
        (native.id, native.realm, native.origin_time_ms)
    } else {
        let native = ctx.with_instance::<progress_timelines::DomScrollTimeline, _>(timeline, |timeline| timeline.timeline.clone())?;
        (native.id, native.realm, 0.0)
    };
    let realm = owner.upgrade().ok_or_else(|| OpError::new("InvalidStateError", "timeline document was retired"))?;
    let (id, _) = attach_effect(ctx, effect_id, false, timeline_id, &realm, origin, timeline.clone())?;
    Ok(WorkletEffectBinding { hub: hub(ctx)?, id, effect_id, lease: Rc::new(()) })
}
pub(crate) fn worklet_timeline_sample(ctx: &mut Ctx, timeline: &Value) -> OpResult<(Option<f64>, bool)> {
    if let Ok(native) = ctx.with_instance::<DomDocumentTimeline, _>(timeline, Clone::clone) {
        return Ok((native.realm.upgrade().and_then(|realm| document_time(&realm)).map(|time| time - native.origin_time_ms), false));
    }
    let native = ctx.with_instance::<progress_timelines::DomScrollTimeline, _>(timeline, |timeline| timeline.timeline.clone())?;
    native.update_geometry()?;
    let time = native.binding.borrow().sampled_time;
    Ok((time, true))
}
pub(crate) fn worklet_timing_object(ctx: &mut Ctx, binding: &WorkletEffectBinding, time: Option<f64>, computed: bool) -> OpResult<Value> {
    let (timing, easing, duration_auto, fill_auto) = binding.timing()?;
    let object = timing_object(ctx, timing, &easing, !computed && duration_auto, !computed && fill_auto)?;
    if computed {
        let sample = time.map(|time| animation::sample_current_time(timing, time, 1.0, true));
        for (name, value) in [
            ("activeDuration", Some(timing.active_duration())), ("endTime", Some(timing.end_time())),
            ("localTime", time), ("currentIteration", sample.and_then(|sample| sample.current_iteration)),
            ("progress", sample.and_then(|sample| sample.progress.and_then(|progress| animation::ease_with_before(&easing, progress, sample.before)))),
        ] {
            ctx.set_member(&object, name, value.map(Value::Num).unwrap_or(Value::Null))
                .map_err(|error| OpError::thrown(lumen::embed::abrupt_value(error)))?;
        }
    }
    Ok(object)
}

// Retired CSS animations have no effect on the cascade. Keep them only while
// script retains an Animation or KeyframeEffect wrapper. Promise caches must
// not themselves keep a resolved Animation alive forever.
fn collect_retired_css_animations(ctx: &mut Ctx, hub: &Rc<RefCell<AnimationHub>>) {
    let retired = {
        let mut state = hub.borrow_mut();
        state.css_generations.retain(|_, (realm, _)| realm.strong_count() != 0);
        let mut retired = state.records.values()
            .filter(|record| (record.cancelled && (record.css_name.is_some() || state.transitions.contains_key(&record.id)))
                || (state.transitions.get(&record.id).is_some_and(|transition|transition.previous==animation::TransitionPhase::After) && sample_record(record,0.0).state==PlaybackState::Finished))
            .filter(|record|state.transitions.get(&record.id).is_none_or(|transition|transition.cancellation_elapsed.is_none()))
            .map(|record| record.id).collect::<Vec<_>>();
        // Frames without cancelled CSS records allocate neither a set nor a list.
        if !retired.is_empty() {
            let active = state.css_records.values().map(|(_, _, id)| *id).collect::<HashSet<_>>();
            retired.retain(|id| !active.contains(id));
        }
        retired
    };
    for id in retired {
        let first_retirement = hub.borrow_mut().records.get_mut(&id).is_some_and(|record| {
            let first = !record.retired;
            record.retired = true;
            first
        });
        if first_retirement && hub.borrow().records.get(&id).is_some_and(|record|record.cancelled) { reject_finished(ctx, hub, id); }
        let mut state = hub.borrow_mut();
        let effect_id = state.records.get(&id).and_then(|record| record.effect_id);
        let animation_live = state.wrappers.get(&id).and_then(WeakValue::upgrade).is_some();
        let effect_live = effect_id.and_then(|effect| state.effects.get(&effect))
            .and_then(WeakValue::upgrade).is_some();
        if animation_live || effect_live || state.dispatching_css.contains_key(&id) {
            continue;
        }
        state.records.remove(&id);
        state.wrappers.remove(&id);
        state.finished.remove(&id);
        state.ready.remove(&id);
        state.css_events.remove(&id);
        state.transitions.remove(&id);
        if let Some(effect) = effect_id {
            state.effect_records.remove(&effect);
            state.effects.remove(&effect);
        }
    }
}

pub fn advance(ctx: &mut Ctx, timestamp_ms: f64) -> OpResult<()> {
    let Some(hub) = RealmServices::<RefCell<AnimationHub>>::current(ctx) else {
        return Ok(());
    };
    let owner = hub.borrow().default_realm.upgrade();
    let Some(owner) = owner.filter(|owner| document_time(owner).is_some()) else {
        return Ok(());
    };
    owner.timeline_sample.set(timestamp_ms);
    crate::animation_worklet::advance(ctx)?;
    let mut realms = hub
        .borrow()
        .records
        .values()
        .filter_map(|record| record.realm.upgrade())
        .collect::<Vec<_>>();
    realms.extend(hub.borrow().css_events.values().filter_map(|event| event.owner.upgrade()));
    if let Some(default_realm) = hub.borrow().default_realm.upgrade() {
        realms.push(default_realm);
    }
    let mut seen = HashSet::new();
    for realm in realms {
        let key = Rc::as_ptr(&realm) as usize;
        if seen.insert(key) && document_time(&realm).is_some() {
            refresh_css_transitions(ctx,&hub,&realm,realm.timeline_sample.get())?;
            refresh_css_animations(ctx, &hub, &realm, realm.timeline_sample.get())?;
            apply_realm(&hub, &realm, timestamp_ms)?;
        }
    }
    settle_ready(ctx, &hub, timestamp_ms)?;
    // Style refresh admits genuine old-animation cancellations. Publish that
    // already admitted event batch before sampling the replacement starts;
    // never run unrelated author tasks here. The batch is snapshotted before
    // callbacks, so listener-created cancellations retain their later turn.
    dispatch_css_cancellations(ctx, &hub)?;
    dispatch_css_events(ctx, &hub, timestamp_ms)?;
    settle_finished(ctx, &hub, timestamp_ms)?;
    collect_retired_css_animations(ctx, &hub);
    Ok(())
}

/// Migrate animation targets when the DOM adapter adopts nodes into another
/// document. The target document's CSS cascade is the new effect environment;
/// source overlays are cleared and the same animation records/wrappers survive.
pub fn adopt_nodes(
    ctx: &mut Ctx,
    source: &Rc<DomRealm>,
    target: &Rc<DomRealm>,
    mapping: &[(NodeId, NodeId)],
) -> OpResult<()> {
    let Some(hub) = RealmServices::<RefCell<AnimationHub>>::current(ctx) else {
        return Ok(());
    };
    let cancelled=hub.borrow().running_transitions.iter().filter(|(key,_)|
        key.0==Rc::as_ptr(source) as usize && mapping.iter().any(|(old,_)|*old==key.1))
        .map(|(key,id)|(key.clone(),*id)).collect::<Vec<_>>();
    for (key,id) in cancelled {
        hub.borrow_mut().running_transitions.remove(&key);
        cancel_owned_transition(ctx,&hub,id)?;
    }
    hub.borrow_mut().completed_transitions.retain(|key,_|
        key.0!=Rc::as_ptr(source) as usize || !mapping.iter().any(|(old,_)|*old==key.1));
    {
        let mut state = hub.borrow_mut();
        // The source document no longer owns adopted nodes. Its next style
        // publication must not clear overlays through obsolete source NodeIds;
        // the migrated records below publish into the destination normally.
        if let Some(overlaid) = state.overlaid_nodes.get_mut(&(Rc::as_ptr(source) as usize)) {
            overlaid.retain(|(node,_,_)|!mapping.iter().any(|(old,_)|old==node));
        }
        for record in state.records.values_mut() {
            if !record
                .realm
                .upgrade()
                .is_some_and(|realm| Rc::ptr_eq(&realm, source))
            {
                continue;
            }
            if let Some((_, new_node)) = mapping
                .iter()
                .find(|(old_node, _)| Some(*old_node) == record.node)
            {
                record.node = Some(*new_node);
                record.realm = Rc::downgrade(target);
            }
        }
        for event in state.css_events.values_mut() {
            if event.owner.upgrade().is_some_and(|realm| Rc::ptr_eq(&realm, source)) {
                if let Some((_, node)) = mapping.iter().find(|(old, _)| *old == event.node) {
                    event.owner = Rc::downgrade(target);
                    event.node = *node;
                    event._retention.adopt_nodes(source, target, mapping);
                }
            }
        }
        let AnimationHub {
            effect_records,
            records,
            ..
        } = &mut *state;
        for effect in effect_records.values_mut() {
            if !effect
                .realm
                .upgrade()
                .is_some_and(|realm| Rc::ptr_eq(&realm, source))
            {
                continue;
            }
            if let Some((_, new_node)) = mapping
                .iter()
                .find(|(old_node, _)| Some(*old_node) == effect.node)
            {
                effect.node = Some(*new_node);
                effect.realm = Rc::downgrade(target);
                if let Some(animation_id) = effect.animation_id {
                    if let Some(record) = records.get_mut(&animation_id) {
                        record.node = effect.node;
                        record.realm = effect.realm.clone();
                    }
                }
            }
        }
    }
    let now = hub_time(&hub);
    apply_realm(&hub, source, now)?;
    apply_realm(&hub, target, now)
}

fn settle_finished(ctx: &mut Ctx, hub: &Rc<RefCell<AnimationHub>>, now: f64) -> OpResult<()> {
    let completed = hub
        .borrow()
        .records
        .values()
        .filter(|record| {
            !record.cancelled
                && record.pending_task.is_none()
                && record.paused_at_ms.is_none()
                && sample_record(record, now).state == PlaybackState::Finished
        })
        .map(|record| record.id)
        .collect::<Vec<_>>();
    for id in completed {
        let should_fire = {
            let mut state = hub.borrow_mut();
            state.records.get_mut(&id).is_some_and(|record| {
                if record.finish_event_fired {
                    false
                } else {
                    record.finish_event_fired = true;
                    true
                }
            })
        };
        let still_finished = hub.borrow().records.get(&id).is_some_and(|record| {
            !record.cancelled
                && record.pending_task.is_none()
                && record.paused_at_ms.is_none()
                && sample_record(record, now).state == PlaybackState::Finished
        });
        if !still_finished {
            continue;
        }
        let deferred = hub
            .borrow_mut()
            .finished
            .get_mut(&id)
            .and_then(|finished| finished.deferred.take());
        if let Some(deferred) = deferred {
            let animation = existing_wrapper(ctx, hub, id)?;
            deferred.resolve(ctx, animation);
        }
        // The finished promise is settled before firing finish. If a finish
        // listener calls play(), it therefore gets a fresh pending promise.
        if should_fire {
            fire_event(ctx, hub, id, "finish")?;
        }
    }
    Ok(())
}

fn reject_finished(ctx: &mut Ctx, hub: &Rc<RefCell<AnimationHub>>, id: u32) {
    // Cancellation starts a new finished-promise lifecycle even when the old
    // promise already settled. Publish the replacement before rejection jobs
    // or cancel listeners can observe the animation again.
    let (_, previous) = replace_finished_promise(ctx, hub, id);
    if let Some(deferred) = previous.and_then(|finished| finished.deferred) {
        let reason = OpError::new("AbortError", "animation was canceled").to_value(ctx);
        deferred.reject_handled(ctx, reason);
    }
}

fn fire_event(ctx: &mut Ctx, hub: &Rc<RefCell<AnimationHub>>, id: u32, kind: &str) -> OpResult<()> {
    if !hub.borrow().records.contains_key(&id) {
        return Err(OpError::new("InvalidStateError", "animation is no longer available"));
    }
    let wrapper = existing_wrapper(ctx, hub, id)?;
    let event = DomEvent::new(ctx, kind, None)?;
    let event = ctx.new_instance(event);
    let event = JsObject::from_value(event)
        .ok_or_else(|| OpError::new("TypeError", "could not create animation event"))?;
    crate::events::dispatch_event(ctx, This(wrapper), event)?;
    Ok(())
}

fn ensure_pending_finished(ctx: &mut Ctx, hub: &Rc<RefCell<AnimationHub>>, id: u32) {
    let should_replace = hub
        .borrow()
        .finished
        .get(&id)
        .is_none_or(|finished| finished.deferred.is_none());
    if should_replace {
        replace_finished_promise(ctx, hub, id);
    }
}

struct DispatchRetention {
    hub: Rc<RefCell<AnimationHub>>,
    ids: Vec<u32>,
}

impl DispatchRetention {
    fn retain(&mut self, id: u32) {
        *self.hub.borrow_mut().dispatching_css.entry(id).or_default() += 1;
        self.ids.push(id);
    }
}

impl Drop for DispatchRetention {
    fn drop(&mut self) {
        let mut state = self.hub.borrow_mut();
        for id in &self.ids {
            if let Some(count) = state.dispatching_css.get_mut(id) {
                *count -= 1;
                if *count == 0 { state.dispatching_css.remove(id); }
            }
        }
    }
}

struct CssCancellation {
    id: u32,
    owner: Rc<DomRealm>,
    node: NodeId,
    pseudo:Option<lumen_html::css::PseudoElement>,
    order: usize,
    name: String,
    elapsed_ms: f64,
    dispatched: Cell<bool>,
    _retention: DispatchRetention,
}

fn queue_css_cancellation(
    ctx: &mut Ctx,
    hub: &Rc<RefCell<AnimationHub>>,
    id: u32,
    now: f64,
    detach: bool,
) -> OpResult<()> {
    // Only copy event metadata: cancellation does not copy keyframes or style.
    let snapshot = {
        let state = hub.borrow();
        let record = state.records.get(&id);
        let event = state.css_events.get(&id);
        match (record, event) {
            (Some(record), Some(event)) if !record.cancelled => {
                let phase = if record.pending_task.is_some() && event.previous.phase.is_none() {None}else{css_sample(record, now).phase};
                if matches!(phase, Some(animation::EffectPhase::Before | animation::EffectPhase::Active)) {
                    event.owner.upgrade().map(|owner| (owner, event.node, event.pseudo, event.order,
                        record.css_name.clone().unwrap_or_default(),
                        (sample_record(record, now).current_time_ms - record.timing.delay_ms)
                            .max(0.0).min(record_sampling_timing(record).active_duration())))
                } else { None }
            }
            _ => None,
        }
    };
    if let Some((owner, node, pseudo, order, name, elapsed_ms)) = snapshot {
        let mut retention = DispatchRetention { hub: hub.clone(), ids: Vec::new() };
        retention.retain(id);
        let snapshot = Rc::new(CssCancellation { id, owner: owner.clone(), node, pseudo, order, name,
            elapsed_ms, dispatched: Cell::new(false), _retention: retention });
        let callback_snapshot = snapshot.clone();
        let callback_hub = hub.clone();
        let enqueue = move |ctx: &mut Ctx| crate::scheduling::queue_task(ctx, move |ctx| {
            // Earlier admitted cancellation tasks may have already dispatched
            // this snapshot in their sorted batch. Newly queued listener work
            // remains for its own later task, not these old placeholders.
            if !callback_snapshot.dispatched.get() {
                dispatch_css_cancellations(ctx, &callback_hub)?;
            }
            Ok(())
        });
        // The shared queue owns admission, microtask checkpoints, and teardown.
        // On failure, the retained snapshot drops and author state is unchanged.
        match owner.child_realm_handle() {
            Ok(Some(realm)) => ctx.with_host_realm(&realm, enqueue)
                .map_err(|error| OpError::new("Error", error.to_string()))??,
            Ok(None) => enqueue(ctx)?,
            Err(_) => { snapshot.dispatched.set(true); }
        }
        if !snapshot.dispatched.get() {
            let mut state = hub.borrow_mut();
            // Live snapshots are bounded by the shared task queue. Prune dead
            // weak metadata at that admission bound, not on every cancellation.
            if state.css_cancellations.len() >= crate::scheduling::MAX_PENDING_HTML_TASKS {
                state.css_cancellations.retain(|entry| entry.strong_count() != 0);
            }
            state.css_cancellations.push(Rc::downgrade(&snapshot));
        }
    }
    if let Some(event) = hub.borrow_mut().css_events.get_mut(&id) {
        // Cancel is a real idle transition even if play() runs before rendering.
        event.previous = animation::CssEventSample::IDLE;
        if detach { event.associated = false; }
    }
    Ok(())
}

// Snapshot reconciliation must retire old CSS identities before publishing any
// Promise hook. Ready-promise settlement follows after the full snapshot commit.
fn retire_css_snapshot_record(ctx: &mut Ctx, hub: &Rc<RefCell<AnimationHub>>, id: u32, now: f64) -> OpResult<()> {
    queue_css_cancellation(ctx, hub, id, now, true)?;
    retire_css_record_state(hub, id);
    Ok(())
}

fn retire_css_record_state(hub: &Rc<RefCell<AnimationHub>>, id: u32) {
    if let Some(record) = hub.borrow_mut().records.get_mut(&id) {
        record.start_resolved = false;
        record.hold_time_ms = None;
        record.paused_at_ms = None;
        record.retired = record.cancelled;
        record.cancelled = true;
    }
}

fn css_tree_order<'a>(owners: impl Iterator<Item = &'a Rc<DomRealm>>) -> OpResult<HashMap<usize, HashMap<NodeId, usize>>> {
    let mut result = HashMap::new();
    for owner in owners {
        let key = Rc::as_ptr(owner) as usize;
        if result.contains_key(&key) { continue; }
        let session = owner.session.borrow();
        let document = session.document();
        let root = document.root();
        let mut order = HashMap::new();
        order.insert(root, 0usize);
        let mut node = lumen_html::selector::next_shadow_including_descendant(document, root, root).map_err(dom_error)?;
        while let Some(id) = node {
            order.insert(id, order.len());
            node = lumen_html::selector::next_shadow_including_descendant(document, root, id).map_err(dom_error)?;
        }
        result.insert(key, order);
    }
    Ok(result)
}

fn dispatch_css_cancellations(ctx: &mut Ctx, hub: &Rc<RefCell<AnimationHub>>) -> OpResult<()> {
    let mut snapshots = std::mem::take(&mut hub.borrow_mut().css_cancellations)
        .into_iter().filter_map(|entry| entry.upgrade()).collect::<Vec<_>>();
    let tree_order = css_tree_order(snapshots.iter().map(|entry| &entry.owner))?;
    snapshots.sort_by_key(|entry| {
        let realm = Rc::as_ptr(&entry.owner) as usize;
        (realm, tree_order[&realm].get(&entry.node).copied().unwrap_or(usize::MAX),pseudo_order(entry.pseudo), entry.order)
    });
    for snapshot in &snapshots { snapshot.dispatched.set(true); }
    let mut first_error = None;
    for snapshot in snapshots {
        if let Err(error) = dispatch_css_event(ctx, hub, snapshot.id,
            animation::CssEvent { kind: "animationcancel", elapsed_ms: snapshot.elapsed_ms },
            &snapshot.owner, snapshot.node, snapshot.pseudo, &snapshot.name) {
            if first_error.is_none() { first_error = Some(error); }
            else {
                let error = error.to_value(ctx);
                DomRealm::report_exception(ctx, error);
            }
        }
    }
    match first_error { Some(error) => Err(error), None => Ok(()) }
}

fn dispatch_css_event(ctx: &mut Ctx, hub: &Rc<RefCell<AnimationHub>>, id: u32,
    sample: animation::CssEvent, owner: &Rc<DomRealm>, node: NodeId, pseudo:Option<lumen_html::css::PseudoElement>, name: &str) -> OpResult<()> {
    let (owner, node) = owner.resolve_adopted_node(node);
    let dispatch = |ctx: &mut Ctx| -> OpResult<()> {
        let animation = existing_wrapper(ctx, hub, id)?;
        let init = ctx.new_object_with_proto(&Value::Null);
        ctx.member_set(&init, "bubbles", Value::Bool(true)).map_err(OpError::thrown)?;
        let event = DomAnimationEvent { base: DomEvent::new(ctx, sample.kind, Some(init))?,
            animation_name: name.to_owned(), elapsed_time: sample.elapsed_ms / 1000.0,
            pseudo_element: pseudo_name(pseudo).to_owned(), animation };
        let value = ctx.new_instance(event);
        let event = JsObject::from_value(value).ok_or_else(|| OpError::type_error("invalid AnimationEvent"))?;
        let target = owner.wrap(ctx, node);
        crate::events::dispatch_user_agent_event(ctx, This(target), event)?;
        Ok(())
    };
    match owner.child_realm_handle() {
        Ok(Some(realm)) => ctx.with_host_realm(&realm, dispatch)
            .map_err(|error| OpError::new("Error", error.to_string()))??,
        Ok(None) => dispatch(ctx)?,
        // Retired browsing-context generations no longer receive user-agent events.
        Err(_) => {}
    }
    Ok(())
}

fn pseudo_order(pseudo:Option<lumen_html::css::PseudoElement>)->u8 {
    use lumen_html::css::PseudoElement::*;
    match pseudo {None=>0,Some(Marker)=>1,Some(Before)=>2,Some(BeforeMarker)=>3,
        Some(After)=>4,Some(AfterMarker)=>5,_=>6}
}
fn pseudo_name(pseudo:Option<lumen_html::css::PseudoElement>)->&'static str {
    use lumen_html::css::PseudoElement::*;
    match pseudo {None=>"",Some(Before)=>"::before",Some(After)=>"::after",Some(Marker)=>"::marker",
        Some(BeforeMarker)=>"::before::marker",Some(AfterMarker)=>"::after::marker",
        Some(FirstLine)=>"::first-line",Some(FirstLetter)=>"::first-letter",Some(Highlight)=>"::highlight",Some(Placeholder)=>"::placeholder",
        Some(ViewTransition)=>"::view-transition",Some(ViewTransitionGroup)=>"::view-transition-group",
        Some(ViewTransitionImagePair)=>"::view-transition-image-pair",Some(ViewTransitionOld)=>"::view-transition-old",
        Some(ViewTransitionNew)=>"::view-transition-new"}
}
fn dispatch_transition_event(ctx:&mut Ctx,hub:&Rc<RefCell<AnimationHub>>,id:u32,sample:animation::CssEvent,owner:&Rc<DomRealm>,node:NodeId,
    pseudo:&str,property:&str)->OpResult<()> {
    let (owner,node)=owner.resolve_adopted_node(node);
    let dispatch=|ctx:&mut Ctx|->OpResult<()> {
        let init=ctx.new_object_with_proto(&Value::Null);
        ctx.member_set(&init,"bubbles",Value::Bool(true)).map_err(OpError::thrown)?;
        let base=DomEvent::new(ctx,sample.kind,Some(init))?;
        let animation=existing_wrapper(ctx,hub,id)?;
        let event=ctx.new_instance(DomTransitionEvent {base,
            property_name:property.to_owned(),elapsed_time:sample.elapsed_ms/1000.0,pseudo_element:pseudo.to_owned(),animation});
        ctx.set_native_identity_owner::<DomTransitionEvent>(&event)?;
        let event=JsObject::from_value(event).ok_or_else(||OpError::type_error("invalid TransitionEvent"))?;
        let target=owner.wrap(ctx,node);
        crate::events::dispatch_user_agent_event(ctx,This(target),event)?;
        Ok(())
    };
    match owner.child_realm_handle() {
        Ok(Some(realm))=>ctx.with_host_realm(&realm,dispatch).map_err(|error|OpError::new("Error",error.to_string()))??,
        Ok(None)=>dispatch(ctx)?,Err(_)=>{},
    }
    Ok(())
}

fn css_sample(record: &Record, now: f64) -> animation::CssEventSample {
    if record.cancelled { return animation::CssEventSample::IDLE; }
    let sampled=sample_record(record,now);
    if sampled.state==PlaybackState::Idle {return animation::CssEventSample::IDLE;}
    let time=sampled.current_time_ms;
    if record.effect_id.is_none() {
        return animation::CssEventSample {
            phase: Some(if time < 0.0 { animation::EffectPhase::Before } else { animation::EffectPhase::After }),
            iteration: None,
        };
    }
    animation::css_event_sample(record_sampling_timing(record), time, record.playback_rate)
}

fn render_revision(realm: &DomRealm) -> (u64, u64) {
    let session = realm.session.borrow();
    (session.document().version(), session.paint_revision())
}

fn dispatch_css_events(ctx: &mut Ctx, hub: &Rc<RefCell<AnimationHub>>, now: f64) -> OpResult<()> {
    struct Queued {
        id: u32, event: animation::CssEvent, owner: Rc<DomRealm>, node: NodeId, pseudo:Option<lumen_html::css::PseudoElement>,
        order: usize,  scheduled: f64, sequence: usize,
        transition:Option<(String,Option<lumen_html::css::PseudoElement>,u64)>,
    }
    let mut queue = Vec::new();
    let mut retention = DispatchRetention { hub: hub.clone(), ids: Vec::new() };
    {
        let mut state = hub.borrow_mut();
        let AnimationHub { records, css_events, transitions, running_transitions, completed_transitions, .. } = &mut *state;
        for record in records.values().filter(|record| record.css_name.is_some()) {
            if record.pending_task.is_some() {continue;}
            let Some(event) = css_events.get_mut(&record.id) else { continue; };
            let current = css_sample(record, now);
            let owner = event.owner.upgrade();
            if event.associated {
                // Backward seeks with a positive playback rate cannot project
                // both boundaries independently: that would reverse the
                // specified start/end transition batch. Keep its anchor and
                // sequence together while other events retain actual times.
                let backwards_pair = event.previous.phase == Some(animation::EffectPhase::After)
                    && current.phase == Some(animation::EffectPhase::Before);
                let mut batch_time = None;
                for (sequence, sample) in animation::css_events(record_sampling_timing(record), event.previous, current,
                    0.0).into_iter().enumerate() {
                    if let (Some(sample), Some(owner)) = (sample, owner.as_ref()) {
                        let scheduled = if record.paused_at_ms.is_some() || record.playback_rate == 0.0
                            || sample.kind == "animationcancel" { now } else {
                            record.start_ms + (record.timing.delay_ms + sample.elapsed_ms - record.start_time_ms)
                                / record.playback_rate
                        };
                        let scheduled = if backwards_pair {
                            *batch_time.get_or_insert(scheduled)
                        } else { scheduled };
                        queue.push(Queued { id: record.id, event: sample, owner: owner.clone(), node: event.node,pseudo:event.pseudo,
                            order: event.order, 
                            scheduled, sequence,transition:None });
                    }
                }
            }
            event.previous = current;
            event.sampled = true;
        }
        for (id,event) in transitions.iter_mut() {
            let Some(record)=records.get(id) else {continue;};
            let sampled=sample_record(record,now);
            let current=if record.cancelled {animation::TransitionPhase::Idle}else {
                animation::transition_phase(record.timing,
                    (sampled.state!=PlaybackState::Idle).then_some(sampled.current_time_ms),record.playback_rate,
                    record.pending_task.is_some(),event.previous,record.effect_id.is_some())
            };
            let Some(owner)=event.owner.upgrade() else {event.retention=None;continue;};
            let mut admitted=Vec::new();
            let mut previous=event.previous;
            if event.cancellation_elapsed.is_some() && previous==animation::TransitionPhase::Idle {
                admitted.extend(animation::transition_events(record.timing,previous,animation::TransitionPhase::Pending,0.0)
                    .into_iter().flatten());previous=animation::TransitionPhase::Pending;
            }
            if let Some(elapsed)=event.cancellation_elapsed {
                admitted.extend(animation::transition_events(record.timing,previous,animation::TransitionPhase::Idle,elapsed).into_iter().flatten());
                previous=animation::TransitionPhase::Idle;
            }
            if event.associated || event.cancellation_elapsed.is_some() {
                admitted.extend(animation::transition_events(record.timing,previous,current,0.0)
                    .into_iter().flatten());
            }
            // Every multi-event phase change is dispatched in immediate succession.
            let immediate_batch=admitted.len()>1;
            let mut anchor=None;
            for (sequence,sample) in admitted.into_iter().enumerate() {
                let scheduled=if sample.kind=="transitionrun" {record.start_ms}
                    else if sample.kind=="transitioncancel" || record.paused_at_ms.is_some() || record.playback_rate==0.0 {now}
                    else {record.start_ms+(record.timing.delay_ms+sample.elapsed_ms-record.start_time_ms)/record.playback_rate};
                let scheduled=if immediate_batch {*anchor.get_or_insert(scheduled)}else{scheduled};
                queue.push(Queued {id:*id,event:sample,owner:owner.clone(),node:event.node,pseudo:event.pseudo,order:0,
                    scheduled,sequence,
                    transition:Some((event.property.clone(),event.pseudo,event.generation))});
            }
            event.previous=current;
            if event.cancellation_elapsed.take().is_some() {event.retention=None;}
            if event.associated && current==animation::TransitionPhase::After && record.playback_rate>0.0 {
                let key=(Rc::as_ptr(&owner) as usize,event.node,event.pseudo,event.property.clone());
                if running_transitions.get(&key)==Some(id) {
                    running_transitions.remove(&key);completed_transitions.insert(key,event.state.end_value.clone());
                    event.retention=None;
                }
            }
        }
        for queued in &queue {
            if retention.ids.last() != Some(&queued.id) {
                *state.dispatching_css.entry(queued.id).or_default() += 1;
                retention.ids.push(queued.id);
            }
        }
        state.rendered_revisions.retain(|_, (realm, _, _)| realm.strong_count() != 0);
        let mut owners = state.records.values().filter_map(|record| record.realm.upgrade()).collect::<Vec<_>>();
        owners.extend(state.css_events.values().filter_map(|event| event.owner.upgrade()));
        owners.extend(state.default_realm.upgrade());
        for owner in owners {
            let (version, paint) = render_revision(&owner);
            state.rendered_revisions.insert(Rc::as_ptr(&owner) as usize, (Rc::downgrade(&owner), version, paint));
        }
    }
    let tree_order = css_tree_order(queue.iter().map(|queued| &queued.owner))?;
    queue.sort_by(|a, b| {
        a.scheduled.total_cmp(&b.scheduled)
            .then_with(|| a.transition.is_none().cmp(&b.transition.is_none()))
            .then_with(|| {
                let ak = Rc::as_ptr(&a.owner) as usize;
                let bk = Rc::as_ptr(&b.owner) as usize;
                ak.cmp(&bk).then_with(|| tree_order[&ak].get(&a.node).unwrap_or(&usize::MAX)
                    .cmp(tree_order[&bk].get(&b.node).unwrap_or(&usize::MAX)))
            })

            .then_with(|| pseudo_order(a.pseudo).cmp(&pseudo_order(b.pseudo)))
            .then_with(|| a.transition.as_ref().map(|(property,_,generation)|(*generation,property))
                .cmp(&b.transition.as_ref().map(|(property,_,generation)|(*generation,property))))
            .then_with(|| a.order.cmp(&b.order))
            .then_with(|| a.id.cmp(&b.id)).then_with(|| a.sequence.cmp(&b.sequence))
    });
    for queued in queue {
        if let Some((property,pseudo,_))=&queued.transition {
            dispatch_transition_event(ctx,hub,queued.id,queued.event,&queued.owner,queued.node,pseudo_name(*pseudo),property)?;
            continue;
        }
        let name = hub.borrow().records.get(&queued.id).and_then(|record| record.css_name.clone())
            .unwrap_or_default();
        dispatch_css_event(ctx, hub, queued.id, queued.event, &queued.owner, queued.node,queued.pseudo, &name)?;
    }
    Ok(())
}

#[lumen_bind::class(name = "AnimationEvent", extends = DomEvent, hint(js(webidl)))]
pub struct DomAnimationEvent {
    base: DomEvent,
    animation_name: String,
    elapsed_time: f64,
    pseudo_element: String,
    animation: Value,
}

#[lumen_bind::methods]
impl DomAnimationEvent {
    #[constructor(coerce)]
    fn new(ctx: &mut Ctx, kind: &str, init: Option<Value>) -> OpResult<Self> {
        let base = DomEvent::new(ctx, kind, init.clone())?;
        let animation_name = crate::ui_events::dictionary_string(ctx, &init, "animationName", "", false)?;
        let elapsed_time = match crate::ui_events::dictionary_member(ctx, &init, "elapsedTime")? {
            Some(value) => ctx.coerce_number(&value).map_err(OpError::thrown)?, None => 0.0,
        };
        if !elapsed_time.is_finite() {
            return Err(OpError::type_error("AnimationEvent.elapsedTime must be finite"));
        }
        let pseudo_element = crate::ui_events::dictionary_string(ctx, &init, "pseudoElement", "", false)?;
        let animation = crate::ui_events::dictionary_member(ctx, &init, "animation")?.unwrap_or(Value::Null);
        if !matches!(animation, Value::Null) {
            ctx.with_instance::<DomCssAnimation, _>(&animation, |_| ())
                .map_err(|_| OpError::type_error("animation must be a CSSAnimation"))?;
        }
        Ok(Self { base, animation_name, elapsed_time, pseudo_element, animation })
    }
    #[getter(name = "animationName")]
    fn animation_name(&self) -> String { self.animation_name.clone() }
    #[getter(name = "elapsedTime")]
    fn elapsed_time(&self) -> f64 { self.elapsed_time }
    #[getter(name = "pseudoElement")]
    fn pseudo_element(&self) -> String { self.pseudo_element.clone() }
    #[getter]
    fn animation(&self) -> Value { self.animation.clone() }
}

pub fn pending(ctx: &mut Ctx) -> bool {
    if crate::animation_worklet::pending(ctx) { return true; }
    RealmServices::<RefCell<AnimationHub>>::current(ctx).is_some_and(|hub| {
        let now = hub_time(&hub);
        let state = hub.borrow();
        let dirty = |realm: &Rc<DomRealm>| {
            let (version, paint) = render_revision(realm);
            state.rendered_revisions.get(&(Rc::as_ptr(realm) as usize))
                .is_none_or(|(_, previous_version, previous_paint)| *previous_version != version || *previous_paint != paint)
        };
        if state.default_realm.upgrade().is_some_and(|realm| dirty(&realm))
            || state.records.values().filter_map(|record| record.realm.upgrade()).any(|realm| dirty(&realm)) {
            return true;
        }
        state.records.values().any(|record| {
            if record.pending_task.is_some() {return record_time(record).is_some();}
            if state.transitions.get(&record.id).is_some_and(|transition| {
                if transition.cancellation_elapsed.is_some() {return true;}
                if !transition.associated {return false;}
                let sample=sample_record(record,now);
                transition.previous!=animation::transition_phase(record.timing,
                    (sample.state!=PlaybackState::Idle).then_some(sample.current_time_ms),record.playback_rate,
                    record.pending_task.is_some(),transition.previous,record.effect_id.is_some())
            }) {return true;}
            if state.css_events.get(&record.id).is_some_and(|event| {
                event.associated && (!event.sampled
                    || event.previous != css_sample(record, now))
            }) { return true; }
            if record.cancelled || record.paused_at_ms.is_some() || record.playback_rate == 0.0 {
                return false;
            }
            match sample_record(record, now).state {
                PlaybackState::Running => true,
                // Keep one rendering opportunity queued so the terminal
                // sample is published and `finished` settles even if the
                // event loop wakes after the nominal end time.
                PlaybackState::Finished => state
                    .finished
                    .get(&record.id)
                    .is_some_and(|finished| finished.deferred.is_some()),
                PlaybackState::Idle | PlaybackState::Paused => false,
            }
        })
    })
}

#[lumen_bind::class(name = "Animation", extends = DomEventTarget, hint(js(webidl)))]
pub struct DomAnimation {
    id: u32,
    base: DomEventTarget,
}

#[lumen_bind::class(name = "CSSAnimation", extends = DomAnimation, hint(js(webidl)))]
pub struct DomCssAnimation {
    base: DomAnimation,
}

#[lumen_bind::methods]
impl DomCssAnimation {
    #[getter]
    fn animation_name(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let (_, record) = record_state(ctx, self.base.id)?;
        Ok(record.css_name.map(|name| Value::Str(name.into())).unwrap_or(Value::Null))
    }
}

#[lumen_bind::class(name="CSSTransition",extends=DomAnimation,hint(js(webidl)))]
pub struct DomCssTransition { base:DomAnimation }
#[lumen_bind::methods]
impl DomCssTransition {
    #[getter]
    fn transition_property(&self,ctx:&mut Ctx)->OpResult<String> {
        let hub=hub(ctx)?;
        let property=hub.borrow().transitions.get(&self.base.id).map(|record|record.property.clone())
            .ok_or_else(||OpError::new("InvalidStateError","transition record is unavailable"))?;
        Ok(property)
    }
}

#[lumen_bind::class(name="TransitionEvent",extends=DomEvent,hint(js(webidl)))]
pub struct DomTransitionEvent {
    base:DomEvent, property_name:String, elapsed_time:f64, pseudo_element:String, animation:Value,
}
impl lumen::embed::NativeIdentityOwner for DomTransitionEvent {
    const TRACES_NATIVE_VALUES:bool=true;
    fn trace_native_identities(&self,id:u64,visit:&mut dyn FnMut(&Value)) {
        lumen::embed::NativeIdentityOwner::trace_native_identities(&self.base,id,visit);
    }
    fn trace_native_values(&self,visit:&mut dyn FnMut(&Value)) {
        lumen::embed::NativeIdentityOwner::trace_native_values(&self.base,visit);visit(&self.animation);
    }
}
struct TransitionEventConstructor(DomTransitionEvent);
impl lumen_bind::CtorRet<JsHost,DomTransitionEvent> for TransitionEventConstructor {
    fn into_ctor(self,cx:&<JsHost as Host>::Cx<'_>)->Result<Value,Value> {
        let value=<JsHost as Host>::construct(cx,self.0)?;
        <JsHost as Host>::with_ctx(cx,|ctx|ctx.set_native_identity_owner::<DomTransitionEvent>(&value).expect("TransitionEvent native brand"));
        Ok(value)
    }
}
#[lumen_bind::methods]
impl DomTransitionEvent {
    #[constructor(coerce)]
    fn new(ctx:&mut Ctx,kind:&str,init:Option<Value>)->OpResult<TransitionEventConstructor> {
        let base=DomEvent::new(ctx,kind,init.clone())?;
        let elapsed_time=match crate::ui_events::dictionary_member(ctx,&init,"elapsedTime")? {
            Some(value)=>ctx.coerce_number(&value).map_err(OpError::thrown)?,None=>0.0,
        };
        if !elapsed_time.is_finite() {return Err(OpError::type_error("TransitionEvent.elapsedTime must be finite"));}
        let pseudo_element=crate::ui_events::dictionary_string(ctx,&init,"pseudoElement","",false)?;
        let property_name=crate::ui_events::dictionary_string(ctx,&init,"propertyName","",false)?;
        let animation=crate::ui_events::dictionary_member(ctx,&init,"animation")?.filter(|value|!matches!(value,Value::Undefined)).unwrap_or(Value::Null);
        if !matches!(animation,Value::Null) {
            ctx.with_instance::<DomCssTransition,_>(&animation,|_|()).map_err(|_|OpError::type_error("animation must be a CSSTransition"))?;
        }
        Ok(TransitionEventConstructor(Self {base,property_name,elapsed_time,pseudo_element,animation}))
    }
    #[getter(name="propertyName")]
    fn property_name(&self)->String {self.property_name.clone()}
    #[getter(name="elapsedTime")]
    fn elapsed_time(&self)->f64 {self.elapsed_time}
    #[getter(name="pseudoElement")]
    fn pseudo_element(&self)->String {self.pseudo_element.clone()}
    #[getter]
    fn animation(&self)->Value {self.animation.clone()}
}

fn record_state(ctx: &mut Ctx, id: u32) -> OpResult<(Rc<RefCell<AnimationHub>>, Record)> {
    let hub = hub(ctx)?;
    let record = hub
        .borrow()
        .records
        .get(&id)
        .cloned()
        .ok_or_else(|| OpError::new("InvalidStateError", "animation is no longer available"))?;
    Ok((hub, record))
}

fn effect_state(ctx: &mut Ctx, id: u32) -> OpResult<(Rc<RefCell<AnimationHub>>, EffectRecord)> {
    let hub = hub(ctx)?;
    let effect = hub
        .borrow()
        .effect_records
        .get(&id)
        .cloned()
        .ok_or_else(|| OpError::new("InvalidStateError", "keyframe effect is unavailable"))?;
    Ok((hub, effect))
}

fn sync_effect_animation(
    hub: &Rc<RefCell<AnimationHub>>,
    effect: &EffectRecord,
) -> Option<(u32, Rc<DomRealm>)> {
    let animation_id = effect.animation_id?;
    let realm = effect.realm.upgrade()?;
    let mut state = hub.borrow_mut();
    let animation = state.records.get_mut(&animation_id)?;
    animation.effect_id = Some(effect.id);
    animation.realm = effect.realm.clone();
    animation.node = effect.node;
    animation.keyframes = effect.keyframes.clone();
    animation.timing = effect.timing;
    animation.easing = effect.easing.clone();
    animation.composite = effect.composite;
    Some((animation_id, realm))
}

struct AnimationConstructor(DomAnimation);
impl lumen_bind::CtorRet<JsHost,DomAnimation> for AnimationConstructor {
    fn into_ctor(self,cx:&<JsHost as Host>::Cx<'_>)->Result<Value,Value> {
        let id=self.0.id;
        let value=<JsHost as Host>::construct(cx,self.0)?;
        <JsHost as Host>::with_ctx(cx,|ctx| {
            let state=hub(ctx).map_err(|error|error.to_value(ctx))?;
            if let Some(weak)=ctx.weak_value(&value) {state.borrow_mut().wrappers.insert(id,weak);}
            publish_animation_effect_owner(ctx,&state,id,&value).map_err(|error|error.to_value(ctx))
        })?;
        Ok(value)
    }
}

#[lumen_bind::methods]
impl DomAnimation {
    #[constructor]
    fn new(
        ctx: &mut Ctx,
        effect: Option<EffectArgument>,
        #[default(TimelineChoice::Default)] timeline: TimelineChoice,
    ) -> OpResult<AnimationConstructor> {
        let hub = hub(ctx)?;
        let null_timeline = matches!(timeline, TimelineChoice::Null);
        let timeline = match timeline {
            TimelineChoice::Timeline(timeline) => Some(timeline),
            _ => None,
        };
        let (id, event_target) = if let Some(effect) = effect {
            if let Some(weak) = ctx.weak_value(&effect.value) {
                hub.borrow_mut().effects.insert(effect.native.id, weak);
            }
            let effect_record = hub
                .borrow()
                .effect_records
                .get(&effect.native.id)
                .cloned()
                .ok_or_else(|| {
                    OpError::new("InvalidStateError", "keyframe effect is unavailable")
                })?;
            let effect_realm = effect_record.realm.upgrade().ok_or_else(|| {
                OpError::new("InvalidStateError", "animation realm was destroyed")
            })?;
            let (timeline_id, timeline_realm, timeline_origin, timeline_value) =
                if let Some(timeline) = timeline.as_ref() {
                    if let Some(weak) = ctx.weak_value(&timeline.value) {
                        hub.borrow_mut()
                            .timeline_values
                            .insert(timeline.native.id, weak);
                    }
                    let expected = effect_realm.clone();
                    let actual = timeline.native.realm.upgrade().ok_or_else(|| {
                        OpError::new("InvalidStateError", "timeline document was destroyed")
                    })?;
                    if !Rc::ptr_eq(&expected, &actual) {
                        return Err(OpError::new(
                            "NotSupportedError",
                            "animations across document timelines are not supported",
                        ));
                    }
                    (
                        timeline.native.id,
                        actual,
                        timeline.native.origin_time_ms,
                        timeline.value.clone(),
                    )
                } else {
                    let value = document_timeline(ctx, &effect_realm)?;
                    let timeline_id = hub
                        .borrow()
                        .default_timeline_ids
                        .get(&(Rc::as_ptr(&effect_realm) as usize))
                        .copied()
                        .ok_or_else(|| {
                            OpError::new("InvalidStateError", "default timeline was not registered")
                        })?;
                    (timeline_id, effect_realm.clone(), 0.0, value)
                };
            attach_effect(
                ctx,
                effect.native.id,
                false,
                timeline_id,
                &timeline_realm,
                timeline_origin,
                timeline_value,
            )?
        } else {
            let realm = if let Some(timeline) = timeline.as_ref() {
                timeline.native.realm.upgrade().ok_or_else(|| {
                    OpError::new("InvalidStateError", "timeline document was destroyed")
                })?
            } else {
                hub.borrow().default_realm.upgrade().ok_or_else(|| {
                    OpError::new("InvalidStateError", "animation has no default document")
                })?
            };
            let (timeline_id, timeline_origin, timeline_value) =
                if let Some(timeline) = timeline.as_ref() {
                    if let Some(weak) = ctx.weak_value(&timeline.value) {
                        hub.borrow_mut()
                            .timeline_values
                            .insert(timeline.native.id, weak);
                    }
                    (
                        timeline.native.id,
                        timeline.native.origin_time_ms,
                        timeline.value.clone(),
                    )
                } else {
                    let value = document_timeline(ctx, &realm)?;
                    let timeline_id = hub
                        .borrow()
                        .default_timeline_ids
                        .get(&(Rc::as_ptr(&realm) as usize))
                        .copied()
                        .ok_or_else(|| {
                            OpError::new("InvalidStateError", "default timeline was not registered")
                        })?;
                    (timeline_id, 0.0, value)
                };
            let id = {
                let mut state = hub.borrow_mut();
                state.next_id = state.next_id.wrapping_add(1).max(1);
                let id = state.next_id;
                let now = document_time(&realm).unwrap_or(0.0);
                state.records.insert(
                    id,
                    Record {
                        id,
                        public_id: String::new(),
                        effect_id: None,
                        css_name: None,
                        css_order: None,
                        realm: Rc::downgrade(&realm),
                        timeline_realm: Rc::downgrade(&realm),
                        timeline_id,
                        timeline_origin_ms: timeline_origin,
                        progress_timeline: None,
                        timeline_value,
                        node: None,
                        keyframes: Vec::new().into(),
                        timing: Timing::default(),
                        easing: "linear".into(),
                        composite: CompositeMode::Replace,
                        start_ms: now,
                        start_time_ms: 0.0,
                        hold_time_ms: None,
                        playback_rate: 1.0,
                        paused_at_ms: None,
                        pending_task: None,
                        pending_task_revision: 0,
                        start_resolved: false,
                        cancelled: true,
                        finish_event_fired: false,
                        retired: false,
                        event_target: DomEventTarget::independent(&realm),
                    },
                );
                id
            };
            let base = hub.borrow().records[&id].event_target.clone();
            mark_ready_pending(ctx, &hub, id);
            ensure_pending_finished(ctx, &hub, id);
            (id, base)
        };
        if null_timeline {
            if let Some(record) = hub.borrow_mut().records.get_mut(&id) {
                record.timeline_realm = Weak::new();
                record.timeline_id = 0;
                record.timeline_origin_ms = 0.0;
                record.timeline_value = Value::Null;
            }
        }
        hub.borrow_mut().ready.remove(&id);
        Ok(AnimationConstructor(Self {
            id,
            base: event_target,
        }))
    }
    #[getter(name = "id")]
    fn public_id(&self, ctx: &mut Ctx) -> OpResult<String> {
        Ok(record_state(ctx, self.id)?.1.public_id)
    }
    #[setter(name = "id")]
    fn set_public_id(&self, ctx: &mut Ctx, id: String) -> OpResult<()> {
        let hub = hub(ctx)?;
        let mut state = hub.borrow_mut();
        let record = state
            .records
            .get_mut(&self.id)
            .ok_or_else(|| OpError::new("InvalidStateError", "animation is no longer available"))?;
        record.public_id = id;
        Ok(())
    }
    #[getter]
    fn effect(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        let hub = hub(ctx)?;
        if let Some(weak) = ctx.weak_value(&this.0) {
            hub.borrow_mut().wrappers.insert(self.id, weak);
        }
        let effect_id = record_state(ctx, self.id)?.1.effect_id;
        match effect_id {
            Some(effect_id) => effect_wrapper(ctx, &hub, effect_id),
            None => Ok(Value::Null),
        }
    }
    #[setter(name = "effect")]
    fn set_effect(&self, ctx: &mut Ctx, effect: Option<EffectArgument>) -> OpResult<()> {
        let hub = hub(ctx)?;
        let previous = record_state(ctx, self.id)?.1;
        if previous.effect_id == effect.as_ref().map(|effect| effect.native.id) {return Ok(());}
        let now = record_time(&previous).unwrap_or(0.0);
        let old_realm = previous.realm.upgrade();
        let new_effect = if let Some(effect) = &effect {
            if let Some(weak) = ctx.weak_value(&effect.value) {
                hub.borrow_mut().effects.insert(effect.native.id, weak);
            }
            Some(effect_state(ctx, effect.native.id)?.1)
        } else {
            None
        };
        if let Some(new_effect) = &new_effect {
            let target_realm = new_effect.realm.upgrade().ok_or_else(|| {
                OpError::new("InvalidStateError", "effect target document was destroyed")
            })?;
            let timeline_realm = previous.timeline_realm.upgrade().ok_or_else(|| {
                OpError::new("InvalidStateError", "animation timeline is inactive")
            })?;
            if !Rc::ptr_eq(&target_realm, &timeline_realm) {
                return Err(OpError::new(
                    "NotSupportedError",
                    "effect target and animation timeline must belong to the same document",
                ));
            }
        }
        let displaced=new_effect.as_ref().and_then(|effect|effect.animation_id).filter(|other|*other!=self.id);
        let mut state = hub.borrow_mut();
        if let Some(old_id) = previous.effect_id {
            if let Some(old) = state.effect_records.get_mut(&old_id) {
                if old.animation_id == Some(self.id) {
                    old.animation_id = None;
                }
            }
        }
        if let Some(mut effect) = new_effect {
            if let Some(other_id) = effect.animation_id.filter(|other| *other != self.id) {
                if let Some(other) = state.records.get_mut(&other_id) {
                    other.effect_id = None;
                }
            }
            effect.animation_id = Some(self.id);
            let record = state.records.get_mut(&self.id).ok_or_else(|| {
                OpError::new("InvalidStateError", "animation is no longer available")
            })?;
            record.effect_id = Some(effect.id);
            record.realm = effect.realm.clone();
            record.node = effect.node;
            record.keyframes = effect.keyframes.clone();
            record.timing = effect.timing;
            if let Some(binding)=&mut record.progress_timeline{binding.duration_auto=effect.duration_auto;}
            record.easing = effect.easing.clone();
            record.composite = effect.composite;
            state.effect_records.insert(effect.id, effect);
        } else if let Some(record) = state.records.get_mut(&self.id) {
            record.effect_id = None;
        }
        drop(state);
        if let Some(transition)=hub.borrow_mut().transitions.get_mut(&self.id) {transition.endpoint_styles=None;}
        update_live_animation_effect_owner(ctx,&hub,self.id)?;
        if let Some(displaced)=displaced {update_live_animation_effect_owner(ctx,&hub,displaced)?;}
        if let Some(realm) = old_realm {
            apply_record_change(&hub, &realm, now, self.id)?;
        }
        if let Some(effect) = effect {
            if let Some(realm) = effect_state(ctx, effect.native.id)?.1.realm.upgrade() {
                apply_record_change(&hub, &realm, now, self.id)?;
            }
        }
        Ok(())
    }
    #[getter]
    fn timeline(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let (_, record) = record_state(ctx, self.id)?;
        Ok(record.timeline_value.clone())
    }
    #[setter(name = "timeline")]
    fn set_timeline(&self, ctx: &mut Ctx, timeline: Option<TimelineArgument>) -> OpResult<()> {
        let hub = hub(ctx)?;
        let (_, record) = record_state(ctx, self.id)?;
        let target_realm = record.realm.upgrade();
        let timeline_realm = timeline
            .as_ref()
            .and_then(|timeline| timeline.native.realm.upgrade());
        if timeline.is_some() && timeline_realm.is_none() {
            return Err(OpError::new(
                "InvalidStateError",
                "timeline document was destroyed",
            ));
        }
        if let (Some(target), Some(timeline)) = (&target_realm, &timeline_realm) {
            if !Rc::ptr_eq(target, timeline) {
                return Err(OpError::new(
                    "NotSupportedError",
                    "animation target and timeline must belong to the same document",
                ));
            }
        }
        let new_id=timeline.as_ref().map_or(0,|timeline|timeline.native.id);
        if new_id==record.timeline_id {return Ok(());}
        let now = timeline_realm.as_ref().and_then(|realm| document_time(realm)).unwrap_or(0.0);
        let timeline_value =
            if let (Some(timeline), Some(_)) = (timeline.as_ref(), timeline_realm.as_ref()) {
                if let Some(weak) = ctx.weak_value(&timeline.value) {
                    hub.borrow_mut()
                        .timeline_values
                        .insert(timeline.native.id, weak);
                }
                timeline.value.clone()
            } else {
                Value::Null
            };
        if let Some(record) = hub.borrow_mut().records.get_mut(&self.id) {
            record.timeline_realm = timeline_realm
                .as_ref()
                .map_or_else(Weak::new, Rc::downgrade);
            record.timeline_id = timeline.as_ref().map_or(0, |timeline| timeline.native.id);
            let origin=timeline.as_ref().map_or(0.0,|timeline|timeline.native.origin_time_ms);
            // Internal starts use document-origin coordinates; public starts
            // remain the same timeline-relative value on a new timeline.
            record.start_ms+=origin-record.timeline_origin_ms;
            record.timeline_origin_ms=origin;
            record.timeline_value = timeline
                .as_ref()
                .map_or(timeline_value, |timeline| timeline.value.clone());
            // Changing timelines preserves the resolved start time. A null
            // timeline makes current time unresolved, without manufacturing
            // a paused hold. An unresolved-start hold remains available.
            if record.start_resolved {record.hold_time_ms=None;}
        }
        if let Some(realm) = target_realm {
            apply_record_change(&hub, &realm, now, self.id)?;
        }
        Ok(())
    }
    #[getter(name = "ready")]
    fn ready(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        let hub = hub(ctx)?;
        refresh_css_animation_for_accessor(ctx,&hub,self.id)?;
        if !hub.borrow().records.contains_key(&self.id) {
            return Err(OpError::new(
                "InvalidStateError",
                "animation is no longer available",
            ));
        }
        if let Some(weak) = ctx.weak_value(&this.0) {
            hub.borrow_mut().wrappers.insert(self.id, weak);
        }
        if hub.borrow().ready.get(&self.id).and_then(|promise| promise.promise.upgrade()).is_none() {
            if hub.borrow().records[&self.id].pending_task.is_some() {
                return Ok(mark_ready_pending(ctx,&hub,self.id));
            }
            let promise = replace_resolved_ready(ctx, &hub, self.id, this.0.clone());
            return Ok(promise);
        }
        let promise = hub.borrow().ready[&self.id].promise.upgrade().expect("new or live ready promise");
        Ok(promise)
    }
    #[getter(name = "finished")]
    fn finished(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let hub = hub(ctx)?;
        if !hub.borrow().records.contains_key(&self.id) {
            return Err(OpError::new(
                "InvalidStateError",
                "animation is no longer available",
            ));
        }
        if hub.borrow().finished.get(&self.id).and_then(|promise| promise.promise.upgrade()).is_none() {
            return Ok(replace_finished_promise(ctx, &hub, self.id).0);
        }
        let promise = hub.borrow().finished[&self.id].promise.upgrade().expect("new or live finished promise");
        Ok(promise)
    }
    #[getter(name = "pending")]
    fn is_pending(&self, ctx: &mut Ctx) -> OpResult<bool> {
        let hub=hub(ctx)?;refresh_css_animation_for_accessor(ctx,&hub,self.id)?;
        let pending=hub.borrow().records.get(&self.id).is_some_and(|record|record.pending_task.is_some());
        Ok(pending)
    }
    #[getter(name = "startTime")]
    fn start_time(&self, ctx: &mut Ctx) -> OpResult<Nullable<f64>> {
        let hub=hub(ctx)?;refresh_css_animation_for_accessor(ctx,&hub,self.id)?;
        let state=hub.borrow();
        let record=state.records.get(&self.id)
            .ok_or_else(||OpError::new("InvalidStateError","animation is no longer available"))?;
        Ok(Nullable(record.start_resolved.then(||record.start_ms-record.timeline_origin_ms
            -if record.playback_rate==0.0 {0.0}else{record.start_time_ms/record.playback_rate})))
    }
    #[setter(name = "startTime")]
    fn set_start_time(&self, ctx: &mut Ctx, value: Option<f64>) -> OpResult<()> {
        if value.is_some_and(|value|!value.is_finite()){return Err(OpError::type_error("startTime must be finite"));}
        let hub=hub(ctx)?;
        let now=hub.borrow().records.get(&self.id)
            .and_then(record_time).unwrap_or(0.0);
        {
            let mut state=hub.borrow_mut();let record=state.records.get_mut(&self.id).ok_or_else(||OpError::new("InvalidStateError","animation is no longer available"))?;
            let current=(!record.cancelled).then(||sample_record(record,now).current_time_ms);
            record.start_resolved=value.is_some();
            if let Some(value)=value {
                record.start_ms=value+record.timeline_origin_ms;record.start_time_ms=0.0;
                if record.playback_rate!=0.0{record.hold_time_ms=None;}
                record.cancelled=false;record.paused_at_ms=None;
            } else {
                record.hold_time_ms=current;record.paused_at_ms=current.map(|_|now);
            }
            record.pending_task=None;
        }
        resolve_ready(ctx,&hub,self.id)?;
        let realm=hub.borrow().records.get(&self.id).and_then(|record|record.realm.upgrade());
        if let Some(realm)=realm {apply_record_change(&hub,&realm,now,self.id)?;}
        settle_finished(ctx,&hub,now)
    }
    #[getter(name = "currentTime")]
    fn current_time(&self, ctx: &mut Ctx) -> OpResult<Nullable<f64>> {
        let hub=hub(ctx)?;refresh_css_animation_for_accessor(ctx,&hub,self.id)?;
        let state = hub.borrow();
        let record = state.records.get(&self.id)
            .ok_or_else(|| OpError::new("InvalidStateError", "animation is no longer available"))?;
        if record.cancelled {
            return Ok(Nullable(None));
        }
        let now = record_time(&record);
        if now.is_none() && record.hold_time_ms.is_none() {
            return Ok(Nullable(None));
        }
        let sample = sample_record(record, now.unwrap_or(0.0));
        Ok(Nullable((sample.state != PlaybackState::Idle).then_some(sample.current_time_ms)))
    }
    #[setter(name = "currentTime")]
    fn set_current_time(&self, ctx: &mut Ctx, value: Option<f64>) -> OpResult<()> {
        let hub = hub(ctx)?;
        let now = record_time(&record_state(ctx, self.id)?.1).unwrap_or(0.0);
        let (realm, was_cancelled, completed_pause) = {
            let mut hub_mut = hub.borrow_mut();
            let record = hub_mut.records.get_mut(&self.id).ok_or_else(|| {
                OpError::new("InvalidStateError", "animation is no longer available")
            })?;
            let was_cancelled = record.cancelled;
            if value.is_none() {
                if !was_cancelled && (record.hold_time_ms.is_some() || (record.start_resolved && record_time(record).is_some())) {
                    return Err(OpError::type_error("resolved currentTime cannot become null"));
                }
                return Ok(());
            }
            let completed_pause=record.pending_task==Some(PlaybackState::Paused);
            if let Some(value) = value {
                if !value.is_finite() {
                    return Err(OpError::new("TypeError", "currentTime must be finite"));
                }
                record.start_time_ms = value;
                record.start_ms = now;
                if completed_pause || !record.start_resolved || record.playback_rate==0.0
                    || record.paused_at_ms.is_some() || record_time(record).is_none() {
                    record.hold_time_ms = Some(value);
                }
                if completed_pause {record.pending_task=None;record.paused_at_ms=Some(now);record.start_resolved=false;}
                record.cancelled = false;
                record.finish_event_fired = false;
            } else {
                record.cancelled = true;
                record.hold_time_ms = None;
                record.paused_at_ms = None;
            }
            (record.realm.upgrade(), was_cancelled, completed_pause)
        };
        if let Some(realm) = realm {
            apply_record_change(&hub, &realm, now, self.id)?;
        }
        if completed_pause {resolve_ready(ctx,&hub,self.id)?;}
        if value.is_some() {
            ensure_pending_finished(ctx, &hub, self.id);
            settle_finished(ctx, &hub, now)?;
        } else if !was_cancelled {
            reject_finished(ctx, &hub, self.id);
        }
        Ok(())
    }
    #[getter(name = "playbackRate")]
    fn playback_rate(&self, ctx: &mut Ctx) -> OpResult<f64> {
        Ok(record_state(ctx, self.id)?.1.playback_rate)
    }
    #[setter(name = "playbackRate")]
    fn set_playback_rate(&self, ctx: &mut Ctx, value: f64) -> OpResult<()> {
        if !value.is_finite() {
            return Err(OpError::new("TypeError", "playbackRate must be finite"));
        }
        let hub = hub(ctx)?;
        let now = record_time(&record_state(ctx, self.id)?.1).unwrap_or(0.0);
        let realm = {
            let mut state = hub.borrow_mut();
            let record = state.records.get_mut(&self.id).ok_or_else(|| {
                OpError::new("InvalidStateError", "animation is no longer available")
            })?;
            let current = sample_record(record, now).current_time_ms;
            record.playback_rate = value;
            record.start_time_ms = current;
            record.start_ms = now;
            if record.paused_at_ms.is_some() || !record.start_resolved || value==0.0 {
                record.hold_time_ms = Some(current);
            } else {
                record.hold_time_ms = None;
            }
            record.realm.upgrade()
        };
        if let Some(realm) = realm {
            apply_record_change(&hub, &realm, now, self.id)?;
        }
        settle_finished(ctx, &hub, now)
    }
    #[getter(name = "playState")]
    fn play_state(&self, ctx: &mut Ctx) -> OpResult<String> {
        let hub=hub(ctx)?;refresh_css_animation_for_accessor(ctx,&hub,self.id)?;
        let (_, record) = record_state(ctx, self.id)?;
        if record.cancelled {
            return Ok("idle".into());
        }
        // A resolved start time remains running when its document timeline
        // becomes inactive; an unresolved current time alone is not idle.
        if record.pending_task.is_none() && record.start_resolved
            && record.hold_time_ms.is_none() && record_time(&record).is_none() {
            return Ok("running".into());
        }
        Ok(
            match record.pending_task.unwrap_or_else(||sample_record(&record, 0.0).state) {
                PlaybackState::Idle => "idle",
                PlaybackState::Running => "running",
                PlaybackState::Paused => "paused",
                PlaybackState::Finished => "finished",
            }
            .into(),
        )
    }
    fn play(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<()> {
        let hub=hub(ctx)?;
        if let Some(weak)=ctx.weak_value(&this.0){hub.borrow_mut().wrappers.insert(self.id,weak);}
        let (_,record)=record_state(ctx,self.id)?;
        if record.cancelled || sample_record(&record,0.0).state==PlaybackState::Finished {
            ensure_pending_finished(ctx,&hub,self.id);
        }
        request_pending_task(ctx,&hub,self.id,PlaybackState::Running,true)
    }
    fn pause(&self, ctx: &mut Ctx) -> OpResult<()> {
        let hub=hub(ctx)?;
        request_pending_task(ctx,&hub,self.id,PlaybackState::Paused,false)
    }
    fn cancel(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<()> {
        let (_, prior) = record_state(ctx, self.id)?;
        let should_fire = !prior.cancelled;
        let state = hub(ctx)?;
        queue_css_cancellation(ctx, &state, self.id, record_time(&prior).unwrap_or(0.0), false)?;
        if should_fire {
            let elapsed=(sample_record(&prior,0.0).current_time_ms-prior.timing.delay_ms).clamp(0.0,prior.timing.active_duration());
            if let Some(transition)=state.borrow_mut().transitions.get_mut(&self.id) {transition.cancellation_elapsed=Some(elapsed);}
        }
        reset_pending_task(ctx,&state,self.id)?;
        mutate(self.id, ctx, |record, _| {
            record.cancelled = true;
            record.start_resolved = false;
            record.paused_at_ms = None;
            record.hold_time_ms = None;
        })?;
        let hub = hub(ctx)?;
        if should_fire {
            reject_finished(ctx, &hub, self.id);
            let event = DomEvent::new(ctx, "cancel", None)?;
            let event = ctx.new_instance(event);
            let event = JsObject::from_value(event)
                .ok_or_else(|| OpError::new("TypeError", "could not create animation event"))?;
            crate::events::dispatch_event(ctx, this, event)?;
        }
        Ok(())
    }
    fn finish(&self, ctx: &mut Ctx) -> OpResult<()> {
        let (_, record) = record_state(ctx, self.id)?;
        let end_time = record_sampling_timing(&record).end_time();
        if record.playback_rate == 0.0 || (record.playback_rate > 0.0 && !end_time.is_finite()) {
            return Err(OpError::new(
                "InvalidStateError",
                "cannot finish an animation with an infinite end time",
            ));
        }
        if record.cancelled {
            let hub = hub(ctx)?;
            ensure_pending_finished(ctx, &hub, self.id);
        }
        let hub=hub(ctx)?;
        if let Some(record)=hub.borrow_mut().records.get_mut(&self.id){record.pending_task=None;record.start_resolved=true;}
        resolve_ready(ctx,&hub,self.id)?;
        mutate(self.id, ctx, |record, now| {
            record.start_time_ms = if record.playback_rate < 0.0 {
                0.0
            } else {
                end_time
            };
            record.start_ms = now;
            record.hold_time_ms = None;
            record.paused_at_ms = None;
            record.cancelled = false;
        })
    }
    fn reverse(&self, ctx: &mut Ctx) -> OpResult<()> {
        let hub=hub(ctx)?;let (_,record)=record_state(ctx,self.id)?;
        if record_time(&record).is_none(){return Err(OpError::new("InvalidStateError","animation timeline is inactive"));}
        if record.cancelled || sample_record(&record,0.0).state==PlaybackState::Finished {
            ensure_pending_finished(ctx,&hub,self.id);
        }
        {
            let mut state=hub.borrow_mut();let record=state.records.get_mut(&self.id).unwrap();
            let current=(!record.cancelled).then(||sample_record(record,0.0).current_time_ms);
            record.playback_rate=-record.playback_rate;record.hold_time_ms=current;record.start_resolved=false;
        }
        request_pending_task(ctx,&hub,self.id,PlaybackState::Running,true)
    }

}

#[lumen_bind::class(name = "KeyframeEffect", hint(js(webidl)))]
#[derive(Clone)]
pub struct DomKeyframeEffect {
    id: u32,
}

struct EffectConstructor(DomKeyframeEffect);
impl lumen_bind::CtorRet<JsHost,DomKeyframeEffect> for EffectConstructor {
    fn into_ctor(self,cx:&<JsHost as Host>::Cx<'_>)->Result<Value,Value> {
        let id=self.0.id;
        let value=<JsHost as Host>::construct(cx,self.0)?;
        <JsHost as Host>::with_ctx(cx,|ctx| {
            let state=hub(ctx).map_err(|error|error.to_value(ctx))?;
            if let Some(weak)=ctx.weak_value(&value) {state.borrow_mut().effects.insert(id,weak);}
            publish_effect_target_owner(ctx,&state,id,&value).map_err(|error|error.to_value(ctx))
        })?;
        Ok(value)
    }
}

#[lumen_bind::methods]
impl DomKeyframeEffect {
    #[constructor]
    fn new(
        ctx: &mut Ctx,
        target_or_source: Value,
        #[varargs] arguments: &[Value],
    ) -> OpResult<EffectConstructor> {
        // Web IDL selects the one-argument copy overload before conversion.
        // Read native slots directly: authored properties on source are not
        // consulted, and the copy has no associated animation.
        if arguments.is_empty() {
            let source_id=ctx.with_instance::<DomKeyframeEffect,_>(&target_or_source,|source|source.id)?;
            let (hub,mut source)=effect_state(ctx,source_id)?;
            let id={
                let mut state=hub.borrow_mut();
                state.next_effect_id=state.next_effect_id.wrapping_add(1).max(1);
                source.id=state.next_effect_id;
                source.animation_id=None;
                let id=source.id;
                state.effect_records.insert(id,source);
                id
            };
            return Ok(EffectConstructor(DomKeyframeEffect{id}));
        }
        let target=match &target_or_source {
            Value::Null|Value::Undefined=>None,
            _=>Some(ctx.with_instance::<super::DomElement,_>(&target_or_source,|target|target.base.realm.resolve_adopted_node(target.base.id))?),
        };
        let realm = if let Some((realm, _)) = target.as_ref() {
            realm.clone()
        } else {
            hub(ctx)?
                .borrow()
                .default_realm
                .upgrade()
                .ok_or_else(|| OpError::new("InvalidStateError", "no active document"))?
        };
        let effect = create_effect(
            ctx,
            &realm,
            target.as_ref().map(|(_, node)| *node),
            arguments[0].clone(),
            arguments.get(1).cloned(),
        )?;
        Ok(EffectConstructor(effect))
    }
    #[getter(name="pseudoElement")]
    fn pseudo_element(&self,ctx:&mut Ctx)->OpResult<Nullable<String>> {
        let effect = effect_state(ctx,self.id)?.1;
        Ok(Nullable(effect.pseudo.map(|pseudo| match &effect.capture_name {
            Some(name)=>pseudo.view_transition_selector(Some(name)).expect("registered capture pseudo target"),
            None=>pseudo_name(Some(pseudo)).to_owned(),
        })))
    }
    #[setter(name="pseudoElement",coerce)]
    fn set_pseudo_element(&self,ctx:&mut Ctx,value:Option<String>)->OpResult<()> {
        let pseudo=value.as_deref().map(parse_effect_pseudo).transpose()?.flatten();
        let (hub,previous)=effect_state(ctx,self.id)?;
        if previous.pseudo==pseudo {return Ok(());}
        let realm=previous.realm.upgrade().ok_or_else(||OpError::new("InvalidStateError","effect target document was destroyed"))?;
        let updated={
            let mut state=hub.borrow_mut();
            let effect=state.effect_records.get_mut(&self.id).ok_or_else(||OpError::new("InvalidStateError","effect unavailable"))?;
            effect.pseudo=pseudo;
            effect.clone()
        };
        sync_effect_animation(&hub,&updated);
        apply_effect_change(&hub,&realm,hub_time(&hub),self.id)?;
        Ok(())
    }
    #[getter]
    fn target(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let (_, effect) = effect_state(ctx, self.id)?;
        let Some(node) = effect.node else {
            return Ok(Value::Null);
        };
        let realm = effect
            .realm
            .upgrade()
            .ok_or_else(|| OpError::new("InvalidStateError", "animation realm was destroyed"))?;
        Ok(realm.wrap(ctx, node))
    }
    #[setter(name = "target")]
    fn set_target(&self, ctx: &mut Ctx, target: Option<&super::DomElement>) -> OpResult<()> {
        let (hub, previous) = effect_state(ctx, self.id)?;
        let (realm, node) = target.map_or_else(
            || {
                previous
                    .realm
                    .upgrade()
                    .map(|realm| (realm, None))
                    .ok_or_else(|| {
                        OpError::new("InvalidStateError", "effect document was destroyed")
                    })
            },
            |target| Ok((target.base.realm.clone(), Some(target.base.id))),
        )?;
        if let Some(node) = node {
            if !matches!(
                realm.session.borrow().document().kind(node),
                Ok(NodeKind::Element { .. })
            ) {
                return Err(OpError::new(
                    "TypeError",
                    "KeyframeEffect target must be an element",
                ));
            }
        }
        let now = hub_time(&hub);
        let updated = {
            let mut state = hub.borrow_mut();
            let effect = state.effect_records.get_mut(&self.id).ok_or_else(|| {
                OpError::new("InvalidStateError", "keyframe effect is unavailable")
            })?;
            effect.realm = Rc::downgrade(&realm);
            effect.node = node;
            effect.clone()
        };
        sync_effect_animation(&hub, &updated);
        let wrapper=hub.borrow().effects.get(&self.id).and_then(WeakValue::upgrade);
        if let Some(wrapper)=wrapper {publish_effect_target_owner(ctx,&hub,self.id,&wrapper)?;}
        if let Some(previous_realm) = previous.realm.upgrade() {
            apply_realm(&hub, &previous_realm, now)?;
        }
        apply_effect_change(&hub, &realm, now, self.id)
    }
    #[getter]
    fn composite(&self, ctx: &mut Ctx) -> OpResult<&'static str> {
        Ok(match effect_state(ctx, self.id)?.1.composite {
            CompositeMode::Replace => "replace",
            CompositeMode::Add => "add",
            CompositeMode::Accumulate => "accumulate",
        })
    }
    #[setter(name = "composite")]
    fn set_composite(&self, ctx: &mut Ctx, value: String) -> OpResult<()> {
        let composite = match value.as_str() {
            "replace" => CompositeMode::Replace,
            "add" => CompositeMode::Add,
            "accumulate" => CompositeMode::Accumulate,
            _ => {
                return Err(OpError::new(
                    "TypeError",
                    "invalid keyframe effect composite",
                ));
            }
        };
        let (hub, mut effect) = effect_state(ctx, self.id)?;
        effect.composite = composite;
        {
            let mut state = hub.borrow_mut();
            state.effect_records.insert(self.id, effect.clone());
        }
        let realm = effect
            .realm
            .upgrade()
            .ok_or_else(|| OpError::new("InvalidStateError", "effect document was destroyed"))?;
        sync_effect_animation(&hub, &effect);
        apply_effect_change(&hub, &realm, hub_time(&hub),self.id)
    }
#[getter(name="iterationComposite")]
fn iteration_composite(&self,ctx:&mut Ctx)->OpResult<&'static str> {
    Ok(if effect_state(ctx,self.id)?.1.iteration_accumulate {"accumulate"}else{"replace"})
}
#[setter(name="iterationComposite")]
fn set_iteration_composite(&self,ctx:&mut Ctx,value:String)->OpResult<()> {
    let value=iteration_composite_value(&value)?;
    let(hub,mut effect)=effect_state(ctx,self.id)?;
    effect.iteration_accumulate=value;
    hub.borrow_mut().effect_records.insert(self.id,effect.clone());
    let realm=effect.realm.upgrade().ok_or_else(||OpError::new("InvalidStateError","effect document unavailable"))?;
    apply_effect_change(&hub,&realm,hub_time(&hub),self.id)
}
    fn get_keyframes(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let (_, effect) = effect_state(ctx, self.id)?;
        frames_array(ctx, &effect.keyframes)
    }
    fn get_timing(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let (_, effect) = effect_state(ctx, self.id)?;
        timing_object(
            ctx,
            effect.timing,
            &effect.easing,
            effect.duration_auto,
            effect.fill_auto,
        )
    }
    fn get_computed_timing(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let (hub, effect) = effect_state(ctx, self.id)?;
        let object = timing_object(ctx, effect.timing, &effect.easing, false, false)?;
        let sampled = hub
            .borrow()
            .records
            .values()
            .find(|record| record.effect_id == Some(self.id) && !record.cancelled)
            .map(|record| sample_record(record,0.0));
        let local_time = sampled
            .filter(|sample| sample.state != PlaybackState::Idle)
            .map(|sample| sample.current_time_ms);
        let progress = sampled.and_then(|sample| {
            sample.progress.and_then(|progress| {
                animation::ease_with_before(&effect.easing, progress, sample.before)
            })
        });
        for (name, value) in [
            ("activeDuration", Some(effect.timing.active_duration())),
            ("endTime", Some(effect.timing.end_time())),
            ("localTime", local_time),
            ("progress", progress),
            (
                "currentIteration",
                sampled.and_then(|sample| sample.current_iteration),
            ),
        ] {
            ctx.set_member(&object, name, value.map(Value::Num).unwrap_or(Value::Null))
                .map_err(|_| OpError::new("Error", "computed timing assignment failed"))?;
        }
        Ok(object)
    }
    fn set_keyframes(&self, ctx: &mut Ctx, keyframes: Value) -> OpResult<()> {
        let frames = parse_keyframes(ctx, &keyframes)?;
        let (hub, effect) = effect_state(ctx, self.id)?;
        let realm = effect
            .realm
            .upgrade()
            .ok_or_else(|| OpError::new("InvalidStateError", "animation realm was destroyed"))?;
        let now = hub_time(&hub);
        let updated = {
            let mut state = hub.borrow_mut();
            let effect = state.effect_records.get_mut(&self.id).ok_or_else(|| {
                OpError::new("InvalidStateError", "animation is no longer available")
            })?;
            effect.keyframes = frames.into();
            effect.clone()
        };
        let attached = sync_effect_animation(&hub, &updated);
        if let Some((id, _)) = attached {
            // Typed endpoints describe the generated keyframes, not later replacements.
            if let Some(transition)=hub.borrow_mut().transitions.get_mut(&id) {transition.endpoint_styles=None;}
            refresh_ready(ctx, &hub, id)?;
        }
        apply_effect_change(&hub, &realm, now, self.id)
    }
    fn update_timing(&self, ctx: &mut Ctx, options: Option<Value>) -> OpResult<()> {
        let (hub, effect) = effect_state(ctx, self.id)?;
        let merged = timing_object(
            ctx,
            effect.timing,
            &effect.easing,
            effect.duration_auto,
            effect.fill_auto,
        )?;
        if let Some(options) =
            options.filter(|value| !matches!(value, Value::Null | Value::Undefined))
        {
            if !matches!(options, Value::Obj(_)) {
                return Err(OpError::new(
                    "TypeError",
                    "updateTiming requires a timing dictionary",
                ));
            }
            for key in [
                "delay",
                "direction",
                "duration",
                "easing",
                "endDelay",
                "fill",
                "iterationStart",
                "iterations",
            ] {
                let value = ctx
                    .get_member(&options, key)
                    .map_err(|error| OpError::thrown(lumen::embed::abrupt_value(error)))?;
                if matches!(value, Value::Undefined) {
                    continue;
                }
                ctx.set_member(&merged, key, value)
                    .map_err(|_| OpError::new("TypeError", "timing update failed"))?;
            }
        }
        let (timing, easing, duration_auto, fill_auto) = parse_options(ctx, Some(&merged))?;
        let now = hub_time(&hub);
        let updated = {
            let mut state = hub.borrow_mut();
            let effect = state.effect_records.get_mut(&self.id).ok_or_else(|| {
                OpError::new("InvalidStateError", "animation is no longer available")
            })?;
            effect.timing = timing;
            effect.easing = easing.into();
            effect.duration_auto = duration_auto;
            effect.fill_auto = fill_auto;
            effect.clone()
        };
        sync_effect_animation(&hub, &updated);
        let realm = updated
            .realm
            .upgrade()
            .ok_or_else(|| OpError::new("InvalidStateError", "animation realm was destroyed"))?;
        apply_effect_change(&hub, &realm, now, self.id)?;
        settle_finished(ctx, &hub, now)
    }
}

#[lumen_bind::class(name = "DocumentTimeline", hint(js(webidl)))]
#[derive(Clone)]
pub struct DomDocumentTimeline {
    id: u32,
    realm: Weak<DomRealm>,
    origin_time_ms: f64,
}

#[lumen_bind::methods]
impl DomDocumentTimeline {
    #[constructor]
    fn new(ctx: &mut Ctx, this: This<Value>, options: Option<Value>) -> OpResult<Self> {
        let hub = hub(ctx)?;
        let realm = hub.borrow().default_realm.upgrade().ok_or_else(|| {
            OpError::new(
                "InvalidStateError",
                "document timeline has no active document",
            )
        })?;
        let mut origin_time_ms = 0.0;
        if let Some(options) =
            options.filter(|value| !matches!(value, Value::Undefined | Value::Null))
        {
            let value = ctx
                .get_member(&options, "originTime")
                .map_err(|_| OpError::new("TypeError", "timeline originTime getter failed"))?;
            if !matches!(value, Value::Undefined) {
                origin_time_ms = ctx.coerce_number(&value).map_err(OpError::thrown)?;
                if !origin_time_ms.is_finite() {
                    return Err(OpError::new("TypeError", "originTime must be finite"));
                }
            }
        }
        let id = {
            let mut state = hub.borrow_mut();
            state.next_timeline_id = state.next_timeline_id.wrapping_add(1).max(1);
            state.next_timeline_id
        };
        if let Some(weak) = ctx.weak_value(&this.0) {
            hub.borrow_mut().timeline_values.insert(id, weak);
        }
        Ok(Self {
            id,
            realm: Rc::downgrade(&realm),
            origin_time_ms,
        })
    }
    #[getter(name = "currentTime")]
    fn current_time(&self) -> Nullable<f64> {
        Nullable(self.realm
            .upgrade()
            .and_then(|realm| document_time(&realm))
            .map(|time| time - self.origin_time_ms))
    }
    #[getter(name = "duration")]
    fn duration(&self) -> Nullable<f64> {
        Nullable(None)
    }
    #[getter(name = "originTime")]
    fn origin_time(&self) -> f64 {
        self.origin_time_ms
    }
}

fn mutate(id: u32, ctx: &mut Ctx, change: impl FnOnce(&mut Record, f64)) -> OpResult<()> {
    let hub = hub(ctx)?;
    let now = hub_time(&hub);
    let realm = {
        let mut state = hub.borrow_mut();
        let record = state
            .records
            .get_mut(&id)
            .ok_or_else(|| OpError::new("InvalidStateError", "animation is no longer available"))?;
        change(record, record_time(record).unwrap_or(0.0));
        record.realm.upgrade()
    };
    if let Some(realm) = realm {
        apply_record_change(&hub, &realm, now, id)?;
    }
    refresh_ready(ctx, &hub, id)?;
    settle_finished(ctx, &hub, now)?;
    Ok(())
}

#[cfg(test)]
mod tests {
#[test]
fn specification_animation_context_computed_endpoints_follow_sampled_font_and_live_query() {
    let mut engine=Engine::new();
    let realm=crate::install(engine.ctx(),"<div id=parent style='width:400px;font-size:10px'><div id=target style='font-size:10px;width:10px;height:10px'></div></div>",256).unwrap();
    progress_layout(&realm);
    eval_ok(&mut engine,r#"globalThis.target=document.getElementById('target');
        globalThis.context=target.animate({width:['10em','20em']},{duration:1000,fill:'both'});
        context.pause();context.currentTime=500;
        if(getComputedStyle(target).width!=='150px')throw Error('actual font endpoint midpoint '+getComputedStyle(target).width);
        if(context.effect.getKeyframes()[0].width!=='10em')throw Error('authored frame was computed eagerly');
        target.style.fontSize='20px';
        if(getComputedStyle(target).width!=='300px')throw Error('live font mutation '+getComputedStyle(target).width);
        context.cancel();target.style.fontSize='10px';
        globalThis.fontAndWidth=target.animate({fontSize:['10px','20px'],width:['10em','20em'],lineHeight:['1em','2em']},{duration:1000,fill:'both'});
        fontAndWidth.pause();fontAndWidth.currentTime=500;
        if(getComputedStyle(target).fontSize!=='15px' || getComputedStyle(target).width!=='225px' || getComputedStyle(target).lineHeight!=='22.5px')throw Error('same effect dependency order '+getComputedStyle(target).cssText);
        fontAndWidth.cancel();
        document.getElementById('parent').style.cssText='width:200px;container-type:inline-size;font-size:10px';
        globalThis.query=target.animate({width:['10cqw','20cqw']},{duration:1000,fill:'both'});
        query.pause();query.currentTime=500;
        if(getComputedStyle(target).width!=='30px')throw Error('actual query endpoint midpoint '+getComputedStyle(target).width);
        document.getElementById('parent').style.width='400px';
        if(getComputedStyle(target).width!=='60px')throw Error('live query endpoint mutation '+getComputedStyle(target).width);
        query.cancel();
        target.style.setProperty('--length','10px');
        globalThis.variable=target.animate({width:['var(--length)','calc(var(--length) * 3)']},{duration:1000,fill:'both'});
        variable.pause();variable.currentTime=500;
        if(getComputedStyle(target).width!=='20px')throw Error('computed variable endpoints '+getComputedStyle(target).width);
        target.style.setProperty('--length','20px');
        if(getComputedStyle(target).width!=='40px')throw Error('live variable endpoints '+getComputedStyle(target).width);
    "#);
}

#[test]
fn specification_connected_effect_demand_scopes_computed_reads_and_preserves_unrelated_overlays(){
    let mut engine=Engine::new();
    let realm=crate::install(engine.ctx(),"<div id=parent><div id=one></div></div><div id=other></div>",128).unwrap();
    progress_layout(&realm);
    eval_ok(&mut engine,r#"globalThis.one=document.getElementById('one');globalThis.other=document.getElementById('other');
        globalThis.firstMotion=one.animate({scale:['1','3']},{duration:1000,fill:'both'});firstMotion.pause();firstMotion.currentTime=250;
        globalThis.otherMotion=other.animate({scale:['1','3']},{duration:1000,fill:'both'});otherMotion.pause();otherMotion.currentTime=250;
        if(getComputedStyle(one).scale!=='1.5'||getComputedStyle(other).scale!=='1.5')throw Error('initial connected samples');
        firstMotion.currentTime=500;
        if(getComputedStyle(one).scale!=='2'||getComputedStyle(other).scale!=='1.5')throw Error('unrelated overlay erased');
    "#);
    let find=|name|{let session=realm.session.borrow();selector::query_selector(session.document(),session.document().root(),name).unwrap().unwrap()};
    let one=find("#one");let other=find("#other");let parent=find("#parent");
    let state=hub(engine.ctx()).unwrap();let key=Rc::as_ptr(&realm)as usize;
    let demand=EffectDemand::for_property(&state,&realm,Some(one),"scale").unwrap();
    assert!(demand.scoped_connected);
    {let session=realm.session.borrow();assert!(demand.includes(session.document(),Some(one)));assert!(demand.includes(session.document(),Some(parent)));assert!(!demand.includes(session.document(),Some(other)));}
    assert!(!EffectDemand::for_property(&state,&realm,Some(one),"width").unwrap().scoped_connected);
    assert_eq!(state.borrow().overlaid_nodes.get(&key).unwrap().len(),2);
    eval_ok(&mut engine,r#"firstMotion.cancel();if(getComputedStyle(one).scale!=='none'||getComputedStyle(other).scale!=='1.5')throw Error('scoped removal');
        otherMotion.effect.target=one;otherMotion.currentTime=500;
        if(getComputedStyle(one).scale!=='2'||getComputedStyle(other).scale!=='none')throw Error('retarget old owner cleanup');
        firstMotion.effect.target=other;firstMotion.play();firstMotion.pause();firstMotion.currentTime=250;
        if(getComputedStyle(other).scale!=='1.5')throw Error('scope resume');
    "#);
    advance(engine.ctx(),lumen_host::perf::now_ms()).unwrap();
    eval_ok(&mut engine,"if(getComputedStyle(one).scale!=='2'||getComputedStyle(other).scale!=='1.5')throw Error('global rendering must preserve both owners');");
}

#[test]
fn specification_animation_context_detached_demand_keeps_records_and_live_ancestor_semantics() {
    let mut engine=Engine::new();
    let realm=crate::install(engine.ctx(),"<div id=target style='font-size:10px;width:10px'></div>",512).unwrap();
    progress_layout(&realm);
    eval_ok(&mut engine,r#"globalThis.detachedEffects=[];
        for(let index=0;index<64;index++){
            const parent=document.createElement('div'),child=document.createElement('div');
            parent.style.fontSize='10px';parent.appendChild(child);document.body.appendChild(parent);
            const effect=child.animate({width:['10em','20em']},{duration:1000,fill:'both'});
            effect.pause();effect.currentTime=500;parent.remove();detachedEffects.push(effect);
        }
        globalThis.target=document.getElementById('target');globalThis.connected=target.animate({width:['10em','20em']},{duration:1000,fill:'both'});
        connected.pause();connected.currentTime=500;
        if(getComputedStyle(target).width!=='150px')throw Error('connected demand');
    "#);
    let state=hub(engine.ctx()).unwrap();
    let key=Rc::as_ptr(&realm) as usize;
    assert_eq!(state.borrow().records.values().filter(|record|!record.cancelled).count(),65,"detached effects must remain retained");
    assert_eq!(state.borrow().overlaid_nodes.get(&key).map_or(0,|nodes|nodes.len()),1,"connected publication must not capture unrelated detached sources");
    eval_ok(&mut engine,r#"globalThis.detached=detachedEffects[0].effect.target;
        if(getComputedStyle(detached).width!=='150px')throw Error('explicit detached query');
        detached.parentNode.style.fontSize='20px';
        if(detached.computedStyleMap().get('width').toString()!=='300px')throw Error('computed map detached inherited mutation');
        globalThis.parentAnimation=detached.parentNode.animate({fontSize:['20px','40px']},{duration:1000,fill:'both'});
        parentAnimation.pause();parentAnimation.currentTime=500;
        if(getComputedStyle(detached).width!=='450px')throw Error('sampled detached ancestor lineage '+getComputedStyle(detached).width);
        detachedEffects[0].effect.target=target;
        if(getComputedStyle(target).width!=='150px')throw Error('actual retarget demand');
        if(getComputedStyle(detached).width==='450px')throw Error('old target retained an effect overlay');
    "#);
    let session=realm.session.borrow();
    assert!(EffectDemand::new(&realm,None).unwrap().detached.is_empty());
    drop(session);
}

#[test]
fn specification_animation_context_iteration_composition_uses_typed_accumulation_and_nonadditive_fallback() {
    let mut engine=Engine::new();
    let realm=crate::install(engine.ctx(),"<div id=target style='width:200px;position:relative'></div><div id=other></div>",128).unwrap();
    progress_layout(&realm);
    eval_ok(&mut engine,r#"globalThis.target=document.getElementById('target');
        const effect=new KeyframeEffect(target,{marginLeft:['0px','10px']},{duration:1000,iterations:3,fill:'both',iterationComposite:'accumulate'});
        if(effect.iterationComposite!=='accumulate' || new KeyframeEffect(effect).iterationComposite!=='accumulate')throw Error('real iteration interface and copy');
        globalThis.iteration=new Animation(effect);iteration.pause();iteration.currentTime=2500;
        if(getComputedStyle(target).marginLeft!=='25px')throw Error('third iteration midpoint '+getComputedStyle(target).marginLeft);
        iteration.currentTime=3000;
        if(getComputedStyle(target).marginLeft!=='30px')throw Error('filled terminal accumulation '+getComputedStyle(target).marginLeft);
        effect.iterationComposite='replace';
        if(getComputedStyle(target).marginLeft!=='10px')throw Error('live operation replacement');
        effect.iterationComposite='accumulate';effect.setKeyframes({marginLeft:['0em','1em']});target.style.fontSize='20px';iteration.currentTime=2500;
        if(getComputedStyle(target).marginLeft!=='50px')throw Error('contextual accumulated final endpoint');
        iteration.cancel();
        target.style.color='blue';globalThis.addColor=target.animate({color:['red','red']},{duration:1000,fill:'both',composite:'add',iterationComposite:'accumulate',iterations:3});
        addColor.pause();addColor.currentTime=2500;
        if(getComputedStyle(target).color!=='rgb(255, 0, 0)')throw Error('nonadditive color returns V_B '+getComputedStyle(target).color);
        addColor.cancel();
        const source='rgb(255, 0, 0) 2px 3px';
        target.style.boxShadow='rgb(0, 0, 255) 8px 9px';
        globalThis.shadow=target.animate({boxShadow:[source,source]},{duration:1000,fill:'both',composite:'add',iterationComposite:'accumulate',iterations:3});shadow.pause();shadow.currentTime=2500;
        const actual=getComputedStyle(target).boxShadow;shadow.effect.composite='replace';shadow.effect.iterationComposite='replace';
        if(getComputedStyle(target).boxShadow!==actual)throw Error('nonadditive shadow fallback changed list');
    "#);
}

    #[test]
    fn specification_animation_demand_constructor_and_unattached_mutations_do_not_capture_styles() {
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<div id=one style='color:red;width:100px'></div><div id=two style='color:blue;width:200px'></div>",128).unwrap();
        progress_layout(&realm);
        let before=realm.session.borrow().style_cache_stats();
        let read_before=realm.session.borrow().read_style_cache_stats();
        eval_ok(&mut engine,r#"globalThis.demandEffects=[];for(let index=0;index<64;index++){
            const effect=new KeyframeEffect(document.getElementById('one'),{left:['0px','100px'],opacity:[0,1]},{duration:1000,fill:'both'});
            effect.target=document.getElementById('two');effect.pseudoElement='::before';
            effect.setKeyframes({left:['20px','80px'],opacity:[.2,.8]});effect.pseudoElement=null;
            demandEffects.push(effect);
        }
        globalThis.authoredColorEffect=new KeyframeEffect(document.getElementById('one'),{borderTopColor:['currentColor','currentColor']},1000);
        if(authoredColorEffect.getKeyframes().some(frame=>frame.borderTopColor.toLowerCase()!=='currentcolor'))throw Error('authored color was replaced by an eager snapshot');"#);
        let after=realm.session.borrow().style_cache_stats();
        let read_after=realm.session.borrow().read_style_cache_stats();
        assert_eq!((after.computed_styles,after.styled_nodes),(before.computed_styles,before.styled_nodes),"unattached keyframes must not walk target ancestry");
        assert_eq!((read_after.computed_styles,read_after.styled_nodes),(read_before.computed_styles,read_before.styled_nodes),"unattached mutations must not acquire resolved style sources");
    }

    #[test]
    fn specification_animation_demand_current_color_replays_live_sampled_parent_and_retarget() {
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<div id=parent style='color:rgb(100,0,0)'><div id=child style='border-top:1px solid'></div></div><div id=other style='color:rgb(0,100,0);border-top:1px solid'></div>",128).unwrap();
        progress_layout(&realm);
        eval_ok(&mut engine,r#"globalThis.demandParent=document.getElementById('parent');globalThis.child=document.getElementById('child');globalThis.other=document.getElementById('other');
            globalThis.colorMotion=child.animate({borderTopColor:['currentColor','currentColor']},{duration:1000,fill:'both'});colorMotion.pause();colorMotion.currentTime=500;
            if(getComputedStyle(child).borderTopColor!==getComputedStyle(demandParent).color)throw Error('initial inherited current color');
            demandParent.style.color='rgb(0,0,100)';colorMotion.currentTime=500;
            if(getComputedStyle(child).borderTopColor!==getComputedStyle(demandParent).color)throw Error('live author color change');
            globalThis.parentMotion=demandParent.animate({color:['rgb(100,0,0)','rgb(0,0,100)']},{duration:1000,fill:'both'});parentMotion.pause();parentMotion.currentTime=500;colorMotion.currentTime=500;
            if(getComputedStyle(child).borderTopColor!==getComputedStyle(demandParent).color)throw Error('sampled animated parent color');
            colorMotion.effect.target=other;colorMotion.currentTime=500;
            if(getComputedStyle(other).borderTopColor!==getComputedStyle(other).color)throw Error('retargeted live current color');
            other.style.color='rgb(100,100,0)';colorMotion.effect.setKeyframes({borderTopColor:['currentColor','currentColor']});colorMotion.currentTime=500;
            if(getComputedStyle(other).borderTopColor!==getComputedStyle(other).color)throw Error('replacement keyframes live color');
            colorMotion.effect.setKeyframes({color:['rgb(100,0,0)','rgb(0,0,100)'],borderTopColor:['currentColor','currentColor']});colorMotion.currentTime=500;
            if(getComputedStyle(other).borderTopColor!==getComputedStyle(other).color)throw Error('same effect sampled color dependency');
            if(colorMotion.effect.getKeyframes().some(frame=>frame.borderTopColor.toLowerCase()!=='currentcolor'))throw Error('sampling mutated author frames');"#);
    }

    #[test]
    fn specification_animation_demand_neutral_percentage_transform_uses_live_reference_box() {
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<div id=one style='width:100px;height:20px;transform:translateX(50%)'></div><div id=two style='width:300px;height:20px;transform:translateX(50%)'></div>",128).unwrap();
        progress_layout(&realm);
        eval_ok(&mut engine,r#"globalThis.percentOne=document.getElementById('one');globalThis.percentTwo=document.getElementById('two');
            globalThis.percentMotion=percentOne.animate([{transform:'matrix(1,0,0,1,100,0)',offset:1}],{duration:1000,fill:'both'});percentMotion.pause();percentMotion.currentTime=500;
            const expect=(target,expected)=>{const value=new DOMMatrix(getComputedStyle(target).transform).m41;if(Math.abs(value-expected)>.001)throw Error('actual neutral transform '+value+' expected '+expected)};
            expect(percentOne,75);percentOne.style.width='200px';percentMotion.currentTime=500;expect(percentOne,100);
            percentMotion.effect.target=percentTwo;percentMotion.currentTime=500;expect(percentTwo,125);
            if(percentMotion.effect.getKeyframes()[0].transform!=='matrix(1,0,0,1,100,0)')throw Error('transform source mutated');"#);
    }

    #[test]
    fn specification_animation_effect_underlying_working_set_admits_real_map_growth_transactionally() {
        let mut budget=EffectSamplingBudget{bytes:0,style_bytes:0};
        let mut entries=HashMap::<usize,usize>::new();
        for index in 0..128 {
            budget.reserve(&mut entries,1).unwrap();entries.insert(index,index);
            assert!(budget.bytes>=entries.capacity()*std::mem::size_of::<(usize,usize)>());
            assert!(budget.bytes<=MAX_EFFECT_UNDERLYING_BYTES);
        }
        let map_bytes=budget.bytes;
        budget.style(&lumen_html::css::Style::initial(),true).unwrap();
        assert!(budget.bytes>map_bytes);budget.release_styles();assert_eq!(budget.bytes,map_bytes);
        let capacity=entries.capacity();let length=entries.len();
        budget.bytes=MAX_EFFECT_UNDERLYING_BYTES-1;
        assert!(budget.reserve(&mut entries,capacity+1).is_err());
        assert_eq!(entries.capacity(),capacity,"quota rejects before allocation");
        assert_eq!(entries.len(),length,"quota preserves actual map entries");
        for index in 0..128{assert_eq!(entries.get(&index),Some(&index));}
    }

    #[test]
    fn specification_animation_effect_stack_publishes_only_highest_origin_and_rebases_inherited_parent() {
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<style>#target{position:relative;left:200px;transition:left 100s linear}#target.end{left:300px}#important{position:relative;left:0px!important;transition:left 100s linear}#important.end{left:100px!important}#parent{font-size:10px;transition:font-size 100s linear}#parent.end{font-size:30px}</style><div id=target></div><div id=important></div><div id=parent><div id=child style='position:relative;left:2em'></div></div>",128).unwrap();
        progress_layout(&realm);advance(engine.ctx(),0.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,r#"(()=>{
            const target=document.getElementById('target'),important=document.getElementById('important'),parent=document.getElementById('parent'),child=document.getElementById('child');
            const expect=(target,property,expected,message)=>{const actual=getComputedStyle(target)[property];if(actual!==expected)throw Error(message+' '+actual)};
            for(const element of [target,important,parent])element.className='end';
            const transition=target.getAnimations()[0],importantTransition=important.getAnimations()[0],parentTransition=parent.getAnimations()[0];
            for(const effect of [transition,importantTransition,parentTransition]){effect.pause();effect.currentTime=50000}
            expect(target,'left','250px','transition origin only');
            expect(important,'left','50px','transition overrides important author');
            const add=target.animate([{left:'20px',composite:'add'},{left:'20px',composite:'add'}],{duration:1000,fill:'both'});
            add.pause();add.currentTime=500;expect(target,'left','270px','higher animation consumes lower transition');
            const importantAdd=important.animate([{left:'20px',composite:'add'},{left:'20px',composite:'add'}],{duration:1000,fill:'both'});
            importantAdd.pause();importantAdd.currentTime=500;expect(important,'left','100px','highest animation origin respects author important');
            importantAdd.cancel();expect(important,'left','50px','lower transition origin restored');
            const font=parent.animate([{fontSize:'10px',composite:'add'},{fontSize:'10px',composite:'add'}],{duration:1000,fill:'both'});
            font.pause();font.currentTime=500;expect(parent,'fontSize','30px','mixed parent stack');
            const childEffect=child.animate([{offset:0,left:'-10px',composite:'add'}],{duration:1000,fill:'both'});
            childEffect.pause();childEffect.currentTime=500;expect(child,'left','55px','child inherits final parent stack');
            parentTransition.currentTime=25000;expect(parent,'fontSize','25px','parent lower transition remains live');expect(child,'left','45px','memo replays changed inherited stack');
            target.style.left='400px';
            const replacement=target.getAnimations().find(effect=>effect instanceof CSSTransition);
if(!replacement)throw Error('retarget missing transition after mixed old stack');
            const frames=replacement.effect.getKeyframes();
            if(frames[0].left!=='270px')throw Error('retarget loses complete old effect stack '+frames[0].left);
            if(frames[frames.length-1].left!=='420px')throw Error('after-change stack includes old transition '+frames[frames.length-1].left);
            add.cancel();
            return true;
        })()"#),Value::Bool(true)));
    }

    #[test]
    fn specification_border_image_web_animation_addition_and_incompatible_whole_value_sampling() {
        let mut engine=Engine::new();let realm=crate::install(engine.ctx(),"<div id=target style='border-image-width:1 2 3 4;border-image-slice:10 20% fill'></div>",64).unwrap();
        progress_layout(&realm);advance(engine.ctx(),0.0).unwrap();
        let value=eval_ok(&mut engine,r#"(()=>{
            const target=document.getElementById('target');
            const expect=(property,value)=>{const actual=getComputedStyle(target).getPropertyValue(property);if(actual!==value)throw Error(property+': '+actual+' expected '+value)};
            const added=target.animate({borderImageWidth:['1 2 3 4','101 102 103 104']},{duration:1000,fill:'both',composite:'add'});
            added.pause();added.currentTime=500;expect('border-image-width','52 54 56 58');
            target.style.borderImageWidth='10 20 30 40';expect('border-image-width','61 72 83 94');added.cancel();
            const slice=target.animate({borderImageSlice:['10 20% fill','110 120% fill']},{duration:1000,fill:'both',composite:'accumulate'});
            slice.pause();slice.currentTime=500;expect('border-image-slice','70 90% fill');slice.cancel();
            const discrete=target.animate({borderImageSlice:['10 20% fill','110% 120']},{duration:1000,fill:'both'});
            discrete.pause();discrete.currentTime=250;expect('border-image-slice','10 20% fill');
            discrete.currentTime=750;expect('border-image-slice','110% 120');discrete.cancel();
            return true;
        })()"#);
        assert!(matches!(value,Value::Bool(true)));
    }
    #[test]
    fn specification_animation_neutral_detached_targets_keep_real_parent_provenance_across_batches() {
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<section id=container style='font-size:20px'><div id=old style='position:relative;left:2em'></div></section><div id=new style='position:relative;left:100px'></div>",96).unwrap();
        let old=realm.with_session(|session|selector::query_selector(session.document(),session.document().root(),"#old").unwrap().unwrap());
        eval_ok(&mut engine,r#"(()=>{
            const container=document.getElementById('container'),old=document.getElementById('old');
            const effect=old.animate([{offset:0,left:'-10px',composite:'add'}],{duration:1000,fill:'both'});
            effect.pause();effect.currentTime=500;
            if(getComputedStyle(old).left!=='35px')throw Error('initial inherited underlying');
            container.remove();container.style.fontSize='30px';
        })()"#);
        engine.ctx().collect_garbage();
        assert!(matches!(eval_ok(&mut engine,r#"(()=>{
            const target=document.getElementById('new');
            const effect=target.animate([{offset:0,left:'-20px',composite:'add'}],{duration:1000,fill:'both'});
            effect.pause();effect.currentTime=500;
            if(getComputedStyle(target).left!=='90px')throw Error('successive connected batch');
            return true;
        })()"#),Value::Bool(true)));
        realm.with_session(|session|{
            assert!(!session.document().is_connected_element(old));
            let style=session.computed_style(old).unwrap();
            assert_eq!(style.font_size,30.0,"retained detached target inherits its actual retained parent");
            assert_eq!(style.left,Some(55.0),"prior live effect resolves neutral source after the batch container is removed");
        });
    }

    #[test]
    fn specification_animation_neutral_underlying_tracks_live_author_and_lower_effect_stack() {
        let mut engine=Engine::new();
        crate::install(engine.ctx(),"<div id=target style='position:relative;left:200px'></div>",64).unwrap();
        assert!(matches!(eval_ok(&mut engine,r#"(()=>{
            const target=document.getElementById('target');
            const expect=(value,message)=>{const actual=getComputedStyle(target).left;if(actual!==value)throw Error(message+' '+actual)};
            const effect=target.animate([{offset:0,left:'-50px',composite:'add'}],{duration:1000,fill:'both'});
            effect.pause();effect.currentTime=500;
            expect('175px','initial neutral boundary');
            if(effect.effect.getKeyframes().length!==1)throw Error('neutral boundary appeared in authored keyframes');
            target.style.left='300px';expect('275px','live changed author underlying');
            effect.cancel();
            const lower=target.animate([{left:'50px',composite:'add'},{left:'50px',composite:'add'}],{duration:1000,fill:'both'});
            lower.pause();lower.currentTime=500;
            const upper=target.animate([{offset:0,left:'20px',composite:'add'}],{duration:1000,fill:'both'});
            upper.pause();upper.currentTime=500;
            expect('360px','neutral boundary consumes lower live effect');
            lower.effect.setKeyframes([{left:'100px',composite:'add'},{left:'100px',composite:'add'}]);
            expect('410px','modified lower effect advances underlying');
            return true;
        })()"#),Value::Bool(true)));
    }

    #[test]
    fn view_transition_capture_animation_uses_real_pause_seek_cancel_and_finish() {
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<div></div>",64).unwrap();
        advance(engine.ctx(),0.0).unwrap();
        let root=realm.with_session(|session|lumen_html::selector::document_element(session.document()).unwrap());
        let mut controls:[Option<Arc<str>>;19]=std::array::from_fn(|_|None);
        controls[1]=Some(Arc::from("1s"));controls[3]=Some(Arc::from("linear"));
        controls[6]=Some(Arc::from("both"));controls[7]=Some(Arc::from("paused"));
        let frames=vec![Keyframe{ offset_is_specified: true,offset:0.0,declarations:vec![("opacity".into(),"0".into())],easing:None,composite:None},
            Keyframe{ offset_is_specified: true,offset:1.0,declarations:vec![("opacity".into(),"1".into())],easing:None,composite:None}];
        let (id,wrapper)=create_capture_animation(engine.ctx(),&realm,root,
            lumen_html::css::PseudoElement::ViewTransitionNew,Arc::from("root"),"-ua-view-transition-fade-in",
            &controls,0,frames).unwrap();
        let global=engine.ctx().global_object();
        engine.ctx().member_set(&global,"captureAnimation",wrapper).unwrap_or_else(|_|panic!("capture wrapper publication failed"));
        advance(engine.ctx(),0.0).unwrap();advance(engine.ctx(),500.0).unwrap();
        assert!(!capture_animation_sample(engine.ctx(),id).unwrap().0,"paused capture must keep transition pending");
        eval_ok(&mut engine,"if(captureAnimation.effect.pseudoElement!=='::view-transition-new(root)')throw 'pseudo identity';captureAnimation.currentTime=500");
        let (_,values)=capture_animation_sample(engine.ctx(),id).unwrap();
        assert_eq!(values,vec![("opacity".to_owned(),"0.5".to_owned())]);
        eval_ok(&mut engine,"captureAnimation.finish()");
        assert!(capture_animation_sample(engine.ctx(),id).unwrap().0);
        cancel_capture_animation(engine.ctx(),id).unwrap();
        assert!(capture_animation_sample(engine.ctx(),id).unwrap().0);
    }
    fn progress_layout(realm: &Rc<DomRealm>) {
        let font=Rc::new(lumen_html_text::FontFace::new(std::sync::Arc::from(lumen_html_text::DEFAULT_FONT_BYTES)).unwrap());
        realm.set_layout_flusher(Rc::new(move|session|session.display_list(800,600,font.as_ref()).map(|_|()).map_err(|error|format!("{error:?}"))));
    }
    #[test]
    fn specification_css_animation_pseudo_inventory_underlying_geometry_and_order() {
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<style>@keyframes fade{from{opacity:0}}#target{width:200px;height:100px;opacity:.6;animation:fade 1s linear both;transition:margin-left 1s linear}#target::before{content:'';display:block;width:80px;height:30px;opacity:.2;animation:fade 1s linear both}#target::after{content:'';display:block;width:40px;height:10px;opacity:.4;animation:fade 1s linear both}#child{animation:fade 1s linear both}#target.end{margin-left:100px}</style><div id='target'><div id='child'></div></div>",192).unwrap();
        progress_layout(&realm);
        eval_ok(&mut engine,"globalThis.target=document.getElementById('target');globalThis.events=[];target.addEventListener('animationstart',e=>{if(e.target===target)events.push([e.pseudoElement,e instanceof AnimationEvent])});globalThis.inventory=target.getAnimations({subtree:true});globalThis.before=inventory.find(a=>a.effect.pseudoElement==='::before');globalThis.after=inventory.find(a=>a.effect.pseudoElement==='::after');globalThis.script=target.animate({opacity:[0,1]},1000)");
        advance(engine.ctx(),0.0).unwrap();advance(engine.ctx(),500.0).unwrap();
        let diagnostic=eval_ok(&mut engine,"JSON.stringify({inventory:inventory.map(a=>[a.constructor.name,a.effect.target.id,a.effect.pseudoElement]),before:getComputedStyle(target,'::before').opacity,after:getComputedStyle(target,'::after').opacity,events,owner:before.effect.target===target,classes:[before instanceof CSSAnimation,after instanceof CSSAnimation]})");
        let diagnostic=match diagnostic {Value::Str(value)=>value.to_string(),_=>panic!("pseudo inventory diagnostic was not JSON")};
        assert!(matches!(eval_ok(&mut engine,"inventory.length===4 && inventory[0].effect.pseudoElement===null && inventory[1]===before && inventory[2]===after && inventory[3].effect.target.id==='child' && before instanceof CSSAnimation && after instanceof CSSAnimation && before.effect.target===target"),Value::Bool(true)),"pseudo inventory and owning identities: {diagnostic}");
        assert!(matches!(eval_ok(&mut engine,"getComputedStyle(target,'::before').opacity==='0.1' && getComputedStyle(target,'::after').opacity==='0.2'"),Value::Bool(true)),"pseudo missing-endpoint samples: {diagnostic}");
        assert!(matches!(eval_ok(&mut engine,"events.length===3 && events.every(e=>e[1]) && events[0][0]==='' && events[1][0]==='::before' && events[2][0]==='::after'"),Value::Bool(true)),"pseudo event phase and order: {diagnostic}");
        realm.flush_layout().unwrap();
        let node=realm.with_session(|session|selector::query_selector(session.document(),session.document().root(),"#target").unwrap().unwrap());
        let rect=realm.session.borrow().pseudo_layout_rect(node,lumen_html::css::PseudoElement::Before).expect("real animation pseudo geometry");
        assert_eq!((rect.width,rect.height),(80.0,30.0));
        assert!(matches!(eval_ok(&mut engine,"target.className='end';globalThis.ordered=target.getAnimations({subtree:true});ordered[0] instanceof CSSTransition && ordered[1] instanceof CSSAnimation && ordered[2]===before && ordered[3]===after && ordered[ordered.length-1]===script"),Value::Bool(true)));
        assert!(matches!(eval_ok(&mut engine,"globalThis.effect=new KeyframeEffect(target,{opacity:[0,1]},{duration:1000,pseudoElement:'::before'});effect.pseudoElement==='::before' && effect.target===target"),Value::Bool(true)));
        assert!(matches!(eval_ok(&mut engine,"effect.pseudoElement='::after';effect.pseudoElement==='::after'"),Value::Bool(true)));
        assert!(matches!(eval_ok(&mut engine,"effect.pseudoElement=null;effect.pseudoElement===null"),Value::Bool(true)));
        assert!(matches!(eval_ok(&mut engine,"globalThis.authored=new Animation(effect);authored.effect===effect && authored.effect.target===target"),Value::Bool(true)));
        assert!(matches!(eval_ok(&mut engine,"globalThis.successor=new Animation(effect);authored.effect===null && successor.effect===effect && successor.effect.target===target"),Value::Bool(true)));
        assert!(matches!(eval_ok(&mut engine,"globalThis.empty=new Animation(null,null);empty.effect===null && empty.timeline===null"),Value::Bool(true)));
    }

    #[test]
    fn specification_animation_float_currentcolor_tracks_live_source_without_rgba_quantization() {
        let mut engine=Engine::new();
        crate::install(engine.ctx(),"<div id='target' style='color:color(display-p3 1.2 -0.1 0.3)'></div>",64).unwrap();
        eval_ok(&mut engine,"globalThis.target=document.getElementById('target');globalThis.effect=target.animate([{backgroundColor:'currentColor'},{backgroundColor:'currentColor'}],{duration:1000,fill:'both'});effect.pause();effect.currentTime=500;");
        for (declaration,expected) in [("",[1.2,-0.1,0.3]),("target.style.color='color(display-p3 0.12345 0.23456 0.34567)';effect.currentTime=501;",[0.12345,0.23456,0.34567])] {
            if !declaration.is_empty(){eval_ok(&mut engine,declaration);}
            let value=eval_ok(&mut engine,"getComputedStyle(target).backgroundColor");
            let text=match value {Value::Str(value)=>value.to_string(),_=>panic!("computed color string")};
            let source=lumen_html::css::parse_animation_unclipped_color(&text,lumen_common::color::Color::new(lumen_common::color::ColorSpace::Srgb,[0.0;3],1.0,0)).expect("computed float color");
            let actual=source.value.to_with_missing(lumen_common::color::ColorSpace::DisplayP3);
            for (actual,expected) in actual.components.into_iter().zip(expected) {assert!((actual-expected).abs()<0.0001,"float source {text}");}
        }
    }

    #[test]
    fn specification_animation_shadow_entries_keep_live_float_sources_and_declared_images() {
        let mut engine=Engine::new();
        crate::install(engine.ctx(),"<div id='target' style='color:color(display-p3 1.2 -0.1 0.3)'></div>",64).unwrap();
        eval_ok(&mut engine,r#"globalThis.target=document.getElementById('target');
            target.style.backgroundImage='linear-gradient(in lab to bottom right, red 1em, blue 100%)';
            if(target.style.backgroundImage!=='linear-gradient(to right bottom in lab, red 1em, blue 100%)')throw Error('declared gradient source');
            globalThis.effect=target.animate([{textShadow:'currentColor 1px 2px, color(srgb 1.2 -0.25 0.3) 3px 4px'},
                {textShadow:'currentColor 1px 2px, color(srgb 1.2 -0.25 0.3) 3px 4px'}],{duration:1000,fill:'both'});
            effect.pause();effect.currentTime=500;"#);
        for (declaration,expected) in [("",[1.2,-0.1,0.3]),("target.style.color='color(display-p3 0.12345 0.23456 0.34567)';effect.currentTime=501;",[0.12345,0.23456,0.34567])] {
            if !declaration.is_empty(){eval_ok(&mut engine,declaration);}
            let value=eval_ok(&mut engine,"getComputedStyle(target).textShadow");
            let text=match value{Value::Str(value)=>value.to_string(),_=>panic!("computed shadow string")};
            let viewport=lumen_html::css::MediaEnvironment::default();
            let list=lumen_html::css::parse_animation_source_shadows_with_query("text-shadow",&text,&lumen_html::css::Style::initial(),viewport,None,
                lumen_html::css::ContainerUnitContext::no_container(viewport)).expect("computed precise list");
            assert_eq!(list.shadows.len(),2);
            let actual=list.color(0).unwrap().value.to_with_missing(lumen_common::color::ColorSpace::DisplayP3);
            for (actual,expected) in actual.components.into_iter().zip(expected){assert!((actual-expected).abs()<0.0001,"live entry {text}");}
            let independent=list.color(1).unwrap().value.to_with_missing(lumen_common::color::ColorSpace::Srgb);
            for (actual,expected) in independent.components.into_iter().zip([1.2,-0.25,0.3]){assert!((actual-expected).abs()<0.0001,"independent entry {text}");}
            assert_eq!(list.shadows[0].offset_x,1.0);assert_eq!(list.shadows[1].offset_x,3.0);
        }
    }

    #[test]
    fn specification_svg_animation_float_currentcolor_uses_live_canonical_color_slots() {
        let mut engine=Engine::new();
        crate::install(engine.ctx(),"<svg><rect id='target' style='color:color(display-p3 1.2 -0.1 0.3)'/></svg>",64).unwrap();
        eval_ok(&mut engine,"globalThis.target=document.getElementById('target');globalThis.effect=target.animate([{fill:'currentColor',stroke:'currentColor'},{fill:'currentColor',stroke:'currentColor'}],{duration:1000,fill:'both'});effect.pause();effect.currentTime=500;");
        for (declaration,expected) in [("",[1.2,-0.1,0.3]),("target.style.color='color(display-p3 0.12345 0.23456 0.34567)';effect.currentTime=501;",[0.12345,0.23456,0.34567])] {
            if !declaration.is_empty(){eval_ok(&mut engine,declaration);}
            for property in ["fill","stroke"] {
                let value=eval_ok(&mut engine,&format!("getComputedStyle(target).getPropertyValue('{property}')"));
                let text=match value {Value::Str(value)=>value.to_string(),_=>panic!("computed SVG paint")};
                let source=lumen_html::css::parse_animation_unclipped_color(&text,lumen_common::color::Color::new(lumen_common::color::ColorSpace::Srgb,[0.0;3],1.0,0)).expect("computed float paint");
                let actual=source.value.to_with_missing(lumen_common::color::ColorSpace::DisplayP3);
                for (actual,expected) in actual.components.into_iter().zip(expected) {assert!((actual-expected).abs()<0.0001,"live SVG source {text}");}
            }
        }
    }

    #[test]
    fn specification_css_transition_reversal_uses_current_transform_and_original_timing() {
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<style>#target{width:100px;height:20px;transform:translateX(0px);transition:transform 1000ms linear}#target.end{transform:translateX(100px)}</style><div id='target'></div>",128).unwrap();
        progress_layout(&realm);advance(engine.ctx(),0.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,"globalThis.target=document.getElementById('target');target.className='end';globalThis.first=target.getAnimations()[0];first instanceof CSSTransition && first.transitionProperty==='transform'"),Value::Bool(true)));
        advance(engine.ctx(),0.0).unwrap();advance(engine.ctx(),400.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,"target.style.transitionDuration='2s';target.getAnimations()[0]===first && first.effect.getTiming().duration===1000"),Value::Bool(true)));
        eval_ok(&mut engine,"target.className='';globalThis.reverse=target.getAnimations()[0]");
        assert!(matches!(eval_ok(&mut engine,"reverse!==first && first.playState==='idle' && Math.abs(reverse.effect.getTiming().duration-800)<=800*Number.EPSILON*2"),Value::Bool(true)));
        let state=hub(engine.ctx()).unwrap();
        let state=state.borrow();let reverse=state.running_transitions.values().next().copied().unwrap();
        let transition=&state.transitions[&reverse];
        let style=lumen_html::css::Style::initial();
        let transforms=lumen_html::css::parse_animation_transforms(&transition.state.start_value,&style,realm.session.borrow().media_environment(),None).expect("canonical current transform");
        assert_eq!(transforms.iter().map(|transform|transform.matrix(100.0,20.0).e).sum::<f32>(),40.0);
        assert!((transition.state.reversing_shortening_factor-0.4).abs()<=0.4*f64::EPSILON*2.0);
    }

    #[test]
    fn specification_keyframe_effect_copy_overload_preserves_native_state_and_independence() {
        let mut engine=Engine::new();
        crate::install(engine.ctx(),"<style>#target::before{content:'';display:block;opacity:.4}</style><div id=target></div><div id=other></div>",128).unwrap();
        let value=eval_ok(&mut engine,r#"(() => {
            const check=(ok,message)=>{if(!ok)throw Error(message)};
            const target=document.getElementById('target'),other=document.getElementById('other');
            const source=new KeyframeEffect(target,{opacity:[0,1]},
                {duration:1000,delay:20,endDelay:-5,iterations:2.5,iterationStart:.25,fill:'both',direction:'alternate-reverse',easing:'ease-in',composite:'add',pseudoElement:'::before'});
            const specified=source.getTiming(),frames=source.getKeyframes();
            const animation=new Animation(source);
            Object.defineProperty(source,'target',{get(){throw Error('authored target getter')}});
            Object.defineProperty(source,'getTiming',{value(){throw Error('authored timing method')}});
            Object.defineProperty(source,'getKeyframes',{value(){throw Error('authored keyframe method')}});
            const copy=new KeyframeEffect(source);
            check(copy!==source && copy.target===target && copy.pseudoElement==='::before' && copy.composite==='add','independent native identity and effect target');
            check(JSON.stringify(copy.getTiming())===JSON.stringify(specified) && JSON.stringify(copy.getKeyframes())===JSON.stringify(frames),'specified timing and frames copied without authored getters');
            const copiedAnimation=new Animation(copy);
            check(animation.effect===source && copiedAnimation.effect===copy,'copy does not displace source animation');
            copy.updateTiming({duration:200});copy.setKeyframes({opacity:[.25,.75]});copy.target=other;
            const nativeTiming=KeyframeEffect.prototype.getTiming.call(source);
            const nativeFrames=KeyframeEffect.prototype.getKeyframes.call(source);
            check(nativeTiming.duration===1000 && nativeFrames[0].opacity==='0' && copy.getTiming().duration===200 && copy.target===other,'mutable copy remains independent of shared immutable source storage');
            const empty=new KeyframeEffect(new KeyframeEffect(null,null));
            check(empty.target===null && empty.getKeyframes().length===0,'null target and empty source');
            for(const args of [[],[target],[null],[{}],[source,null]]) {
                let error;try{Reflect.construct(KeyframeEffect,args)}catch(reason){error=reason}
                check(error instanceof TypeError,'constructor overload arity and brand checks');
            }
            check(KeyframeEffect.length===1,'minimum required overloaded constructor arity');
            return true;
        })()"#);
        assert!(matches!(value,Value::Bool(true)));
    }

    #[test]
    fn specification_text_shadow_transition_and_web_animation_typed_composition() {
        let mut engine=Engine::new();let realm=crate::install(engine.ctx(),"<style>#target{font-size:10px;text-shadow:black 1px 2px 3px;transition:text-shadow 1s linear}</style><div id=target></div>",96).unwrap();
        progress_layout(&realm);advance(engine.ctx(),0.0).unwrap();
        let target=lumen_html::selector::query_selector(realm.session.borrow().document(),realm.session.borrow().document().root(),"#target").unwrap().unwrap();
        eval_ok(&mut engine,"globalThis.target=document.getElementById('target');target.style.textShadow='black 3px 6px 9px';globalThis.transition=target.getAnimations()[0]");
        // Resolve this CSS transition's real pending play task at its creation
        // opportunity before advancing half of its active duration.
        advance(engine.ctx(),0.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,"!transition.pending && transition.startTime!==null && transition.currentTime===0"),Value::Bool(true)));
        advance(engine.ctx(),500.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,"transition instanceof CSSTransition && transition.transitionProperty==='text-shadow'"),Value::Bool(true)));
        let style=realm.session.borrow_mut().computed_style(target).unwrap();let shadows=style.text_shadows.as_ref().unwrap();
        assert_eq!((shadows[0].offset_x,shadows[0].offset_y,shadows[0].blur),(2.0,4.0,6.0));drop(style);
        eval_ok(&mut engine,"transition.cancel();target.style.transition='none';target.style.textShadow='black 1px 2px 3px';globalThis.added=target.animate({textShadow:['black 4px 8px 12px','black 6px 12px 18px']},{duration:1000,fill:'both',composite:'add'});added.currentTime=500");
        advance(engine.ctx(),500.0).unwrap();let style=realm.session.borrow_mut().computed_style(target).unwrap();let shadows=style.text_shadows.as_ref().unwrap();
        assert_eq!(shadows.len(),2);assert_eq!((shadows[0].offset_x,shadows[1].offset_x),(1.0,5.0));drop(style);
        eval_ok(&mut engine,"added.cancel();globalThis.accumulated=target.animate({textShadow:['black 4px 8px 12px','black 6px 12px 18px']},{duration:1000,fill:'both',composite:'accumulate'});accumulated.currentTime=500");
        advance(engine.ctx(),500.0).unwrap();let style=realm.session.borrow_mut().computed_style(target).unwrap();let shadows=style.text_shadows.as_ref().unwrap();
        assert_eq!(shadows.len(),1);assert_eq!((shadows[0].offset_x,shadows[0].offset_y,shadows[0].blur),(6.0,12.0,18.0));drop(style);
        eval_ok(&mut engine,"accumulated.cancel();target.style.textShadow='none';globalThis.relative=target.animate({fontSize:['10px','20px'],textShadow:['black 1em 0','black 2em 0']},{duration:1000,fill:'both'});relative.currentTime=500");
        advance(engine.ctx(),500.0).unwrap();let style=realm.session.borrow_mut().computed_style(target).unwrap();
        assert_eq!(style.font_size,15.0);assert_eq!(style.text_shadows.as_ref().unwrap()[0].offset_x,22.5,"shadow endpoints use this opportunity's actual animated font");
    }

    #[test]
    fn specification_text_shadow_sampling_uses_computed_dependencies_without_repeated_layout() {
        use lumen_html::paint::{TextShaper,ShapedRun};
        struct CountedFont{font:lumen_html_text::FontFace,shapes:Rc<Cell<usize>>}
        impl TextShaper for CountedFont {
            fn glyph_ink_bounds(&self,glyph:&lumen_html::paint::Glyph,size:f32)->Option<lumen_html::paint::Rect>{TextShaper::glyph_ink_bounds(&self.font,glyph,size)}
            fn shape(&self,text:&str,size:f32)->Result<ShapedRun,()> {self.shapes.set(self.shapes.get()+1);TextShaper::shape(&self.font,text,size)}
            fn ascent(&self,size:f32)->f32{self.font.ascent(size)}
            fn line_height(&self,size:f32)->f32{self.font.line_height(size)}
        }
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<style>.sample{font-size:10px;color:blue;width:100px;height:20px}#container{container-type:inline-size;width:200px}#query{width:100px;height:20px}</style><div class=sample>A</div><div class=sample>B</div><div class=sample>C</div><div class=sample>D</div><div id=container><div id=query>Q</div></div>",128).unwrap();
        let layouts=Rc::new(Cell::new(0));let shapes=Rc::new(Cell::new(0));
        let font=Rc::new(CountedFont{font:lumen_html_text::FontFace::new(std::sync::Arc::from(lumen_html_text::DEFAULT_FONT_BYTES)).unwrap(),shapes:shapes.clone()});
        let layout_count=layouts.clone();
        realm.set_layout_flusher(Rc::new(move|session|{layout_count.set(layout_count.get()+1);session.display_list(800,600,font.as_ref()).map(|_|()).map_err(|error|format!("{error:?}"))}));
        realm.flush_layout().unwrap();
        eval_ok(&mut engine,"globalThis.shadowSamples=Array.from(document.querySelectorAll('.sample'));globalThis.shadowEffects=shadowSamples.map(target=>target.animate({textShadow:['currentColor 1em 0','currentColor 2em 0']},{duration:1000,fill:'both'}));for(const effect of shadowEffects)effect.currentTime=500;globalThis.fontEffect=shadowSamples[0].animate({fontSize:['10px','20px'],color:['rgb(0, 0, 100)','rgb(0, 0, 200)']},{duration:1000,fill:'both'});fontEffect.currentTime=500");
        advance(engine.ctx(),0.0).unwrap();
        let layout_before=layouts.get();let shapes_before=shapes.get();
        assert!(matches!(eval_ok(&mut engine,"(()=>{for(let round=0;round<8;round++)for(const target of shadowSamples){const computed=getComputedStyle(target);if(!computed.textShadow.includes('px'))return false;}return true})()"),Value::Bool(true)));
        assert_eq!(layouts.get(),layout_before,"absolute/font/color shadow samples do not request layout reference boxes");
        assert_eq!(shapes.get(),shapes_before,"batched reads do not reshape the rendered document");
        let node=realm.with_session(|session|selector::query_selector(session.document(),session.document().root(),".sample").unwrap().unwrap());
        let style=realm.session.borrow_mut().computed_style(node).unwrap();let shadow=style.text_shadows.as_ref().unwrap()[0];
        assert_eq!(style.font_size,15.0);assert_eq!(shadow.offset_x,22.5);assert_eq!((shadow.color.r,shadow.color.g,shadow.color.b),(0,0,150));drop(style);
        eval_ok(&mut engine,"fontEffect.cancel();shadowSamples[0].style.fontSize='20px';shadowSamples[0].style.color='red';getComputedStyle(shadowSamples[0]).textShadow");
        let style=realm.session.borrow_mut().computed_style(node).unwrap();let shadow=style.text_shadows.as_ref().unwrap()[0];
        assert_eq!(shadow.offset_x,30.0);assert_eq!((shadow.color.r,shadow.color.g,shadow.color.b),(255,0,0));drop(style);
        assert_eq!(layouts.get(),layout_before,"author font/color changes use canonical computed context without layout");
        eval_ok(&mut engine,"globalThis.viewportShadow=shadowSamples[1].animate({textShadow:['black 1vw 0','black 2vw 0']},{duration:1000,fill:'both'});viewportShadow.currentTime=500");
        let viewport_node=realm.with_session(|session|selector::query_selector(session.document(),session.document().root(),".sample:nth-child(2)").unwrap().unwrap());
        let mut environment=realm.session.borrow().media_environment();environment.width=400.0;realm.session.borrow_mut().set_media_environment(environment).unwrap();
        eval_ok(&mut engine,"getComputedStyle(shadowSamples[1]).textShadow");
        assert_eq!(realm.session.borrow_mut().computed_style(viewport_node).unwrap().text_shadows.as_ref().unwrap()[0].offset_x,6.0,"viewport units sample the current environment");
        assert_eq!(layouts.get(),layout_before);
        eval_ok(&mut engine,"globalThis.queryTarget=document.getElementById('query');globalThis.queryEffect=queryTarget.animate({textShadow:['black 10cqw 0','black 20cqw 0']},{duration:1000,fill:'both'});queryEffect.currentTime=500;globalThis.containerEffect=document.getElementById('container').animate({width:['200px','400px']},{duration:1000,fill:'both'});containerEffect.currentTime=500;getComputedStyle(queryTarget).textShadow");
        let query_node=realm.with_session(|session|selector::query_selector(session.document(),session.document().root(),"#query").unwrap().unwrap());
        assert_eq!(realm.session.borrow_mut().computed_style(query_node).unwrap().text_shadows.as_ref().unwrap()[0].offset_x,45.0,"eligible container units consume this sample's actual used query box");
        assert!(layouts.get()>layout_before,"real query geometry remains synchronized");
        eval_ok(&mut engine,"queryEffect.cancel();containerEffect.cancel()");
        let before_absolute=layouts.get();
        eval_ok(&mut engine,"globalThis.absoluteEffect=shadowSamples[2].animate({transform:['translateX(0px) rotate(0deg)','translateX(40px) rotate(90deg)']},{duration:1000,fill:'both'});absoluteEffect.currentTime=500");
        assert_eq!(layouts.get(),before_absolute,"typed absolute transform functions have no reference-box dependency");
        eval_ok(&mut engine,"absoluteEffect.cancel()");
        let before_transform=layouts.get();
        eval_ok(&mut engine,"globalThis.referenceEffect=shadowSamples[2].animate({transform:['translateX(0%)','translateX(100%)']},{duration:1000,fill:'both'});referenceEffect.currentTime=500");
        assert!(layouts.get()>before_transform,"percentage transform samples retain actual reference-box layout");
    }

    #[test]
    fn specification_text_shadow_css_keyframe_composition_metadata_and_mixed_lists() {
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<style>@keyframes mixed{from{text-shadow:black 20px 0;animation-composition:add}to{text-shadow:black 30px 0;animation-composition:replace}}#target{width:100px;height:20px;text-shadow:black 10px 0;animation:mixed 1s linear both;animation-composition:accumulate}</style><div id=target></div>",96).unwrap();
        progress_layout(&realm);
        advance(engine.ctx(),0.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,"globalThis.target=document.getElementById('target');globalThis.animation=target.getAnimations()[0];globalThis.shadowKeyframes=animation.effect.getKeyframes();animation instanceof CSSAnimation && animation.effect.composite==='accumulate' && shadowKeyframes.length===2 && shadowKeyframes[0].composite==='add' && shadowKeyframes[1].composite==='replace' && shadowKeyframes.every(frame=>!('animation-composition' in frame))"),Value::Bool(true)),"keyframe metadata overrides the effect default without creating a property track");
        eval_ok(&mut engine,"animation.currentTime=500");
        advance(engine.ctx(),0.0).unwrap();
        let node=realm.with_session(|session|selector::query_selector(session.document(),session.document().root(),"#target").unwrap().unwrap());
        let style=realm.session.borrow_mut().computed_style(node).unwrap();
        let shadows=style.text_shadows.as_ref().unwrap();
        assert_eq!(shadows.len(),2,"add concatenates the real underlying list before interpolation with replace");
        assert_eq!((shadows[0].offset_x,shadows[1].offset_x),(20.0,10.0));
        assert_eq!((shadows[0].color.a,shadows[1].color.a),(255,128));
    }

    #[test]
    fn specification_registered_font_animation_owner_context_and_single_addition() {
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<style>@property --length {syntax:'<length>';inherits:false;initial-value:0px}@keyframes sampled {from{font-size:10px;width:10em;--length:10em}to{font-size:20px;width:20em;--length:20em}}#target{font-size:1px;animation:sampled 10s -5s linear paused}</style><div id=target></div>",128).unwrap();
        progress_layout(&realm);advance(engine.ctx(),0.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,"globalThis.target=document.getElementById('target');let style=getComputedStyle(target);style.fontSize==='15px' && style.width==='225px' && style.getPropertyValue('--length')===style.width"),Value::Bool(true)),"registered and ordinary endpoints use the same sampled owner font");
        eval_ok(&mut engine,"target.getAnimations()[0].cancel();target.style.setProperty('--length','5px');globalThis.font=target.animate({fontSize:['10px','20px']},{duration:1000,fill:'both'});font.currentTime=500;globalThis.length=target.animate({'--length':['10em','20em']},{duration:1000,fill:'both',composite:'add'});length.currentTime=500");
        advance(engine.ctx(),0.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,"getComputedStyle(target).fontSize==='15px' && getComputedStyle(target).getPropertyValue('--length')==='230px'"),Value::Bool(true)),"second custom phase cannot compose its earlier additive output again");
    }

    #[test]
    fn specification_registered_deferred_transform_consumers_keep_independent_boxes() {
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<style>@property --motion{syntax:'<transform-list>';inherits:true;initial-value:translateX(0px)}#owner{width:300px}#a,#b{height:100px;transform:var(--motion)}#a{width:200px}#b{width:400px}</style><div id=owner><div id=a></div><div id=b></div></div>",128).unwrap();
        progress_layout(&realm);advance(engine.ctx(),0.0).unwrap();
        eval_ok(&mut engine,"globalThis.owner=document.getElementById('owner');globalThis.a=document.getElementById('a');globalThis.b=document.getElementById('b');globalThis.motion=owner.animate({'--motion':['translateX(50%)','scale(4)']},{duration:1000,fill:'both'});motion.currentTime=500");
        advance(engine.ctx(),0.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,"globalThis.usedTransformMatches=(node,x)=>{const m=new DOMMatrix(getComputedStyle(node).transform);return Math.abs(m.a-2.5)<1e-12&&Math.abs(m.b)<1e-12&&Math.abs(m.c)<1e-12&&Math.abs(m.d-2.5)<1e-12&&m.e===x&&m.f===0};getComputedStyle(owner).getPropertyValue('--motion')==='transform-interpolate(0.5, 0: translateX(50%), 1: scale(4))' && usedTransformMatches(a,50) && usedTransformMatches(b,100)"),Value::Bool(true)),"registered computed percentages resolve independently in their var consumers");
        eval_ok(&mut engine,"b.style.width='300px'");advance(engine.ctx(),0.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,"usedTransformMatches(b,75)"),Value::Bool(true)),"consumer box changes recompute used geometry without resampling source percentages");
        eval_ok(&mut engine,"a.style.transform='none';motion.cancel()");advance(engine.ctx(),0.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,"getComputedStyle(a).transform==='none' && getComputedStyle(b).transform==='matrix(1, 0, 0, 1, 0, 0)'"),Value::Bool(true)),"declaration replacement and animation removal clear the sparse transform source");
    }

    #[test]
    fn specification_registered_transform_sampling_function_identity_and_list_operations() {
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<div id=target style='width:200px;height:100px'></div>",128).unwrap();
        progress_layout(&realm);advance(engine.ctx(),0.0).unwrap();
        eval_ok(&mut engine,"globalThis.target=document.getElementById('target');CSS.registerProperty({name:'--function',syntax:'<transform-function>',inherits:false,initialValue:'translateX(100px)'});globalThis.fn=target.animate({'--function':['translateX(200px)','translateX(300px)']},{duration:1000,fill:'both',composite:'add'});fn.currentTime=500;CSS.registerProperty({name:'--list',syntax:'<transform-list>',inherits:false,initialValue:'translateX(100px)'});globalThis.list=target.animate({'--list':['translateX(200px)','translateX(300px)']},{duration:1000,fill:'both',composite:'add'});list.currentTime=500");
        advance(engine.ctx(),0.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,"getComputedStyle(target).getPropertyValue('--function')==='translateX(350px)' && getComputedStyle(target).getPropertyValue('--list')==='translateX(100px) translateX(250px)'"),Value::Bool(true)),"single computed function addition and transform-list concatenation differ");
        eval_ok(&mut engine,"CSS.registerProperty({name:'--padded',syntax:'<transform-function>+',inherits:false,initialValue:'translateX(0px)'});globalThis.padded=target.animate({'--padded':['translateX(10px)','translateY(20px) rotate(90deg)']},{duration:1000,fill:'both'});padded.currentTime=500;CSS.registerProperty({name:'--scale',syntax:'<transform-list>',inherits:false,initialValue:'scale(1)'});globalThis.scale=target.animate({'--scale':['scale(1)','scale(2)']},{duration:1000,iterations:3,fill:'both',iterationComposite:'accumulate'});scale.currentTime=2500;CSS.registerProperty({name:'--rotation',syntax:'<transform-function>',inherits:false,initialValue:'rotateX(0deg)'});globalThis.rotation=target.animate({'--rotation':['rotateX(10deg)','rotateX(20deg)']},{duration:1000,fill:'both'});rotation.currentTime=500");
        advance(engine.ctx(),0.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,"getComputedStyle(target).getPropertyValue('--padded')==='translate(5px, 10px) rotate(45deg)' && getComputedStyle(target).getPropertyValue('--scale')==='scale(3.5)' && getComputedStyle(target).getPropertyValue('--rotation')==='rotateX(15deg)'"),Value::Bool(true)),"padding, iteration accumulation and 3D function identity use the same registered path");
    }

    #[test]
    fn specification_registered_additive_base_after_registration_epoch_change() {
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<div id=target></div>",128).unwrap();
        progress_layout(&realm);advance(engine.ctx(),0.0).unwrap();
        eval_ok(&mut engine,"globalThis.target=document.getElementById('target');CSS.registerProperty({name:'--first',syntax:'<number>',inherits:false,initialValue:'0'});globalThis.first=target.animate({'--first':['100','200']},{duration:1000,fill:'both'});first.currentTime=500");
        advance(engine.ctx(),0.0).unwrap();
        eval_ok(&mut engine,"CSS.registerProperty({name:'--next',syntax:'<number>',inherits:false,initialValue:'100'});globalThis.next=target.animate({'--next':['200','300']},{duration:1000,fill:'both',composite:'add'});next.currentTime=500");
        advance(engine.ctx(),0.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,"getComputedStyle(target).getPropertyValue('--first')==='150' && getComputedStyle(target).getPropertyValue('--next')==='350'"),Value::Bool(true)),"new registration composes its canonical initial base while retaining the earlier effect");
        eval_ok(&mut engine,"CSS.registerProperty({name:'--list',syntax:'<number>#',inherits:false,initialValue:'10, 20'});globalThis.list=target.animate({'--list':['100, 200','200, 300']},{duration:1000,fill:'both',composite:'add'});list.currentTime=500");
        advance(engine.ctx(),0.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,"getComputedStyle(target).getPropertyValue('--list')==='160, 270'"),Value::Bool(true)),"list initial base follows the same source epoch correction");
        eval_ok(&mut engine,"CSS.registerProperty({name:'--neutral',syntax:'<number>',inherits:false,initialValue:'100'});globalThis.neutral=target.animate({'--neutral':'300'},{duration:1000,fill:'both',composite:'add'});neutral.currentTime=500;CSS.registerProperty({name:'--iteration',syntax:'<number>',inherits:false,initialValue:'100'});globalThis.iteration=target.animate({'--iteration':['0','100']},{duration:1000,iterations:3,fill:'both',iterationComposite:'accumulate'});iteration.currentTime=2500");
        advance(engine.ctx(),0.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,"getComputedStyle(target).getPropertyValue('--neutral')==='250' && getComputedStyle(target).getPropertyValue('--iteration')==='250'"),Value::Bool(true)),"neutral and iteration endpoints retain the current registration grammar");
    }

    #[test]
    fn specification_registered_transition_admission_explicit_all_and_list_sampling() {
        for (property,syntax,from,to,expected) in [
            ("--value","<length>","100px","200px","150px"),
            ("all","<length>","100px","200px","150px"),
            ("all","<number>#","100, 150","200, 250","150, 200"),
        ] {
            let mut engine=Engine::new();
            let markup=format!("<style>@property --value {{syntax:'{syntax}';inherits:false;initial-value:{from}}}#target{{transition:{property} 100s linear}}#target.end{{--value:{to}}}</style><div id=target></div>");
            let realm=crate::install(engine.ctx(),&markup,128).unwrap();
            progress_layout(&realm);advance(engine.ctx(),0.0).unwrap();
            assert!(matches!(eval_ok(&mut engine,"globalThis.target=document.getElementById('target');target.className='end';globalThis.transitions=target.getAnimations();transitions.length===1 && transitions[0] instanceof CSSTransition && transitions[0].transitionProperty==='--value'"),Value::Bool(true)),"registered {syntax} transition via {property}");
            eval_ok(&mut engine,"transitions[0].currentTime=50000");advance(engine.ctx(),0.0).unwrap();
            assert!(matches!(eval_ok(&mut engine,&format!("getComputedStyle(target).getPropertyValue('--value')==='{expected}'")),Value::Bool(true)),"registered transition sampled value");
        }
    }

    #[test]
    fn specification_css_transition_zero_duration_delay_events_and_completed_reconciliation() {
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<style>#target{opacity:0;transition:opacity 0s 100ms linear}#target.end{opacity:1}</style><div id='target'></div>",128).unwrap();
        progress_layout(&realm);advance(engine.ctx(),0.0).unwrap();
        eval_ok(&mut engine,"globalThis.target=document.getElementById('target');globalThis.events=[];for(const name of ['transitionrun','transitionstart','transitionend','transitioncancel'])target.addEventListener(name,e=>events.push([e.type,e.elapsedTime,e.propertyName,e.target===target,e instanceof TransitionEvent]));target.className='end';globalThis.first=target.getAnimations()[0]");
        advance(engine.ctx(),0.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,"events.length===1 && events[0][0]==='transitionrun' && events[0][1]===0 && events[0][2]==='opacity' && events[0][3] && events[0][4] && getComputedStyle(target).opacity==='0'"),Value::Bool(true)));
        // A synchronous style-change event after the active interval must use
        // completed state even if end-event delivery has not run yet.
        realm.timeline_sample.set(100.0);
        eval_ok(&mut engine,"target.style.opacity='1';getComputedStyle(target).opacity");
        assert!(hub(engine.ctx()).unwrap().borrow().running_transitions.is_empty());
        advance(engine.ctx(),100.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,"events.length===3 && events[1][0]==='transitionstart' && events[2][0]==='transitionend' && events[1][1]===0 && events[2][1]===0 && first.playState==='finished' && target.getAnimations().length===0"),Value::Bool(true)));
    }

    #[test]
    fn specification_css_transition_pseudo_geometry_starting_style_and_event_identity() {
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<style>#target{width:200px;height:100px}#target::before{content:'';display:block;width:80px;height:30px;opacity:1;transition:opacity 1000ms linear;@starting-style{opacity:0}}#target.end::before{opacity:0.5;transition-property:none}</style><div id='target'></div>",128).unwrap();
        progress_layout(&realm);advance(engine.ctx(),0.0).unwrap();realm.flush_layout().unwrap();
        let node=realm.with_session(|session|selector::query_selector(session.document(),session.document().root(),"#target").unwrap().unwrap());
        let rect=realm.session.borrow().pseudo_layout_rect(node,lumen_html::css::PseudoElement::Before).expect("actual generated box");
        assert_eq!((rect.width,rect.height),(80.0,30.0));
        assert!(matches!(eval_ok(&mut engine,"globalThis.target=document.getElementById('target');globalThis.first=target.getAnimations()[0];first instanceof CSSTransition && first.effect.pseudoElement==='::before' && first.effect.target===target"),Value::Bool(true)));
        eval_ok(&mut engine,"globalThis.events=[];target.ontransitioncancel=e=>events.push([e.pseudoElement,e.propertyName,e.target===target]);target.style.transition='none';target.className='end';getComputedStyle(target,'::before').opacity");
        advance(engine.ctx(),16.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,"events.length===1 && events[0][0]==='::before' && events[0][1]==='opacity' && events[0][2] && first.playState==='idle'"),Value::Bool(true)));
    }

#[test]
fn specification_css_transition_all_and_logical_lists_create_one_physical_effect() {
    let mut engine=Engine::new();
    let realm=crate::install(engine.ctx(),"<style>#target::after{content:'';display:block;width:0px;height:0px;transition:all 10s linear}#target.end::after{width:100px;height:100px}</style><div id='target'></div>",128).unwrap();
    progress_layout(&realm);advance(engine.ctx(),0.0).unwrap();
    assert!(matches!(eval_ok(&mut engine,"globalThis.target=document.getElementById('target');getComputedStyle(target,'::after').width;target.className='end';globalThis.effects=target.getAnimations();effects.length===2 && effects.map(a=>a.transitionProperty).sort().join(',')==='height,width' && effects.every(a=>a.effect.pseudoElement==='::after')"),Value::Bool(true)));
    assert!(matches!(eval_ok(&mut engine,"globalThis.authored=target.animate({opacity:[0,1]},{duration:3000,pseudoElement:'::after'});target.getAnimations().length===3 && target.getAnimations().filter(a=>a instanceof CSSTransition).length===2"),Value::Bool(true)));

    let mut engine=Engine::new();
    let realm=crate::install(engine.ctx(),"<style>#target{width:0px;height:0px;transition:all 10s linear,inline-size 2s linear}#target.end{width:100px;height:100px}</style><div id='target'></div>",128).unwrap();
    progress_layout(&realm);advance(engine.ctx(),0.0).unwrap();
    assert!(matches!(eval_ok(&mut engine,"globalThis.target=document.getElementById('target');getComputedStyle(target).width;target.className='end';globalThis.effects=target.getAnimations();effects.length===2 && effects.find(a=>a.transitionProperty==='width').effect.getTiming().duration===2000 && effects.find(a=>a.transitionProperty==='height').effect.getTiming().duration===10000"),Value::Bool(true)));

    let mut engine=Engine::new();
    let realm=crate::install(engine.ctx(),"<style>#target{writing-mode:vertical-rl;direction:rtl;width:0px;height:0px;transition:inline-size 2s linear,width 3s linear}#target.end{width:100px;height:100px}</style><div id='target'></div>",128).unwrap();
    progress_layout(&realm);advance(engine.ctx(),0.0).unwrap();
    assert!(matches!(eval_ok(&mut engine,"globalThis.target=document.getElementById('target');getComputedStyle(target).height;target.className='end';globalThis.effects=target.getAnimations();effects.length===2 && effects.find(a=>a.transitionProperty==='height').effect.getTiming().duration===2000 && effects.find(a=>a.transitionProperty==='width').effect.getTiming().duration===3000"),Value::Bool(true)));
}

    #[test]
    fn specification_css_transition_modified_effects_retarget_from_complete_before_stack() {
        for (frames,expected) in [("{left:['200px','300px','100px']}","300px"),
            ("{top:['200px','300px','100px']}","100px"),("{}","100px")] {
            let mut engine=Engine::new();
            let realm=crate::install(engine.ctx(),"<style>#target{left:0;transition:left 100s linear}#target.end{left:100px}</style><div id='target'></div>",128).unwrap();
            progress_layout(&realm);advance(engine.ctx(),0.0).unwrap();
            eval_ok(&mut engine,"globalThis.target=document.getElementById('target');target.className='end';globalThis.original=target.getAnimations()[0]");
            advance(engine.ctx(),16.0).unwrap();
            assert!(matches!(eval_ok(&mut engine,&format!("original.currentTime=50000;original.effect.setKeyframes({frames});target.style.left='0px';globalThis.reversed=target.getAnimations()[0];reversed!==original && reversed.transitionProperty==='left' && reversed.effect.getComputedTiming().activeDuration===50000 && reversed.effect.getKeyframes()[0].left==='{expected}'")),Value::Bool(true)),"modified effect {frames}");
        }
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<style>#target{left:100px;top:100px;transition:left 100s linear,top 100s linear}#target.end{left:200px;top:200px}</style><div id='target'></div>",128).unwrap();
        progress_layout(&realm);advance(engine.ctx(),0.0).unwrap();
        eval_ok(&mut engine,"globalThis.target=document.getElementById('target');target.className='end';globalThis.effects=target.getAnimations()");
        advance(engine.ctx(),16.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,"if(effects.length!==2 || effects[0].transitionProperty!=='left' || effects[1].transitionProperty!=='top')throw Error('actual transition ordering');effects[0].effect.setKeyframes({});effects[1].effect.setKeyframes({left:['100px','400px'],top:['100px','200px']});getComputedStyle(target).left;effects[0].currentTime=50000;target.style.left='100px';effects[1].currentTime=60000;globalThis.reversed=target.getAnimations().find(effect=>effect.transitionProperty==='left');reversed!==effects[0] && reversed.effect.getComputedTiming().activeDuration===50000 && reversed.effect.getKeyframes()[0].left==='280px'"),Value::Bool(true)));
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<style>#target{left:100px;transition:left 100s linear}#target.end{left:200px}</style><div id='target'></div>",128).unwrap();
        progress_layout(&realm);advance(engine.ctx(),0.0).unwrap();
        eval_ok(&mut engine,"globalThis.target=document.getElementById('target');target.className='end';globalThis.original=target.getAnimations()[0]");
        advance(engine.ctx(),16.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,"original.effect.setKeyframes([{offset:0,left:'-50px',composite:'add',easing:'linear'}]);original.currentTime=50000;if(getComputedStyle(target).left!=='175px')throw Error('actual additive effect '+getComputedStyle(target).left);if(original.effect.getKeyframes().length!==1)throw Error('implicit neutral boundary exposed');target.style.left='100px';target.getAnimations()[0].effect.getKeyframes()[0].left==='175px'"),Value::Bool(true)));
    }

    #[test]
    fn specification_css_transition_phase_batches_event_identity_and_timeline_removal() {
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<style>#target{margin-left:0;transition:margin-left 100s 100s linear}#target.end{margin-left:100px}</style><div id='target'></div>",128).unwrap();
        progress_layout(&realm);advance(engine.ctx(),0.0).unwrap();
        eval_ok(&mut engine,"globalThis.target=document.getElementById('target');globalThis.events=[];for(const type of ['transitionrun','transitionstart','transitionend','transitioncancel'])target.addEventListener(type,e=>events.push(e));target.className='end';globalThis.transition=target.getAnimations()[0];transition.finish()");
        advance(engine.ctx(),0.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,"events.map(e=>e.type).join(',')==='transitionrun,transitionstart,transitionend' && events.every(e=>e.animation===transition && e.target===target && e.propertyName==='margin-left') && events[0].elapsedTime===0 && events[1].elapsedTime===0 && events[2].elapsedTime===100"),Value::Bool(true)));
        eval_ok(&mut engine,"globalThis.authoredEvent=new TransitionEvent('authored',{animation:transition});globalThis.nullEvent=new TransitionEvent('null',{animation:undefined});globalThis.badAnimation=false;try{new TransitionEvent('bad',{animation:target.animate({opacity:[0,1]},10)})}catch(e){badAnimation=e instanceof TypeError};transition=null");
        engine.ctx().collect_garbage();
        assert!(matches!(eval_ok(&mut engine,"authoredEvent.animation===events[0].animation && authoredEvent.animation.transitionProperty==='margin-left' && nullEvent.animation===null && badAnimation"),Value::Bool(true)));

        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<style>#target{margin-left:0;transition:margin-left 100s 100s linear}#target.end{margin-left:100px}</style><div id='target'></div>",128).unwrap();
        progress_layout(&realm);advance(engine.ctx(),0.0).unwrap();
        eval_ok(&mut engine,"globalThis.target=document.getElementById('target');globalThis.events=[];for(const type of ['transitionrun','transitionstart','transitionend','transitioncancel'])target.addEventListener(type,e=>events.push([e.type,e.elapsedTime]));target.className='end';globalThis.transition=target.getAnimations()[0]");
        advance(engine.ctx(),16.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,"if(events.length!==1 || events[0][0]!=='transitionrun')throw Error('actual initial run batch');globalThis.savedStart=transition.startTime;transition.timeline=null;transition.currentTime===null && transition.startTime===savedStart"),Value::Bool(true)));
        advance(engine.ctx(),32.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,"if(events.length!==2 || events[1][0]!=='transitioncancel' || events[1][1]!==0)throw Error('actual null timeline cancellation');transition.timeline=document.timeline;transition.pause();transition.currentTime=250;transition.timeline=null;transition.currentTime===250"),Value::Bool(true)));
    }

    #[test]
    fn specification_css_transition_effect_removal_and_keyframes_retain_public_contracts() {
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<style>#target{left:0;transition:left 100s linear}#target.end{left:100px}</style><div id='target'></div>",128).unwrap();
        progress_layout(&realm);advance(engine.ctx(),0.0).unwrap();
        eval_ok(&mut engine,"globalThis.target=document.getElementById('target');target.className='end';globalThis.transition=target.getAnimations()[0]");
        advance(engine.ctx(),16.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,"globalThis.frames=transition.effect.getKeyframes();frames.length===2 && frames[0].offset===0 && frames[0].computedOffset===0 && frames[1].computedOffset===1 && frames.every(frame=>frame.easing==='linear' && frame.composite==='auto') && frames[0].left==='0px' && frames[1].left==='100px'"),Value::Bool(true)));
        assert!(matches!(eval_ok(&mut engine,"globalThis.originalEffect=transition.effect;transition.effect=null;transition.playState==='finished' && transition.transitionProperty==='left' && originalEffect.getTiming().duration===100000 && getComputedStyle(target).left==='100px'"),Value::Bool(true)));
        assert!(matches!(eval_ok(&mut engine,"target.style.left='150px';globalThis.replacement=target.getAnimations()[0];replacement!==transition && replacement.transitionProperty==='left' && replacement.effect.getKeyframes()[0].left==='100px'"),Value::Bool(true)));
        assert!(matches!(eval_ok(&mut engine,"globalThis.effect=new KeyframeEffect(target,{'--Case':['first','second']},100);effect.getKeyframes().every(frame=>('--Case' in frame) && !('Case' in frame) && frame.easing==='linear')"),Value::Bool(true)));
    }

    #[test]
    fn specification_css_transition_internal_removal_cancels_before_reinsertion_and_releases_history() {
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<style>#target{left:0;transition:left 10s -5s linear}</style><div id='parent1'><div id='subtree'><div id='target'></div></div></div><div id='parent2'></div>",128).unwrap();
        progress_layout(&realm);advance(engine.ctx(),0.0).unwrap();
        eval_ok(&mut engine,"globalThis.target=document.getElementById('target');globalThis.subtree=document.getElementById('subtree');globalThis.parent1=document.getElementById('parent1');globalThis.parent2=document.getElementById('parent2');globalThis.events=[];target.addEventListener('transitioncancel',event=>events.push(event));target.style.left='100px';globalThis.original=target.getAnimations()[0]");
        advance(engine.ctx(),16.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,"if(getComputedStyle(target).left==='100px')throw Error('actual running effect');globalThis.savedElapsed=original.currentTime/1000+5;parent2.appendChild(subtree);original.playState==='idle' && target.getAnimations().length===0 && getComputedStyle(target).left==='100px'"),Value::Bool(true)));
        advance(engine.ctx(),32.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,"events.length===1 && events[0].animation===original && events[0].target===target && events[0].propertyName==='left' && events[0].elapsedTime===savedElapsed"),Value::Bool(true)));
        // A new style change can transition after reconnection has established
        // its own before-change style. An in-turn removal invalidates it again.
        eval_ok(&mut engine,"target.style.left='200px';globalThis.next=target.getAnimations()[0]");
        advance(engine.ctx(),48.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,"if(!(next instanceof CSSTransition) || next===original)throw Error('new connected transition');globalThis.nextElapsed=next.currentTime/1000+5;parent2.removeChild(subtree);parent2.appendChild(subtree);next.playState==='idle' && target.getAnimations().length===0 && getComputedStyle(target).left==='200px'"),Value::Bool(true)));
        advance(engine.ctx(),64.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,"events.length===2 && events[1].animation===next && events[1].elapsedTime===nextElapsed"),Value::Bool(true)));
        eval_ok(&mut engine,"target.style.left='300px';globalThis.completed=target.getAnimations()[0];completed.finish()");
        advance(engine.ctx(),80.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,"parent2.removeChild(subtree);target.style.left='0px';parent2.appendChild(subtree);target.getAnimations().length===0 && getComputedStyle(target).left==='0px'"),Value::Bool(true)));
    }

    #[test]
    fn specification_css_transition_display_exit_keeps_descendant_effects_until_completion() {
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<style>#target{display:block;transition:display 100ms linear allow-discrete}#target.end{display:none}#child{opacity:0;transition:opacity 1000ms linear}#target.end #child{opacity:1}</style><div id='target'><div id='child'></div></div>",128).unwrap();
        progress_layout(&realm);advance(engine.ctx(),0.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,"globalThis.target=document.getElementById('target');globalThis.child=document.getElementById('child');target.className='end';target.getAnimations({subtree:true}).length===2"),Value::Bool(true)));
        advance(engine.ctx(),0.0).unwrap();advance(engine.ctx(),50.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,"getComputedStyle(target).display==='block' && child.getAnimations().length===1"),Value::Bool(true)));
        advance(engine.ctx(),100.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,"getComputedStyle(target).display==='none' && child.getAnimations().length===0"),Value::Bool(true)));
    }

    #[test]
    fn specification_css_transition_important_aliases_custom_discrete_and_compound_retarget() {
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<style>#target{width:100px;height:20px;margin-inline-start:7px!important;--token:first;text-underline-offset:calc(10px + 20%);transition:margin-left 1s,--token 1s allow-discrete,text-underline-offset 1s linear}#target.end{margin-left:40px;--token:last;text-underline-offset:calc(30px + 40%)}</style><div id='target'></div>",128).unwrap();
        progress_layout(&realm);advance(engine.ctx(),0.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,"globalThis.target=document.getElementById('target');target.className='end';globalThis.transitions=target.getAnimations();transitions.length===2 && transitions.every(a=>a.transitionProperty!=='margin-left') && getComputedStyle(target).getPropertyValue('margin-left')==='7px'"),Value::Bool(true)));
        advance(engine.ctx(),0.0).unwrap();advance(engine.ctx(),400.0).unwrap();
        // The declared default ease has crossed its discrete midpoint by
        // 400ms; universal custom values follow the eased, not raw, progress.
        assert!(animation::ease("ease",0.4).unwrap()>0.5);
        assert!(matches!(eval_ok(&mut engine,"getComputedStyle(target).getPropertyValue('--token').trim()==='last'"),Value::Bool(true)));
        let state=hub(engine.ctx()).unwrap();
        let node=realm.with_session(|session|selector::query_selector(session.document(),session.document().root(),"#target").unwrap().unwrap());
        let old=state.borrow().running_transitions[&(Rc::as_ptr(&realm) as usize,node,None,"text-underline-offset".into())];
        eval_ok(&mut engine,"target.style.textUnderlineOffset='calc(50px + 60%)';getComputedStyle(target).getPropertyValue('text-underline-offset')");
        let state=state.borrow();let next=state.running_transitions[&(Rc::as_ptr(&realm) as usize,node,None,"text-underline-offset".into())];
        assert_ne!(old,next);
        assert!(state.transitions[&next].endpoint_styles.is_some(),"typed affine endpoints retained only for compound interpolation");
        let start=&state.transitions[&next].state.start_value;
        assert!(start.contains("18px") && start.contains("28%"),"retarget starts at the sampled affine value: {start}");
    }

    #[test]
    fn specification_css_transition_rebound_global_preserves_retained_animation_identity() {
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<style>#target{opacity:0;transition:opacity 1s linear}#target.end{opacity:1}</style><div id='target'></div>",128).unwrap();
        progress_layout(&realm);advance(engine.ctx(),0.0).unwrap();
        eval_ok(&mut engine,"globalThis.target=document.getElementById('target');globalThis.cancelled=[];target.ontransitioncancel=e=>cancelled.push(e.type);target.className='end';globalThis.retained=target.getAnimations()[0];globalThis.retainedEffect=retained.effect");
        advance(engine.ctx(),0.0).unwrap();
        let old_hub=hub(engine.ctx()).unwrap();
        retire_document(engine.ctx(),&realm).unwrap();
        let successor=crate::install(engine.ctx(),"<div id='successor'></div>",64).unwrap();
        progress_layout(&successor);advance(engine.ctx(),100.0).unwrap();
        assert!(Rc::ptr_eq(&old_hub,&hub(engine.ctx()).unwrap()),"one animation service per reused global");
        assert!(matches!(eval_ok(&mut engine,"retained instanceof CSSTransition && retained.transitionProperty==='opacity' && retained.effect===retainedEffect && retainedEffect.target===target && retained.playState==='idle' && document.getAnimations().length===0 && cancelled.length===0"),Value::Bool(true)));
        let weak=Rc::downgrade(&realm);
        eval_ok(&mut engine,"retainedEffect=null;target=null");
        drop(realm);engine.ctx().collect_garbage();
        assert!(weak.upgrade().is_some(),"retaining only Animation owns its actual effect and target document");
        assert!(matches!(eval_ok(&mut engine,"retained.effect.target.id==='target' && retained.effect.target.ownerDocument!==document"),Value::Bool(true)));
        eval_ok(&mut engine,"retained.effect=null;retained=null");
        engine.ctx().collect_garbage();
        assert!(weak.upgrade().is_none(),"weak hub metadata cannot retain the obsolete target document after actual effect release");

    }

    fn progress_translation(realm: &Rc<DomRealm>) -> f32 {
        realm.with_session(|session|{
            let node=selector::query_selector(session.document(),session.document().root(),"#target").unwrap().unwrap();
            let style=session.computed_style(node).unwrap();
            style.transforms.as_deref().unwrap_or(&[]).iter().map(|transform|transform.matrix(100.0,100.0).e).sum()
        })
    }

    #[test]
    fn named_scroll_timeline_attachment_preserves_reversed_animation_identity() {
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<style>@keyframes anim{from{transform:translateX(100px)}to{transform:translateX(0px)}}#scroller{border:1px solid black;overflow:hidden;width:300px;height:200px;scroll-timeline:--timeline}#target{margin-top:800px;margin-bottom:800px;width:100px;height:100px;animation:anim 10s linear paused}#target.update{animation-play-state:running;animation-timeline:--timeline;animation-duration:auto}</style><div id='scroller'><div id='target'></div></div>",128).unwrap();
        progress_layout(&realm);
        eval_ok(&mut engine,"globalThis.scroller=document.getElementById('scroller');globalThis.target=document.getElementById('target');globalThis.original=target.getAnimations()[0]");
        advance(engine.ctx(),0.0).unwrap();
        eval_ok(&mut engine,"original.playbackRate=-1;scroller.scrollTop=825;target.classList.add('update')");
        advance(engine.ctx(),16.0).unwrap();
        advance(engine.ctx(),32.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,"original===target.getAnimations()[0] && !original.pending && original.playbackRate===-1"),Value::Bool(true)));
        assert!((progress_translation(&realm)-55.0).abs()<0.01);
        eval_ok(&mut engine,"scroller.scrollTop=300");
        advance(engine.ctx(),48.0).unwrap();
        assert!((progress_translation(&realm)-20.0).abs()<0.01);
    }

    #[test]
    fn named_view_timeline_exit_range_updates_committed_subject_bounds() {
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<style>@keyframes anim{from{transform:translateX(100px)}to{transform:translateX(0px)}}#scroller{border:1px solid black;overflow:hidden;width:300px;height:200px}#target{margin-top:700px;margin-bottom:800px;width:100px;height:200px;animation:anim auto both linear;animation-timeline:--timeline;view-timeline:--timeline;animation-range:exit}#target.update{height:100px;margin-top:800px}</style><div id='scroller'><div id='target'></div></div>",128).unwrap();
        progress_layout(&realm);
        eval_ok(&mut engine,"globalThis.scroller=document.getElementById('scroller');globalThis.target=document.getElementById('target');globalThis.original=target.getAnimations()[0];scroller.scrollTop=800");
        advance(engine.ctx(),0.0).unwrap();
        advance(engine.ctx(),16.0).unwrap();
        let translation=progress_translation(&realm);
        assert!((translation-50.0).abs()<0.01,"translation={translation}, progress={:?}",hub(engine.ctx()).unwrap().borrow().records.values().map(|record|(record_time(record),record.timing,record.hold_time_ms,record.pending_task)).collect::<Vec<_>>());
        eval_ok(&mut engine,"target.classList.add('update')");
        advance(engine.ctx(),32.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,"original===target.getAnimations()[0] && !original.pending"),Value::Bool(true)));
        assert!((progress_translation(&realm)-100.0).abs()<0.01);
    }

    #[test]
    fn commented_css_animation_inventory_uses_real_pending_ready_lifecycle() {
        let mut engine = Engine::new();
        let realm = crate::install(engine.ctx(),
            "<style>@keyframes bgcolor {from {opacity:0} to {opacity:1}}\
            .target {animation:bgcolor/* name boundary */10000000s -8717082s /* trailing comment */;}\
            @keyframes fade {from {opacity:0} to {opacity:1}}\
            .longhand {animation-name:fade/* trailing */;animation-duration:100ms;animation-fill-mode:both}</style>\
            <div class='target'></div><div class='longhand'></div>", 64).unwrap();
        assert!(matches!(eval_ok(&mut engine,
            "var animations=document.getAnimations();var animation=animations.find(a=>a.animationName==='bgcolor');\
            var readyCount=0;animation.ready.then(a=>{if(a===animation)readyCount++});\
            animations.length===2 && animation.pending && animation.currentTime===0 && readyCount===0"), Value::Bool(true)));
        let now = realm.timeline_sample.get();
        advance(engine.ctx(), now).unwrap();
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(eval_ok(&mut engine,
            "readyCount===1 && !animation.pending && animation.playState==='running'"), Value::Bool(true)));
        eval_ok(&mut engine, "animation.pause();animation.currentTime=500;document.querySelector('.target').style.animation='none'");
        assert!(matches!(eval_ok(&mut engine,
            "document.getAnimations().length===1 && animation.playState==='idle'"), Value::Bool(true)));
    }

    #[test]
    fn paint_snapshot_retains_immutable_payload_across_reentrant_layout_flush() {
        let mut engine = Engine::new();
        let realm = crate::install(engine.ctx(),"<body style='margin:0'><div id='target' style='width:100px;height:20px;transform-origin:0 0'></div></body>",64).unwrap();
        let font = Rc::new(lumen_html_text::FontFace::new(std::sync::Arc::from(
            lumen_html_text::DEFAULT_FONT_BYTES)).unwrap());
        let setup_font = font.clone();
        realm.set_layout_flusher(Rc::new(move |session| session.display_list(300,100,setup_font.as_ref())
            .map(|_| ()).map_err(|error| format!("{error:?}"))));
        eval_ok(&mut engine,"var sampled=document.getElementById('target').animate([{width:'100px',transform:'translateX(100%)'},{width:'200px',transform:'matrix(1,0,0,1,0,0)'}],{duration:100,fill:'both'});sampled.pause();sampled.currentTime=50;");
        let state = hub(engine.ctx()).unwrap();
        let id = *state.borrow().records.keys().next().unwrap();
        let old_frames = state.borrow().records[&id].keyframes.clone();
        let callback_state = state.clone();
        let replaced = Rc::new(Cell::new(false));
        let callback_replaced = replaced.clone();
        let layout_font = font.clone();
        realm.set_layout_flusher(Rc::new(move |session| {
            if !callback_replaced.replace(true) {
                let mut state = callback_state.borrow_mut();
                let record = state.records.get_mut(&id).unwrap();
                assert!(Rc::strong_count(&record.keyframes) >= 3);
                let mut frames = record.keyframes.to_vec();
                for frame in &mut frames {
                    for (property,value) in &mut frame.declarations {
                        if property == "width" { *value = "300px".into(); }
                        if property == "transform" && value.contains("100%") {
                            *value = "translateX(200%)".into();
                        }
                    }
                }
                record.keyframes = frames.into();
                let payload = record.keyframes.clone();
                let effect_id = record.effect_id.unwrap();
                state.effect_records.get_mut(&effect_id).unwrap().keyframes = payload;
            }
            session.display_list(300,100,layout_font.as_ref()).map(|_| ())
                .map_err(|error| format!("{error:?}"))
        }));
        let geometry = |width: f32, translation: f32| realm.with_session(|session| {
            session.display_list(300,100,font.as_ref()).unwrap();
            let node = selector::query_selector(session.document(),session.document().root(),"#target").unwrap().unwrap();
            let rect = session.layout_rect(node).unwrap();
            let matrix = session.computed_style(node).unwrap().transform_matrix(rect).unwrap();
            assert!((rect.width-width).abs() < 0.001);
            assert!((matrix.e-translation).abs() < 0.001);
        });
        let now = realm.timeline_sample.get();
        apply_realm(&state,&realm,now).unwrap();
        assert!(replaced.get());
        geometry(150.0,75.0);
        assert_eq!(old_frames[0].declarations[0].1,"100px");
        assert!(!Rc::ptr_eq(&old_frames,&state.borrow().records[&id].keyframes));
        engine.ctx().collect_garbage();
        assert!(matches!(eval_ok(&mut engine,
            "sampled.effect.getKeyframes().every(frame=>frame.width==='300px')"),Value::Bool(true)));
        apply_realm(&state,&realm,now).unwrap();
        geometry(300.0,300.0);
    }

    #[test]
    fn specification_transition_before_change_demand_replays_only_old_sampled_lineage_with_checked_budget() {
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<div id=parent style='font-size:20px'><div id=leaf style='position:relative;left:2em'></div></div>",4096).unwrap();
        eval_ok(&mut engine,"for(let i=0;i<1024;i++){const sibling=document.createElement('span');sibling.textContent='unrelated';document.body.appendChild(sibling)}");
        let (parent,leaf,source)=realm.with_session(|session|{
            let document=session.document();
            let parent=selector::query_selector(document,document.root(),"#parent").unwrap().unwrap();
            let leaf=selector::query_selector(document,document.root(),"#leaf").unwrap().unwrap();
            (parent,leaf,session.transition_snapshot().unwrap())
        });
        assert!(source.nodes.len()>1024);
        let inputs=source.nodes.iter().map(|input|((input.node,input.pseudo),input)).collect::<HashMap<_,_>>();
        let samples=HashMap::from([((parent,None),vec![(String::from("font-size"),String::from("30px"))])]);
        let mut prepared=HashMap::new();let mut budget=EffectSamplingBudget{bytes:0,style_bytes:0};
        // New author inputs must not replace the retained BEFORE lineage.
        eval_ok(&mut engine,"document.getElementById('parent').style.fontSize='10px'");
        let old=realm.with_session(|session|before_change_style((leaf,None),&inputs,&samples,&HashMap::new(),
            session.document(),session.media_environment(),None,&mut prepared,&mut budget,0)).unwrap();
        assert_eq!(old.left,Some(60.0));
        assert!(prepared.len()<8,"unrelated old snapshot entries were reconstructed");
        assert!(prepared.contains_key(&(parent,None)) && prepared.contains_key(&(leaf,None)));
        assert!(budget.bytes<1024*1024 && budget.style_bytes>0,"canonical retained style admission covers the selected memo");
    }

    #[test]
    fn paint_snapshot_steady_sampling_measurement() {
        let mut engine = Engine::new();
        let realm = crate::install(engine.ctx(), "<body></body>", 64).unwrap();
        let font = Rc::new(lumen_html_text::FontFace::new(
            Arc::from(lumen_html_text::DEFAULT_FONT_BYTES)).unwrap());
        realm.set_layout_flusher(Rc::new(move |session| {
            session.display_list(300,150,font.as_ref()).map(|_| ())
                .map_err(|error| format!("{error:?}"))
        }));
        eval_ok(&mut engine, r#"
            globalThis.sampledAnimations = [];
            for (let index = 0; index < 4; index++) {
                const target = document.createElement('div');
                target.id = 'sample-' + index;
                target.style.height = '20px';
                document.body.appendChild(target);
                const frames = [];
                for (let frame = 0; frame < 128; frame++) {
                    frames.push({offset:frame/127, width:frame+'px',
                        opacity:frame/127, transform:'translateX('+frame+'%)'});
                }
                const animation = target.animate(frames,{duration:1000,fill:'both'});
                animation.pause();
                animation.currentTime = 500;
                sampledAnimations.push(animation);
            }
        "#);
        let state = hub(engine.ctx()).unwrap();
        let now = realm.timeline_sample.get();
        apply_realm(&state,&realm,now).unwrap();
        let started = std::time::Instant::now();
        for _ in 0..64 { apply_realm(&state,&realm,now).unwrap(); }
        let elapsed = started.elapsed();
        eprintln!("paint steady sampling: 4 effects x128 mixed keyframes x64 samples: {} us",elapsed.as_micros());
        assert!(matches!(eval_ok(&mut engine,r#"
            sampledAnimations.every(animation => animation.currentTime === 500 &&
                animation.effect.getKeyframes().length === 128 &&
                getComputedStyle(animation.effect.target).width === '63.5px')
        "#),Value::Bool(true)));
    }
    use super::*;
    use lumen::Engine;
    use lumen_html::selector;

    fn eval_ok(engine: &mut Engine, source: &str) -> Value {
        match engine.eval_value(source).expect("JS engine failed") {
            Ok(value) => value,
            Err(error) => {
                let message = engine
                    .ctx()
                    .coerce_string(&error)
                    .map(|message| message.to_string())
                    .unwrap_or_else(|_| "unknown JavaScript exception".into());
                panic!("JavaScript threw while evaluating `{source}`: {message}");
            }
        }
    }

    #[test]
    fn nullable_keyframes_create_empty_effects_without_accepting_primitives() {
        let mut engine = Engine::new();
        crate::install(engine.ctx(), "<div id='target'></div>", 64).unwrap();
        let result = eval_ok(&mut engine, r#"
            const target = document.getElementById('target');
            for (const frames of [null, undefined]) {
                const effect = new KeyframeEffect(target, frames, 1000);
                if (effect.getKeyframes().length !== 0) throw Error('constructor frames');
                effect.setKeyframes([{opacity: 0}, {opacity: 1}]);
                effect.setKeyframes(frames);
                if (effect.getKeyframes().length !== 0) throw Error('replacement frames');
                const animation = target.animate(frames, 1000);
                if (animation.effect.getKeyframes().length !== 0 || !animation.pending)
                    throw Error('empty effect must retain timing and pending play');
                animation.cancel();
            }
            for (const frames of [true, 1, 'opacity']) {
                let rejected = false;
                try { new KeyframeEffect(target, frames, 1000); }
                catch (error) { rejected = error instanceof TypeError; }
                if (!rejected) throw Error('primitive accepted');
            }
            true
        "#);
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn animation_promise_init_hook_can_inspect_animation_list() {
        let mut engine = Engine::new();
        crate::install(engine.ctx(),
            "<style>@keyframes a{}</style><div id='target'></div>", 64).unwrap();
        let init = eval_ok(&mut engine,
            "globalThis.hookRuns = 0; (() => { hookRuns++; document.getAnimations(); })");
        engine.ctx().set_promise_hooks(Some([
            init, Value::Undefined, Value::Undefined, Value::Undefined,
        ]));
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            eval_ok(&mut engine,
                "document.getElementById('target').style.animation = 'a 100s'; document.getAnimations();");
        }));
        engine.ctx().set_promise_hooks(None);
        assert!(outcome.is_ok(), "Promise init hook must not reenter a borrowed animation hub");
    }

    #[test]
    fn animation_promise_hooks_observe_committed_identity_and_style_replacement() {
        let mut engine = Engine::new();
        crate::install(engine.ctx(),
            "<style>@keyframes a{} @keyframes b{}</style><div id='target'></div>", 64).unwrap();
        let init = eval_ok(&mut engine, r#"
            globalThis.observed = null;
            globalThis.replaceStyle = false;
            (() => {
                const animations = document.getAnimations();
                if (animations.length) {
                    const animation = animations[0];
                    observed = {animation, ready: animation.ready, finished: animation.finished};
                    if (replaceStyle) {
                        replaceStyle = false;
                        document.getElementById('target').style.animation = 'b 100s';
                        document.getAnimations();
                    }
                }
            })
        "#);
        engine.ctx().set_promise_hooks(Some([
            init, Value::Undefined, Value::Undefined, Value::Undefined,
        ]));
        eval_ok(&mut engine, r#"
            document.getElementById('target').style.animation = 'a 100s';
            let animation = document.getAnimations()[0];
            if (observed.animation !== animation || observed.ready !== animation.ready ||
                observed.finished !== animation.finished) throw Error('hook identity overwritten');
            animation.cancel();
            document.getElementById('target').style.animation = 'none';
            document.getAnimations();
            replaceStyle = true;
            document.getElementById('target').style.animation = 'a 99s';
            document.getAnimations();
            const current = document.getAnimations();
            if (current.length !== 1 || current[0].animationName !== 'b')
                throw Error('outer snapshot overwrote hook style replacement');
        "#);
        engine.ctx().set_promise_hooks(None);
    }

    #[test]
    fn animation_promise_init_hook_retains_unsampled_cancelled_siblings() {
        let mut engine = Engine::new();
        crate::install(engine.ctx(),
            "<style>@keyframes a{} @keyframes b{}</style><div id='target' style='animation:a 100s,b 100s'></div>", 64).unwrap();
        let global = engine.ctx().global_object();
        assert!(engine.ctx().install_module::<event_test_gc::Module>(&global).is_ok());
        eval_ok(&mut engine, "document.getAnimations();");
        engine.ctx().collect_garbage();
        let init = eval_ok(&mut engine, "(() => { forceEventGC(); document.getAnimations(); })");
        engine.ctx().set_promise_hooks(Some([init, Value::Undefined, Value::Undefined, Value::Undefined]));
        eval_ok(&mut engine,
            "document.getElementById('target').style.animation='none'; document.getAnimations();");
        engine.ctx().set_promise_hooks(None);
        let state = hub(engine.ctx()).unwrap();
        assert!(state.borrow().dispatching_css.is_empty(), "publication leases release after hooks");
        engine.ctx().collect_garbage();
        collect_retired_css_animations(engine.ctx(), &state);
        assert!(state.borrow().records.is_empty(), "unobserved retired siblings remain collectible");
    }

    #[test]
    fn animation_finished_getter_keeps_promise_alive_when_init_hook_settles_it() {
        let mut engine = Engine::new();
        crate::install(engine.ctx(), "<div id='target'></div>", 64).unwrap();
        eval_ok(&mut engine,
            "globalThis.animation=document.getElementById('target').animate([],1); animation.finish();");
        engine.ctx().collect_garbage();
        let init = eval_ok(&mut engine, "(() => animation.finish())");
        engine.ctx().set_promise_hooks(Some([init, Value::Undefined, Value::Undefined, Value::Undefined]));
        let promise = eval_ok(&mut engine, "animation.finished");
        engine.ctx().set_promise_hooks(None);
        assert!(matches!(promise, Value::Obj(_)), "getter returns its actual promise after reentrant settlement");
    }

    #[test]
    fn animation_ready_resolve_hook_defers_restarted_sibling_to_next_frame() {
        let mut engine = Engine::new();
        crate::install(engine.ctx(), "<div id='target'></div>", 64).unwrap();
        eval_ok(&mut engine, r#"
            const target = document.getElementById('target');
            globalThis.first = target.animate([],1000);
            globalThis.second = target.animate([],1000);
            globalThis.firstReady = first.ready;
            globalThis.armed = true;
        "#);
        let resolve = eval_ok(&mut engine, r#"
            (promise => {
                if (armed) {
                    armed = false;
                    globalThis.restarted = promise === firstReady ? second : first;
                    restarted.cancel();
                    restarted.play();
                }
            })
        "#);
        engine.ctx().set_promise_hooks(Some([Value::Undefined,Value::Undefined,Value::Undefined,resolve]));
        let state = hub(engine.ctx()).unwrap();
        let start = state.borrow().records.values().map(|record|record.start_ms).fold(0.0,f64::max);
        advance(engine.ctx(), start).unwrap();
        engine.ctx().set_promise_hooks(None);
        assert!(matches!(eval_ok(&mut engine,"restarted.pending"),Value::Bool(true)),
            "hook-created task must not consume a previous task's frame opportunity");
        advance(engine.ctx(), start+20.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,"restarted.pending"),Value::Bool(false)));
        assert!(state.borrow().dispatching_css.is_empty());
    }

    #[test]
    fn animation_hub_lookup_is_scoped_to_the_active_realm() {
        let mut engine = Engine::new();
        let ctx = engine.ctx();
        let parent = RealmServices::<RefCell<AnimationHub>>::replace_current(
            ctx,
            RefCell::new(AnimationHub::default()),
        );
        parent.borrow_mut().next_id = 10;

        let child = ctx.create_host_realm();
        let child_hub = ctx
            .with_host_realm(&child, |ctx| {
                assert!(hub(ctx).is_err());
                let child = RealmServices::<RefCell<AnimationHub>>::replace_current(
                    ctx,
                    RefCell::new(AnimationHub::default()),
                );
                child.borrow_mut().next_id = 2;
                assert!(Rc::ptr_eq(&child, &hub(ctx).expect("child animation hub")));
                child
            })
            .expect("enter child realm");

        assert_eq!(parent.borrow().next_id, 10);
        assert_eq!(child_hub.borrow().next_id, 2);
        assert!(Rc::ptr_eq(
            &parent,
            &hub(ctx).expect("parent animation hub")
        ));
        let child_again = ctx
            .with_host_realm(&child, hub)
            .expect("re-enter child realm")
            .expect("child animation hub remains installed");
        assert!(Rc::ptr_eq(&child_hub, &child_again));
    }

    #[test]
    fn adapter_accepts_cubic_and_keyframe_step_easing() {
        let mut engine = Engine::new();
        let realm = crate::install(
            engine.ctx(),
            "<div id='target' style='opacity: 0'></div>",
            64,
        )
        .unwrap();
        let result = eval_ok(
            &mut engine,
            "window.anim = document.getElementById('target').animate(\
                 [{opacity: 0, easing: 'steps(2, start)'}, {opacity: 1}], \
                 {duration: 100, fill: 'both', easing: 'cubic-bezier(0.25, 0.1, 0.25, 1)'}); \
                 anim instanceof Animation",
        );
        assert!(matches!(result, Value::Bool(true)));
        let hub = hub(engine.ctx()).unwrap();
        let start = {
            let mut state = hub.borrow_mut();
            let record = state.records.values_mut().next().unwrap();
            assert_eq!(
                record.keyframes[0].easing.as_deref(),
                Some("steps(2, start)")
            );
            let start = lumen_host::perf::now_ms() - 20.0;
            record.start_ms = start;
            start
        };
        advance(engine.ctx(), start + 20.0).unwrap();
        realm.with_session(|session| {
            let document = session.document();
            let node = selector::query_selector(document, document.root(), "#target")
                .unwrap()
                .unwrap();
            assert_eq!(session.computed_style(node).unwrap().opacity, 0.5);
        });
    }

    #[test]
    fn specification_property_indexed_keyframes_merge_tracks_repeat_metadata_and_validate_values() {
        let mut engine = Engine::new();
        let _realm = crate::install(engine.ctx(), "<div id=target style='margin-left:10px'></div>", 64).unwrap();
        let result = eval_ok(&mut engine, r#"(() => {
            const check=(ok,message)=>{if(!ok)throw Error(message)};
            const target=document.getElementById('target');
            const effect=new KeyframeEffect(target,{left:['0px','10px'],top:['0px','bad','20px'],
                easing:['ease-in','linear'],composite:'add',offset:[0,.5,1,1,1]},100);
            const frames=effect.getKeyframes();
            check(frames.length===3,'offset metadata does not add frames');
            check(frames[0].left==='0px' && frames[0].top==='0px','equal initial offsets merge');
            check(!('left' in frames[1]) && !('top' in frames[1]),'unequal tracks and invalid declarations stay separate');
            check(frames[2].left==='10px' && frames[2].top==='20px','equal final offsets merge');
            check(frames.every(frame=>frame.composite==='add'),'scalar composite repeats');
            check(frames[0].easing==='ease-in' && frames[1].easing==='linear' && frames[2].easing==='ease-in','easing list repeats');
            let threw=false;
            try{effect.setKeyframes({left:['0px','10px'],easing:['linear','linear','invalid']})}catch(e){threw=e.name==='TypeError'}
            check(threw,'unused easing still validates');
            check(effect.getKeyframes().length===3,'failed setKeyframes preserves source');
            const animation=target.animate({marginLeft:['0px','10px'],composite:'add'},100);
            animation.currentTime=50;
            check(getComputedStyle(target).marginLeft==='15px','both endpoints compose against real underlying value');
            animation.cancel();
            return true;
        })()"#);
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn property_indexed_keyframes_apply_offsets_easing_color_and_important_origin() {
        let mut engine = Engine::new();
        let realm = crate::install(
            engine.ctx(),
            "<style>.protected { opacity: .9 !important }</style>\
             <div id='animated'></div><div id='protected' class='protected'></div>",
            64,
        )
        .unwrap();
        let result = eval_ok(
            &mut engine,
            "window.effect = document.getElementById('animated').animate({\
                   opacity: [0, 1], width: ['10px', '30px'], \
                   backgroundColor: ['red', 'blue'], offset: [0, 1], \
                   easing: ['ease-in', 'linear']\
                 }, {duration: 100, iterations: 2, direction: 'alternate', fill: 'both'}); \
                 window.importantEffect = document.getElementById('protected').animate(\
                   {opacity: [0, 1], offset: [0, 1]}, {duration: 100, fill: 'both'}); \
                 effect instanceof Animation && importantEffect instanceof Animation",
        );
        assert!(matches!(result, Value::Bool(true)));

        // Resolve actual first-frame readiness before manipulating the test clock.
        advance(engine.ctx(),lumen_host::perf::now_ms()).unwrap();
        let hub = hub(engine.ctx()).unwrap();
        let now = lumen_host::perf::now_ms();
        for record in hub.borrow_mut().records.values_mut() {
            record.start_ms = now
                - if record.timing.iterations > 1.0 {
                    125.0
                } else {
                    50.0
                };
        }
        advance(engine.ctx(), now).unwrap();
        realm.with_session(|session| {
            let (animated, protected) = {
                let document = session.document();
                (
                    selector::query_selector(document, document.root(), "#animated")
                        .unwrap()
                        .unwrap(),
                    selector::query_selector(document, document.root(), "#protected")
                        .unwrap()
                        .unwrap(),
                )
            };
            let animated_style = session.computed_style(animated).unwrap();
            assert!((animated_style.opacity - 0.621862).abs() < 0.0001);
            assert!((animated_style.width.unwrap() - 22.43724).abs() < 0.001);
            // ease-in(.75) is .621862: an opaque red/blue blend rounds to
            // (96, 0, 159), rather than the old polynomial easing's color.
            assert_eq!(animated_style.background.r, 96);
            assert_eq!(animated_style.background.g, 0);
            assert_eq!(animated_style.background.b, 159);
            assert!((session.computed_style(protected).unwrap().opacity - 0.9).abs() < 0.02);
        });
    }

    #[test]
    fn public_discrete_animation_updates_layout_and_visibility_at_defined_boundaries() {
        let mut engine = Engine::new();
        let realm = crate::install(
            engine.ctx(),
            "<div id='target' style='width:10px;height:10px'></div>",
            64,
        )
        .unwrap();
        eval_ok(
            &mut engine,
            r#"
            var discrete=document.getElementById('target').animate(
                [{display:'none',visibility:'hidden'}, {display:'block',visibility:'visible'}],
                {duration:100,fill:'both'});
            discrete.pause(); discrete.currentTime=25;
        "#,
        );
        realm.with_session(|session| {
            let document = session.document();
            let node = selector::query_selector(document, document.root(), "#target")
                .unwrap()
                .unwrap();
            let style = session.computed_style(node).unwrap();
            assert_eq!(style.display, lumen_html::css::Display::Block);
            assert!(style.visibility_visible);
        });
        eval_ok(&mut engine, "discrete.currentTime=0");
        realm.with_session(|session| {
            let document = session.document();
            let node = selector::query_selector(document, document.root(), "#target")
                .unwrap()
                .unwrap();
            let style = session.computed_style(node).unwrap();
            assert_eq!(style.display, lumen_html::css::Display::None);
            assert!(!style.visibility_visible);
        });
    }

    #[test]
    fn public_keyframe_composite_overrides_effect_and_interpolates_endpoints() {
        let mut engine = Engine::new();
        let realm = crate::install(engine.ctx(),
            "<div id='mixed' style='width:10px'></div><div id='indexed' style='width:10px'></div><div id='repeat' style='width:10px'></div>", 64).unwrap();
        let result = eval_ok(
            &mut engine,
            r#"
            var mixed=document.getElementById('mixed').animate(
                [{width:'2px', composite:'add'}, {width:'30px', composite:'replace'}],
                {duration:100, fill:'both', composite:'accumulate'});
            var indexed=document.getElementById('indexed').animate(
                {width:['2px','30px'], composite:['add','replace']},
                {duration:100, fill:'both'});
            var repeat=document.getElementById('repeat').animate(
                [{width:'2px'}, {width:'6px'}],
                {duration:100, iterations:3, fill:'both', composite:'accumulate'});
            mixed.pause(); mixed.currentTime=50;
            indexed.pause(); indexed.currentTime=50;
            repeat.pause(); repeat.currentTime=250;
            var frames=mixed.effect.getKeyframes();
            var indexedFrames=indexed.effect.getKeyframes();
            frames[0].composite==='add' && frames[1].composite==='replace' &&
                indexedFrames[0].composite==='add' && indexedFrames[1].composite==='replace'
        "#,
        );
        assert!(matches!(result, Value::Bool(true)));
        realm.with_session(|session| {
            for (id, width) in [("mixed", 21.0), ("indexed", 21.0), ("repeat", 14.0)] {
                let document = session.document();
                let node = selector::query_selector(document, document.root(), &format!("#{id}"))
                    .unwrap()
                    .unwrap();
                assert_eq!(session.computed_style(node).unwrap().width, Some(width));
            }
        });
        let result = eval_ok(
            &mut engine,
            r#"
            var rejected=false;
            try {mixed.effect.setKeyframes([{width:'2px',composite:'invalid'}]);}
            catch(error) { rejected=error.name==='TypeError'; }
            rejected && mixed.effect.getKeyframes()[0].composite==='add'
        "#,
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn public_transform_animation_reaches_retained_geometry_and_preserves_turns() {
        let mut engine = Engine::new();
        let realm = crate::install(engine.ctx(),
            "<body style='margin:0'><div id='target' style='width:100px;height:20px;transform-origin:0 0'></div></body>", 64).unwrap();
        let font = Rc::new(
            lumen_html_text::FontFace::new(std::sync::Arc::from(
                lumen_html_text::DEFAULT_FONT_BYTES,
            ))
            .unwrap(),
        );
        let layout_font = font.clone();
        realm.set_layout_flusher(Rc::new(move |session| {
            session
                .display_list(300, 100, layout_font.as_ref())
                .map(|_| ())
                .map_err(|e| format!("{e:?}"))
        }));
        eval_ok(&mut engine, "var target=document.getElementById('target');var motion=target.animate([{transform:'translateX(0%) rotate(0deg)'},{transform:'translateX(100%) rotate(360deg)'}],{duration:100,fill:'both'});motion.pause();motion.currentTime=50");
        realm.with_session(|session| {
            let node =
                selector::query_selector(session.document(), session.document().root(), "#target")
                    .unwrap()
                    .unwrap();
            session.display_list(300, 100, font.as_ref()).unwrap();
            let rect = session.layout_rect(node).unwrap();
            let matrix = session
                .computed_style(node)
                .unwrap()
                .transform_matrix(rect)
                .unwrap();
            assert!((matrix.a + 1.0).abs() < 0.001 && (matrix.d + 1.0).abs() < 0.001);
            assert!((matrix.e - 50.0).abs() < 0.001);
        });
        // Percentage interpolation continues to resolve against the new box.
        eval_ok(
            &mut engine,
            "target.style.width='200px';motion.currentTime=50",
        );
        realm.with_session(|session| {
            let node =
                selector::query_selector(session.document(), session.document().root(), "#target")
                    .unwrap()
                    .unwrap();
            session.display_list(300, 100, font.as_ref()).unwrap();
            let rect = session.layout_rect(node).unwrap();
            assert!(
                (session
                    .computed_style(node)
                    .unwrap()
                    .transform_matrix(rect)
                    .unwrap()
                    .e
                    - 100.0)
                    .abs()
                    < 0.001
            );
        });
        // A mismatched primitive uses matrix decomposition, not a discrete jump.
        eval_ok(&mut engine,"motion.effect.setKeyframes([{transform:'translateX(0px)'},{transform:'matrix(1,0,0,1,80,0)'}]);motion.currentTime=50");
        realm.with_session(|session| {
            let node =
                selector::query_selector(session.document(), session.document().root(), "#target")
                    .unwrap()
                    .unwrap();
            session.display_list(300, 100, font.as_ref()).unwrap();
            let rect = session.layout_rect(node).unwrap();
            assert!(
                (session
                    .computed_style(node)
                    .unwrap()
                    .transform_matrix(rect)
                    .unwrap()
                    .e
                    - 40.0)
                    .abs()
                    < 0.001
            );
        });
    }

    #[test]
    fn public_transform_composition_and_animated_reference_box_share_frame() {
        let mut engine = Engine::new();
        let realm=crate::install(engine.ctx(),"<body style='margin:0'><div id='target' style='width:100px;height:20px;transform-origin:0 0'></div><div id='add' style='transform:scale(2);transform-origin:0 0;width:20px;height:20px'></div><div id='acc' style='transform:scale(2);transform-origin:0 0;width:20px;height:20px'></div></body>",64).unwrap();
        let font = Rc::new(
            lumen_html_text::FontFace::new(std::sync::Arc::from(
                lumen_html_text::DEFAULT_FONT_BYTES,
            ))
            .unwrap(),
        );
        let layout_font = font.clone();
        realm.set_layout_flusher(Rc::new(move |session| {
            session
                .display_list(300, 100, layout_font.as_ref())
                .map(|_| ())
                .map_err(|e| format!("{e:?}"))
        }));
        eval_ok(&mut engine,"var target=document.getElementById('target');var resize=target.animate([{width:'100px',transform:'translateX(100%)'},{width:'200px',transform:'matrix(1,0,0,1,0,0)'}],{duration:100,fill:'both'});resize.pause();resize.currentTime=50;var add=document.getElementById('add').animate([{transform:'scale(1)'},{transform:'scale(3)'}],{duration:100,fill:'both',composite:'add'});add.pause();add.currentTime=50;var acc=document.getElementById('acc').animate([{transform:'scale(1)'},{transform:'scale(3)'}],{duration:100,fill:'both',composite:'accumulate'});acc.pause();acc.currentTime=50;");
        realm.with_session(|session| {
            session.display_list(300, 100, font.as_ref()).unwrap();
            for (id, expected) in [("target", 75.0), ("add", 4.0), ("acc", 3.0)] {
                let node = selector::query_selector(
                    session.document(),
                    session.document().root(),
                    &format!("#{id}"),
                )
                .unwrap()
                .unwrap();
                let rect = session.layout_rect(node).unwrap();
                let matrix = session
                    .computed_style(node)
                    .unwrap()
                    .transform_matrix(rect)
                    .unwrap();
                let actual = if id == "target" {
                    assert!((rect.width - 150.0).abs() < 0.001);
                    matrix.e
                } else {
                    matrix.a
                };
                assert!(
                    (actual - expected).abs() < 0.001,
                    "{id}: {actual} != {expected}"
                );
            }
        });
    }

    #[test]
    fn public_effect_options_preserve_auto_numeric_and_inherited_updates() {
        let mut engine = Engine::new();
        let realm = crate::install(engine.ctx(), "<div id='target'></div>", 64).unwrap();
        let result = eval_ok(
            &mut engine,
            r#"
            var target = document.getElementById('target');
            var defaults = new KeyframeEffect(target, [{width:'0px'}, {width:'100px'}]);
            var specified = defaults.getTiming(), computed = defaults.getComputedTiming();
            var valid = specified.duration === 'auto' && specified.fill === 'auto' &&
                computed.duration === 0 && computed.fill === 'none';
            var numeric = target.animate([{width:'0px'}, {width:'100px'}], 100);
            numeric.pause(); numeric.currentTime = 50;
            valid = valid && numeric.effect.getTiming().duration === 100;
            var direct = new KeyframeEffect(null, [], 250);
            valid = valid && direct.getTiming().duration === 250;
            var inherited = Object.create({duration: 200, fill:'both', delay:10});
            Object.defineProperty(inherited, 'unknown', {get:function(){throw 'ignored';}});
            defaults.updateTiming(inherited);
            valid = valid && defaults.getTiming().duration === 200 &&
                defaults.getTiming().fill === 'both' && defaults.getTiming().delay === 10;
            defaults.updateTiming({duration: {toString:function(){return 'auto';}}, fill:'auto'});
            valid = valid && defaults.getTiming().duration === 'auto' &&
                defaults.getTiming().fill === 'auto' && defaults.getComputedTiming().duration === 0;
            var rejected = 0;
            for (var duration of ['100', true, null, {valueOf:function(){return 100;}}]) {
                try { defaults.updateTiming({duration:duration, delay:99}); }
                catch (error) { if (error.name === 'TypeError') rejected++; }
            }
            var sentinel = {}, preserved = false;
            var throwing = Object.create(null);
            Object.defineProperty(throwing, 'duration', {get:function(){throw sentinel;}});
            try { defaults.updateTiming(throwing); }
            catch (error) { preserved = error === sentinel; }
            valid && rejected === 4 && preserved && defaults.getTiming().delay === 10 &&
                defaults.getTiming().duration === 'auto'
        "#,
        );
        assert!(matches!(result, Value::Bool(true)));
        realm.with_session(|session| {
            let document = session.document();
            let node = selector::query_selector(document, document.root(), "#target")
                .unwrap()
                .unwrap();
            assert_eq!(session.computed_style(node).unwrap().width, Some(50.0));
        });
    }

    #[test]
    fn public_element_animate_selects_timeline_and_reflects_id() {
        let mut engine = Engine::new();
        crate::install(engine.ctx(), "<div id='target'></div>", 64).unwrap();
        let result = eval_ok(
            &mut engine,
            r#"
            var target = document.getElementById('target');
            var timeline = new DocumentTimeline({originTime:25});
            var selected = target.animate([], {duration:100, id:42, timeline:timeline});
            var valid = selected.timeline === timeline && selected.id === '42';
            selected.id = 'renamed';
            var detached = target.animate([], {duration:100, timeline:null});
            var idle = new Animation();
            var rejected = false;
            try { target.animate([], {timeline:{}}); }
            catch (error) { rejected = error.name === 'TypeError'; }
            valid && selected.id === 'renamed' && detached.timeline === null &&
                detached.currentTime === 0 && detached.pending && idle.id === '' && rejected
        "#,
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn public_effect_timing_reports_iteration_start_and_end_delay() {
        let mut engine = Engine::new();
        let realm = crate::install(engine.ctx(), "<div id='target'></div>", 64).unwrap();
        let result = eval_ok(
            &mut engine,
            r#"
            var timingAnimation = document.getElementById('target').animate(
                [{width:'0px'}, {width:'100px'}],
                {delay:50, endDelay:50, duration:100, iterationStart:1.25,
                 iterations:1.5, direction:'alternate', fill:'both'});
            timingAnimation.pause(); timingAnimation.currentTime=25;
            var before=timingAnimation.effect.getComputedTiming();
            var input=timingAnimation.effect.getTiming();
            var valid=before.progress===.75 && before.currentIteration===1 &&
                before.localTime===25 && before.activeDuration===150 && before.endTime===250 &&
                input.iterationStart===1.25 && input.endDelay===50;
            timingAnimation.currentTime=125;
            var active=timingAnimation.effect.getComputedTiming();
            valid=valid && active.progress===0 && active.currentIteration===2;
            timingAnimation.effect.updateTiming({endDelay:-75, iterationStart:undefined});
            timingAnimation.finish();
            var terminal=timingAnimation.effect.getComputedTiming();
            valid && terminal.endTime===125 && terminal.localTime===125 &&
                terminal.progress===0 && terminal.currentIteration===2 &&
                timingAnimation.effect.getTiming().iterationStart===1.25
        "#,
        );
        assert!(matches!(result, Value::Bool(true)));
        realm.with_session(|session| {
            let document = session.document();
            let node = selector::query_selector(document, document.root(), "#target")
                .unwrap()
                .unwrap();
            assert_eq!(session.computed_style(node).unwrap().width, Some(0.0));
        });
    }

    #[test]
    fn public_computed_timing_handles_infinite_zero_duration_and_invalid_updates() {
        let mut engine = Engine::new();
        crate::install(engine.ctx(), "<div id='target'></div>", 64).unwrap();
        let result = eval_ok(
            &mut engine,
            r#"
            var emptyEffect = new KeyframeEffect(document.getElementById('target'),
                [{opacity:0},{opacity:1}], {duration:0, iterations:Infinity,
                 iterationStart:1.25, direction:'alternate-reverse', fill:'both'});
            var unattached=emptyEffect.getComputedTiming();
            var zero = new Animation(emptyEffect, document.timeline);
            zero.finish();
            var terminal=emptyEffect.getComputedTiming();
            var invalid=0;
            for (var options of [{iterationStart:-1}, {iterationStart:Infinity},
                                 {delay:NaN}, {endDelay:Infinity}, {duration:NaN}]) {
                try { emptyEffect.updateTiming(options); }
                catch (error) { if(error.name==='TypeError') invalid++; }
            }
            unattached.localTime===null && unattached.progress===null &&
                unattached.currentIteration===null && terminal.activeDuration===0 &&
                terminal.endTime===0 && terminal.currentIteration===Infinity &&
                terminal.progress===.25 && invalid===5 &&
                emptyEffect.getTiming().iterationStart===1.25
        "#,
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn public_fractional_iteration_fill_preserves_final_rendered_value() {
        let mut engine = Engine::new();
        let realm = crate::install(engine.ctx(), "<div id='target'></div>", 64).unwrap();
        eval_ok(&mut engine, "var fractional=document.getElementById('target').animate([{width:'0px'},{width:'100px'}],{duration:100,iterations:2.25,fill:'forwards'});fractional.pause();fractional.currentTime=300");
        advance(engine.ctx(), lumen_host::perf::now_ms()).unwrap();
        realm.with_session(|session| {
            let node =
                selector::query_selector(session.document(), session.document().root(), "#target")
                    .unwrap()
                    .unwrap();
            assert!((session.computed_style(node).unwrap().width.unwrap() - 25.0).abs() < 0.001);
        });
    }

    #[test]
    fn public_step_easing_uses_fill_phase_for_effect_and_keyframe_boundaries() {
        let mut engine = Engine::new();
        let realm = crate::install(
            engine.ctx(),
            "<div id='effect'></div><div id='keyframe'></div>",
            64,
        )
        .unwrap();
        eval_ok(&mut engine, "var effectStep=document.getElementById('effect').animate([{width:'0px'},{width:'100px'}],{duration:100,delay:100,fill:'both',easing:'steps(2,start)'});var keyframeStep=document.getElementById('keyframe').animate([{width:'0px',easing:'steps(2,start)'},{width:'100px'}],{duration:100,delay:100,fill:'both'});effectStep.pause();keyframeStep.pause();effectStep.currentTime=50;keyframeStep.currentTime=50");
        for (time, expected) in [(50, 0.0), (100, 50.0)] {
            eval_ok(
                &mut engine,
                &format!("effectStep.currentTime={time};keyframeStep.currentTime={time}"),
            );
            advance(engine.ctx(), lumen_host::perf::now_ms()).unwrap();
            realm.with_session(|session| {
                for selector in ["#effect", "#keyframe"] {
                    let node = selector::query_selector(
                        session.document(),
                        session.document().root(),
                        selector,
                    )
                    .unwrap()
                    .unwrap();
                    assert!(
                        (session.computed_style(node).unwrap().width.unwrap() - expected).abs()
                            < 0.001
                    );
                }
            });
        }
    }

    #[test]
    fn public_animation_easing_overshoot_reaches_retained_layout() {
        let mut engine = Engine::new();
        let realm = crate::install(
            engine.ctx(),
            "<div id='target' style='height:10px'></div>",
            64,
        )
        .unwrap();
        eval_ok(&mut engine, "var overshoot=document.getElementById('target').animate([{width:'0px'},{width:'100px'}],{duration:100,fill:'both',easing:'cubic-bezier(0.25,2,0.75,2)'});overshoot.pause();overshoot.currentTime=50");
        advance(engine.ctx(), lumen_host::perf::now_ms()).unwrap();
        let font = lumen_html_text::FontFace::new(std::sync::Arc::from(
            lumen_html_text::DEFAULT_FONT_BYTES,
        ))
        .unwrap();
        realm.with_session(|session| {
            let node =
                selector::query_selector(session.document(), session.document().root(), "#target")
                    .unwrap()
                    .unwrap();
            assert!((session.computed_style(node).unwrap().width.unwrap() - 162.5).abs() < 0.001);
            session.display_list(300, 100, &font).unwrap();
            assert!((session.layout_rect(node).unwrap().width - 162.5).abs() < 0.001);
        });
    }

    #[test]
    fn retired_css_animation_records_are_bounded_and_observed_wrappers_survive() {
        let mut engine = Engine::new();
        let realm = crate::install(engine.ctx(),
            "<style>@keyframes fade{from{opacity:0}to{opacity:1}}</style><div id='target'></div>",64).unwrap();
        let state = hub(engine.ctx()).unwrap();
        for _ in 0..128 {
            eval_ok(&mut engine,"document.getElementById('target').style.animation='100s fade'");
            advance(engine.ctx(),lumen_host::perf::now_ms()).unwrap();
            assert_eq!(state.borrow().records.len(),1);
            eval_ok(&mut engine,"document.getElementById('target').style.animation='none';document.getElementById('target').getAnimations();");
            assert!(crate::scheduling::run_tasks(&mut engine,32).is_empty());
            engine.ctx().collect_garbage();
            advance(engine.ctx(),lumen_host::perf::now_ms()).unwrap();
            assert!(state.borrow().records.is_empty());
            assert!(state.borrow().effect_records.is_empty());
        }
        eval_ok(&mut engine,"document.getElementById('target').style.animation='100s fade'; window.saved=document.getElementById('target').getAnimations()[0]; window.ready=saved.ready; window.finished=saved.finished; finished.catch(()=>{});");
        eval_ok(&mut engine,"document.getElementById('target').style.animation='none'");
        advance(engine.ctx(),lumen_host::perf::now_ms()).unwrap();
        assert_eq!(state.borrow().records.len(),1);
        assert!(matches!(eval_ok(&mut engine,"saved.playState==='idle' && saved.effect!==null"),Value::Bool(true)));
        assert!(crate::scheduling::run_tasks(&mut engine,32).is_empty());
        eval_ok(&mut engine,"window.saved=null;window.ready=null;window.finished=null;");
        while engine.run_one_job() {}
        engine.ctx().collect_garbage();
        advance(engine.ctx(),lumen_host::perf::now_ms()).unwrap();
        assert!(state.borrow().records.is_empty());
        assert!(state.borrow().effect_records.is_empty());
        // The realm remains usable after record collection.
        assert!(realm.with_session(|session| session.animation_snapshot().unwrap().nodes.len()) > 0);
    }

    #[test]
    fn empty_css_keyframes_preserve_timing_cancellation_and_sibling_settlement() {
        let mut engine = Engine::new();
        let _realm = crate::install(engine.ctx(),
            "<style>@keyframes full{to{opacity:0.5}}@keyframes empty{}</style><div id='target' style='animation:full 100s'></div><div id='sibling'></div>",
            64).unwrap();
        eval_ok(&mut engine, r#"
            globalThis.target=document.getElementById('target');
            globalThis.original=target.getAnimations()[0];
            globalThis.rejected=0; globalThis.completed=0;
            original.finished.catch(error=>{if(error.name==='AbortError')rejected++;});
            globalThis.sibling=document.getElementById('sibling').animate([], {duration:20,fill:'both'});
            sibling.finished.then(()=>completed++);
            target.style.animation='empty 100s';
            globalThis.emptyAnimation=target.getAnimations()[0];
            globalThis.emptyCompleted=0;
            emptyAnimation.finished.then(()=>emptyCompleted++);
        "#);
        // Publish the first actual frame before measuring elapsed playback.
        advance(engine.ctx(),lumen_host::perf::now_ms()).unwrap();
        while engine.run_one_job() {}
        assert!(matches!(eval_ok(&mut engine, r#"
            rejected===1 && original.playState==='idle' && emptyAnimation instanceof CSSAnimation
            && emptyAnimation.animationName==='empty' && emptyAnimation.effect.getKeyframes().length===0
            && emptyAnimation.effect.getTiming().duration===100000
        "#),Value::Bool(true)));
        let state = hub(engine.ctx()).unwrap();
        let start = state.borrow().records.values().map(|record|record.start_ms)
            .fold(0.0, f64::max);
        for offset in [25.0, 50.0, 75.0] {
            advance(engine.ctx(), start + offset).unwrap();
            while engine.run_one_job() {}
        }
        assert!(matches!(eval_ok(&mut engine,"completed===1 && emptyCompleted===0 && rejected===1"),Value::Bool(true)));
        advance(engine.ctx(),start+100001.0).unwrap();
        while engine.run_one_job() {}
        assert!(matches!(eval_ok(&mut engine,"emptyCompleted===1 && completed===1 && rejected===1"),Value::Bool(true)));
        for _ in 0..3 {
            assert!(crate::scheduling::run_animation_frame(&mut engine).is_empty(),
                "valid empty keyframes must not repeatedly report frame errors");
        }
    }

    #[test]
    fn cancellation_replaces_finished_promise_once_and_replay_settles_replacement() {
        let mut engine = Engine::new();
        let _realm = crate::install(engine.ctx(), "<div id='target'></div>", 64).unwrap();
        eval_ok(&mut engine, r#"
            globalThis.animation=document.getElementById('target').animate([],100000);
            globalThis.old=animation.finished;
            globalThis.aborted=0; globalThis.resolved=0; globalThis.cancelCount=0;
            old.catch(e=>{if(e.name==='AbortError')aborted++;});
            // Listener observes the new promise synchronously during dispatch.
            animation.addEventListener('cancel',()=>{cancelCount++; globalThis.listenerPromise=animation.finished;});
            animation.cancel();
            globalThis.replacement=animation.finished;
            replacement.then(()=>resolved++);
            globalThis.identity=old!==replacement && replacement===listenerPromise;
            animation.cancel(); animation.currentTime=null;
            identity=identity && replacement===animation.finished;
        "#);
        while engine.run_one_job() {}
        assert!(matches!(eval_ok(&mut engine,"identity && aborted===1 && resolved===0 && cancelCount===1"),Value::Bool(true)));
        eval_ok(&mut engine,"animation.play(); animation.finish();");
        while engine.run_one_job() {}
        assert!(matches!(eval_ok(&mut engine,"animation.finished===replacement && resolved===1 && aborted===1"),Value::Bool(true)));
        eval_ok(&mut engine,r#"
            animation.cancel();
            globalThis.idlePromise=animation.finished;
            globalThis.idleSettled=0;
            idlePromise.then(()=>idleSettled++,()=>idleSettled++);
            animation.cancel();
        "#);
        while engine.run_one_job() {}
        assert!(matches!(eval_ok(&mut engine,"idlePromise!==replacement && idlePromise===animation.finished && idleSettled===0 && resolved===1"),Value::Bool(true)));
    }

    #[test]
    fn automatic_css_cancellation_replaces_finished_promise_without_settling_siblings() {
        let mut engine = Engine::new();
        let _realm = crate::install(engine.ctx(),
            "<style>@keyframes full{to{opacity:0.5}}@keyframes empty{}</style><div id='target' style='animation:full 100s'></div><div id='sibling'></div>",64).unwrap();
        eval_ok(&mut engine,r#"
            globalThis.target=document.getElementById('target');
            globalThis.animation=target.getAnimations()[0];
            globalThis.old=animation.finished;
            globalThis.aborted=0; globalThis.replacementSettled=0; globalThis.siblingDone=0;
            old.catch(e=>{if(e.name==='AbortError')aborted++;});
            globalThis.sibling=document.getElementById('sibling').animate([],20);
            sibling.finished.then(()=>siblingDone++);
            target.style.animation='empty 100s'; target.getAnimations();
            globalThis.replacement=animation.finished;
            replacement.then(()=>replacementSettled++,()=>replacementSettled++);
        "#);
        while engine.run_one_job() {}
        assert!(matches!(eval_ok(&mut engine,"old!==replacement && replacement===animation.finished && aborted===1 && replacementSettled===0 && animation.playState==='idle'"),Value::Bool(true)));
        let state=hub(engine.ctx()).unwrap();
        let start=state.borrow().records.values().map(|record|record.start_ms).fold(0.0,f64::max);
        for offset in [25.0,50.0,75.0] {
            advance(engine.ctx(),start+offset).unwrap();
            while engine.run_one_job() {}
        }
        assert!(matches!(eval_ok(&mut engine,"replacement===animation.finished && replacementSettled===0 && aborted===1 && siblingDone===1"),Value::Bool(true)));
        eval_ok(&mut engine, r#"
            globalThis.explicit=target.getAnimations()[0];
            explicit.finished.catch(()=>{}); explicit.cancel();
            globalThis.explicitPending=explicit.finished;
            target.style.animation='none'; target.getAnimations();
        "#);
        advance(engine.ctx(),start+100.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,"explicit.finished===explicitPending"),Value::Bool(true)));
    }

    #[lumen_bind::module(name = "animation_event_test_gc")]
    mod event_test_gc {
        use super::*;
        #[op(rename(js = "forceEventGC"))]
        fn force_event_gc(ctx: &mut Ctx) { ctx.collect_garbage(); }
    }

    #[test]
    fn running_animation_seek_uses_committed_document_frame_clock() {
        let mut engine = Engine::new();
        let _realm = crate::install(engine.ctx(), "<div id='target'></div>", 64).unwrap();
        eval_ok(&mut engine, r#"
            globalThis.animation = document.getElementById('target').animate(
                [{opacity: 0}, {opacity: 1}], {duration: 1000, fill: 'both'});
        "#);
        advance(engine.ctx(), lumen_host::perf::web_now_ms()).unwrap();
        while engine.run_one_job() {}
        assert!(matches!(eval_ok(&mut engine,
            "!animation.pending && animation.startTime !== null && animation.playState === 'running'"),
            Value::Bool(true)));
        // Wall-clock passage without another rendering opportunity must not change
        // the committed document timeline used by a synchronous seek and read.
        std::thread::sleep(std::time::Duration::from_millis(5));
        eval_ok(&mut engine, r#"
            globalThis.frameTime = document.timeline.currentTime;
            animation.currentTime = 500;
            globalThis.seekResult = animation.currentTime;
        "#);
        let checks = eval_ok(&mut engine,
            "document.timeline.currentTime === frameTime && seekResult === 500 && animation.currentTime === 500");
        if !matches!(checks, Value::Bool(true)) {
            let diagnostics = eval_ok(&mut engine,
                "JSON.stringify({frameTime, timeline: document.timeline.currentTime, seekResult, currentTime: animation.currentTime})");
            let diagnostics = engine.ctx().coerce_string(&diagnostics).ok().expect("diagnostics string");
            panic!("running currentTime seek regression: {diagnostics}");
        }
        eval_ok(&mut engine, "animation.playbackRate=2;");
        assert!(matches!(eval_ok(&mut engine,
            "animation.currentTime===500 && animation.effect.getComputedTiming().localTime===500 && getComputedStyle(document.getElementById('target')).opacity==='0.5'"), Value::Bool(true)));
        let frame = _realm.timeline_sample.get();
        advance(engine.ctx(), frame + 25.0).unwrap();
        let time = eval_ok(&mut engine, "animation.currentTime");
        // Subtracting fractional document timestamps may round the elapsed
        // result by a few ulps; retain a tolerance far below the clock quantum.
        assert!(matches!(time, Value::Num(value) if (value - 550.0).abs() < 1e-7), "advanced animation time: {}", engine.ctx().coerce_string(&time).ok().expect("time diagnostic"));
        eval_ok(&mut engine, "animation.playbackRate=-1;");
        advance(engine.ctx(), frame + 75.0).unwrap();
        assert!(matches!(eval_ok(&mut engine, "Math.abs(animation.currentTime-500)<1e-7"), Value::Bool(true)));
        eval_ok(&mut engine, "animation.playbackRate=0;");
        advance(engine.ctx(), frame + 125.0).unwrap();
        assert!(matches!(eval_ok(&mut engine, "Math.abs(animation.currentTime-500)<1e-7 && Math.abs(animation.effect.getComputedTiming().localTime-500)<1e-7"), Value::Bool(true)));
    }

    #[test]
    fn adopted_animation_preserves_timeline_clock_and_inactive_owner_is_unresolved() {
        let mut engine = Engine::new();
        let source = crate::install(engine.ctx(), "<div id='target'></div>", 64).unwrap();
        eval_ok(&mut engine, r#"
            globalThis.target=document.getElementById('target');
            globalThis.animation=target.animate([{opacity:0},{opacity:1}],{duration:1000,fill:'both'});
            globalThis.otherDocument=document.implementation.createHTMLDocument('destination');
        "#);
        let frame = source.timeline_sample.get();
        advance(engine.ctx(), frame).unwrap();
        while engine.run_one_job() {}
        eval_ok(&mut engine, "otherDocument.adoptNode(target);");
        let state = hub(engine.ctx()).unwrap();
        let destination = state.borrow().records.values().next().unwrap().realm.upgrade().unwrap();
        let destination_time = destination.timeline_sample.get();
        advance(engine.ctx(), frame + 250.0).unwrap();
        assert_eq!(destination.timeline_sample.get(), destination_time,
            "applying an adopted target must not manufacture its document frame");
        let checks = eval_ok(&mut engine,
            "Math.abs(animation.currentTime-250)<1e-7 && Math.abs(animation.effect.getComputedTiming().localTime-250)<1e-7 && getComputedStyle(target).opacity==='0.25' && target.ownerDocument===otherDocument");
        assert!(matches!(checks, Value::Bool(true)), "adopted animation diagnostic: {}", {
            let value = eval_ok(&mut engine, "JSON.stringify({time:animation.currentTime,local:animation.effect.getComputedTiming().localTime,opacity:getComputedStyle(target).opacity,owner:target.ownerDocument===otherDocument})");
            engine.ctx().coerce_string(&value).ok().expect("adoption diagnostic")
        });

        // Use the same active-document replacement primitive as navigation,
        // keeping the old document and animation alive through author roots.
        let context = source.browsing_context().unwrap();
        browsing_context::bind_context_document(&context, &destination);
        assert!(matches!(eval_ok(&mut engine,
            "document.timeline.currentTime===null && animation.currentTime===null && animation.playState==='running' && animation.effect.getComputedTiming().localTime===null"), Value::Bool(true)));
        advance(engine.ctx(), frame + 500.0).unwrap();
        assert_eq!(source.timeline_sample.get(), frame + 250.0,
            "inactive document must not commit a new timeline sample");
        eval_ok(&mut engine, "animation.currentTime=125;animation.pause();");
        advance(engine.ctx(), frame + 750.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,
            "animation.currentTime===125 && animation.pending && animation.startTime===null"), Value::Bool(true)));
    }

    #[test]
    fn pending_animation_start_time_seek_completes_ready_and_updates_actual_style() {
        let mut engine=Engine::new();
        let _realm=crate::install(engine.ctx(),"<style>@keyframes a{from{opacity:0}to{opacity:1}}</style><div id='target' style='animation:a 100s linear'></div>",64).unwrap();
        eval_ok(&mut engine,r#"
            globalThis.target=document.getElementById('target');globalThis.animation=target.getAnimations()[0];
            globalThis.originalReady=animation.ready;globalThis.readyCount=0;originalReady.then(()=>readyCount++);
            animation.startTime=document.timeline.currentTime-50000;
        "#);
        while engine.run_one_job() {}
        let checks = eval_ok(&mut engine,"!animation.pending && originalReady===animation.ready && readyCount===1 && Math.abs(animation.currentTime-50000)<20 && Math.abs(parseFloat(getComputedStyle(target).opacity)-0.5)<0.01");
        if !matches!(checks,Value::Bool(true)) {
            let diagnostics = eval_ok(&mut engine,"JSON.stringify({pending:animation.pending,sameReady:originalReady===animation.ready,readyCount,time:animation.currentTime,opacity:getComputedStyle(target).opacity})");
            let diagnostics = engine.ctx().coerce_string(&diagnostics).ok().expect("diagnostics string");
            panic!("startTime seek regression: {diagnostics}");
        }
        eval_ok(&mut engine,"animation.startTime=null;globalThis.held=animation.currentTime;");
        assert!(matches!(eval_ok(&mut engine,"!animation.pending && animation.playState==='paused' && animation.startTime===null && animation.currentTime===held && originalReady===animation.ready"),Value::Bool(true)));
    }

    #[test]
    fn pending_animation_inactive_timeline_and_signed_rate_readiness_are_truthful() {
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<div id='target'></div>",64).unwrap();
        eval_ok(&mut engine,r#"
            globalThis.target=document.getElementById('target');globalThis.animation=target.animate([],1000);
            globalThis.originalReady=animation.ready;globalThis.resolved=0;originalReady.then(()=>resolved++);
            animation.timeline=null;
        "#);
        advance(engine.ctx(),lumen_host::perf::now_ms()).unwrap();
        while engine.run_one_job() {}
        assert!(matches!(eval_ok(&mut engine,"animation.pending && animation.currentTime===0 && animation.startTime===null && animation.ready===originalReady && resolved===0"),Value::Bool(true)));
        eval_ok(&mut engine,"animation.timeline=document.timeline;");
        advance(engine.ctx(),lumen_host::perf::now_ms()).unwrap();
        while engine.run_one_job() {}
        assert!(matches!(eval_ok(&mut engine,"!animation.pending && resolved===1 && originalReady===animation.ready"),Value::Bool(true)));
        eval_ok(&mut engine,"animation.cancel();animation.playbackRate=-1;animation.pause();globalThis.pauseReady=animation.ready;animation.pause();");
        assert!(matches!(eval_ok(&mut engine,"animation.pending && animation.currentTime===1000 && animation.ready===pauseReady"),Value::Bool(true)));
        advance(engine.ctx(),lumen_host::perf::now_ms()).unwrap();
        assert!(matches!(eval_ok(&mut engine,"!animation.pending && animation.playState==='paused' && animation.currentTime===1000 && animation.startTime===null"),Value::Bool(true)));
        eval_ok(&mut engine,r#"
            globalThis.infinite=target.animate([],{duration:Infinity});infinite.cancel();infinite.playbackRate=-1;
            globalThis.infiniteReady=infinite.ready;globalThis.invalidPause=false;
            try{infinite.pause();}catch(e){invalidPause=e.name==='InvalidStateError';}
            globalThis.zero=target.animate([{opacity:0},{opacity:1}],{duration:100,fill:'both'});zero.playbackRate=0;globalThis.zeroReady=zero.ready;
        "#);
        assert!(matches!(eval_ok(&mut engine,"invalidPause && !infinite.pending && infinite.playState==='idle' && infiniteReady===infinite.ready && zero.pending && zero.currentTime===0"),Value::Bool(true)));
        advance(engine.ctx(),lumen_host::perf::now_ms()).unwrap();
        advance(engine.ctx(),lumen_host::perf::now_ms()+1000.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,"!zero.pending && zero.currentTime===0 && zero.startTime!==null && zeroReady===zero.ready"),Value::Bool(true)));
        eval_ok(&mut engine,"zero.playbackRate=1;zero.id='resumed-zero-rate';");
        let state=hub(engine.ctx()).unwrap();
        let resumed=state.borrow().records.values().find(|record|record.public_id=="resumed-zero-rate").unwrap().start_ms;
        advance(engine.ctx(),resumed+50.0).unwrap();
        realm.with_session(|session|{
            let document=session.document();
            let target=selector::query_selector(document,document.root(),"#target").unwrap().unwrap();
            assert!((session.computed_style(target).unwrap().opacity-0.5).abs()<0.001);
        });
    }

    #[test]
    fn pending_animation_initial_css_time_and_ready_settle_at_real_frame() {
        let mut engine=Engine::new();
        let _realm=crate::install(engine.ctx(),"<style>@keyframes a{}</style><div id='target' style='animation:a 100s'></div>",64).unwrap();
        eval_ok(&mut engine,"globalThis.animation=document.getElementById('target').getAnimations()[0];globalThis.initialReady=animation.ready;globalThis.readyCount=0;initialReady.then(()=>readyCount++);");
        assert!(matches!(eval_ok(&mut engine,"animation.pending===true && animation.currentTime===0 && animation.startTime===null && animation.timeline===document.timeline && initialReady===animation.ready && readyCount===0"),Value::Bool(true)));
        advance(engine.ctx(),lumen_host::perf::now_ms()).unwrap();
        while engine.run_one_job() {}
        assert!(matches!(eval_ok(&mut engine,"animation.pending===false && animation.startTime!==null && initialReady===animation.ready && readyCount===1"),Value::Bool(true)));
    }

    #[test]
    fn pending_animation_play_pause_switch_reuses_ready_and_seek_completes_pause() {
        let mut engine=Engine::new();
        let _realm=crate::install(engine.ctx(),"<div id='target'></div>",64).unwrap();
        eval_ok(&mut engine,"globalThis.animation=document.getElementById('target').animate([],100000);globalThis.originalReady=animation.ready;animation.pause();globalThis.pauseReady=animation.ready;animation.pause();animation.play();");
        assert!(matches!(eval_ok(&mut engine,"animation.pending===true && originalReady===pauseReady && pauseReady===animation.ready && animation.playState==='running' && animation.currentTime===0"),Value::Bool(true)));
        advance(engine.ctx(),lumen_host::perf::now_ms()).unwrap();
        eval_ok(&mut engine,"animation.pause();globalThis.nextReady=animation.ready;animation.pause();");
        assert!(matches!(eval_ok(&mut engine,"animation.pending===true && nextReady!==originalReady && nextReady===animation.ready && animation.playState==='paused'"),Value::Bool(true)));
        eval_ok(&mut engine,"animation.currentTime=400;");
        while engine.run_one_job() {}
        assert!(matches!(eval_ok(&mut engine,"animation.pending===false && animation.currentTime===400 && animation.startTime===null && nextReady===animation.ready && animation.playState==='paused'"),Value::Bool(true)));
    }

    #[test]
    fn pending_animation_cancel_unsampled_replay_rejects_ready_without_extra_css_cancel() {
        let mut engine=Engine::new();
        let _realm=crate::install(engine.ctx(),"<style>@keyframes a{}</style><div id='target' style='animation:a 100s'></div>",64).unwrap();
        eval_ok(&mut engine,"globalThis.target=document.getElementById('target');globalThis.animation=target.getAnimations()[0];globalThis.cancels=0;target.addEventListener('animationcancel',()=>cancels++);");
        advance(engine.ctx(),lumen_host::perf::now_ms()).unwrap();
        eval_ok(&mut engine,"animation.cancel();animation.play();globalThis.pendingReady=animation.ready;globalThis.rejected=0;pendingReady.catch(e=>{if(e.name==='AbortError')rejected++;});animation.cancel();globalThis.replacementReady=animation.ready;");
        while engine.run_one_job() {}
        assert!(matches!(eval_ok(&mut engine,"animation.pending===false && animation.playState==='idle' && animation.currentTime===null && pendingReady!==replacementReady && rejected===1"),Value::Bool(true)));
        assert!(crate::scheduling::run_tasks(&mut engine,32).is_empty());
        advance(engine.ctx(),lumen_host::perf::now_ms()).unwrap();
        assert!(matches!(eval_ok(&mut engine,"cancels===1 && animation.ready===replacementReady"),Value::Bool(true)));
    }

    #[test]
    fn css_animation_event_automatic_replacement_cancels_before_new_start_without_author_task_drain() {
        let mut engine=Engine::new();
        let _realm=crate::install(engine.ctx(),
            "<style>@keyframes a{}@keyframes b{}@keyframes c{}</style><div id='target' style='animation:a 100s paused,b 100s paused'></div>",64).unwrap();
        eval_ok(&mut engine,r#"
            globalThis.target=document.getElementById('target');globalThis.events=[];globalThis.authorTasks=0;
            target.getAnimations();
            for(const kind of ['animationstart','animationcancel'])target.addEventListener(kind,e=>events.push([e.type,e.animationName]));
        "#);
        advance(engine.ctx(),lumen_host::perf::now_ms()).unwrap();
        eval_ok(&mut engine,"events.length=0;target.style.animation='c 100s paused,b 100s paused';");
        crate::scheduling::queue_task(engine.ctx(),|ctx| {
            let global=ctx.global_object();
            ctx.member_set(&global,"authorTasks",Value::Num(1.0)).map_err(OpError::thrown)?;Ok(())
        }).unwrap();
        advance(engine.ctx(),lumen_host::perf::now_ms()).unwrap();
        assert!(matches!(eval_ok(&mut engine,r#"JSON.stringify(events)==='[["animationcancel","a"],["animationstart","c"]]' && authorTasks===0"#),Value::Bool(true)));
        assert!(crate::scheduling::task_pending(engine.ctx()));
        assert!(crate::scheduling::run_tasks(&mut engine,32).is_empty());
        assert!(matches!(eval_ok(&mut engine,"events.length===2 && authorTasks===1"),Value::Bool(true)));
    }

    #[test]
    fn css_animation_event_frame_flush_keeps_listener_cancellation_for_next_task() {
        let mut engine=Engine::new();
        let _realm=crate::install(engine.ctx(),
            "<style>@keyframes a{}@keyframes b{}</style><div id='first' style='animation:a 100s paused'></div><div id='second' style='animation:b 100s paused'></div>",64).unwrap();
        eval_ok(&mut engine,r#"
            globalThis.first=document.getElementById('first');globalThis.second=document.getElementById('second');
            first.getAnimations();globalThis.otherAnimation=second.getAnimations()[0];globalThis.cancels=[];
            first.addEventListener('animationcancel',e=>{cancels.push(e.animationName);otherAnimation.cancel();});
            second.addEventListener('animationcancel',e=>cancels.push(e.animationName));
        "#);
        advance(engine.ctx(),lumen_host::perf::now_ms()).unwrap();
        eval_ok(&mut engine,"first.style.animation='none';");
        advance(engine.ctx(),lumen_host::perf::now_ms()).unwrap();
        assert!(matches!(eval_ok(&mut engine,r#"JSON.stringify(cancels)==='["a"]'"#),Value::Bool(true)));
        assert!(crate::scheduling::task_pending(engine.ctx()));
        assert!(crate::scheduling::run_tasks(&mut engine,32).is_empty());
        assert!(matches!(eval_ok(&mut engine,r#"JSON.stringify(cancels)==='["a","b"]'"#),Value::Bool(true)));
    }

    #[test]
    fn css_animation_event_backward_seek_preserves_start_end_pair_order() {
        let mut engine=Engine::new();
        let _realm=crate::install(engine.ctx(),
            "<style>@keyframes empty{}</style><div id='target' style='animation:empty 100s 100s paused'></div>",64).unwrap();
        eval_ok(&mut engine,r#"
            globalThis.target=document.getElementById('target');globalThis.animation=target.getAnimations()[0];
            globalThis.events=[];
            for(const kind of ['animationstart','animationend'])target.addEventListener(kind,e=>events.push([e.type,e.elapsedTime]));
            animation.finish();
        "#);
        advance(engine.ctx(),lumen_host::perf::now_ms()).unwrap();
        eval_ok(&mut engine,"events=[];animation.currentTime=0;");
        advance(engine.ctx(),lumen_host::perf::now_ms()).unwrap();
        assert!(matches!(eval_ok(&mut engine,r#"JSON.stringify(events)==='[["animationstart",100],["animationend",0]]'"#),Value::Bool(true)));
    }

    #[test]
    fn css_animation_event_rapid_cancellations_preserve_each_real_transition() {
        let mut engine=Engine::new();
        let _realm=crate::install(engine.ctx(),
            "<style>@keyframes empty{}</style><div id='target' style='animation:empty 100s paused'></div>",64).unwrap();
        eval_ok(&mut engine,r#"
            globalThis.target=document.getElementById('target');globalThis.animation=target.getAnimations()[0];
            globalThis.events=[];globalThis.aborted=0;
            for(const kind of ['animationstart','animationcancel'])target.addEventListener(kind,e=>events.push(e.type));
            animation.finished.catch(e=>{if(e.name==='AbortError')aborted++;});
        "#);
        advance(engine.ctx(),lumen_host::perf::now_ms()).unwrap();
        eval_ok(&mut engine,r#"
            animation.cancel();globalThis.firstPending=animation.finished;
            firstPending.catch(e=>{if(e.name==='AbortError')aborted++;});
            animation.play();
        "#);
        assert!(crate::scheduling::run_tasks(&mut engine,32).is_empty());
        advance(engine.ctx(),lumen_host::perf::now_ms()).unwrap();
        // A second genuine cancellation follows a sampled replay, not an
        // aborted pending-play task that never reaches an active CSS phase.
        eval_ok(&mut engine,"animation.cancel();globalThis.lastPending=animation.finished;animation.cancel();animation.play();");
        assert!(matches!(eval_ok(&mut engine,"firstPending!==lastPending && lastPending===animation.finished"),Value::Bool(true)));
        assert!(crate::scheduling::run_tasks(&mut engine,32).is_empty());
        advance(engine.ctx(),lumen_host::perf::now_ms()).unwrap();
        while engine.run_one_job() {}
        assert!(matches!(eval_ok(&mut engine,r#"JSON.stringify(events)==='["animationstart","animationcancel","animationstart","animationcancel","animationstart"]' && aborted===2 && animation.finished===lastPending"#),Value::Bool(true)));
    }

    #[test]
    fn css_animation_event_queued_sibling_resolves_identity_after_listener_adoption() {
        let mut engine=Engine::new();
        let _realm=crate::install(engine.ctx(),
            "<style>@keyframes a{}@keyframes b{}</style><div id='first' style='animation:a 100s paused'></div><div id='second' style='animation:b 100s paused'></div>",64).unwrap();
        let global=engine.ctx().global_object();
        assert!(engine.ctx().install_module::<event_test_gc::Module>(&global).is_ok());
        eval_ok(&mut engine,r#"
            globalThis.first=document.getElementById('first');globalThis.second=document.getElementById('second');
            first.getAnimations();second.getAnimations();globalThis.received=[];
            globalThis.otherDocument=document.implementation.createHTMLDocument('destination');
            first.addEventListener('animationstart',()=>{
                otherDocument.adoptNode(second);document.getAnimations();forceEventGC();
            });
            second.addEventListener('animationstart',e=>received.push([e.animationName,e.target===second,e.target.ownerDocument===otherDocument]));
        "#);
        advance(engine.ctx(),lumen_host::perf::now_ms()).unwrap();
        assert!(matches!(eval_ok(&mut engine,"received.length===1 && received[0][0]==='b' && received[0][1] && received[0][2]"),Value::Bool(true)));
    }

    #[test]
    fn css_animation_event_listener_cancellation_waits_for_its_own_task_turn() {
        let mut engine=Engine::new();
        let _realm=crate::install(engine.ctx(),
            "<style>@keyframes empty{}</style><div id='target' style='animation:empty 100s paused'></div><div id='sibling' style='animation:empty 100s paused'></div>",64).unwrap();
        eval_ok(&mut engine,r#"
            globalThis.target=document.getElementById('target');globalThis.animation=target.getAnimations()[0];
            globalThis.sibling=document.getElementById('sibling');globalThis.other=sibling.getAnimations()[0];
            globalThis.cancels=0;
            target.addEventListener('animationcancel',()=>{
                cancels++;other.cancel();
            });
            sibling.addEventListener('animationcancel',()=>cancels++);
        "#);
        advance(engine.ctx(),lumen_host::perf::now_ms()).unwrap();
        eval_ok(&mut engine,"animation.cancel();");
        assert!(crate::scheduling::run_tasks(&mut engine,32).is_empty());
        assert!(matches!(eval_ok(&mut engine,"cancels===1"),Value::Bool(true)));
        assert!(crate::scheduling::task_pending(engine.ctx()));
        assert!(crate::scheduling::run_tasks(&mut engine,32).is_empty());
        assert!(matches!(eval_ok(&mut engine,"cancels===2"),Value::Bool(true)));
        assert!(!crate::scheduling::task_pending(engine.ctx()));
    }

    #[test]
    fn css_animation_event_task_admission_failure_preserves_author_state() {
        let mut engine=Engine::new();
        let _realm=crate::install(engine.ctx(),
            "<style>@keyframes empty{}</style><div id='target' style='animation:empty 100s paused'></div>",64).unwrap();
        eval_ok(&mut engine,"globalThis.animation=document.getElementById('target').getAnimations()[0];globalThis.promise=animation.finished;");
        advance(engine.ctx(),lumen_host::perf::now_ms()).unwrap();
        for _ in 0..crate::scheduling::MAX_PENDING_HTML_TASKS {
            crate::scheduling::queue_task(engine.ctx(),|_|Ok(())).unwrap();
        }
        assert!(matches!(eval_ok(&mut engine,r#"
            (()=>{try{animation.cancel();return false;}catch(e){return e.name==='QuotaExceededError';}})()
                && animation.playState==='paused' && animation.finished===promise
        "#),Value::Bool(true)));
        let state=hub(engine.ctx()).unwrap();
        assert!(state.borrow().dispatching_css.is_empty());
        assert!(state.borrow().css_cancellations.is_empty());
        while crate::scheduling::task_pending(engine.ctx()) {
            assert!(crate::scheduling::run_tasks(&mut engine,256).is_empty());
        }
    }

    #[test]
    fn css_animation_event_retired_child_tasks_release_snapshots_and_keep_parent_work() {
        let mut engine=Engine::new();
        let _parent=crate::install(engine.ctx(),
            "<style>@keyframes empty{}</style><div id='target' style='animation:empty 100s paused'></div>",64).unwrap();
        eval_ok(&mut engine,"globalThis.animation=document.getElementById('target').getAnimations()[0];globalThis.cancels=0;document.getElementById('target').addEventListener('animationcancel',()=>cancels++);");
        advance(engine.ctx(),lumen_host::perf::now_ms()).unwrap();
        let child=engine.ctx().create_host_realm();
        let _child_realm=engine.ctx().with_host_realm(&child,|ctx| crate::install(ctx,
            "<style>@keyframes empty{}</style><div id='target' style='animation:empty 100s paused'></div>",64).unwrap()).unwrap();
        assert!(engine.eval_value_in_host_realm(&child,
            "globalThis.animation=document.getElementById('target').getAnimations()[0];globalThis.cancels=0;document.getElementById('target').addEventListener('animationcancel',()=>cancels++);",false).unwrap().is_ok());
        engine.ctx().with_host_realm(&child,|ctx|advance(ctx,lumen_host::perf::now_ms()).unwrap()).unwrap();
        assert!(engine.eval_value_in_host_realm(&child,"animation.cancel();",false).unwrap().is_ok());
        let child_hub=engine.ctx().with_host_realm(&child,|ctx|hub(ctx).unwrap()).unwrap();
        assert_eq!(child_hub.borrow().dispatching_css.len(),1);
        eval_ok(&mut engine,"animation.cancel();");
        assert_eq!(crate::scheduling::cancel_tasks_for_realm(engine.ctx(),&child),1);
        assert!(child_hub.borrow().dispatching_css.is_empty());
        assert!(crate::scheduling::run_tasks(&mut engine,32).is_empty());
        assert!(matches!(eval_ok(&mut engine,"cancels===1"),Value::Bool(true)));
        let child_count=engine.eval_value_in_host_realm(&child,"cancels===0",false).unwrap().ok().unwrap();
        assert!(matches!(child_count,Value::Bool(true)));
    }

    #[test]
    fn css_animation_event_sampling_keeps_parent_and_child_realms_independent() {
        let mut engine=Engine::new();
        let _parent=crate::install(engine.ctx(),
            "<style>@keyframes parent{}</style><div id='target' style='animation:parent 100s paused'></div>",64).unwrap();
        eval_ok(&mut engine,"globalThis.count=0;document.getElementById('target').addEventListener('animationstart',e=>{if(e instanceof AnimationEvent && e.animationName==='parent')count++;});");
        let child=engine.ctx().create_host_realm();
        let _child_realm=engine.ctx().with_host_realm(&child,|ctx| crate::install(ctx,
            "<style>@keyframes child{}</style><div id='target' style='animation:child 100s paused'></div>",64).unwrap()).unwrap();
        assert!(engine.eval_value_in_host_realm(&child,
            "globalThis.count=0;document.getElementById('target').addEventListener('animationstart',e=>{if(e instanceof AnimationEvent && e.animationName==='child' && e.target.ownerDocument===document)count++;});",false).unwrap().is_ok());
        advance(engine.ctx(),lumen_host::perf::now_ms()).unwrap();
        assert!(matches!(eval_ok(&mut engine,"count===1"),Value::Bool(true)));
        let child_count=engine.eval_value_in_host_realm(&child,"count===0",false).unwrap().ok().unwrap();
        assert!(matches!(child_count,Value::Bool(true)));
        engine.ctx().with_host_realm(&child,|ctx| advance(ctx,lumen_host::perf::now_ms()).unwrap()).unwrap();
        let child_count=engine.eval_value_in_host_realm(&child,"count===1",false).unwrap().ok().unwrap();
        assert!(matches!(child_count,Value::Bool(true)));
        assert!(matches!(eval_ok(&mut engine,"count===1"),Value::Bool(true)));
        assert!(!pending(engine.ctx()));
    }

    #[test]
    fn css_animation_event_init_rejects_nonfinite_and_preserves_conversion_errors() {
        let mut engine=Engine::new();
        let _realm=crate::install(engine.ctx(),"<div></div>",32).unwrap();
        assert!(matches!(eval_ok(&mut engine,r#"
            (()=>{
                let rejected=0;
                for(const value of [NaN,Infinity,-Infinity]) {
                    try { new AnimationEvent('test',{elapsedTime:value}); }
                    catch(e) { if(e instanceof TypeError)rejected++; }
                }
                const sentinel={};let preserved=false;
                try { new AnimationEvent('test',{elapsedTime:{valueOf(){throw sentinel;}}}); }
                catch(e) { preserved=e===sentinel; }
                return rejected===3 && preserved;
            })()
        "#),Value::Bool(true)));
    }

    #[test]
    fn css_animation_event_queue_retains_cancelled_sibling_during_listener_gc() {
        let mut engine=Engine::new();
        let _realm=crate::install(engine.ctx(),
            "<style>@keyframes a{}@keyframes b{}</style><div id='first' style='animation:a 100s paused'></div><div id='second' style='animation:b 100s paused'></div>",64).unwrap();
        let global=engine.ctx().global_object();
        assert!(engine.ctx().install_module::<event_test_gc::Module>(&global).is_ok());
        eval_ok(&mut engine,r#"
            globalThis.first=document.getElementById('first'); globalThis.second=document.getElementById('second');
            first.getAnimations();second.getAnimations();globalThis.received=[];
            first.addEventListener('animationstart',()=>{
                second.style.animation='none'; second.getAnimations(); document.getAnimations();
                forceEventGC();
            });
            second.addEventListener('animationstart',e=>received.push([e.animationName,e.animation.animationName,e.target===second]));
        "#);
        let state=hub(engine.ctx()).unwrap();
        let start=state.borrow().records.values().map(|record|record.start_ms).fold(0.0,f64::max);
        advance(engine.ctx(),start).unwrap();
        assert!(matches!(eval_ok(&mut engine,"received.length===1 && received[0][0]==='b' && received[0][1]==='b' && received[0][2]"),Value::Bool(true)));
        assert!(crate::scheduling::task_pending(engine.ctx()), "listener cancellation remains admitted for a later task turn");
        assert!(crate::scheduling::run_tasks(&mut engine,32).is_empty());
        assert!(state.borrow().dispatching_css.is_empty());
        advance(engine.ctx(),start+20.0).unwrap();
    }

    #[test]
    fn css_animation_event_class_and_real_negative_delay_lifecycle() {
        let mut engine = Engine::new();
        let _realm = crate::install(engine.ctx(),
            "<style>@keyframes empty{}</style><div id='parent'><div id='target' style='animation:empty 1s -4s 7 paused'></div></div>",64).unwrap();
        eval_ok(&mut engine,r#"
            globalThis.target=document.getElementById('target');
            globalThis.parent=document.getElementById('parent');
            globalThis.animation=target.getAnimations()[0];
            globalThis.events=[]; globalThis.bubbled=0;
            for(const name of ['animationstart','animationiteration','animationend','animationcancel'])
                target.addEventListener(name,e=>events.push([e.type,e.elapsedTime,e.animationName,
                    e.animation===animation,e instanceof AnimationEvent,e instanceof Event,e.isTrusted,e.bubbles,e.cancelable]));
            parent.addEventListener('animationstart',()=>bubbled++);
            const init=new AnimationEvent('sample',{animationName:null,elapsedTime:'1.5',pseudoElement:'::before',bubbles:true});
            globalThis.constructorOK=init.animationName==='null' && init.elapsedTime===1.5
                && init.pseudoElement==='::before' && !init.isTrusted && init.animation===null;
        "#);
        assert!(pending(engine.ctx()));
        let state=hub(engine.ctx()).unwrap();
        let start=state.borrow().records.values().next().unwrap().start_ms;
        advance(engine.ctx(),start).unwrap();
        assert!(!pending(engine.ctx()),"paused sampled effects must not continuously render");
        assert!(matches!(eval_ok(&mut engine,r#"constructorOK && bubbled===1 && JSON.stringify(events)==='[["animationstart",4,"empty",true,true,true,true,true,false]]'"#),Value::Bool(true)));
        eval_ok(&mut engine,"target.style.animationPlayState='running';target.getAnimations();");
        advance(engine.ctx(),lumen_host::perf::now_ms()).unwrap();
        let resume=state.borrow().records.values().next().unwrap().start_ms;
        advance(engine.ctx(),resume+1100.0).unwrap();
        advance(engine.ctx(),resume+2100.0).unwrap();
        advance(engine.ctx(),resume+3100.0).unwrap();
        advance(engine.ctx(),resume+4100.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,r#"JSON.stringify(events.map(e=>[e[0],e[1]]))==='[["animationstart",4],["animationiteration",5],["animationiteration",6],["animationend",7]]'"#),Value::Bool(true)));
    }

    #[test]
    fn css_animation_event_cancel_is_queued_and_owner_survives_effect_retargeting() {
        let mut engine = Engine::new();
        let _realm=crate::install(engine.ctx(),
            "<style>@keyframes empty{}</style><div id='target' style='animation:empty 100s paused'></div><div id='other'></div>",64).unwrap();
        eval_ok(&mut engine,r#"
            globalThis.target=document.getElementById('target'); globalThis.other=document.getElementById('other');
            globalThis.animation=target.getAnimations()[0]; globalThis.events=[];globalThis.wrong=0;
            for(const kind of ['animationstart','animationcancel']) {
                target.addEventListener(kind,e=>events.push([e.type,e.elapsedTime,e.target===target,e.animation===animation]));
                other.addEventListener(kind,()=>wrong++);
            }
            animation.finished.catch(()=>{});
            animation.effect.target=other;
        "#);
        let state=hub(engine.ctx()).unwrap();
        let start=state.borrow().records.values().next().unwrap().start_ms;
        advance(engine.ctx(),start).unwrap();
        eval_ok(&mut engine,"animation.currentTime=1234;animation.cancel();globalThis.newFinished=animation.finished;");
        assert!(matches!(eval_ok(&mut engine,"events.length===1"),Value::Bool(true)),"CSS cancel must not synchronously dispatch");
        assert!(crate::scheduling::run_tasks(&mut engine,32).is_empty());
        advance(engine.ctx(),start+2000.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,"wrong===0 && events.length===2 && events[1][0]==='animationcancel' && events[1][1]===1.234 && events[1][2] && events[1][3]"),Value::Bool(true)));
        eval_ok(&mut engine,"animation.cancel();target.style.animation='none';target.getAnimations();animation.play();");
        advance(engine.ctx(),start+3000.0).unwrap();
        assert!(matches!(eval_ok(&mut engine,"events.length===2 && wrong===0 && animation.finished===newFinished"),Value::Bool(true)),"orphan replay and idle retirement cannot restart CSS events");
    }

    #[test]
    fn empty_css_keyframes_do_not_accept_invalid_keyframe_syntax() {
        let mut rule = lumen_html::css::parse_keyframes_rule("@keyframes example{to{opacity:1}}")
            .unwrap().unwrap();
        rule.rules[0].key_text="not-a-keyframe-offset".into();
        assert!(css_keyframe_effect(&rule, Direction::Normal, CompositeMode::Replace).is_none());
        let empty = lumen_html::css::parse_keyframes_rule("@keyframes empty{}")
            .unwrap().unwrap();
        assert!(css_keyframe_effect(&empty, Direction::Normal, CompositeMode::Replace)
            .is_some_and(|frames| frames.is_empty()));
    }

    #[test]
    fn specification_css_keyframes_use_local_easing_and_canonical_declaration_cascade() {
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),
            "<style>@keyframes specification {from {width:0px;width:20px!important;margin:2px 4px;opacity:.25;animation-timing-function:linear;animation-timing-function:steps(2,start);animation-duration:9s;animation-name:wrong}to {width:100px;margin:10px 12px;opacity:.75;animation-timing-function:steps(1,start)}}</style><div id=target style='animation:1000ms linear both specification'></div>",96).unwrap();
        advance(engine.ctx(),lumen_host::perf::now_ms()).unwrap();
        let state=hub(engine.ctx()).unwrap();
        let record=state.borrow().records.values().find(|record|
            record.css_name.as_deref()==Some("specification")).unwrap().clone();
        assert_eq!(record.timing.duration_ms,1000.0,"keyframe animation controls never replace the element's timing");
        assert!(record.keyframes.iter().all(|frame|frame.declarations.iter().all(|(name,_)|
            !name.starts_with("animation-"))),"keyframe easing is metadata rather than an animated property");
        for (fraction,width,top,side,opacity) in [(0.25,50.0,6.0,8.0,0.5),(0.75,100.0,10.0,12.0,0.75)] {
            advance(engine.ctx(),record.start_ms+fraction*1000.0).unwrap();
            realm.with_session(|session| {
                let document=session.document();
                let node=selector::query_selector(document,document.root(),"#target").unwrap().unwrap();
                let style=session.computed_style(node).unwrap();
                assert_eq!(style.width,Some(width),"local keyframe easing overrides the global linear function");
                assert_eq!(style.margin_sides,[top,side,top,side],"shared declaration expansion preserves shorthand tracks");
                assert!((style.opacity-opacity).abs()<0.001);
            });
        }
    }

    #[test]
    fn specification_animation_shorthand_endpoints_and_end_delay_reach_actual_sampler(){
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<style>@keyframes fade{from{opacity:0}to{opacity:1}}@keyframes 'none'{from{opacity:0}to{opacity:1}}</style><div id=target style='font-size:24px;animation:fade calc(2 * 1s) linear calc(-.25s) 2 both;animation-delay-end:calc(.5s)'></div><div style='animation-name:&quot;none&quot;;animation-duration:2s'></div>",64).unwrap();
        advance(engine.ctx(),lumen_host::perf::now_ms()).unwrap();
        {
            let state=hub(engine.ctx()).unwrap();let state=state.borrow();
            let record=state.records.values().find(|record|record.css_name.as_deref()==Some("fade")).unwrap();
            assert_eq!(record.timing.duration_ms,2000.0);
            assert_eq!(record.timing.delay_ms,-250.0);
            assert_eq!(record.timing.end_delay_ms,500.0);
            assert_eq!(record.timing.iterations,2.0);
            assert!(state.records.values().any(|record|record.css_name.as_deref()==Some("none")),"quoted none creates a real keyframe effect");
        }
        assert!(matches!(eval_ok(&mut engine,r#"
            (()=>{
                const s=document.getElementById('target').style;
                s.animationRange='entry 20px exit calc(100% - 10px)';
                if(s.animationRangeStart!=='entry 20px'||s.animationRangeEnd!=='exit calc(100% - 10px)')throw Error('range endpoints');
                s.animationComposition='add';if(s.animation!=='')throw Error('reset-only composition serialized');
                s.animation='fade auto linear view(inline 10px 20px)';
                if(s.animationComposition!=='replace'||s.animationRangeStart!=='normal'||s.animationRangeEnd!=='normal'||s.animationDelayEnd!=='0s')throw Error('reset-only values');
                if(s.animationTimeline!=='view(inline 10px 20px)')throw Error('timeline shorthand value');
                const text=s.cssText;s.cssText=text;
                if(s.animationTimeline!=='view(inline 10px 20px)')throw Error('declaration roundtrip');
                return true;
            })()
        "#),Value::Bool(true)));
        let snapshot=realm.session.borrow_mut().animation_snapshot().unwrap();
        let target=selector::query_selector(realm.session.borrow().document(),realm.session.borrow().document().root(),"#target").unwrap().unwrap();
        let binding=resolve_named_progress_timeline(&realm,&snapshot,target,None,"scroll(self x)",animation::ProgressRange::parse("normal").unwrap()).unwrap();
        assert_eq!(binding.source,Some(target));assert!(binding.horizontal);assert_eq!(binding.subject,None);
        let view=resolve_named_progress_timeline(&realm,&snapshot,target,None,"view(inline 10px 20px)",animation::ProgressRange::parse("normal").unwrap()).unwrap();
        assert_eq!(view.subject,Some(target));assert!(view.horizontal);
        let insets=view.insets.unwrap();assert_eq!(insets[0].used(100.0),Some(10.0));assert_eq!(insets[1].used(100.0),Some(20.0));
    }

    #[test]
    fn css_animation_uses_shared_timeline_sampler_and_get_animations()
 {
        let mut engine = Engine::new();
        let realm = crate::install(
            engine.ctx(),
            "<style>@keyframes fade { from { opacity: 0 } to { opacity: 1 } }</style><div id='target' style='animation: 100ms linear both fade'></div>",
            64,
        ).unwrap();
        let start = lumen_host::perf::now_ms();
        advance(engine.ctx(), start).unwrap();
        let record_start = hub(engine.ctx()).unwrap().borrow().records.values()
            .find(|record| record.css_name.as_deref() == Some("fade"))
            .expect("CSS animation record").start_ms;
        advance(engine.ctx(), record_start + 50.0).unwrap();
        realm.with_session(|session| {
            let document = session.document();
            let node = selector::query_selector(document, document.root(), "#target").unwrap().unwrap();
            assert!((session.computed_style(node).unwrap().opacity - 0.5).abs() < 0.02);
        });
        let animation = eval_ok(&mut engine, "document.getElementById('target').getAnimations()[0]");
        assert!(matches!(animation, Value::Obj(_)));
        assert!(matches!(eval_ok(&mut engine, "document.getElementById('target').getAnimations()[0] instanceof CSSAnimation"), Value::Bool(true)));
        assert!(matches!(eval_ok(&mut engine, "document.getElementById('target').getAnimations()[0].animationName"), Value::Str(ref value) if value.as_ref() == "fade"));
    }

    #[test]
    fn public_animation_samples_style_and_resolves_finished_promise() {
        let mut engine = Engine::new();
        let realm = crate::install(
            engine.ctx(),
            "<div id='target' style='opacity: 0'></div>",
            64,
        )
        .unwrap();
        let result = eval_ok(
            &mut engine,
            "window.anim = document.getElementById('target').animate(\
                 [{opacity: 0, width: '10px'}, {opacity: 1, width: '30px'}], \
                 {duration: 100, fill: 'forwards'}); \
                 window.done = false; window.finishes = 0; \
                 anim.addEventListener('finish', () => window.finishes++); \
                 anim.finished.then(() => window.done = true); \
                 anim instanceof Animation && document.getElementById('target').getAnimations()[0] === anim",
        );
        assert!(matches!(result, Value::Bool(true)));

        // Resolve actual first-frame readiness before manipulating the test clock.
        advance(engine.ctx(),lumen_host::perf::now_ms()).unwrap();
        let hub = hub(engine.ctx()).unwrap();
        let started = {
            let hub = hub.borrow();
            let record = hub.records.values().next().unwrap();
            record.start_ms
        };
        advance(engine.ctx(), started + 50.0).unwrap();
        realm.with_session(|session| {
            let (node, style_attribute) = {
                let document = session.document();
                let node = selector::query_selector(document, document.root(), "#target")
                    .unwrap()
                    .unwrap();
                let style_attribute = match document.kind(node).unwrap() {
                    NodeKind::Element { attributes, .. } => attributes
                        .iter()
                        .find(|(name, _)| name.as_str() == "style")
                        .map(|(_, value)| value.clone()),
                    _ => None,
                };
                (node, style_attribute)
            };
            let style = session.computed_style(node).unwrap();
            assert!((style.opacity - 0.5).abs() < 0.02);
            assert_eq!(style.width, Some(20.0));
            assert_eq!(style_attribute.as_deref(), Some("opacity: 0"));
        });

        let terminal_time = lumen_host::perf::now_ms();
        hub.borrow_mut()
            .records
            .values_mut()
            .next()
            .unwrap()
            .start_ms = terminal_time - 100.0;
        assert!(pending(engine.ctx()));
        advance(engine.ctx(), terminal_time).unwrap();
        assert!(!pending(engine.ctx()));
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(
            engine.eval_value("window.done").unwrap().ok().unwrap(),
            Value::Bool(true)
        ));
        assert!(matches!(
            engine
                .eval_value("anim.playState === 'finished'")
                .unwrap()
                .ok()
                .unwrap(),
            Value::Bool(true)
        ));
        assert!(matches!(
            engine
                .eval_value("window.finishes === 1")
                .unwrap()
                .ok()
                .unwrap(),
            Value::Bool(true)
        ));

        engine
            .eval_value("window.cancelled = false; window.cancels = 0; anim.addEventListener('cancel', () => { window.cancels++; anim.cancel(); }); anim.play(); anim.finished.then(() => {}, () => window.cancelled = true)")
            .unwrap()
            .ok()
            .unwrap();
        engine.eval_value("anim.cancel()").unwrap().ok().unwrap();
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(
            engine
                .eval_value("anim.playState === 'idle' && window.cancelled && window.cancels === 1")
                .unwrap()
                .ok()
                .unwrap(),
            Value::Bool(true)
        ));
        realm.with_session(|session| {
            let document = session.document();
            let node = selector::query_selector(document, document.root(), "#target")
                .unwrap()
                .unwrap();
            let style = session.computed_style(node).unwrap();
            assert_eq!(style.opacity, 0.0);
            assert_eq!(style.width, None);
        });
    }

    #[test]
    fn independent_effect_association_replacement_additive_cascade_and_finish() {
        let mut engine = Engine::new();
        let realm = crate::install(
            engine.ctx(),
            "<div id='target' style='opacity: .2'></div>",
            64,
        )
        .unwrap();
        let result = eval_ok(
            &mut engine,
            "window.target = document.getElementById('target'); \
             window.effectA = new KeyframeEffect(target, {opacity: [.1, .3]}, \
               {duration: 100, fill: 'both', composite: 'add'}); \
             window.effectB = new KeyframeEffect(target, {opacity: [.2, .6]}, \
               {duration: 100, fill: 'both'}); \
             window.effectC = new KeyframeEffect(target, {opacity: [.2, .4]}, \
               {duration: 100, fill: 'both'}); \
             window.timeline = new DocumentTimeline({originTime: 0}); \
             window.timelineAnim = new Animation(effectC, timeline); \
             timelineAnim.timeline = timeline; \
             window.anim = new Animation(effectA); window.finished = false; \
             window.readyIdentityStable = anim.ready === anim.ready; \
             anim.ready.then(value => window.readyIdentity = value === anim); anim.play(); \
             anim.finished.then(value => { window.finished = true; window.finishedIdentity = value === anim; }); \
             [anim.effect === effectA, target.getAnimations().length, \
               target.getAnimations()[0] === anim, anim.playState, \
               timelineAnim.timeline === timeline, window.readyIdentityStable].join(':')",
        );
        let diagnostic = engine
            .ctx()
            .coerce_string(&result)
            .map(|value| value.to_string())
            .unwrap_or_else(|_| "<not a string>".into());
        assert_eq!(
            diagnostic, "true:1:true:running:true:true",
            "association diagnostic: {diagnostic}"
        );

        // Resolve actual first-frame readiness before manipulating the test clock.
        advance(engine.ctx(),lumen_host::perf::now_ms()).unwrap();
        let hub = hub(engine.ctx()).unwrap();
        let animation_id = hub
            .borrow()
            .records
            .values()
            .find(|record| record.composite == CompositeMode::Add)
            .unwrap()
            .id;
        let start = {
            let mut state = hub.borrow_mut();
            let record = state.records.get_mut(&animation_id).unwrap();
            record.start_ms = lumen_host::perf::now_ms() - 50.0;
            record.start_ms
        };
        advance(engine.ctx(), start + 50.0).unwrap();
        realm.with_session(|session| {
            let document = session.document();
            let node = selector::query_selector(document, document.root(), "#target")
                .unwrap()
                .unwrap();
            assert!((session.computed_style(node).unwrap().opacity - 0.4).abs() < 0.02);
        });

        let replaced = eval_ok(
            &mut engine,
            "anim.effect = effectB; anim.effect === effectB && target.getAnimations()[0] === anim",
        );
        assert!(matches!(replaced, Value::Bool(true)));
        let detached = eval_ok(
            &mut engine,
            "anim.effect = null; anim.effect === null && target.getAnimations().length === 0",
        );
        assert!(matches!(detached, Value::Bool(true)));
        realm.with_session(|session| {
            let document = session.document();
            let node = selector::query_selector(document, document.root(), "#target")
                .unwrap()
                .unwrap();
            assert!((session.computed_style(node).unwrap().opacity - 0.2).abs() < 0.02);
        });

        let finished_at = lumen_host::perf::now_ms();
        hub.borrow_mut()
            .records
            .get_mut(&animation_id)
            .unwrap()
            .start_ms = finished_at - 100.0;
        advance(engine.ctx(), finished_at).unwrap();
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(
            engine.eval_value("window.finished && window.finishedIdentity && window.readyIdentity && anim.playState === 'finished'")
                .unwrap()
                .ok()
                .unwrap(),
            Value::Bool(true)
        ));
    }

    #[test]
    fn document_and_shadow_root_get_animations_filter_scope_and_relevance() {
        let mut engine = Engine::new();
        crate::install(
            engine.ctx(),
            "<div id='parent'><span id='child'></span></div><div id='host'></div>",
            128,
        )
        .unwrap();
        let result = eval_ok(
            &mut engine,
            r#"
                const failures = [];
                const check = (name, value) => { if (!value) failures.push(name); };
                const parent = document.getElementById('parent');
                const child = document.getElementById('child');
                const first = child.animate({opacity: [0, 1]}, {duration: 10000});
                const second = parent.animate({opacity: [0, 1]}, {duration: 10000});
                const host = document.getElementById('host');
                const shadow = host.attachShadow({mode: 'open'});
                const shadowTarget = document.createElement('i');
                shadow.appendChild(shadowTarget);
                const shadowAnimation = shadowTarget.animate({opacity: [0, 1]}, {duration: 10000});

                const initial = document.getAnimations();
                check('Document uses animation creation order',
                    initial.length === 2 && initial[0] === first && initial[1] === second);
                check('Document excludes shadow-tree effects', !initial.includes(shadowAnimation));
                check('ShadowRoot returns its descendant effect',
                    shadow.getAnimations().length === 1 && shadow.getAnimations()[0] === shadowAnimation);
                check('Element query only returns its own target effects',
                    parent.getAnimations().length === 1 && parent.getAnimations()[0] === second);

                const delayed = child.animate({opacity: [0, 1]}, {delay: 10000, duration: 10000});
                delayed.pause();
                delayed.currentTime = 0;
                check('positive-rate delayed effect is current', document.getAnimations().includes(delayed));

                const zeroRate = child.animate({opacity: [0, 1]}, {delay: 10000, duration: 10000});
                zeroRate.pause();
                zeroRate.currentTime = 0;
                zeroRate.playbackRate = 0;
                check('zero-rate before-phase effect is not relevant', !document.getAnimations().includes(zeroRate));

                const finished = child.animate({opacity: [0, 1]}, {duration: 10000});
                finished.pause();
                finished.currentTime = 10000;
                check('finished no-fill effect is excluded', !child.getAnimations().includes(finished));

                const forwards = child.animate({opacity: [0, 1]}, {duration: 10000, fill: 'forwards'});
                forwards.pause();
                forwards.currentTime = 10000;
                check('finished forwards-fill effect remains relevant', document.getAnimations().includes(forwards));

                const reverse = child.animate({opacity: [0, 1]}, {duration: 10000});
                reverse.pause();
                reverse.currentTime = 10001;
                reverse.playbackRate = -1;
                check('reverse playback after the active phase is current', document.getAnimations().includes(reverse));

                const cancelled = child.animate({opacity: [0, 1]}, {duration: 10000});
                cancelled.cancel();
                check('cancelled effect is excluded', !document.getAnimations().includes(cancelled));

                const detached = document.createElement('div');
                const detachedAnimation = detached.animate({opacity: [0, 1]}, {duration: 10000});
                check('detached target remains visible on Element only',
                    !document.getAnimations().includes(detachedAnimation) &&
                    detached.getAnimations()[0] === detachedAnimation);

                const otherDocument = document.implementation.createHTMLDocument();
                const foreignTarget = otherDocument.createElement('div');
                otherDocument.body.appendChild(foreignTarget);
                const foreignAnimation = foreignTarget.animate({opacity: [0, 1]}, {duration: 10000});
                check('cross-document effects stay scoped to their owner document',
                    !document.getAnimations().includes(foreignAnimation) &&
                    otherDocument.getAnimations()[0] === foreignAnimation);

                const ordered = document.getAnimations();
                check('current relevant effects retain composite creation order',
                    ordered.length === 5 && ordered[0] === first && ordered[1] === second &&
                    ordered[2] === delayed && ordered[3] === forwards && ordered[4] === reverse);
                window.documentAnimationFailures = failures;
                failures.length === 0
            "#,
        );
        if !matches!(result, Value::Bool(true)) {
            let diagnostic = eval_ok(&mut engine, "window.documentAnimationFailures.join(', ')");
            let diagnostic = engine
                .ctx()
                .coerce_string(&diagnostic)
                .map(|value| value.to_string())
                .unwrap_or_else(|_| "<diagnostic was not a string>".into());
            panic!("DocumentOrShadowRoot.getAnimations failed: {diagnostic}");
        }
    }

    #[test]
    fn nullable_webidl_accessors_return_null_not_undefined() {
        let mut engine = Engine::new();
        crate::install(engine.ctx(),"<body><img id='img'><input id='num' type='number'><div id='target' class='a'></div></body>",64).unwrap();
        let result = eval_ok(&mut engine, r#"
            const failures = [];
            const check = (name, ok) => { if (!ok) failures.push(name); };
            const target = document.getElementById('target');
            const animation = target.animate({opacity: [0, 1]}, {duration: 100});
            animation.cancel();
            check('cancelled Animation.currentTime', animation.currentTime === null);
            check('DocumentTimeline.duration', document.timeline.duration === null);
            check('KeyframeEffect.pseudoElement', animation.effect.pseudoElement === null);
            const pending = target.animate({opacity: [0, 1]}, {duration: 100});
            pending.pause();
            check('pending Animation.startTime', pending.startTime === null);
            check('img.crossOrigin', document.getElementById('img').crossOrigin === null);
            const number = document.getElementById('num');
            check('number input selectionStart', number.selectionStart === null);
            check('number input selectionEnd', number.selectionEnd === null);
            check('number input selectionDirection', number.selectionDirection === null);
            check('DOMTokenList.item out of range', target.classList.item(5) === null);
            check('oninput default', number.oninput === null);
            check('NodeIterator.filter', document.createNodeIterator(target).filter === null);
            window.nullableFailures = failures;
            failures.length === 0
        "#);
        if !matches!(result, Value::Bool(true)) {
            let diagnostic = eval_ok(&mut engine, "window.nullableFailures.join(', ')");
            let diagnostic = engine.ctx().coerce_string(&diagnostic).map(|value| value.to_string())
                .unwrap_or_else(|_| "<diagnostic was not a string>".into());
            panic!("nullable accessors returned undefined: {diagnostic}");
        }
    }
}
