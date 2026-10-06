//! Web Animations adapter backed by shared HTML timing, CSS parsing and the
//! retained style/cascade pipeline.
use super::*;
use crate::realm_services::RealmServices;
use lumen::embed::{Deferred, JsFunction, JsObject};
use lumen_bind::{Class, FromArg, Host, Slot, This};
use lumen_html::animation::{
    self, CompositeMode, Direction, FillMode, Keyframe, PlaybackState, Timing,
};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::{Rc, Weak};
use std::sync::Arc;

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
    timeline_value: Value,
    node: Option<NodeId>,
    keyframes: Rc<[Keyframe]>,
    underlying: Rc<HashMap<String, String>>,
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
    id: u32,
    css_order: Option<usize>,
    node: Option<NodeId>,
    keyframes: Rc<[Keyframe]>,
    underlying: Rc<HashMap<String, String>>,
    easing: Rc<str>,
    composite: CompositeMode,
    sample: animation::Sample,
}

#[derive(Clone)]
struct EffectRecord {
    id: u32,
    realm: Weak<DomRealm>,
    node: Option<NodeId>,
    keyframes: Rc<[Keyframe]>,
    underlying: Rc<HashMap<String, String>>,
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
    order: usize,
    _retention: NodeRetention,
    previous: animation::CssEventSample,
    sampled: bool,
    associated: bool,
    css_paused: bool,
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
    overlaid_nodes: HashMap<usize, HashSet<NodeId>>,
    css_records: HashMap<(usize, NodeId, usize), (String, String, u32)>,
    css_generations: HashMap<usize, (Weak<DomRealm>, u64)>,
    css_events: HashMap<u32, CssEventState>,
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
    let mut hub = AnimationHub::default();
    hub.default_realm = Rc::downgrade(realm);
    RealmServices::replace_current(ctx, RefCell::new(hub));
    ctx.class_constructor::<DomAnimation>();
    ctx.class_constructor::<DomCssAnimation>();
    ctx.class_constructor::<DomKeyframeEffect>();
    ctx.class_constructor::<DomDocumentTimeline>();
    let constructor = ctx.class_constructor::<DomAnimationEvent>();
    let global = ctx.global_object();
    assert!(crate::install_interface(ctx, &global, "AnimationEvent", constructor).is_ok(),
        "AnimationEvent interface install");
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

fn parse_frame(ctx: &mut Ctx, object: &Value, offset: f64) -> OpResult<Keyframe> {
    let global = ctx.global_object();
    let object_ctor = ctx
        .get_member(&global, "Object")
        .map_err(|_| OpError::new("TypeError", "Object.keys is unavailable"))?;
    let keys_function = ctx
        .get_member(&object_ctor, "keys")
        .map_err(|_| OpError::new("TypeError", "Object.keys is unavailable"))?;
    let keys_function = JsFunction::from_value(keys_function)
        .ok_or_else(|| OpError::new("TypeError", "Object.keys is not callable"))?;
    let keys = keys_function.call(ctx, object_ctor, &[object.clone()])?;
    let Value::Num(length) = ctx
        .get_member(&keys, "length")
        .map_err(|_| OpError::new("TypeError", "keyframe keys are unavailable"))?
    else {
        return Err(OpError::new("TypeError", "keyframe keys are unavailable"));
    };
    let mut declarations = Vec::new();
    let mut explicit_offset = None;
    let mut easing = None;
    let mut composite = None;
    for index in 0..(length.max(0.0) as usize).min(512) {
        let key = ctx
            .get_member(&keys, &index.to_string())
            .map_err(|_| OpError::new("TypeError", "keyframe key is unavailable"))?;
        let key = ctx
            .coerce_string(&key)
            .map_err(OpError::thrown)?
            .to_string();
        let value = ctx
            .get_member(object, &key)
            .map_err(|_| OpError::new("TypeError", "keyframe property getter failed"))?;
        if key == "offset" {
            if !matches!(value, Value::Undefined | Value::Null) {
                explicit_offset = Some(ctx.coerce_number(&value).map_err(OpError::thrown)?);
            }
            continue;
        }
        if key == "easing" {
            if !matches!(value, Value::Undefined | Value::Null) {
                let value = ctx
                    .coerce_string(&value)
                    .map_err(OpError::thrown)?
                    .to_string();
                if animation::ease(&value, 0.5).is_none() {
                    return Err(OpError::new(
                        "TypeError",
                        "unsupported keyframe easing function",
                    ));
                }
                easing = Some(value);
            }
            continue;
        }
        if key == "composite" {
            if !matches!(value, Value::Undefined | Value::Null) {
                let value = ctx
                    .coerce_string(&value)
                    .map_err(OpError::thrown)?
                    .to_string();
                composite = parse_keyframe_composite(&value)?;
            }
            continue;
        }
        if matches!(value, Value::Undefined | Value::Null) {
            continue;
        }
        let value = ctx
            .coerce_string(&value)
            .map_err(OpError::thrown)?
            .to_string();
        declarations.push((css_property_name(&key), value));
    }
    let offset = explicit_offset.unwrap_or(offset);
    if !offset.is_nan() && (!offset.is_finite() || !(0.0..=1.0).contains(&offset)) {
        return Err(OpError::new(
            "TypeError",
            "keyframe offset must be between zero and one",
        ));
    }
    Ok(Keyframe {
        offset,
        declarations,
        easing,
        composite,
    })
}

fn object_keys(ctx: &mut Ctx, object: &Value) -> OpResult<Vec<String>> {
    let global = ctx.global_object();
    let object_ctor = ctx
        .get_member(&global, "Object")
        .map_err(|_| OpError::new("TypeError", "Object.keys is unavailable"))?;
    let keys_function = ctx
        .get_member(&object_ctor, "keys")
        .map_err(|_| OpError::new("TypeError", "Object.keys is unavailable"))?;
    let keys_function = JsFunction::from_value(keys_function)
        .ok_or_else(|| OpError::new("TypeError", "Object.keys is not callable"))?;
    let keys = keys_function.call(ctx, object_ctor, &[object.clone()])?;
    let Value::Num(length) = ctx
        .get_member(&keys, "length")
        .map_err(|_| OpError::new("TypeError", "object keys are unavailable"))?
    else {
        return Err(OpError::new("TypeError", "object keys are unavailable"));
    };
    (0..(length.max(0.0) as usize).min(512))
        .map(|index| {
            let key = ctx
                .get_member(&keys, &index.to_string())
                .map_err(|_| OpError::new("TypeError", "object key is unavailable"))?;
            Ok(ctx
                .coerce_string(&key)
                .map_err(OpError::thrown)?
                .to_string())
        })
        .collect()
}

fn values(ctx: &mut Ctx, value: &Value) -> OpResult<Vec<Value>> {
    if ctx.is_array_value(value).map_err(OpError::thrown)? {
        let Value::Num(length) = ctx
            .get_member(value, "length")
            .map_err(|_| OpError::new("TypeError", "keyframe sequence length failed"))?
        else {
            return Err(OpError::new("TypeError", "keyframe sequence length failed"));
        };
        return (0..(length.max(0.0) as usize).min(512))
            .map(|index| {
                ctx.get_member(value, &index.to_string())
                    .map_err(|_| OpError::new("TypeError", "keyframe sequence read failed"))
            })
            .collect();
    }
    Ok(vec![value.clone()])
}

fn property_indexed_keyframes(ctx: &mut Ctx, object: &Value) -> OpResult<Vec<Keyframe>> {
    let mut properties = Vec::<(String, Vec<Value>)>::new();
    let mut offset_values = Vec::new();
    let mut easing_values = Vec::new();
    let mut composite_values = Vec::new();
    for key in object_keys(ctx, object)? {
        let value = ctx
            .get_member(object, &key)
            .map_err(|_| OpError::new("TypeError", "keyframe property getter failed"))?;
        match key.as_str() {
            "offset" => offset_values = values(ctx, &value)?,
            "easing" => easing_values = values(ctx, &value)?,
            "composite" => composite_values = values(ctx, &value)?,
            _ => properties.push((css_property_name(&key), values(ctx, &value)?)),
        }
    }
    let count = properties
        .iter()
        .map(|(_, values)| values.len())
        .chain([
            offset_values.len(),
            easing_values.len(),
            composite_values.len(),
        ])
        .max()
        .unwrap_or(0)
        .min(512);
    let mut frames = (0..count)
        .map(|_| Keyframe {
            offset: f64::NAN,
            declarations: Vec::new(),
            easing: None,
            composite: None,
        })
        .collect::<Vec<_>>();
    for (property, property_values) in properties {
        for (index, value) in property_values.into_iter().take(count).enumerate() {
            let value = ctx
                .coerce_string(&value)
                .map_err(OpError::thrown)?
                .to_string();
            frames[index].declarations.push((property.clone(), value));
        }
    }
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
        }
    }
    for (index, value) in easing_values.into_iter().take(count).enumerate() {
        if !matches!(value, Value::Undefined | Value::Null) {
            let easing = ctx
                .coerce_string(&value)
                .map_err(OpError::thrown)?
                .to_string();
            if animation::ease(&easing, 0.5).is_none() {
                return Err(OpError::new(
                    "TypeError",
                    "unsupported keyframe easing function",
                ));
            }
            frames[index].easing = Some(easing);
        }
    }
    for (index, value) in composite_values.into_iter().take(count).enumerate() {
        if !matches!(value, Value::Undefined | Value::Null) {
            let value = ctx
                .coerce_string(&value)
                .map_err(OpError::thrown)?
                .to_string();
            frames[index].composite = parse_keyframe_composite(&value)?;
        }
    }
    normalize_offsets(&mut frames)?;
    Ok(frames)
}

fn normalize_offsets(frames: &mut [Keyframe]) -> OpResult<()> {
    let mut previous = None;
    for frame in frames.iter() {
        if frame.offset.is_finite() {
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
    if ctx.is_array_value(value).map_err(OpError::thrown)? {
        let sequence = values(ctx, value)?;
        let mut frames = Vec::with_capacity(sequence.len());
        for object in sequence {
            if !matches!(object, Value::Obj(_)) {
                return Err(OpError::new("TypeError", "keyframe must be an object"));
            }
            frames.push(parse_frame(ctx, &object, f64::NAN)?);
        }
        normalize_offsets(&mut frames)?;
        Ok(frames)
    } else if matches!(value, Value::Obj(_)) {
        property_indexed_keyframes(ctx, value)
    } else {
        Err(OpError::new(
            "TypeError",
            "keyframes must be an object or sequence",
        ))
    }
}

fn base_css_value(style: &lumen_html::css::Style, name: &str) -> Option<String> {
    let length = |value: Option<f32>| value.map(|value| format!("{value}px"));
    let color = |color: lumen_html::paint::Rgba| {
        format!(
            "rgba({}, {}, {}, {})",
            color.r,
            color.g,
            color.b,
            color.a as f32 / 255.0
        )
    };
    match name {
        "transform" => Some(animation::serialize_transforms(
            style.transforms.as_deref().unwrap_or(&[]),
        )),
        "opacity" => Some(style.opacity.to_string()),
        "width" => length(style.width),
        "height" => length(style.height),
        "font-size" => Some(format!("{}px", style.font_size)),
        "margin" => Some(format!("{}px", style.margin)),
        "padding" => Some(format!("{}px", style.padding)),
        "border-radius" => Some(format!("{}px", style.border_radius)),
        "gap" => Some(format!("{}px", style.gap)),
        "color" => Some(color(style.color)),
        "background-color" => Some(color(style.background)),
        _ => None,
    }
}

fn color_css_value(color: lumen_html::paint::Rgba) -> String {
    format!(
        "rgba({}, {}, {}, {:.6})",
        color.r,
        color.g,
        color.b,
        color.a as f32 / 255.0
    )
}

fn interpolate_animation_value(from: &str, to: &str, progress: f64) -> Option<String> {
    animation::interpolate_numeric(from, to, progress).or_else(|| {
        let current = lumen_html::css::Style::initial().color;
        let from = lumen_html::css::parse_animation_color(from, current)?;
        let to = lumen_html::css::parse_animation_color(to, current)?;
        Some(color_css_value(lumen_html_image::interpolate_color(
            from,
            to,
            progress as f32,
        )))
    })
}

fn add_colors(base: &str, value: &str, delta: Option<(&str, u64)>) -> Option<String> {
    use lumen_html::paint::Rgba;
    let initial = lumen_html::css::Style::initial().color;
    let base = lumen_html::css::parse_animation_color(base, initial)?;
    let value = lumen_html::css::parse_animation_color(value, initial)?;
    let mut channels = [base.r, base.g, base.b, base.a];
    let added = [value.r, value.g, value.b, value.a];
    let extra = if let Some((delta, count)) = delta {
        let delta = lumen_html::css::parse_animation_color(delta, initial)?;
        [delta.r, delta.g, delta.b, delta.a].map(|channel| (channel as i64) * (count as i64))
    } else {
        [0; 4]
    };
    for index in 0..4 {
        channels[index] =
            ((channels[index] as i64 + added[index] as i64 + extra[index]).clamp(0, 255)) as u8;
    }
    Some(color_css_value(Rgba {
        r: channels[0],
        g: channels[1],
        b: channels[2],
        a: channels[3],
    }))
}

fn composite_value(
    property: &str,
    base: &str,
    value: &str,
    iteration_delta: Option<(&str, u64)>,
) -> Option<String> {
    animation::add_numeric_values(base, value, iteration_delta)
        .or_else(|| add_colors(base, value, iteration_delta))
        .or_else(|| {
            // Discrete properties have no additive representation; their
            // composite operation follows the property's discrete sampling.
            (!matches!(
                property,
                "opacity" | "width" | "height" | "font-size" | "margin" | "padding" | "gap"
            ))
            .then(|| value.to_owned())
        })
}

fn resolve_underlying(realm: &DomRealm, node: NodeId, frames: &mut Vec<Keyframe>) -> OpResult<()> {
    if frames.is_empty() {
        return Ok(());
    }
    let properties = frames
        .iter()
        .flat_map(|frame| frame.declarations.iter().map(|(name, _)| name.clone()))
        .collect::<HashSet<_>>();
    let mut session = realm.session.borrow_mut();
    let style = session
        .computed_style(node)
        .map_err(|_| OpError::new("Error", "could not resolve animation underlying style"))?;
    for frame in frames.iter_mut() {
        for (property, value) in &mut frame.declarations {
            if (property == "color" || property.ends_with("-color"))
                && value.to_ascii_lowercase().contains("currentcolor")
            {
                if let Some(color) = lumen_html::css::parse_animation_color(value, style.color) {
                    *value = color_css_value(color);
                }
            }
        }
    }
    let mut before = Vec::new();
    let mut after = Vec::new();
    for property in properties {
        let Some(base) = base_css_value(&style, &property) else {
            continue;
        };
        if !frames[0]
            .declarations
            .iter()
            .any(|(name, _)| name == &property)
        {
            if frames[0].offset > 0.0 {
                before.push((property.clone(), base.clone()));
            } else {
                frames[0]
                    .declarations
                    .push((property.clone(), base.clone()));
            }
        }
        let last = frames.len() - 1;
        if !frames[last]
            .declarations
            .iter()
            .any(|(name, _)| name == &property)
        {
            if frames[last].offset < 1.0 {
                after.push((property, base));
            } else {
                frames[last].declarations.push((property, base));
            }
        }
    }
    if !before.is_empty() {
        frames.push(Keyframe {
            offset: 0.0,
            declarations: before,
            easing: None,
            composite: Some(CompositeMode::Replace),
        });
    }
    if !after.is_empty() {
        frames.push(Keyframe {
            offset: 1.0,
            declarations: after,
            easing: None,
            composite: Some(CompositeMode::Replace),
        });
    }
    frames.sort_by(|a, b| a.offset.total_cmp(&b.offset));
    Ok(())
}

fn capture_underlying(
    realm: &DomRealm,
    node: Option<NodeId>,
    frames: &[Keyframe],
) -> OpResult<HashMap<String, String>> {
    let Some(node) = node else {
        return Ok(HashMap::new());
    };
    let style = realm
        .session
        .borrow_mut()
        .computed_style(node)
        .map_err(|_| OpError::new("Error", "could not resolve animation underlying style"))?;
    Ok(frames
        .iter()
        .flat_map(|frame| frame.declarations.iter().map(|(name, _)| name))
        .filter_map(|name| base_css_value(&style, name).map(|value| (name.clone(), value)))
        .collect())
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
    let mut frames = parse_keyframes(ctx, &keyframes)?;
    if let Some(node) = node {
        resolve_underlying(realm, node, &mut frames)?;
    }
    let underlying = capture_underlying(realm, node, &frames)?;
    let composite = parse_composite(ctx, options.as_ref())?;
    let (timing, easing, duration_auto, fill_auto) = parse_options(ctx, options.as_ref())?;
    let hub = hub(ctx)?;
    let effect_id = {
        let mut state = hub.borrow_mut();
        state.next_effect_id = state.next_effect_id.wrapping_add(1).max(1);
        let id = state.next_effect_id;
        state.effect_records.insert(
            id,
            EffectRecord {
                id,
                animation_id: None,
                realm: Rc::downgrade(realm),
                node,
                keyframes: frames.into(),
                underlying: Rc::new(underlying),
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
    let hub = hub(ctx)?;
    let now = document_time(timeline_realm).unwrap_or(0.0);
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
                css_name: None,
                css_order: None,
                realm: effect.realm.clone(),
                timeline_realm: Rc::downgrade(timeline_realm),
                timeline_id,
                timeline_origin_ms,
                timeline_value,
                node: effect.node,
                keyframes: effect.keyframes.clone(),
                underlying: effect.underlying.clone(),
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
    mark_ready_pending(ctx, &hub, id);
    ensure_pending_finished(ctx, &hub, id);
    let _ = start_playing;
    for realm in realms {
        apply_realm(&hub, &realm, now)?;
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
    let now = document_time(realm).unwrap_or(0.0);
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
    apply_realm(&hub, realm, now)?;
    Ok(wrapper)
}

pub fn for_element(ctx: &mut Ctx, realm: &Rc<DomRealm>, node: NodeId, subtree: bool) -> OpResult<Value> {
    let tree_order = if subtree {
        let session = realm.session.borrow();
        let document = session.document();
        let mut order = HashMap::new();
        order.insert(node, 0usize);
        let mut cursor = super::next_descendant(document, node, node).map_err(dom_error)?;
        while let Some(id) = cursor {
            order.insert(id, order.len());
            cursor = super::next_descendant(document, node, id).map_err(dom_error)?;
        }
        order
    } else {
        HashMap::from([(node, 0usize)])
    };
    let hub = hub(ctx)?;
    let now = document_time(realm).unwrap_or(0.0);
    refresh_css_animations(ctx, &hub, realm, now)?;
    apply_realm(&hub, realm, now)?;
    collect_retired_css_animations(ctx, &hub);
    let mut matched = hub
        .borrow()
        .records
        .values()
        .filter(|record| {
            record.node.is_some_and(|target| tree_order.contains_key(&target))
                && record_is_relevant(record, realm, now)
        })
        .map(|record| (
            record.node.and_then(|target| tree_order.get(&target).copied()).unwrap_or(0),
            record.css_name.is_none(),
            record.css_order.unwrap_or(0),
            record.id,
        ))
        .collect::<Vec<_>>();
    matched.sort_unstable();
    let ids = matched.into_iter().map(|(_, _, _, id)| id).collect::<Vec<_>>();
    animation_array(ctx, &hub, &ids)
}

/// Return relevant animations whose targets are descendants of a Document or
/// ShadowRoot. The ordinary node walk deliberately stops at shadow boundaries;
/// each shadow root is queried through its own root instead.
pub fn for_tree_root(ctx: &mut Ctx, realm: &Rc<DomRealm>, root: NodeId) -> OpResult<Value> {
    let targets = {
        let session = realm.session.borrow();
        let document = session.document();
        let mut targets = HashSet::new();
        let mut node = super::next_descendant(document, root, root).map_err(dom_error)?;
        while let Some(id) = node {
            targets.insert(id);
            node = super::next_descendant(document, root, id).map_err(dom_error)?;
        }
        targets
    };
    let hub = hub(ctx)?;
    let now = document_time(realm).unwrap_or(0.0);
    refresh_css_animations(ctx, &hub, realm, now)?;
    apply_realm(&hub, realm, now)?;
    collect_retired_css_animations(ctx, &hub);
    // Animation IDs follow creation order in the hub. Iterating its BTreeMap
    // preserves the effect stack's order for native WAAPI animations.
    let ids = hub
        .borrow()
        .records
        .values()
        .filter(|record| {
            record_is_relevant(record, realm, now)
                && record.node.is_some_and(|node| targets.contains(&node))
        })
        .map(|record| record.id)
        .collect::<Vec<_>>();
    animation_array(ctx, &hub, &ids)
}

fn record_is_relevant(record: &Record, realm: &Rc<DomRealm>, now: f64) -> bool {
    record.effect_id.is_some()
        && !record.cancelled
        && record
            .realm
            .upgrade()
            .is_some_and(|candidate| Rc::ptr_eq(&candidate, realm))
        && animation::effect_is_relevant(
            record.timing,
            sample_record(record, now),
            record.playback_rate,
        )
}

fn css_list_value(style: &[Option<Arc<str>>; 9], property: usize, index: usize, default: &str) -> String {
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
        for offset in offsets {
            frames.push(Keyframe {
                offset,
                declarations: declarations.clone(),
                easing: easing.clone(),
                composite: Some(composite),
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
    names: &[String],
    snapshot: &lumen_html::session::AnimationSnapshot,
    scope: Option<NodeId>,
    environment: lumen_html::css::MediaEnvironment,
) {
    let wanted = names.iter().map(|authored| {
        lumen_html::css::normalize_animation_name(authored).filter(|name| name != "none"
            && snapshot.keyframes.iter().any(|rule| rule.name == *name && rule.scope == scope
                && rule.media.iter().all(|query| lumen_html::css::media_query_matches(query, environment))))
    }).collect::<Vec<_>>();
    let mut state = hub.borrow_mut();
    let displaced = wanted.iter().enumerate().any(|(index, name)| name.as_ref().is_some_and(|name|
        state.css_records.get(&(realm_key, node, index)).is_some_and(|(old, _, _)| old != name)));
    if !displaced { return; }
    let mut old = state.css_records.iter()
        .filter(|(key, _)| key.0 == realm_key && key.1 == node)
        .map(|(key, entry)| (key.2, entry.clone()))
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
    for (from, _, _) in &moves { state.css_records.remove(&(realm_key, node, *from)); }
    for (_, to, entry) in moves {
        let id = entry.2;
        state.css_records.insert((realm_key, node, to), entry);
        if let Some(record) = state.records.get_mut(&id) { record.css_order = Some(to); }
        if let Some(event) = state.css_events.get_mut(&id) { event.order = to; }
    }
}

fn refresh_css_animations(
    ctx: &mut Ctx,
    hub: &Rc<RefCell<AnimationHub>>,
    realm: &Rc<DomRealm>,
    now: f64,
) -> OpResult<()> {
    let snapshot = realm
        .session
        .borrow_mut()
        .animation_snapshot()
        .map_err(|_| OpError::new("SyntaxError", "CSS animation style resolution failed"))?;
    let environment = realm.session.borrow().media_environment();
    let realm_key = Rc::as_ptr(realm) as usize;
    if hub.borrow().css_generations.get(&realm_key).is_some_and(|(owner, generation)| {
        *generation == snapshot.generation && owner.upgrade().is_some_and(|owner| Rc::ptr_eq(&owner, realm))
    }) {
        return Ok(());
    }
    let mut active = HashSet::new();
    let mut created = Vec::new();
    let mut cancelled = Vec::new();
    let mut playback_changes = Vec::new();
    for (node, scope, style) in &snapshot.nodes {
        let (node, scope) = (*node, *scope);
        let names = style[0].as_deref().map(lumen_html::css::css_list_items).unwrap_or_default();
        reposition_css_records(hub, realm_key, node, &names, &snapshot, scope, environment);
        for (index, authored_name) in names.iter().enumerate() {
            let Some(name) = lumen_html::css::normalize_animation_name(authored_name) else { continue; };
            if name == "none" { continue; }
            let selected = snapshot.keyframes.iter().filter(|rule| {
                rule.name == name && rule.scope == scope
                    && rule.media.iter().all(|query| lumen_html::css::media_query_matches(query, environment))
            }).last();
            let Some(selected) = selected else { continue };
            let key = (realm_key, node, index);
            active.insert(key);
            let signature = format!("{:?}|{}", style, selected.css_text);
            let old_entry = hub.borrow().css_records.get(&key).cloned();
            if old_entry.as_ref().is_some_and(|(old_name, old_signature, _)| old_name == &name && old_signature == &signature) {
                continue;
            }
            let duration = css_time_value(&css_list_value(&style, 1, index, "0s")).unwrap_or(0.0).max(0.0);
            let delay = css_time_value(&css_list_value(&style, 2, index, "0s")).unwrap_or(0.0);
            let easing = css_list_value(&style, 3, index, "ease");
            let iterations = match css_list_value(&style, 4, index, "1").as_str() {
                "infinite" => f64::INFINITY,
                value => value.parse::<f64>().unwrap_or(1.0).max(0.0),
            };
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
            let mut frames = css_keyframe_effect(selected, direction, composite)
                .ok_or_else(|| OpError::new("SyntaxError", "invalid CSS keyframe block"))?;
            resolve_underlying(realm, node, &mut frames)?;
            let underlying = capture_underlying(realm, Some(node), &frames)?;
            let timing = Timing {
                delay_ms: delay,
                end_delay_ms: 0.0,
                duration_ms: duration,
                iteration_start: 0.0,
                iterations,
                fill,
                direction,
            };
            let frames: Rc<[Keyframe]> = frames.into();
            let underlying = Rc::new(underlying);
            let easing: Rc<str> = easing.into();
            if let Some((old_name, _old_signature, old_id)) = old_entry {
                if old_name == name {
                    let mut state = hub.borrow_mut();
                    let Some(record_snapshot) = state.records.get(&old_id).cloned() else { continue; };
                    let desired_pause=play_state=="paused";
                    let play_state_changed=state.css_events.get(&old_id).is_some_and(|event|event.css_paused!=desired_pause);
                    if let Some(event)=state.css_events.get_mut(&old_id){event.css_paused=desired_pause;}
                    if let Some(record) = state.records.get_mut(&old_id) {
                        record.timing = timing;
                        record.keyframes = frames.clone();
                        record.underlying = underlying.clone();
                        record.easing = easing.clone();
                        record.composite = composite;
                    }
                    if let Some(effect_id) = record_snapshot.effect_id {
                        if let Some(effect) = state.effect_records.get_mut(&effect_id) {
                            effect.timing = timing;
                            effect.keyframes = frames;
                            effect.underlying = underlying;
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
                id: effect_id,
                realm: Rc::downgrade(realm),
                node: Some(node),
                keyframes: frames.clone(),
                underlying: underlying.clone(),
                timing,
                easing: easing.clone(),
                duration_auto: false,
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
                timeline_value,
                node: Some(node),
                keyframes: frames,
                underlying,
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
                owner: Rc::downgrade(realm), node, order: index,
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
    let value = if css_name.is_some() {
        ctx.new_instance(DomCssAnimation { base: animation })
    } else {
        ctx.new_instance(animation)
    };
    if let Some(weak) = ctx.weak_value(&value) {
        hub.borrow_mut().wrappers.insert(id, weak);
    }
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
    if let Some(weak) = ctx.weak_value(&value) {
        hub.borrow_mut().effects.insert(id, weak);
    }
    Ok(value)
}

fn keyframe_object(ctx: &mut Ctx, frame: &Keyframe) -> OpResult<Value> {
    let object = Value::Obj(ctx.new_object());
    ctx.set_member(&object, "offset", Value::Num(frame.offset))
        .map_err(|_| OpError::new("Error", "keyframe offset assignment failed"))?;
    if let Some(easing) = &frame.easing {
        ctx.set_member(&object, "easing", Value::Str(easing.clone().into()))
            .map_err(|_| OpError::new("Error", "keyframe easing assignment failed"))?;
    }
    let composite = match frame.composite {
        None => "auto",
        Some(CompositeMode::Replace) => "replace",
        Some(CompositeMode::Add) => "add",
        Some(CompositeMode::Accumulate) => "accumulate",
    };
    ctx.set_member(&object, "composite", Value::Str(composite.into()))
        .map_err(|_| OpError::new("Error", "keyframe composite assignment failed"))?;
    for (name, value) in &frame.declarations {
        let mut property = String::new();
        let mut uppercase = false;
        for character in name.chars() {
            if character == '-' {
                uppercase = true;
            } else if uppercase {
                property.push(character.to_ascii_uppercase());
                uppercase = false;
            } else {
                property.push(character);
            }
        }
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

fn timeline_wrapper(
    ctx: &mut Ctx,
    hub: &Rc<RefCell<AnimationHub>>,
    id: u32,
    realm: &Rc<DomRealm>,
    origin_time_ms: f64,
) -> Value {
    if let Some(value) = hub
        .borrow()
        .timeline_values
        .get(&id)
        .and_then(WeakValue::upgrade)
    {
        return value;
    }
    let value = ctx.new_instance(DomDocumentTimeline {
        id,
        realm: Rc::downgrade(realm),
        origin_time_ms,
    });
    if let Some(weak) = ctx.weak_value(&value) {
        hub.borrow_mut().timeline_values.insert(id, weak);
    }
    value
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
            time<0.0 || time>=record.timing.end_time()
        } else {time<=0.0 || time>record.timing.end_time()});
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
            let seek=if record.playback_rate<0.0 {record.timing.end_time()}else{0.0};
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
    let owner={let state=hub.borrow();state.records.get(&id).filter(|record|record.css_name.is_some()).and_then(|record|record.realm.upgrade())};
    if let Some(owner)=owner {refresh_css_animations(ctx,hub,&owner,owner.timeline_sample.get())?;}
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

fn apply_realm(hub: &Rc<RefCell<AnimationHub>>, realm: &Rc<DomRealm>, now: f64) -> OpResult<()> {
    let key = Rc::as_ptr(realm) as usize;
    let mut snapshot = hub
        .borrow()
        .records
        .values()
        .filter(|record| {
            !record.cancelled && record.effect_id.is_some()
                && record
                    .realm
                    .upgrade()
                    .is_some_and(|candidate| Rc::ptr_eq(&candidate, realm))
        })
        .map(|record| PaintSnapshot {
            id: record.id, css_order: record.css_order, node: record.node,
            keyframes: record.keyframes.clone(), underlying: record.underlying.clone(),
            easing: record.easing.clone(), composite: record.composite,
            sample: sample_record(record, now),
        })
        .collect::<Vec<_>>();
    // CSS animations precede script-created Web Animations in the effect
    // stack; later CSS list entries win among CSS animations.
    snapshot.sort_by_key(|record| match record.css_order {
        Some(order) => (0u8, order as u64),
        None => (1u8, record.id as u64),
    });
    let mut declarations = HashMap::<NodeId, BTreeMap<String, String>>::new();
    let has_transforms = snapshot.iter().any(|record| {
        record.keyframes.iter().any(|frame| {
            frame
                .declarations
                .iter()
                .any(|(name, _)| name == "transform")
        })
    });
    // Resolve layout-affecting animation values first. Matrix fallback for a
    // percentage translation must use the same frame's animated reference box.
    for transform_pass in [false, true] {
        for record in &snapshot {
            if !record.keyframes.iter().any(|frame| frame.declarations.iter()
                .any(|(name, _)| (name == "transform") == transform_pass)) {
                continue;
            }
            let sample = record.sample;
            if let Some(progress) = sample.progress {
                let eased = animation::ease_with_before(&record.easing, progress, sample.before)
                    .unwrap_or(progress);
                if let Some(node) = record.node {
                    let transform_context = if transform_pass {
                        let mut session = realm.session.borrow_mut();
                        let style = session.computed_style(node).map_err(|_| {
                            OpError::new("Error", "could not resolve transform animation style")
                        })?;
                        let rect = session
                            .layout_rect(node)
                            .unwrap_or(lumen_html::paint::Rect {
                                x: 0.0,
                                y: 0.0,
                                width: 0.0,
                                height: 0.0,
                            });
                        let viewport = session.media_environment();
                        Some((style, rect, viewport))
                    } else {
                        None
                    };
                    let properties = declarations.entry(node).or_default();
                    let mut composition_failed = false;
                    let sampled = animation::sample_keyframes_composed_with_property_filter(
                        &record.keyframes,
                        eased,
                        sample.before,
                        |property| (property == "transform") == transform_pass,
                        |property, frame, value| {
                            let mode = frame.composite.unwrap_or(record.composite);
                            if mode == CompositeMode::Replace {
                                return Some(value.to_owned());
                            }
                            let base = properties
                                .get(property)
                                .or_else(|| record.underlying.get(property));
                            let composed = base.and_then(|base| {
                                if property == "transform" {
                                    let (style, rect, viewport) = transform_context.as_ref()?;
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
                                } else {
                                    composite_value(property, base, value, None)
                                }
                            });
                            composition_failed |= composed.is_none();
                            composed
                        },
                        |property, from, to, progress| {
                            if property == "transform" {
                                let (style, rect, viewport) = transform_context.as_ref()?;
                                let from = lumen_html::css::parse_animation_transforms(
                                    from, style, *viewport, None,
                                )?;
                                let to = lumen_html::css::parse_animation_transforms(
                                    to, style, *viewport, None,
                                )?;
                                animation::interpolate_transforms(
                                    &from,
                                    &to,
                                    progress,
                                    rect.width,
                                    rect.height,
                                )
                            } else {
                                interpolate_animation_value(from, to, progress)
                            }
                        },
                    );
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
        if !transform_pass && has_transforms {
            publish_animation_declarations(hub, realm, key, &declarations)?;
            realm.flush_layout()?;
        }
    }
    publish_animation_declarations(hub, realm, key, &declarations)?;
    let active = snapshot_active_nodes(hub, realm, now);
    hub.borrow_mut().overlaid_nodes.insert(key, active);
    Ok(())
}

fn publish_animation_declarations(
    hub: &Rc<RefCell<AnimationHub>>,
    realm: &DomRealm,
    key: usize,
    declarations: &HashMap<NodeId, BTreeMap<String, String>>,
) -> OpResult<()> {
    let previous = hub
        .borrow()
        .overlaid_nodes
        .get(&key)
        .cloned()
        .unwrap_or_default();
    let mut targets = previous;
    targets.extend(declarations.keys().copied());
    for node in targets {
        let pairs = declarations
            .get(&node)
            .map(|values| {
                values
                    .iter()
                    .map(|(name, value)| (name.clone(), value.clone()))
                    .collect()
            })
            .unwrap_or_default();
        realm
            .session
            .borrow_mut()
            .set_animation_declarations(node, pairs)
            .map_err(|_| OpError::new("SyntaxError", "animation style application failed"))?;
    }
    Ok(())
}

fn snapshot_active_nodes(
    hub: &Rc<RefCell<AnimationHub>>,
    realm: &Rc<DomRealm>,
    now: f64,
) -> HashSet<NodeId> {
    hub.borrow()
        .records
        .values()
        .filter(|record| {
            record.effect_id.is_some()
                && !record.cancelled
                && record
                    .realm
                    .upgrade()
                    .is_some_and(|candidate| Rc::ptr_eq(&candidate, realm))
                && sample_record(record, now).progress.is_some()
        })
        .filter_map(|record| record.node)
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
    record.timeline_realm.upgrade().and_then(|realm| document_time(&realm))
}

fn hub_time(hub: &Rc<RefCell<AnimationHub>>) -> f64 {
    hub.borrow().default_realm.upgrade().and_then(|realm| document_time(&realm)).unwrap_or(0.0)
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
    if record.hold_time_ms.is_none() && record.paused_at_ms.is_none() {
        let end_time = record.timing.end_time();
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
        record.timing,
        current_time,
        record.playback_rate,
        record.paused_at_ms.is_some(),
    )
}

// Retired CSS animations have no effect on the cascade. Keep them only while
// script retains an Animation or KeyframeEffect wrapper. Promise caches must
// not themselves keep a resolved Animation alive forever.
fn collect_retired_css_animations(ctx: &mut Ctx, hub: &Rc<RefCell<AnimationHub>>) {
    let retired = {
        let mut state = hub.borrow_mut();
        state.css_generations.retain(|_, (realm, _)| realm.strong_count() != 0);
        let mut retired = state.records.values()
            .filter(|record| record.cancelled && record.css_name.is_some())
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
        if first_retirement { reject_finished(ctx, hub, id); }
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
        if seen.insert(key) {
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
    collect_retired_css_animations(ctx, &hub);
    settle_finished(ctx, &hub, timestamp_ms)?;
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
    {
        let mut state = hub.borrow_mut();
        // The source document no longer owns adopted nodes. Its next style
        // publication must not clear overlays through obsolete source NodeIds;
        // the migrated records below publish into the destination normally.
        if let Some(overlaid) = state.overlaid_nodes.get_mut(&(Rc::as_ptr(source) as usize)) {
            for (old_node, _) in mapping { overlaid.remove(old_node); }
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
    let record = hub
        .borrow()
        .records
        .get(&id)
        .cloned()
        .ok_or_else(|| OpError::new("InvalidStateError", "animation is no longer available"))?;
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
                    event.owner.upgrade().map(|owner| (owner, event.node, event.order,
                        record.css_name.clone().unwrap_or_default(),
                        (sample_record(record, now).current_time_ms - record.timing.delay_ms)
                            .max(0.0).min(record.timing.active_duration())))
                } else { None }
            }
            _ => None,
        }
    };
    if let Some((owner, node, order, name, elapsed_ms)) = snapshot {
        let mut retention = DispatchRetention { hub: hub.clone(), ids: Vec::new() };
        retention.retain(id);
        let snapshot = Rc::new(CssCancellation { id, owner: owner.clone(), node, order, name,
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

fn cancel_css_record(ctx: &mut Ctx, hub: &Rc<RefCell<AnimationHub>>, id: u32, now: f64, detach: bool) -> OpResult<()> {
    queue_css_cancellation(ctx, hub, id, now, detach)?;
    reset_pending_task(ctx,hub,id)?;
    retire_css_record_state(hub, id);
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
        let mut node = super::next_descendant(document, root, root).map_err(dom_error)?;
        while let Some(id) = node {
            order.insert(id, order.len());
            node = super::next_descendant(document, root, id).map_err(dom_error)?;
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
        (realm, tree_order[&realm].get(&entry.node).copied().unwrap_or(usize::MAX), entry.order)
    });
    for snapshot in &snapshots { snapshot.dispatched.set(true); }
    let mut first_error = None;
    for snapshot in snapshots {
        if let Err(error) = dispatch_css_event(ctx, hub, snapshot.id,
            animation::CssEvent { kind: "animationcancel", elapsed_ms: snapshot.elapsed_ms },
            &snapshot.owner, snapshot.node, &snapshot.name) {
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
    sample: animation::CssEvent, owner: &Rc<DomRealm>, node: NodeId, name: &str) -> OpResult<()> {
    let (owner, node) = owner.resolve_adopted_node(node);
    let dispatch = |ctx: &mut Ctx| -> OpResult<()> {
        let animation = existing_wrapper(ctx, hub, id)?;
        let init = ctx.new_object_with_proto(&Value::Null);
        ctx.member_set(&init, "bubbles", Value::Bool(true)).map_err(OpError::thrown)?;
        let event = DomAnimationEvent { base: DomEvent::new(ctx, sample.kind, Some(init))?,
            animation_name: name.to_owned(), elapsed_time: sample.elapsed_ms / 1000.0,
            pseudo_element: String::new(), animation };
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
    animation::css_event_sample(record.timing, time, record.playback_rate)
}

fn render_revision(realm: &DomRealm) -> (u64, u64) {
    let session = realm.session.borrow();
    (session.document().version(), session.paint_revision())
}

fn dispatch_css_events(ctx: &mut Ctx, hub: &Rc<RefCell<AnimationHub>>, now: f64) -> OpResult<()> {
    struct Queued {
        id: u32, event: animation::CssEvent, owner: Rc<DomRealm>, node: NodeId,
        order: usize, cancelled: bool, scheduled: f64, sequence: usize,
    }
    let mut queue = Vec::new();
    let mut retention = DispatchRetention { hub: hub.clone(), ids: Vec::new() };
    {
        let mut state = hub.borrow_mut();
        let AnimationHub { records, css_events, .. } = &mut *state;
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
                for (sequence, sample) in animation::css_events(record.timing, event.previous, current,
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
                        queue.push(Queued { id: record.id, event: sample, owner: owner.clone(), node: event.node,
                            order: event.order, cancelled: sample.kind == "animationcancel",
                            scheduled, sequence });
                    }
                }
            }
            event.previous = current;
            event.sampled = true;
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
            .then_with(|| {
                let ak = Rc::as_ptr(&a.owner) as usize;
                let bk = Rc::as_ptr(&b.owner) as usize;
                ak.cmp(&bk).then_with(|| tree_order[&ak].get(&a.node).unwrap_or(&usize::MAX)
                    .cmp(tree_order[&bk].get(&b.node).unwrap_or(&usize::MAX)))
            })
            .then_with(|| (!a.cancelled).cmp(&(!b.cancelled)))
            .then_with(|| a.order.cmp(&b.order))
            .then_with(|| a.id.cmp(&b.id)).then_with(|| a.sequence.cmp(&b.sequence))
    });
    for queued in queue {
        let name = hub.borrow().records.get(&queued.id).and_then(|record| record.css_name.clone())
            .unwrap_or_default();
        dispatch_css_event(ctx, hub, queued.id, queued.event, &queued.owner, queued.node, &name)?;
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
    animation.underlying = effect.underlying.clone();
    animation.timing = effect.timing;
    animation.easing = effect.easing.clone();
    animation.composite = effect.composite;
    Some((animation_id, realm))
}

#[lumen_bind::methods]
impl DomAnimation {
    #[constructor]
    fn new(
        ctx: &mut Ctx,
        this: This<Value>,
        effect: Option<EffectArgument>,
        #[default(TimelineChoice::Default)] timeline: TimelineChoice,
    ) -> OpResult<Self> {
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
                        timeline_value,
                        node: None,
                        keyframes: Vec::new().into(),
                        underlying: Rc::new(HashMap::new()),
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
        if let Some(weak) = ctx.weak_value(&this.0) {
            hub.borrow_mut().wrappers.insert(id, weak);
        }
        if null_timeline {
            if let Some(record) = hub.borrow_mut().records.get_mut(&id) {
                record.timeline_realm = Weak::new();
                record.timeline_id = 0;
                record.timeline_origin_ms = 0.0;
                record.timeline_value = Value::Null;
            }
        }
        hub.borrow_mut().ready.remove(&id);
        Ok(Self {
            id,
            base: event_target,
        })
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
            record.underlying = effect.underlying.clone();
            record.timing = effect.timing;
            record.easing = effect.easing.clone();
            record.composite = effect.composite;
            state.effect_records.insert(effect.id, effect);
        } else if let Some(record) = state.records.get_mut(&self.id) {
            record.effect_id = None;
        }
        drop(state);
        if let Some(realm) = old_realm {
            apply_realm(&hub, &realm, now)?;
        }
        if let Some(effect) = effect {
            if let Some(realm) = effect_state(ctx, effect.native.id)?.1.realm.upgrade() {
                apply_realm(&hub, &realm, now)?;
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
        let current = sample_record(&record, record_time(&record).unwrap_or(0.0)).current_time_ms;
        let now = timeline_realm.as_ref().and_then(|realm| document_time(realm)).unwrap_or(0.0);
        let timeline_value =
            if let (Some(timeline), Some(realm)) = (timeline.as_ref(), timeline_realm.as_ref()) {
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
            record.timeline_origin_ms = timeline
                .as_ref()
                .map_or(0.0, |timeline| timeline.native.origin_time_ms);
            record.timeline_value = timeline
                .as_ref()
                .map_or(timeline_value, |timeline| timeline.value.clone());
            record.start_time_ms = current;
            record.start_ms = now;
            if timeline.is_none() && !record.cancelled
                && record.hold_time_ms.is_none() && record.start_resolved {
                record.hold_time_ms = Some(current);
                record.start_resolved = false;
            }
        }
        if let Some(realm) = target_realm {
            apply_realm(&hub, &realm, now)?;
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
        if let Some(realm)=realm {apply_realm(&hub,&realm,now)?;}
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
            apply_realm(&hub, &realm, now)?;
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
            apply_realm(&hub, &realm, now)?;
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
        let end_time = record.timing.end_time();
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

#[lumen_bind::methods]
impl DomKeyframeEffect {
    #[constructor]
    fn new(
        ctx: &mut Ctx,
        this: This<Value>,
        target: Option<&super::DomElement>,
        keyframes: Value,
        options: Option<Value>,
    ) -> OpResult<Self> {
        let realm = if let Some(target) = target {
            target.base.realm.clone()
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
            target.map(|target| target.base.id),
            keyframes,
            options,
        )?;
        if let Some(weak) = ctx.weak_value(&this.0) {
            hub(ctx)?.borrow_mut().effects.insert(effect.id, weak);
        }
        Ok(effect)
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
            if let Some(node) = node {
                let mut frames = effect.keyframes.to_vec();
                resolve_underlying(&realm, node, &mut frames)?;
                effect.keyframes = frames.into();
            }
            effect.underlying = Rc::new(capture_underlying(&realm, node, &effect.keyframes)?);
            effect.clone()
        };
        sync_effect_animation(&hub, &updated);
        if let Some(previous_realm) = previous.realm.upgrade() {
            apply_realm(&hub, &previous_realm, now)?;
        }
        apply_realm(&hub, &realm, now)
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
        apply_realm(&hub, &realm, hub_time(&hub))
    }
    #[getter]
    fn pseudo_element(&self) -> Nullable<String> {
        Nullable(None)
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
        let mut frames = parse_keyframes(ctx, &keyframes)?;
        let (hub, effect) = effect_state(ctx, self.id)?;
        let realm = effect
            .realm
            .upgrade()
            .ok_or_else(|| OpError::new("InvalidStateError", "animation realm was destroyed"))?;
        if let Some(node) = effect.node {
            resolve_underlying(&realm, node, &mut frames)?;
        }
        let underlying = capture_underlying(&realm, effect.node, &frames)?;
        let now = hub_time(&hub);
        let updated = {
            let mut state = hub.borrow_mut();
            let effect = state.effect_records.get_mut(&self.id).ok_or_else(|| {
                OpError::new("InvalidStateError", "animation is no longer available")
            })?;
            effect.keyframes = frames.into();
            effect.underlying = Rc::new(underlying);
            effect.clone()
        };
        let attached = sync_effect_animation(&hub, &updated);
        if let Some((id, _)) = attached {
            refresh_ready(ctx, &hub, id)?;
        }
        apply_realm(&hub, &realm, now)
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
        let attached = sync_effect_animation(&hub, &updated);
        let realm = updated
            .realm
            .upgrade()
            .ok_or_else(|| OpError::new("InvalidStateError", "animation realm was destroyed"))?;
        apply_realm(&hub, &realm, now)?;
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
        apply_realm(&hub, &realm, now)?;
    }
    refresh_ready(ctx, &hub, id)?;
    settle_finished(ctx, &hub, now)?;
    Ok(())
}

#[cfg(test)]
mod tests {
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
    fn css_animation_uses_shared_timeline_sampler_and_get_animations() {
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
