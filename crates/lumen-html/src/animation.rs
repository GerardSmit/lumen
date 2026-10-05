//! Shared Web Animations effect timing and CSS-value interpolation.
use alloc::{borrow::ToOwned, format, string::String, vec::Vec};

#[derive(Clone, Debug, PartialEq)]
pub struct Keyframe {
    pub offset: f64,
    pub declarations: Vec<(String, String)>,
    /// Timing function for the interval beginning at this frame.
    pub easing: Option<String>,
    /// None uses the effect's composite operation (the keyframe's `auto`).
    pub composite: Option<CompositeMode>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompositeMode {
    Replace,
    Add,
    Accumulate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FillMode {
    None,
    Forwards,
    Backwards,
    Both,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    Normal,
    Reverse,
    Alternate,
    AlternateReverse,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaybackState {
    Idle,
    Running,
    Paused,
    Finished,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Timing {
    pub delay_ms: f64,
    pub end_delay_ms: f64,
    pub duration_ms: f64,
    pub iteration_start: f64,
    pub iterations: f64,
    pub fill: FillMode,
    pub direction: Direction,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            delay_ms: 0.0,
            end_delay_ms: 0.0,
            duration_ms: 0.0,
            iteration_start: 0.0,
            iterations: 1.0,
            fill: FillMode::None,
            direction: Direction::Normal,
        }
    }
}

impl Timing {
    pub fn active_duration(self) -> f64 {
        // IEEE infinity * zero is NaN, but an empty active interval is zero.
        if self.duration_ms == 0.0 || self.iterations == 0.0 {
            0.0
        } else {
            self.duration_ms * self.iterations
        }
    }

    pub fn end_time(self) -> f64 {
        (self.delay_ms + self.active_duration() + self.end_delay_ms).max(0.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    pub current_time_ms: f64,
    pub progress: Option<f64>,
    pub state: PlaybackState,
    pub before: bool,
    pub current_iteration: Option<f64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EffectPhase {
    Before,
    Active,
    After,
}

pub fn effect_phase(timing: Timing, current_time_ms: f64, playback_rate: f64) -> Option<EffectPhase> {
    if !current_time_ms.is_finite() || !playback_rate.is_finite() {
        return None;
    }
    let active_duration = timing.active_duration();
    let end_time = timing.end_time();
    let before_active_boundary = timing.delay_ms.min(end_time).max(0.0);
    let active_after_boundary = (timing.delay_ms + active_duration).min(end_time).max(0.0);
    let backwards = playback_rate < 0.0;
    if current_time_ms < before_active_boundary
        || (backwards && current_time_ms == before_active_boundary)
    {
        Some(EffectPhase::Before)
    } else if current_time_ms > active_after_boundary
        || (!backwards && current_time_ms == active_after_boundary)
    {
        Some(EffectPhase::After)
    } else {
        Some(EffectPhase::Active)
    }
}

pub fn sample(timing: Timing, start_ms: f64, now_ms: f64, paused_at_ms: Option<f64>) -> Sample {
    let current_time_ms = paused_at_ms.unwrap_or(now_ms) - start_ms;
    sample_current_time(timing, current_time_ms, 1.0, paused_at_ms.is_some())
}

pub fn sample_current_time(
    timing: Timing,
    current_time_ms: f64,
    playback_rate: f64,
    paused: bool,
) -> Sample {
    let duration = timing.duration_ms.max(0.0);
    let phase = effect_phase(timing, current_time_ms, playback_rate);
    let state = if paused {
        PlaybackState::Paused
    } else if (playback_rate > 0.0 && current_time_ms >= timing.end_time())
        || (playback_rate < 0.0 && current_time_ms <= 0.0)
    {
        PlaybackState::Finished
    } else {
        PlaybackState::Running
    };
    let active_duration = timing.active_duration();
    let active_time = match phase {
        None => None,
        Some(EffectPhase::Before) => matches!(timing.fill, FillMode::Backwards | FillMode::Both)
            .then_some((current_time_ms - timing.delay_ms).max(0.0)),
        Some(EffectPhase::After) => matches!(timing.fill, FillMode::Forwards | FillMode::Both)
            .then_some(
                (current_time_ms - timing.delay_ms)
                    .min(active_duration)
                    .max(0.0),
            ),
        Some(EffectPhase::Active) => Some(current_time_ms - timing.delay_ms),
    };
    let mut current_iteration = None;
    let progress = active_time.map(|active_time| {
        let overall = timing.iteration_start
            + if duration == 0.0 {
                if phase == Some(EffectPhase::Before) {
                    0.0
                } else {
                    timing.iterations
                }
            } else {
                active_time / duration
            };
        let mut simple = if overall.is_infinite() {
            timing.iteration_start.fract()
        } else {
            overall.fract()
        };
        if simple == 0.0
            && phase != Some(EffectPhase::Before)
            && active_time == active_duration
            && timing.iterations != 0.0
        {
            simple = 1.0;
        }
        let iteration = if phase == Some(EffectPhase::After) && timing.iterations.is_infinite() {
            f64::INFINITY
        } else if simple == 1.0 {
            overall.floor() - 1.0
        } else {
            overall.floor()
        };
        current_iteration = Some(iteration);
        directed(simple, timing.direction, iteration)
    });
    let reverse = reversed(timing.direction, current_iteration.unwrap_or(0.0));
    Sample {
        current_time_ms,
        progress,
        state,
        before: (phase == Some(EffectPhase::Before) && !reverse)
            || (phase == Some(EffectPhase::After) && reverse),
        current_iteration,
    }
}

/// Frame-to-frame CSS event timing, independent of DOM or language bindings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CssEventSample {
    pub phase: Option<EffectPhase>,
    pub iteration: Option<f64>,
}

impl CssEventSample {
    pub const IDLE: Self = Self { phase: None, iteration: None };
}

pub fn css_event_sample(timing: Timing, time_ms: f64, rate: f64) -> CssEventSample {
    let sample = sample_current_time(Timing { fill: FillMode::Both, ..timing }, time_ms, rate, false);
    CssEventSample { phase: effect_phase(timing, time_ms, rate), iteration: sample.current_iteration }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CssEvent {
    pub kind: &'static str,
    pub elapsed_ms: f64,
}

/// CSS Animations 2 event dispatch. A skipped frame emits a single iteration
/// event at the newly sampled boundary, never an unbounded historical loop.
pub fn css_events(
    timing: Timing,
    previous: CssEventSample,
    current: CssEventSample,
    cancellation_elapsed_ms: f64,
) -> [Option<CssEvent>; 2] {
    use EffectPhase::{Active, After, Before};
    let start = (-timing.delay_ms).min(timing.active_duration()).max(0.0);
    let end = (timing.end_time() - timing.delay_ms).min(timing.active_duration()).max(0.0);
    let event = |kind, elapsed_ms| Some(CssEvent { kind, elapsed_ms });
    match (previous.phase, current.phase) {
        (None | Some(Before), Some(Active)) => [event("animationstart", start), None],
        (None | Some(Before), Some(After)) => [event("animationstart", start), event("animationend", end)],
        (Some(Active), Some(Before)) => [event("animationend", start), None],
        (Some(Active), Some(Active)) if previous.iteration != current.iteration => {
            let boundary = current.iteration.unwrap_or(0.0)
                + if previous.iteration > current.iteration { 1.0 } else { 0.0 };
            [event("animationiteration", (boundary - timing.iteration_start) * timing.duration_ms), None]
        }
        (Some(Active), Some(After)) => [event("animationend", end), None],
        (Some(After), Some(Active)) => [event("animationstart", end), None],
        (Some(After), Some(Before)) => [event("animationstart", end), event("animationend", start)],
        (Some(Before | Active), None) => [event("animationcancel", cancellation_elapsed_ms), None],
        _ => [None, None],
    }
}

/// Whether a sampled effect is current or in effect for the monotonically
/// increasing timelines currently exposed by the DOM adapter. The caller is
/// responsible for checking that the animation still has this associated
/// effect and has not been cancelled or replaced.
pub fn effect_is_relevant(timing: Timing, sample: Sample, playback_rate: f64) -> bool {
    let Some(phase) = effect_phase(timing, sample.current_time_ms, playback_rate) else {
        return false;
    };
    if sample.state == PlaybackState::Idle {
        return false;
    }
    let current = (phase == EffectPhase::Active && sample.state != PlaybackState::Finished)
        || (phase == EffectPhase::Before && playback_rate > 0.0)
        || (phase == EffectPhase::After && playback_rate < 0.0);
    let in_effect = phase == EffectPhase::Active
        || (phase == EffectPhase::Before
            && matches!(timing.fill, FillMode::Backwards | FillMode::Both))
        || (phase == EffectPhase::After
            && matches!(timing.fill, FillMode::Forwards | FillMode::Both));
    current || in_effect
}

fn directed(progress: f64, direction: Direction, iteration: f64) -> f64 {
    if reversed(direction, iteration) {
        1.0 - progress
    } else {
        progress
    }
}

fn reversed(direction: Direction, iteration: f64) -> bool {
    match direction {
        Direction::Normal => false,
        Direction::Reverse => true,
        Direction::Alternate => iteration.is_finite() && iteration % 2.0 != 0.0,
        Direction::AlternateReverse => iteration.is_finite() && iteration % 2.0 == 0.0,
    }
}

/// Evaluate CSS named, cubic Bézier, and step timing functions.
pub fn ease(name: &str, progress: f64) -> Option<f64> {
    ease_with_before(name, progress, false)
}

/// The before flag selects the lower side of a step discontinuity.
pub fn ease_with_before(name: &str, progress: f64, before: bool) -> Option<f64> {
    if !progress.is_finite() {
        return None;
    }
    let normalized = name.trim().to_ascii_lowercase();
    let name = match normalized.as_str() {
        "linear" => return Some(progress),
        "ease" => return Some(cubic_bezier(0.25, 0.1, 0.25, 1.0, progress)),
        "ease-in" => return Some(cubic_bezier(0.42, 0.0, 1.0, 1.0, progress)),
        "ease-out" => return Some(cubic_bezier(0.0, 0.0, 0.58, 1.0, progress)),
        "ease-in-out" => return Some(cubic_bezier(0.42, 0.0, 0.58, 1.0, progress)),
        "step-start" => "steps(1, start)",
        "step-end" => "steps(1, end)",
        name => name,
    };
    if let Some(body) = name
        .strip_prefix("cubic-bezier(")
        .and_then(|s| s.strip_suffix(')'))
    {
        let values = body
            .split(',')
            .map(|part| part.trim().parse::<f64>().ok())
            .collect::<Option<Vec<_>>>()?;
        let [x1, y1, x2, y2] = values.as_slice() else {
            return None;
        };
        if !x1.is_finite()
            || !y1.is_finite()
            || !x2.is_finite()
            || !y2.is_finite()
            || !(0.0..=1.0).contains(x1)
            || !(0.0..=1.0).contains(x2)
        {
            return None;
        }
        return Some(cubic_bezier(*x1, *y1, *x2, *y2, progress));
    }
    if let Some(body) = name
        .strip_prefix("steps(")
        .and_then(|s| s.strip_suffix(')'))
    {
        let mut args = body.split(',').map(str::trim);
        let count = args.next()?.parse::<u32>().ok()?;
        let position = args.next().unwrap_or("end");
        if args.next().is_some() || count == 0 {
            return None;
        }
        let jumps = match position {
            "start" | "jump-start" => "start",
            "end" | "jump-end" => "end",
            "jump-none" => "none",
            "jump-both" => "both",
            _ => return None,
        };
        if jumps == "none" && count < 2 {
            return None;
        }
        let n = count as f64;
        let mut step = (progress * n).floor();
        if matches!(jumps, "start" | "both") {
            step += 1.0;
        }
        if before && (progress * n).fract() == 0.0 {
            step -= 1.0;
        }
        if progress >= 0.0 {
            step = step.max(0.0);
        }
        let jumps = match jumps {
            "start" | "end" => n,
            "none" => n - 1.0,
            "both" => n + 1.0,
            _ => unreachable!(),
        };
        if progress <= 1.0 {
            step = step.min(jumps);
        }
        return Some(step / jumps);
    }
    None
}

fn cubic_bezier(x1: f64, y1: f64, x2: f64, y2: f64, progress: f64) -> f64 {
    if progress < 0.0 {
        return if x1 > 0.0 {
            progress * y1 / x1
        } else if x2 > 0.0 {
            progress * y2 / x2
        } else {
            0.0
        };
    }
    if progress > 1.0 {
        return if x2 < 1.0 {
            1.0 + (progress - 1.0) * (1.0 - y2) / (1.0 - x2)
        } else if x1 < 1.0 {
            1.0 + (progress - 1.0) * (1.0 - y1) / (1.0 - x1)
        } else {
            1.0
        };
    }
    if progress == 0.0 || progress == 1.0 {
        return progress;
    }
    let bezier = |t: f64, p1: f64, p2: f64| {
        3.0 * (1.0 - t).powi(2) * t * p1 + 3.0 * (1.0 - t) * t.powi(2) * p2 + t.powi(3)
    };
    let (mut low, mut high) = (0.0, 1.0);
    for _ in 0..40 {
        let middle = (low + high) / 2.0;
        if bezier(middle, x1, x2) < progress {
            low = middle;
        } else {
            high = middle;
        }
    }
    bezier((low + high) / 2.0, y1, y2)
}

/// Interpolate finite numeric CSS values only when units are compatible.
/// Unsupported CSS syntax can use discrete keyframe sampling in the adapter.
pub fn interpolate_numeric(from: &str, to: &str, progress: f64) -> Option<String> {
    let (a, unit_a) = numeric_unit(from)?;
    let (b, unit_b) = numeric_unit(to)?;
    if unit_a != unit_b || !progress.is_finite() {
        return None;
    }
    let mut value = format!("{:.6}", a + (b - a) * progress);
    while value.contains('.') && value.ends_with('0') {
        value.pop();
    }
    if value.ends_with('.') {
        value.pop();
    }
    value.push_str(unit_a);
    Some(value)
}

/// Serialize computed transform functions without prematurely resolving percentages.
pub fn serialize_transforms(transforms: &[crate::css::Transform]) -> String {
    use crate::css::{Transform, TransformLength};
    if transforms.is_empty() {
        return "none".to_owned();
    }
    let length = |v: TransformLength| {
        if v.percent == 0.0 {
            format!("{}px", v.pixels)
        } else if v.pixels == 0.0 {
            format!("{}%", v.percent)
        } else {
            format!("calc({}px + {}%)", v.pixels, v.percent)
        }
    };
    transforms
        .iter()
        .map(|v| match *v {
            Transform::Matrix(m) => format!(
                "matrix({}, {}, {}, {}, {}, {})",
                m.a, m.b, m.c, m.d, m.e, m.f
            ),
            Transform::Translate(x, y) => format!("translate({}, {})", length(x), length(y)),
            Transform::Scale(x, y) => format!("scale({x}, {y})"),
            Transform::Rotate(a) => format!("rotate({a}rad)"),
            Transform::Skew(x, y) => format!("skew({x}rad, {y}rad)"),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Interpolate matching function primitives, then decompose the remaining
/// matrices at the first mismatch. Matching translations retain percentages
/// so later layout changes still use the current reference box.
pub fn interpolate_transforms(
    from: &[crate::css::Transform],
    to: &[crate::css::Transform],
    progress: f64,
    width: f32,
    height: f32,
) -> Option<String> {
    combine_transforms(from, to, progress, width, height, false)
}

/// Accumulate transform parameters relative to each primitive's identity.
pub fn accumulate_transforms(
    from: &[crate::css::Transform],
    to: &[crate::css::Transform],
    width: f32,
    height: f32,
) -> Option<String> {
    combine_transforms(from, to, 1.0, width, height, true)
}

fn combine_transforms(
    from: &[crate::css::Transform],
    to: &[crate::css::Transform],
    progress: f64,
    width: f32,
    height: f32,
    accumulate: bool,
) -> Option<String> {
    use crate::css::{Transform, TransformLength};
    use crate::paint::Affine;
    if !progress.is_finite() {
        return None;
    }
    let lerp = |a: f32, b: f32| {
        if accumulate {
            a + b
        } else {
            (a as f64 + (b as f64 - a as f64) * progress) as f32
        }
    };
    let len = |a: TransformLength, b: TransformLength| TransformLength {
        pixels: lerp(a.pixels, b.pixels),
        percent: lerp(a.percent, b.percent),
    };
    let zero = TransformLength {
        pixels: 0.0,
        percent: 0.0,
    };
    let identity = |v: Transform| match v {
        Transform::Matrix(_) => Transform::Matrix(Affine::IDENTITY),
        Transform::Translate(_, _) => Transform::Translate(zero, zero),
        Transform::Scale(_, _) => Transform::Scale(1.0, 1.0),
        Transform::Rotate(_) => Transform::Rotate(0.0),
        Transform::Skew(_, _) => Transform::Skew(0.0, 0.0),
    };
    let matrix = |list: &[Transform]| {
        list.iter()
            .fold(Affine::IDENTITY, |m, t| m.then(t.matrix(width, height)))
    };
    let coefficients = |m: Affine| [m.a, m.b, m.c, m.d, m.e, m.f].map(f64::from);
    let mut result = Vec::new();
    for index in 0..from.len().max(to.len()) {
        let a = from
            .get(index)
            .copied()
            .unwrap_or_else(|| identity(to[index]));
        let b = to
            .get(index)
            .copied()
            .unwrap_or_else(|| identity(from[index]));
        let primitive = match (a, b) {
            (Transform::Translate(ax, ay), Transform::Translate(bx, by)) => {
                Some(Transform::Translate(len(ax, bx), len(ay, by)))
            }
            (Transform::Scale(ax, ay), Transform::Scale(bx, by)) => Some(Transform::Scale(
                lerp(ax, bx) - if accumulate { 1.0 } else { 0.0 },
                lerp(ay, by) - if accumulate { 1.0 } else { 0.0 },
            )),
            (Transform::Rotate(a), Transform::Rotate(b)) => Some(Transform::Rotate(lerp(a, b))),
            (Transform::Skew(ax, ay), Transform::Skew(bx, by)) => {
                Some(Transform::Skew(lerp(ax, bx), lerp(ay, by)))
            }
            _ => None,
        };
        if let Some(v) = primitive {
            result.push(v);
            continue;
        }
        let a = matrix(from.get(index..).unwrap_or(&[]));
        let b = matrix(to.get(index..).unwrap_or(&[]));
        let [a, b, c, d, e, f] = if accumulate {
            lumen_common::affine::accumulate(coefficients(a), coefficients(b))?
        } else {
            lumen_common::affine::interpolate(coefficients(a), coefficients(b), progress)?
        };
        let values = [a, b, c, d, e, f].map(|v| v as f32);
        if !values.iter().all(|v| v.is_finite()) {
            return None;
        }
        let [a, b, c, d, e, f] = values;
        result.push(Transform::Matrix(Affine { a, b, c, d, e, f }));
        break;
    }
    // A finite progress can still overflow the renderer's finite coefficients.
    if result.iter().any(|t| {
        !coefficients(t.matrix(width, height))
            .iter()
            .all(|v| v.is_finite())
    }) {
        return None;
    }
    Some(serialize_transforms(&result))
}

/// Add compatible computed CSS numeric values, optionally repeating `delta`
/// for an accumulated effect iteration. Unit conversion is intentionally left
/// to CSS computed-value resolution; incompatible units are not composable.
pub fn add_numeric_values(base: &str, value: &str, delta: Option<(&str, u64)>) -> Option<String> {
    let (base, base_unit) = numeric_unit(base)?;
    let (value, value_unit) = numeric_unit(value)?;
    if base_unit != value_unit {
        return None;
    }
    let accumulated = if let Some((delta, iterations)) = delta {
        let (delta, delta_unit) = numeric_unit(delta)?;
        if delta_unit != base_unit {
            return None;
        }
        delta * iterations as f64
    } else {
        0.0
    };
    let mut result = format!("{:.6}", base + value + accumulated);
    while result.contains('.') && result.ends_with('0') {
        result.pop();
    }
    if result.ends_with('.') {
        result.pop();
    }
    result.push_str(base_unit);
    Some(result)
}

fn numeric_unit(value: &str) -> Option<(f64, &str)> {
    let value = value.trim();
    let split = value
        .find(|ch: char| !(ch.is_ascii_digit() || matches!(ch, '+' | '-' | '.' | 'e' | 'E')))
        .unwrap_or(value.len());
    let number = value[..split].parse::<f64>().ok()?;
    number.is_finite().then_some((number, &value[split..]))
}

pub fn sample_keyframes(keyframes: &[Keyframe], progress: f64) -> Vec<(String, String)> {
    sample_keyframes_with(keyframes, progress, interpolate_numeric)
}

pub fn sample_keyframes_with(
    keyframes: &[Keyframe],
    progress: f64,
    interpolate: impl FnMut(&str, &str, f64) -> Option<String>,
) -> Vec<(String, String)> {
    sample_keyframes_with_before(keyframes, progress, false, interpolate)
}

pub fn sample_keyframes_with_before(
    keyframes: &[Keyframe],
    progress: f64,
    before: bool,
    interpolate: impl FnMut(&str, &str, f64) -> Option<String>,
) -> Vec<(String, String)> {
    sample_keyframes_composed(
        keyframes,
        progress,
        before,
        |_, _, value| Some(value.to_owned()),
        interpolate,
    )
}

/// Composite each interval endpoint against its underlying value before
/// interpolation. This allows different keyframe composite operations within
/// one effect without changing the language-neutral interval calculation.
pub fn sample_keyframes_composed(
    keyframes: &[Keyframe],
    progress: f64,
    before: bool,
    compose: impl FnMut(&str, &Keyframe, &str) -> Option<String>,
    mut interpolate: impl FnMut(&str, &str, f64) -> Option<String>,
) -> Vec<(String, String)> {
    sample_keyframes_composed_with_property(
        keyframes,
        progress,
        before,
        compose,
        |_, from, to, progress| interpolate(from, to, progress),
    )
}

/// Property-aware interpolation shares interval selection and composition.
pub fn sample_keyframes_composed_with_property(
    keyframes: &[Keyframe],
    progress: f64,
    before: bool,
    compose: impl FnMut(&str, &Keyframe, &str) -> Option<String>,
    interpolate: impl FnMut(&str, &str, &str, f64) -> Option<String>,
) -> Vec<(String, String)> {
    sample_keyframes_composed_with_property_filter(keyframes, progress, before, |_| true, compose, interpolate)
}

/// Samples selected properties directly from the original immutable keyframes.
/// Filtering precedes interval selection and composition, so excluded properties
/// allocate no tracks and never reach the property callbacks.
pub fn sample_keyframes_composed_with_property_filter(
    keyframes: &[Keyframe],
    progress: f64,
    before: bool,
    mut include: impl FnMut(&str) -> bool,
    mut compose: impl FnMut(&str, &Keyframe, &str) -> Option<String>,
    mut interpolate: impl FnMut(&str, &str, &str, f64) -> Option<String>,
) -> Vec<(String, String)> {
    if keyframes.is_empty() || !progress.is_finite() {
        return Vec::new();
    }
    let mut properties = Vec::<&str>::new();
    for frame in keyframes {
        for (name, _) in &frame.declarations {
            if include(name) && !properties.iter().any(|property| *property == name) {
                properties.push(name.as_str());
            }
        }
    }
    properties
        .into_iter()
        .filter_map(|property| {
            let indexes = keyframes
                .iter()
                .enumerate()
                .filter_map(|(index, frame)| {
                    frame
                        .declarations
                        .iter()
                        .any(|(name, _)| name == property)
                        .then_some(index)
                })
                .collect::<Vec<_>>();
            let first = *indexes.first()?;
            let last = *indexes.last()?;
            let left_index = indexes
                .iter()
                .copied()
                .take_while(|index| keyframes[*index].offset <= progress)
                .last();
            let right_index = indexes
                .iter()
                .copied()
                .find(|index| keyframes[*index].offset > progress);
            let (left_index, right_index) = match (left_index, right_index) {
                (Some(left), Some(right)) => (left, right),
                (None, Some(_)) if indexes.len() > 1 => (first, indexes[1]),
                (Some(_), None) if indexes.len() > 1 => (indexes[indexes.len() - 2], last),
                (None, Some(right)) => (right, right),
                (Some(left), None) => (left, left),
                (None, None) => (first, last),
            };
            let value_at = |index: usize| {
                keyframes[index]
                    .declarations
                    .iter()
                    .find(|(name, _)| name == property)
                    .map(|(_, value)| value)
            };
            let left_value = compose(property, &keyframes[left_index], value_at(left_index)?)?;
            let right_value = compose(property, &keyframes[right_index], value_at(right_index)?)?;
            if left_index == right_index {
                return Some((property.to_owned(), right_value));
            }
            let left = &keyframes[left_index];
            let right = &keyframes[right_index];
            let span = right.offset - left.offset;
            let local = if span > 0.0 {
                (progress - left.offset) / span
            } else {
                1.0
            };
            let local = left
                .easing
                .as_deref()
                .and_then(|easing| ease_with_before(easing, local, before))
                .unwrap_or(local);
            let value =
                interpolate(property, &left_value, &right_value, local).unwrap_or_else(|| {
                    if property == "display" && local > 0.0 && local < 1.0 {
                        if left_value.eq_ignore_ascii_case("none") {
                            return right_value.clone();
                        }
                        if right_value.eq_ignore_ascii_case("none") {
                            return left_value.clone();
                        }
                    }
                    if property == "visibility"
                        && local > 0.0
                        && local < 1.0
                        && (left_value.eq_ignore_ascii_case("visible")
                            || right_value.eq_ignore_ascii_case("visible"))
                    {
                        return String::from("visible");
                    }
                    if local < 0.5 {
                        left_value.clone()
                    } else {
                        right_value.clone()
                    }
                });
            Some((property.to_owned(), value))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn filtered_keyframes_preserve_intervals_metadata_and_original_frames() {
        let frames = [0.0, 0.5, 0.5, 1.0].into_iter().enumerate().map(|(index, offset)| Keyframe {
            offset,
            declarations: vec![("width".into(), format!("{}px",index * 10)),
                ("transform".into(), "unsupported additive transform".into())],
            easing: Some("steps(2, start)".into()),
            composite: Some(CompositeMode::Add),
        }).collect::<Vec<_>>();
        for before in [false, true] {
            for progress in [-0.5, 0.0, 0.25, 0.5, 0.75, 1.0, 1.5] {
                let compose = |property: &str, frame: &Keyframe, value: &str| {
                    assert!(frames.iter().any(|original| core::ptr::eq(original, frame)));
                    assert_eq!(frame.composite, Some(CompositeMode::Add));
                    if property == "width" { add_numeric_values("10px", value, None) } else { None }
                };
                let all = sample_keyframes_composed_with_property(&frames,progress,before,
                    compose,|_,from,to,at| interpolate_numeric(from,to,at));
                let selected = sample_keyframes_composed_with_property_filter(&frames,progress,before,
                    |property| property == "width",
                    |property,frame,value| {
                        assert_eq!(property,"width");
                        compose(property,frame,value)
                    },|property,from,to,at| {
                        assert_eq!(property,"width");
                        interpolate_numeric(from,to,at)
                    });
                assert_eq!(selected,all);
                assert_eq!(selected.len(),1);
            }
        }
    }

    #[test]
    fn css_animation_event_transitions_follow_shared_phases_and_skipped_frames() {
        let timing = Timing { delay_ms: -4000.0, duration_ms: 1000.0, iterations: 7.0, ..Timing::default() };
        let initial = css_event_sample(timing, 0.0, 1.0);
        assert_eq!(css_events(timing, CssEventSample::IDLE, initial, 0.0),
            [Some(CssEvent { kind: "animationstart", elapsed_ms: 4000.0 }), None]);
        let skipped = css_event_sample(timing, 2200.0, 1.0);
        assert_eq!(css_events(timing, initial, skipped, 0.0),
            [Some(CssEvent { kind: "animationiteration", elapsed_ms: 6000.0 }), None]);
        let ended = css_event_sample(timing, 3000.0, 1.0);
        assert_eq!(css_events(timing, skipped, ended, 0.0),
            [Some(CssEvent { kind: "animationend", elapsed_ms: 7000.0 }), None]);
        assert_eq!(css_events(timing, ended, ended, 0.0), [None, None]);
        assert_eq!(css_events(timing, skipped, initial, 0.0),
            [Some(CssEvent { kind: "animationiteration", elapsed_ms: 5000.0 }), None]);
        assert_eq!(css_events(timing, skipped, CssEventSample::IDLE, 6234.0),
            [Some(CssEvent { kind: "animationcancel", elapsed_ms: 6234.0 }), None]);
        assert_eq!(css_events(timing, ended, CssEventSample::IDLE, 7000.0), [None, None]);
        let empty = Timing::default();
        assert_eq!(css_events(empty, CssEventSample::IDLE, css_event_sample(empty, 0.0, 1.0), 0.0),
            [Some(CssEvent { kind: "animationstart", elapsed_ms: 0.0 }), Some(CssEvent { kind: "animationend", elapsed_ms: 0.0 })]);
        let delayed = Timing { delay_ms: 100.0, duration_ms: 200.0, iterations: 2.0, ..Timing::default() };
        let before = css_event_sample(delayed, 50.0, 1.0);
        let after = css_event_sample(delayed, 600.0, 1.0);
        assert_eq!(css_events(delayed, before, after, 0.0),
            [Some(CssEvent { kind: "animationstart", elapsed_ms: 0.0 }), Some(CssEvent { kind: "animationend", elapsed_ms: 400.0 })]);
        assert_eq!(css_events(delayed, after, before, 0.0),
            [Some(CssEvent { kind: "animationstart", elapsed_ms: 400.0 }), Some(CssEvent { kind: "animationend", elapsed_ms: 0.0 })]);
        // Reverse playback equality uses the shared phase calculation.
        assert_eq!(effect_phase(delayed,100.0,-1.0),Some(EffectPhase::Before));
    }

    #[test]
    fn timing_honors_fill_direction_and_pausing() {
        let timing = Timing {
            delay_ms: 10.0,
            duration_ms: 20.0,
            iterations: 2.0,
            fill: FillMode::Both,
            direction: Direction::Alternate,
            ..Timing::default()
        };
        assert_eq!(sample(timing, 100.0, 105.0, None).progress, Some(0.0));
        assert_eq!(sample(timing, 100.0, 120.0, None).progress, Some(0.5));
        assert_eq!(sample(timing, 100.0, 130.0, None).progress, Some(1.0));
        let paused = sample(timing, 100.0, 500.0, Some(130.0));
        assert_eq!(paused.state, PlaybackState::Paused);
        assert_eq!(paused.current_time_ms, 30.0);
    }

    #[test]
    fn effect_relevance_covers_current_and_filled_phases() {
        let timing = Timing {
            delay_ms: 10.0,
            duration_ms: 20.0,
            iterations: 1.0,
            fill: FillMode::None,
            direction: Direction::Normal,
            ..Timing::default()
        };
        let relevant = |timing, current_time, playback_rate, paused| {
            let sample = sample_current_time(timing, current_time, playback_rate, paused);
            effect_is_relevant(timing, sample, playback_rate)
        };

        assert!(
            relevant(timing, 0.0, 1.0, true),
            "positive-rate delay is current"
        );
        assert!(
            !relevant(timing, 0.0, 0.0, true),
            "zero-rate delay is not current"
        );
        assert!(
            !relevant(timing, 0.0, -1.0, true),
            "reverse playback at the start is before and not in effect"
        );
        let reverse_at_start = sample_current_time(timing, 0.0, -1.0, false);
        assert_eq!(reverse_at_start.state, PlaybackState::Finished);
        assert_eq!(reverse_at_start.progress, None);
        assert!(!effect_is_relevant(timing, reverse_at_start, -1.0));
        assert!(
            relevant(timing, 15.0, 0.0, true),
            "active paused effect is current"
        );
        assert!(
            !relevant(timing, 30.0, 1.0, true),
            "finished no-fill effect is not relevant"
        );
        assert!(
            relevant(timing, 31.0, -1.0, true),
            "reverse playback can re-enter an after phase"
        );
        assert!(
            !relevant(timing, 31.0, 0.0, true),
            "zero-rate after-phase effect is not relevant"
        );
        assert_eq!(
            sample_current_time(timing, 31.0, 0.0, false).state,
            PlaybackState::Running,
            "zero playback rate does not finish the animation"
        );

        let forwards = Timing {
            fill: FillMode::Forwards,
            ..timing
        };
        assert!(
            relevant(forwards, 30.0, 1.0, true),
            "forwards fill remains in effect"
        );
        let backwards = Timing {
            fill: FillMode::Backwards,
            ..timing
        };
        assert!(
            relevant(backwards, 0.0, 0.0, true),
            "backwards fill remains in effect"
        );
    }

    #[test]
    fn keyframe_sampler_interpolates_numeric_values_and_steps_keywords() {
        let frames = vec![
            Keyframe {
                offset: 0.0,
                declarations: vec![
                    ("opacity".into(), "0".into()),
                    ("display".into(), "none".into()),
                ],
                easing: None,
                composite: None,
            },
            Keyframe {
                offset: 1.0,
                declarations: vec![
                    ("opacity".into(), "1".into()),
                    ("display".into(), "block".into()),
                ],
                easing: None,
                composite: None,
            },
        ];
        assert_eq!(
            sample_keyframes(&frames, 0.5),
            vec![
                ("opacity".into(), "0.5".into()),
                ("display".into(), "block".into())
            ]
        );
        assert_eq!(
            interpolate_numeric("0px", "10px", 0.25).as_deref(),
            Some("2.5px")
        );
        assert!(interpolate_numeric("0px", "1em", 0.5).is_none());
    }

    #[test]
    fn discrete_animation_keeps_display_and_visibility_during_active_interval() {
        let frames = vec![
            Keyframe {
                offset: 0.0,
                declarations: vec![
                    ("display".into(), "none".into()),
                    ("visibility".into(), "hidden".into()),
                    ("overflow".into(), "hidden".into()),
                ],
                easing: None,
                composite: None,
            },
            Keyframe {
                offset: 1.0,
                declarations: vec![
                    ("display".into(), "block".into()),
                    ("visibility".into(), "visible".into()),
                    ("overflow".into(), "scroll".into()),
                ],
                easing: None,
                composite: None,
            },
        ];
        let value = |property: &str, progress| {
            sample_keyframes(&frames, progress)
                .into_iter()
                .find(|(name, _)| name == property)
                .unwrap()
                .1
        };
        assert_eq!(value("display", 0.0), "none");
        assert_eq!(value("display", 0.001), "block");
        assert_eq!(value("visibility", 0.0), "hidden");
        assert_eq!(value("visibility", 0.001), "visible");
        assert_eq!(value("overflow", 0.499), "hidden");
        assert_eq!(value("overflow", 0.5), "scroll");
        let mut exit = frames.clone();
        exit[0].declarations[0].1 = "block".into();
        exit[1].declarations[0].1 = "none".into();
        exit[0].declarations[1].1 = "visible".into();
        exit[1].declarations[1].1 = "hidden".into();
        let at = |progress| sample_keyframes(&exit, progress);
        assert_eq!(at(0.999)[0].1, "block");
        assert_eq!(at(0.999)[1].1, "visible");
        assert_eq!(at(1.0)[0].1, "none");
        assert_eq!(at(1.0)[1].1, "hidden");
    }

    #[test]
    fn named_css_easing_matches_standard_bezier_curves() {
        for (name, explicit, expected) in [
            ("ease", "cubic-bezier(0.25, 0.1, 0.25, 1)", 0.8024033876),
            ("ease-in", "cubic-bezier(0.42, 0, 1, 1)", 0.3153568125),
            ("ease-out", "cubic-bezier(0, 0, 0.58, 1)", 0.6846431874),
            ("ease-in-out", "cubic-bezier(0.42, 0, 0.58, 1)", 0.5),
        ] {
            assert!((ease(name, 0.5).unwrap() - expected).abs() < 1e-8);
            for progress in [-0.5, 0.0, 0.25, 0.5, 0.75, 1.0, 1.5] {
                assert_eq!(ease(name, progress), ease(explicit, progress));
            }
        }
        assert_eq!(ease("linear", -0.5), Some(-0.5));
        assert_eq!(ease("linear", 1.5), Some(1.5));
        assert_eq!(ease("ease", 0.0), Some(0.0));
        assert_eq!(ease("ease", 1.0), Some(1.0));
        assert!(ease("ease", f64::NAN).is_none());
    }

    #[test]
    fn easing_overshoot_and_endpoint_tangents_reach_sampled_values() {
        let progress = ease("cubic-bezier(0.25, 2, 0.75, 2)", 0.5).unwrap();
        assert!((progress - 1.625).abs() < 1e-9);
        assert!((ease("cubic-bezier(0.5, 0.2, 0.8, 1.4)", -0.5).unwrap() + 0.2).abs() < 1e-9);
        assert!(ease("cubic-bezier(0.5, 0.2, 0.8, 1.4)", 1.5).unwrap().abs() < 1e-9);
        let frames = vec![
            Keyframe {
                offset: 0.0,
                declarations: vec![("width".into(), "0px".into())],
                easing: Some("linear".into()),
                composite: None,
            },
            Keyframe {
                offset: 1.0,
                declarations: vec![("width".into(), "10px".into())],
                easing: None,
                composite: None,
            },
        ];
        assert_eq!(
            sample_keyframes(&frames, progress),
            vec![("width".into(), "16.25px".into())]
        );
        assert_eq!(
            sample_keyframes(&frames, -0.5),
            vec![("width".into(), "-5px".into())]
        );
    }

    #[test]
    fn keyframe_endpoint_composition_precedes_interpolation() {
        let frames = vec![
            Keyframe {
                offset: 0.0,
                declarations: vec![("width".into(), "2px".into())],
                easing: None,
                composite: Some(CompositeMode::Add),
            },
            Keyframe {
                offset: 1.0,
                declarations: vec![("width".into(), "30px".into())],
                easing: None,
                composite: Some(CompositeMode::Replace),
            },
        ];
        let compose = |_: &str, frame: &Keyframe, value: &str| {
            if frame.composite == Some(CompositeMode::Add) {
                add_numeric_values("10px", value, None)
            } else {
                Some(value.to_owned())
            }
        };
        assert_eq!(
            sample_keyframes_composed(&frames, 0.0, false, compose, interpolate_numeric),
            vec![("width".into(), "12px".into())]
        );
        assert_eq!(
            sample_keyframes_composed(&frames, 0.5, false, compose, interpolate_numeric),
            vec![("width".into(), "21px".into())]
        );
        assert_eq!(
            sample_keyframes_composed(&frames, 1.0, false, compose, interpolate_numeric),
            vec![("width".into(), "30px".into())]
        );
    }

    #[test]
    fn fractional_iteration_fill_uses_the_actual_final_progress() {
        let timing = Timing {
            duration_ms: 100.0,
            iterations: 2.25,
            fill: FillMode::Both,
            ..Timing::default()
        };
        let end = sample_current_time(timing, 225.0, 1.0, false);
        assert_eq!(end.state, PlaybackState::Finished);
        assert_eq!(end.progress, Some(0.25));
        assert_eq!(
            sample_current_time(timing, 1000.0, 1.0, false).progress,
            Some(0.25)
        );
        assert_eq!(
            sample_current_time(
                Timing {
                    direction: Direction::Reverse,
                    ..timing
                },
                225.0,
                1.0,
                false
            )
            .progress,
            Some(0.75)
        );
        assert_eq!(
            sample_current_time(
                Timing {
                    direction: Direction::Alternate,
                    iterations: 1.25,
                    ..timing
                },
                125.0,
                1.0,
                false
            )
            .progress,
            Some(0.75)
        );
        assert_eq!(
            sample_current_time(
                Timing {
                    iterations: 0.0,
                    ..timing
                },
                0.0,
                1.0,
                false
            )
            .progress,
            Some(0.0)
        );
        assert_eq!(
            sample_current_time(
                Timing {
                    iterations: 2.0,
                    ..timing
                },
                200.0,
                1.0,
                false
            )
            .progress,
            Some(1.0)
        );
    }

    #[test]
    fn iteration_start_and_end_delays_preserve_phase_and_iteration() {
        let timing = Timing {
            delay_ms: 50.0,
            end_delay_ms: 50.0,
            duration_ms: 100.0,
            iteration_start: 1.25,
            iterations: 1.5,
            direction: Direction::Alternate,
            fill: FillMode::Both,
        };
        let before = sample_current_time(timing, 25.0, 1.0, false);
        assert_eq!(before.progress, Some(0.75));
        assert_eq!(before.current_iteration, Some(1.0));
        assert!(!before.before);
        let active = sample_current_time(timing, 125.0, 1.0, false);
        assert_eq!(active.progress, Some(0.0));
        assert_eq!(active.current_iteration, Some(2.0));
        let end_delay = sample_current_time(timing, 225.0, 1.0, false);
        assert_eq!(end_delay.progress, Some(0.75));
        assert_eq!(end_delay.state, PlaybackState::Running);
        assert_eq!(
            sample_current_time(timing, 250.0, 1.0, false).state,
            PlaybackState::Finished
        );

        let truncated = Timing {
            end_delay_ms: -75.0,
            ..timing
        };
        assert_eq!(truncated.end_time(), 125.0);
        let terminal = sample_current_time(truncated, 125.0, 1.0, false);
        assert_eq!(terminal.progress, Some(0.0));
        assert_eq!(terminal.current_iteration, Some(2.0));
        assert_eq!(terminal.state, PlaybackState::Finished);
        assert_eq!(
            sample_current_time(timing, 25.0, -1.0, false).state,
            PlaybackState::Running
        );
        assert_eq!(
            sample_current_time(timing, 0.0, -1.0, false).state,
            PlaybackState::Finished
        );
    }

    #[test]
    fn zero_duration_infinite_repeats_finish_without_nan() {
        let timing = Timing {
            duration_ms: 0.0,
            iterations: f64::INFINITY,
            iteration_start: 1.25,
            fill: FillMode::Both,
            direction: Direction::AlternateReverse,
            ..Timing::default()
        };
        assert_eq!(timing.active_duration(), 0.0);
        let terminal = sample_current_time(timing, 0.0, 1.0, false);
        assert_eq!(terminal.state, PlaybackState::Finished);
        assert_eq!(terminal.current_iteration, Some(f64::INFINITY));
        assert_eq!(terminal.progress, Some(0.25));
        let integral = sample_current_time(
            Timing {
                iteration_start: 0.0,
                ..timing
            },
            0.0,
            1.0,
            false,
        );
        assert_eq!(integral.progress, Some(1.0));
        let empty = Timing {
            duration_ms: f64::INFINITY,
            iterations: 0.0,
            ..timing
        };
        assert_eq!(empty.active_duration(), 0.0);
        assert_eq!(
            sample_current_time(empty, 0.0, 1.0, false).progress,
            Some(0.25)
        );
    }

    #[test]
    fn effect_fill_phase_supplies_step_before_flag_for_both_directions() {
        let timing = Timing {
            delay_ms: 100.0,
            duration_ms: 100.0,
            fill: FillMode::Both,
            ..Timing::default()
        };
        let before = sample_current_time(timing, 50.0, 1.0, true);
        assert!(before.before);
        assert_eq!(
            ease_with_before("steps(2, start)", before.progress.unwrap(), before.before),
            Some(0.0)
        );
        let active = sample_current_time(timing, 100.0, 1.0, true);
        assert!(!active.before);
        assert_eq!(
            ease_with_before("steps(2, start)", active.progress.unwrap(), active.before),
            Some(0.5)
        );
        let reverse = Timing {
            direction: Direction::Reverse,
            ..timing
        };
        assert!(!sample_current_time(reverse, 50.0, 1.0, true).before);
        assert!(sample_current_time(reverse, 250.0, 1.0, true).before);
    }

    #[test]
    fn step_easing_before_flag_and_outside_progress_are_preserved() {
        assert_eq!(ease_with_before("step-start", 0.0, true), Some(0.0));
        assert_eq!(ease("step-start", 0.0), Some(1.0));
        assert_eq!(ease_with_before("steps(2, end)", 0.5, true), Some(0.0));
        assert_eq!(ease("steps(2, end)", 0.5), Some(0.5));
        assert_eq!(ease("steps(2, end)", 1.5), Some(1.5));
        assert_eq!(ease("steps(2, start)", -1.0), Some(-0.5));
        assert_eq!(ease("steps(2, jump-none)", 1.0), Some(1.0));
        assert_eq!(ease("steps(2, jump-both)", 0.0), Some(1.0 / 3.0));
    }

    #[test]
    fn css_timing_functions_and_keyframe_easing_are_applied() {
        assert_eq!(ease("steps(4, start)", 0.0), Some(0.25));
        assert_eq!(ease("steps(4, end)", 0.5), Some(0.5));
        assert!(ease("steps(1, jump-none)", 0.5).is_none());
        let bezier = ease("cubic-bezier(0.25, 0.1, 0.25, 1)", 0.5).unwrap();
        assert!((bezier - 0.8024).abs() < 0.002);
        let frames = vec![
            Keyframe {
                offset: 0.0,
                declarations: vec![("opacity".into(), "0".into())],
                easing: Some("ease-in".into()),
                composite: None,
            },
            Keyframe {
                offset: 1.0,
                declarations: vec![("opacity".into(), "1".into())],
                easing: None,
                composite: None,
            },
        ];
        assert_eq!(
            sample_keyframes(&frames, 0.5),
            vec![("opacity".into(), "0.315357".into())]
        );
    }
}
