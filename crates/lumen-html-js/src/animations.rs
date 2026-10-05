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
    realm: Weak<DomRealm>,
    timeline_realm: Weak<DomRealm>,
    timeline_id: u32,
    timeline_origin_ms: f64,
    timeline_value: Value,
    node: Option<NodeId>,
    keyframes: Vec<Keyframe>,
    underlying: HashMap<String, String>,
    timing: Timing,
    easing: String,
    composite: CompositeMode,
    start_ms: f64,
    start_time_ms: f64,
    hold_time_ms: Option<f64>,
    playback_rate: f64,
    paused_at_ms: Option<f64>,
    cancelled: bool,
    finish_event_fired: bool,
    event_target: DomEventTarget,
}

#[derive(Clone)]
struct EffectRecord {
    id: u32,
    realm: Weak<DomRealm>,
    node: Option<NodeId>,
    keyframes: Vec<Keyframe>,
    underlying: HashMap<String, String>,
    timing: Timing,
    easing: String,
    duration_auto: bool,
    fill_auto: bool,
    composite: CompositeMode,
    animation_id: Option<u32>,
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
            default_realm: Weak::new(),
        }
    }
}

struct FinishedPromise {
    promise: Value,
    deferred: Option<Deferred>,
}

struct ReadyPromise {
    promise: Value,
    deferred: Option<Deferred>,
}

fn new_finished_promise(ctx: &mut Ctx) -> FinishedPromise {
    let deferred = Deferred::new(ctx);
    FinishedPromise {
        promise: deferred.promise(),
        deferred: Some(deferred),
    }
}

fn new_ready_promise(ctx: &mut Ctx) -> ReadyPromise {
    let deferred = Deferred::new(ctx);
    ReadyPromise {
        promise: deferred.promise(),
        deferred: Some(deferred),
    }
}

fn resolved_promise(ctx: &mut Ctx, value: Value) -> Value {
    let deferred = Deferred::new(ctx);
    let promise = deferred.promise();
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
    ctx.class_constructor::<DomKeyframeEffect>();
    ctx.class_constructor::<DomDocumentTimeline>();
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
                keyframes: frames,
                underlying,
                timing,
                easing,
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
    let now = lumen_host::perf::now_ms();
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
                hold_time_ms: None,
                playback_rate: 1.0,
                paused_at_ms: None,
                cancelled: !start_playing,
                finish_event_fired: false,
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
    hub.borrow_mut()
        .finished
        .insert(id, new_finished_promise(ctx));
    hub.borrow_mut().ready.insert(id, new_ready_promise(ctx));
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
    let now = lumen_host::perf::now_ms();
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

pub fn for_element(ctx: &mut Ctx, realm: &Rc<DomRealm>, node: NodeId) -> OpResult<Value> {
    let hub = hub(ctx)?;
    let now = lumen_host::perf::now_ms();
    let ids = hub
        .borrow()
        .records
        .values()
        .filter(|record| record.node == Some(node) && record_is_relevant(record, realm, now))
        .map(|record| record.id)
        .collect::<Vec<_>>();
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
    let now = lumen_host::perf::now_ms();
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
    let event_target = hub
        .borrow()
        .records
        .get(&id)
        .map(|record| record.event_target.clone())
        .ok_or_else(|| OpError::new("InvalidStateError", "animation is no longer available"))?;
    let value = ctx.new_instance(DomAnimation {
        id,
        base: event_target,
    });
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
    let promise = resolved_promise(ctx, animation);
    hub.borrow_mut().ready.insert(
        id,
        ReadyPromise {
            promise,
            deferred: None,
        },
    );
    Ok(())
}

fn mark_ready_pending(ctx: &mut Ctx, hub: &Rc<RefCell<AnimationHub>>, id: u32) {
    hub.borrow_mut().ready.insert(id, new_ready_promise(ctx));
}

fn settle_ready(ctx: &mut Ctx, hub: &Rc<RefCell<AnimationHub>>) -> OpResult<()> {
    let pending = {
        let state = hub.borrow();
        state
            .ready
            .iter()
            .filter_map(|(id, promise)| {
                let record = state.records.get(id)?;
                (!record.cancelled
                    && promise.deferred.is_some()
                    && sample_record(record, lumen_host::perf::now_ms()).state
                        != PlaybackState::Idle)
                    .then_some(*id)
            })
            .collect::<Vec<_>>()
    };
    for id in pending {
        let animation = existing_wrapper(ctx, hub, id)?;
        let deferred = hub
            .borrow_mut()
            .ready
            .get_mut(&id)
            .and_then(|ready| ready.deferred.take());
        if let Some(deferred) = deferred {
            deferred.resolve(ctx, animation);
        }
    }
    Ok(())
}

fn apply_realm(hub: &Rc<RefCell<AnimationHub>>, realm: &Rc<DomRealm>, now: f64) -> OpResult<()> {
    let key = Rc::as_ptr(realm) as usize;
    let snapshot = hub
        .borrow()
        .records
        .values()
        .filter(|record| {
            record.effect_id.is_some()
                && record
                    .realm
                    .upgrade()
                    .is_some_and(|candidate| Rc::ptr_eq(&candidate, realm))
        })
        .cloned()
        .collect::<Vec<_>>();
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
            if record.cancelled {
                continue;
            }
            let keyframes = record
                .keyframes
                .iter()
                .cloned()
                .map(|mut frame| {
                    frame
                        .declarations
                        .retain(|(name, _)| (name == "transform") == transform_pass);
                    frame
                })
                .collect::<Vec<_>>();
            if !keyframes.iter().any(|frame| !frame.declarations.is_empty()) {
                continue;
            }
            let sample = sample_record(record, now);
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
                    let sampled = animation::sample_keyframes_composed_with_property(
                        &keyframes,
                        eased,
                        sample.before,
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

fn sample_record(record: &Record, now: f64) -> animation::Sample {
    if record.timeline_realm.upgrade().is_none() {
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
        .unwrap_or(record.start_time_ms + (now - record.start_ms) * record.playback_rate);
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

pub fn advance(ctx: &mut Ctx, timestamp_ms: f64) -> OpResult<()> {
    let Some(hub) = RealmServices::<RefCell<AnimationHub>>::current(ctx) else {
        return Ok(());
    };
    let realms = hub
        .borrow()
        .records
        .values()
        .filter_map(|record| record.realm.upgrade())
        .collect::<Vec<_>>();
    let mut seen = HashSet::new();
    for realm in realms {
        let key = Rc::as_ptr(&realm) as usize;
        if seen.insert(key) {
            apply_realm(&hub, &realm, timestamp_ms)?;
        }
    }
    settle_finished(ctx, &hub, timestamp_ms)?;
    settle_ready(ctx, &hub)?;
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
    let now = lumen_host::perf::now_ms();
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
    let deferred = hub
        .borrow_mut()
        .finished
        .get_mut(&id)
        .and_then(|finished| finished.deferred.take());
    if let Some(deferred) = deferred {
        deferred.reject(ctx, OpError::new("AbortError", "animation was canceled"));
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
    DomEventTarget::dispatch_event(ctx, This(wrapper), event)?;
    Ok(())
}

fn ensure_pending_finished(ctx: &mut Ctx, hub: &Rc<RefCell<AnimationHub>>, id: u32) {
    let should_replace = hub
        .borrow()
        .finished
        .get(&id)
        .is_none_or(|finished| finished.deferred.is_none());
    if should_replace {
        hub.borrow_mut()
            .finished
            .insert(id, new_finished_promise(ctx));
    }
}

pub fn pending(ctx: &mut Ctx) -> bool {
    RealmServices::<RefCell<AnimationHub>>::current(ctx).is_some_and(|hub| {
        let now = lumen_host::perf::now_ms();
        let state = hub.borrow();
        state.records.values().any(|record| {
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
        timeline: Option<TimelineArgument>,
    ) -> OpResult<Self> {
        let hub = hub(ctx)?;
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
                let now = lumen_host::perf::now_ms();
                state.records.insert(
                    id,
                    Record {
                        id,
                        public_id: String::new(),
                        effect_id: None,
                        realm: Rc::downgrade(&realm),
                        timeline_realm: Rc::downgrade(&realm),
                        timeline_id,
                        timeline_origin_ms: timeline_origin,
                        timeline_value,
                        node: None,
                        keyframes: Vec::new(),
                        underlying: HashMap::new(),
                        timing: Timing::default(),
                        easing: "linear".into(),
                        composite: CompositeMode::Replace,
                        start_ms: now,
                        start_time_ms: 0.0,
                        hold_time_ms: None,
                        playback_rate: 1.0,
                        paused_at_ms: None,
                        cancelled: true,
                        finish_event_fired: false,
                        event_target: DomEventTarget::independent(&realm),
                    },
                );
                id
            };
            let base = hub.borrow().records[&id].event_target.clone();
            hub.borrow_mut()
                .finished
                .insert(id, new_finished_promise(ctx));
            hub.borrow_mut().ready.insert(id, new_ready_promise(ctx));
            (id, base)
        };
        if let Some(weak) = ctx.weak_value(&this.0) {
            hub.borrow_mut().wrappers.insert(id, weak);
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
        let now = lumen_host::perf::now_ms();
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
        let now = lumen_host::perf::now_ms();
        let current = sample_record(&record, now).current_time_ms;
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
        }
        if let Some(realm) = target_realm {
            apply_realm(&hub, &realm, now)?;
        }
        Ok(())
    }
    #[getter(name = "ready")]
    fn ready(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        let hub = hub(ctx)?;
        if !hub.borrow().records.contains_key(&self.id) {
            return Err(OpError::new(
                "InvalidStateError",
                "animation is no longer available",
            ));
        }
        if let Some(weak) = ctx.weak_value(&this.0) {
            hub.borrow_mut().wrappers.insert(self.id, weak);
        }
        if !hub.borrow().ready.contains_key(&self.id) {
            let promise = resolved_promise(ctx, this.0.clone());
            hub.borrow_mut().ready.insert(
                self.id,
                ReadyPromise {
                    promise,
                    deferred: None,
                },
            );
        }
        let promise = hub.borrow().ready[&self.id].promise.clone();
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
        if !hub.borrow().finished.contains_key(&self.id) {
            hub.borrow_mut()
                .finished
                .insert(self.id, new_finished_promise(ctx));
        }
        let promise = hub.borrow().finished[&self.id].promise.clone();
        Ok(promise)
    }
    #[getter(name = "currentTime")]
    fn current_time(&self, ctx: &mut Ctx) -> OpResult<Option<f64>> {
        let (_, record) = record_state(ctx, self.id)?;
        if record.cancelled {
            return Ok(None);
        }
        let sample = sample_record(&record, lumen_host::perf::now_ms());
        Ok((sample.state != PlaybackState::Idle).then_some(sample.current_time_ms))
    }
    #[setter(name = "currentTime")]
    fn set_current_time(&self, ctx: &mut Ctx, value: Option<f64>) -> OpResult<()> {
        let hub = hub(ctx)?;
        let now = lumen_host::perf::now_ms();
        let realm = {
            let mut hub_mut = hub.borrow_mut();
            let record = hub_mut.records.get_mut(&self.id).ok_or_else(|| {
                OpError::new("InvalidStateError", "animation is no longer available")
            })?;
            if let Some(value) = value {
                if !value.is_finite() {
                    return Err(OpError::new("TypeError", "currentTime must be finite"));
                }
                record.start_time_ms = value;
                record.start_ms = now;
                if record.paused_at_ms.is_some() || record.timeline_realm.upgrade().is_none() {
                    record.hold_time_ms = Some(value);
                }
                record.cancelled = false;
                record.finish_event_fired = false;
            } else {
                record.cancelled = true;
                record.hold_time_ms = None;
                record.paused_at_ms = None;
            }
            record.realm.upgrade()
        };
        if let Some(realm) = realm {
            apply_realm(&hub, &realm, now)?;
        }
        refresh_ready(ctx, &hub, self.id)?;
        if value.is_some() {
            ensure_pending_finished(ctx, &hub, self.id);
            settle_finished(ctx, &hub, now)?;
        } else {
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
        let now = lumen_host::perf::now_ms();
        let realm = {
            let mut state = hub.borrow_mut();
            let record = state.records.get_mut(&self.id).ok_or_else(|| {
                OpError::new("InvalidStateError", "animation is no longer available")
            })?;
            let current = sample_record(record, now).current_time_ms;
            record.playback_rate = value;
            record.start_time_ms = current;
            record.start_ms = now;
            if record.paused_at_ms.is_some() {
                record.hold_time_ms = Some(current);
            }
            record.realm.upgrade()
        };
        if let Some(realm) = realm {
            apply_realm(&hub, &realm, now)?;
        }
        refresh_ready(ctx, &hub, self.id)?;
        settle_finished(ctx, &hub, now)
    }
    #[getter(name = "playState")]
    fn play_state(&self, ctx: &mut Ctx) -> OpResult<String> {
        let (_, record) = record_state(ctx, self.id)?;
        if record.cancelled {
            return Ok("idle".into());
        }
        Ok(
            match sample_record(&record, lumen_host::perf::now_ms()).state {
                PlaybackState::Idle => "idle",
                PlaybackState::Running => "running",
                PlaybackState::Paused => "paused",
                PlaybackState::Finished => "finished",
            }
            .into(),
        )
    }
    fn play(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<()> {
        let hub = hub(ctx)?;
        if let Some(weak) = ctx.weak_value(&this.0) {
            hub.borrow_mut().wrappers.insert(self.id, weak);
        }
        let (_, record) = record_state(ctx, self.id)?;
        let restart = record.cancelled
            || sample_record(&record, lumen_host::perf::now_ms()).state == PlaybackState::Finished;
        if restart {
            ensure_pending_finished(ctx, &hub, self.id);
        }
        mutate(self.id, ctx, |record, now| {
            if let Some(paused) = record.paused_at_ms.take() {
                record.start_time_ms = record.hold_time_ms.take().unwrap_or_else(|| {
                    (paused - record.start_ms) * record.playback_rate + record.start_time_ms
                });
                record.start_ms = now;
                record.paused_at_ms = None;
            } else if record.cancelled
                || sample_record(record, now).state == PlaybackState::Finished
            {
                record.start_time_ms = if record.playback_rate < 0.0 {
                    record.timing.end_time()
                } else {
                    0.0
                };
                record.start_ms = now;
            }
            record.hold_time_ms = None;
            record.cancelled = false;
            record.finish_event_fired = false;
        })?;
        mark_ready_pending(ctx, &hub, self.id);
        Ok(())
    }
    fn pause(&self, ctx: &mut Ctx) -> OpResult<()> {
        mutate(self.id, ctx, |record, now| {
            if record.paused_at_ms.is_none() {
                record.hold_time_ms = Some(sample_record(record, now).current_time_ms);
                record.paused_at_ms = Some(now);
            }
        })
    }
    fn cancel(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<()> {
        let (_, prior) = record_state(ctx, self.id)?;
        let should_fire = !prior.cancelled;
        mutate(self.id, ctx, |record, _| {
            record.cancelled = true;
            record.paused_at_ms = None;
            record.hold_time_ms = None;
        })?;
        let hub = hub(ctx)?;
        reject_finished(ctx, &hub, self.id);
        if should_fire {
            let event = DomEvent::new(ctx, "cancel", None)?;
            let event = ctx.new_instance(event);
            let event = JsObject::from_value(event)
                .ok_or_else(|| OpError::new("TypeError", "could not create animation event"))?;
            DomEventTarget::dispatch_event(ctx, this, event)?;
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
        let (_, record) = record_state(ctx, self.id)?;
        let restart = record.cancelled
            || sample_record(&record, lumen_host::perf::now_ms()).state == PlaybackState::Finished;
        if restart {
            let hub = hub(ctx)?;
            ensure_pending_finished(ctx, &hub, self.id);
        }
        mutate(self.id, ctx, |record, now| {
            let current_time = if record.cancelled {
                if record.playback_rate > 0.0 {
                    record.timing.end_time()
                } else {
                    0.0
                }
            } else {
                sample_record(record, now).current_time_ms
            };
            record.playback_rate = -record.playback_rate;
            record.start_time_ms = record.hold_time_ms.take().unwrap_or(current_time);
            record.start_ms = now;
            record.paused_at_ms = None;
            record.cancelled = false;
            record.finish_event_fired = false;
        })
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
        let now = lumen_host::perf::now_ms();
        let updated = {
            let mut state = hub.borrow_mut();
            let effect = state.effect_records.get_mut(&self.id).ok_or_else(|| {
                OpError::new("InvalidStateError", "keyframe effect is unavailable")
            })?;
            effect.realm = Rc::downgrade(&realm);
            effect.node = node;
            if let Some(node) = node {
                resolve_underlying(&realm, node, &mut effect.keyframes)?;
            }
            effect.underlying = capture_underlying(&realm, node, &effect.keyframes)?;
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
        apply_realm(&hub, &realm, lumen_host::perf::now_ms())
    }
    #[getter]
    fn pseudo_element(&self) -> Option<String> {
        None
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
            .map(|record| sample_record(record, lumen_host::perf::now_ms()));
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
        let now = lumen_host::perf::now_ms();
        let updated = {
            let mut state = hub.borrow_mut();
            let effect = state.effect_records.get_mut(&self.id).ok_or_else(|| {
                OpError::new("InvalidStateError", "animation is no longer available")
            })?;
            effect.keyframes = frames;
            effect.underlying = underlying;
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
        let now = lumen_host::perf::now_ms();
        let updated = {
            let mut state = hub.borrow_mut();
            let effect = state.effect_records.get_mut(&self.id).ok_or_else(|| {
                OpError::new("InvalidStateError", "animation is no longer available")
            })?;
            effect.timing = timing;
            effect.easing = easing;
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
    fn current_time(&self) -> Option<f64> {
        self.realm
            .upgrade()
            .map(|_| lumen_host::perf::now_ms() - self.origin_time_ms)
    }
    #[getter(name = "duration")]
    fn duration(&self) -> Option<f64> {
        None
    }
    #[getter(name = "originTime")]
    fn origin_time(&self) -> f64 {
        self.origin_time_ms
    }
}

fn mutate(id: u32, ctx: &mut Ctx, change: impl FnOnce(&mut Record, f64)) -> OpResult<()> {
    let hub = hub(ctx)?;
    let now = lumen_host::perf::now_ms();
    let realm = {
        let mut state = hub.borrow_mut();
        let record = state
            .records
            .get_mut(&id)
            .ok_or_else(|| OpError::new("InvalidStateError", "animation is no longer available"))?;
        change(record, now);
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
                panic!("JavaScript threw: {message}");
            }
        }
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
                detached.currentTime === null && idle.id === '' && rejected
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
}
