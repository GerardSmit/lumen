//! Shared Web Animations effect timing and CSS-value interpolation.
pub mod transition_values;
pub mod progress_timelines;

use alloc::{borrow::ToOwned, boxed::Box, format, string::String, vec, vec::Vec};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ProgressRangeName { Cover, Contain, Entry, Exit, EntryCrossing, ExitCrossing }

#[derive(Clone, Debug, PartialEq)]
pub struct ProgressRange {
    pub start: (ProgressRangeName, f64),
    pub end: (ProgressRangeName, f64),
    offsets:[crate::css::DecorationLength;2],
}

impl ProgressRange {
    pub fn parse(value: &str) -> Option<Self> {
        let [(start,start_offset),(end,end_offset)]=crate::css::animation_controls::progress_range_parts(value)?;
        let fraction=|offset:&crate::css::DecorationLength|match offset {
            crate::css::DecorationLength::Length(value)=>f64::from(value.percent)/100.0,_=>0.0,
        };
        Some(Self{start:(start,fraction(&start_offset)),end:(end,fraction(&end_offset)),offsets:[start_offset,end_offset]})
    }

    pub fn checked_retained_bytes(&self)->Option<usize>{crate::css::checked_progress_offset_bytes(&self.offsets)}

    pub fn scroll_bounds(&self,start:f64,end:f64)->Option<(f64,f64)> {
        let size=end-start;
        if !start.is_finite()||!size.is_finite()||size<=0.0{return None;}
        Some((start+f64::from(self.offsets[0].used(size as f32)?),start+f64::from(self.offsets[1].used(size as f32)?)))
    }

    /// View ranges in the scroll container's untransformed CSS-pixel coordinate space.
    pub fn view_bounds(&self, subject_start: f64, subject_size: f64, viewport_size: f64) -> Option<(f64,f64)> {
        if !subject_start.is_finite() || !subject_size.is_finite() || !viewport_size.is_finite()
            || subject_size <= 0.0 || viewport_size <= 0.0 { return None; }
        let cover=(subject_start-viewport_size,subject_start+subject_size);
        let a=subject_start;let b=subject_start+subject_size-viewport_size;
        let contain=(a.min(b),a.max(b));
        let bounds=|name|match name {
            ProgressRangeName::Cover=>cover,ProgressRangeName::Contain=>contain,
            ProgressRangeName::Entry=>(cover.0,contain.0),ProgressRangeName::Exit=>(contain.1,cover.1),
            ProgressRangeName::EntryCrossing=>(cover.0,b),ProgressRangeName::ExitCrossing=>(a,cover.1),
        };
        let position=|name,offset:&crate::css::DecorationLength|{let(start,end)=bounds(name);Some(start+f64::from(offset.used((end-start) as f32)?))};
        Some((position(self.start.0,&self.offsets[0])?,position(self.end.0,&self.offsets[1])?))
    }
}

pub fn progress_fraction(offset: f64, start: f64, end: f64) -> Option<f64> {
    (offset.is_finite() && start.is_finite() && end.is_finite() && end>start)
        .then(|| (offset-start)/(end-start))
}

#[derive(Clone, Debug, PartialEq)]
pub struct Keyframe {
    pub offset: f64,
    /// Authored nullable offset; `offset` always holds the sampling coordinate.
    pub offset_is_specified: bool,
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
    /// Web Animations 2 proportional timing on a finite progress timeline.
    /// The caller retains specified timing separately for getTiming().
    pub fn normalized_for_progress(mut self,auto:bool,timeline_duration:f64)->Self{
        if auto{
            self.delay_ms=0.0;self.end_delay_ms=0.0;
            self.duration_ms=if self.iterations.is_finite()&&self.iterations>0.0{timeline_duration/self.iterations}else{0.0};
        }else{
            let total=self.end_time();
            let scale=if total.is_finite()&&total>0.0{timeline_duration/total}else{0.0};
            self.delay_ms*=scale;self.duration_ms*=scale;self.end_delay_ms*=scale;
        }
        self
    }

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

/// CSS Transitions retain logical reversal endpoints independently of the
/// currently sampled value. Hosts reuse their existing Animation timing record.
#[derive(Clone, Debug, PartialEq)]
pub struct TransitionState<T> {
    pub start_value: T,
    pub end_value: T,
    pub reversing_adjusted_start_value: T,
    pub reversing_shortening_factor: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TransitionParameters {
    pub duration_ms: f64,
    pub delay_ms: f64,
}

impl TransitionParameters {
    pub fn combined_duration(self) -> f64 { self.duration_ms.max(0.0) + self.delay_ms }
}

#[derive(Clone, Debug, PartialEq)]
pub enum TransitionAction<T> {
    Keep,
    Cancel,
    Start { state: TransitionState<T>, parameters: TransitionParameters },
}

#[derive(Clone, Debug, PartialEq)]
pub struct TransitionUpdate<T> {
    pub remove_completed: bool,
    pub action: TransitionAction<T>,
}

/// CSS Transitions 1 §3. Endpoint transitionability is supplied by the shared
/// property/value algorithm; timing cannot make a non-animatable pair eligible.
/// `eased_progress` is the old effect's transformed progress, including overshoot.
pub fn transition_update<T: Clone + PartialEq>(
    before: &T, after: &T, current: &T, running: Option<&TransitionState<T>>,
    completed_end: Option<&T>, parameters: Option<TransitionParameters>,
    initial_transitionable: bool, retarget_transitionable: bool, eased_progress: f64,
) -> TransitionUpdate<T> {
    transition_update_with(before, after, current, running, completed_end, parameters,
        |retarget| if retarget { retarget_transitionable } else { initial_transitionable }, eased_progress)
}

/// Resolve endpoint transitionability only when the canonical lifecycle branch
/// needs it. Unchanged properties and completed endpoints do not parse compound
/// interpolation values during each unrelated style change.
pub fn transition_update_with<T: Clone + PartialEq>(
    before: &T, after: &T, current: &T, running: Option<&TransitionState<T>>,
    completed_end: Option<&T>, parameters: Option<TransitionParameters>,
    mut transitionable: impl FnMut(bool) -> bool, eased_progress: f64,
) -> TransitionUpdate<T> {
    let remove_completed = completed_end.is_some_and(|end| end != after || parameters.is_none());
    let fresh = |start: &T, parameters| TransitionAction::Start {
        state: TransitionState { start_value: start.clone(), end_value: after.clone(),
            reversing_adjusted_start_value: start.clone(), reversing_shortening_factor: 1.0 },
        parameters,
    };
    let action = match running {
        None => match parameters {
            Some(parameters) if before != after
                && completed_end.is_none_or(|end| end != after)
                && parameters.combined_duration() > 0.0 && transitionable(false) => fresh(before, parameters),
            _ => TransitionAction::Keep,
        },
        Some(old) => match parameters {
            None => TransitionAction::Cancel,
            Some(_) if old.end_value == *after => TransitionAction::Keep,
            Some(parameters) if current == after || parameters.combined_duration() <= 0.0
                || !transitionable(true) => TransitionAction::Cancel,
            Some(parameters) if old.reversing_adjusted_start_value == *after => {
                let factor = (eased_progress * old.reversing_shortening_factor
                    + 1.0 - old.reversing_shortening_factor).abs().clamp(0.0,1.0);
                TransitionAction::Start {
                    state: TransitionState { start_value: current.clone(), end_value: after.clone(),
                        reversing_adjusted_start_value: old.end_value.clone(), reversing_shortening_factor: factor },
                    parameters: TransitionParameters { duration_ms: parameters.duration_ms.max(0.0) * factor,
                        delay_ms: if parameters.delay_ms < 0.0 { parameters.delay_ms * factor } else { parameters.delay_ms } },
                }
            }
            Some(parameters) => fresh(current, parameters),
        },
    };
    TransitionUpdate { remove_completed, action }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransitionPhase { Idle, Pending, Before, Active, After }

/// Pending play/pause is a separate CSS transition phase only before its first
/// resolved frame; seeking and negative playback use the ordinary effect phase.
pub fn transition_phase(timing: Timing, current_time: Option<f64>, rate: f64,
    pending: bool, previous: TransitionPhase, has_effect: bool) -> TransitionPhase {
    use TransitionPhase::*;
    if !has_effect { return match current_time { None => Idle, Some(time) if time < 0.0 => Before, Some(_) => After }; }
    if pending && matches!(previous, Idle | Pending) { return Pending; }
    match current_time.and_then(|time| effect_phase(timing,time,rate)) {
        None => Idle, Some(EffectPhase::Before) => Before,
        Some(EffectPhase::Active) => Active, Some(EffectPhase::After) => After,
    }
}

/// CSS Transitions 2 phase table. The fixed batch preserves skipped-boundary
/// ordering without allocating or replaying an unbounded series of frames.
pub fn transition_events(timing: Timing, previous: TransitionPhase,
    current: TransitionPhase, cancellation_elapsed_ms: f64) -> [Option<CssEvent>;3] {
    use TransitionPhase::*;
    let start = (-timing.delay_ms).min(timing.active_duration()).max(0.0);
    let end = (timing.end_time()-timing.delay_ms).min(timing.active_duration()).max(0.0);
    let event = |kind,elapsed_ms| Some(CssEvent { kind, elapsed_ms });
    match (previous,current) {
        (Idle,Pending | Before) => [event("transitionrun",start),None,None],
        (Idle,Active) => [event("transitionrun",start),event("transitionstart",start),None],
        (Idle,After) => [event("transitionrun",start),event("transitionstart",start),event("transitionend",end)],
        (Pending | Before,Active) => [event("transitionstart",start),None,None],
        (Pending | Before,After) => [event("transitionstart",start),event("transitionend",end),None],
        (Active,After) => [event("transitionend",end),None,None],
        (Active,Before) => [event("transitionend",start),None,None],
        (After,Active) => [event("transitionstart",end),None,None],
        (After,Before) => [event("transitionstart",end),event("transitionend",start),None],
        (Pending | Before | Active,Idle) => [event("transitioncancel",cancellation_elapsed_ms),None,None],
        _ => [None,None,None],
    }
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
    if let Some(points) = linear_easing_points(name) {
        if points.len()==1 {return Some(points[0].1);}
        if before && points[0].0==progress {return Some(points[0].1);}
        if let Some(point)=points.iter().rev().find(|point|point.0==progress) {return Some(point.1);}
        let index=points.iter().rposition(|point|point.0<progress).unwrap_or(0).min(points.len()-2);
        let (a,b)=(points[index],points[index+1]);
        return Some(if a.0==b.0 {if progress<a.0 {a.1}else{b.1}}
            else {a.1+(progress-a.0)/(b.0-a.0)*(b.1-a.1)});
    }
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

/// CSS Easing 2 linear stop fixup, shared by declaration validation,
/// serialization and evaluation. Numeric tokens reuse the CSS tokenizer.
fn specified_linear_easing_points(input: &str) -> Option<Vec<(Option<f64>,f64)>> {
    use crate::css::typed_numeric::{parse_numeric_value, NumericUnit};
    let body = input.trim().strip_prefix("linear(")?.strip_suffix(')')?;
    let stops = crate::css::css_list_items(body);
    if stops.len()<2 { return None; }
    let mut points: Vec<(Option<f64>,f64)> = Vec::new();
    let mut largest = f64::NEG_INFINITY;
    for (index,stop) in stops.iter().enumerate() {
        let mut output = None;
        let mut positions = [0.0;2];
        let mut count = 0;
        for token in stop.split_ascii_whitespace() {
            let numeric = parse_numeric_value(token)?;
            if !numeric.value.is_finite() { return None; }
            match numeric.unit {
                NumericUnit::Number if output.is_none() => output=Some(numeric.value),
                NumericUnit::Percent if count < 2 => { positions[count]=numeric.value/100.0; count+=1; }
                _ => return None,
            }
        }
        let output=output?;
        if count > 0 {
            for position in &positions[..count] {
                largest=largest.max(*position);
                points.push((Some(largest),output));
            }
        } else {
            if index == 0 {largest=0.0;}
            points.push((None,output));
        }
    }
    Some(points)
}

pub fn linear_easing_points(input:&str)->Option<Vec<(f64,f64)>> {
    let mut points=specified_linear_easing_points(input)?;
    if points[0].0.is_none() {points[0].0=Some(0.0);}
    let last=points.len()-1;
    if points[last].0.is_none() {points[last].0=Some(1.0);}
    let mut largest=f64::NEG_INFINITY;
    for point in &mut points {if let Some(input)=point.0 {largest=largest.max(input);point.0=Some(largest);}}
    let mut index=1;
    while index < points.len() {
        if points[index].0.is_some() { index+=1; continue; }
        let start=index-1;
        while points[index].0.is_none() { index+=1; }
        let lower=points[start].0?;
        let upper=points[index].0?;
        let distance=(index-start) as f64;
        for position in start+1..index {
            points[position].0=Some(lower+(upper-lower)*(position-start) as f64/distance);
        }
    }
    Some(points.into_iter().map(|(input,output)|(input.expect("fixed linear input"),output)).collect())
}

pub fn serialize_linear_easing(input: &str) -> Option<String> {
    use crate::css::typed_numeric::{serialize_numeric_value, NumericUnit};
    let points=specified_linear_easing_points(input)?;
    let mut result=String::from("linear(");
    for (index,(input,output)) in points.into_iter().enumerate() {
        if index != 0 { result.push_str(", "); }
        result.push_str(&serialize_numeric_value(output,NumericUnit::Number));
        if let Some(input)=input {
            result.push(' ');
            result.push_str(&serialize_numeric_value(input*100.0,NumericUnit::Percent));
        }
    }
    result.push(')'); Some(result)
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
    if !progress.is_finite() {return None;}
    if let (Some((a,unit_a)),Some((b,unit_b)))=(numeric_unit(from),numeric_unit(to)) {
        if unit_a==unit_b {
            if progress==0.0{return Some(from.to_owned());}
            if progress==1.0{return Some(to.to_owned());}
            return serialize_numeric(a+(b-a)*progress,unit_a);
        }
    }
    // Validate dimensional compatibility before preserving exact endpoints.
    // This also avoids constructing a zero-weight mixed calculation tree.
    if progress==0.0 || progress==1.0 {
        use crate::css::typed_numeric::{NumericExpression as E,parse_numeric_expression};
        E::Sum(vec![parse_numeric_expression(from)?,parse_numeric_expression(to)?]).numeric_type()?;
        return Some(if progress==0.0{from}else{to}.to_owned());
    }
    combine_numeric_values(&[(from,1.0-progress),(to,progress)])
}

fn serialize_numeric(value:f64,unit:&str)->Option<String> {
    if !value.is_finite(){return None;}
    let mut result=format!("{value:.6}");
    while result.contains('.') && result.ends_with('0'){result.pop();}
    if result.ends_with('.'){result.pop();}
    result.push_str(unit);Some(result)
}

/// Combine computed scalar expressions without resolving their percentage
/// basis. The canonical CSS math tree owns dimensional checking, simplification
/// and bounded serialization; interpolation and composition share this path.
pub fn combine_numeric_values(values:&[(&str,f64)])->Option<String> {
    use crate::css::typed_numeric::{NumericExpression as E,NumericValue,NumericUnit,parse_numeric_expression};
    if values.is_empty() || values.len()>3{return None;}
    let mut terms=Vec::new();terms.try_reserve_exact(values.len()).ok()?;
    for &(value,weight) in values {
        if !weight.is_finite(){return None;}
        let expression=parse_numeric_expression(value)?;
        terms.push(if weight==1.0{expression}else{
            E::Product(vec![E::Value(NumericValue{value:weight,unit:NumericUnit::Number}),expression])
        });
    }
    let mut expression=E::Sum(terms);
    expression.numeric_type()?;
    if !expression.within_limits(){return None;}
    expression.simplify_absolute_units();
    if let Some(value)=expression.computed_percentage_dimension_mix(){expression=E::Value(value);}
    if expression.single_numeric_value().is_none(){expression=E::Calc(Box::new(expression));}
    if !expression.within_limits(){return None;}
    expression.serialize()
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
    use crate::css::registered_properties::ComputedValueOperation as Op;
    crate::css::typed_transforms::combine_resolved(from,to,
        if accumulate {Op::Accumulate(1.0)}else{Op::Interpolate(progress)},width,height)
}

/// Add compatible computed CSS numeric values, optionally repeating `delta`
/// for an accumulated effect iteration. Unit conversion is intentionally left
/// to CSS computed-value resolution; incompatible units are not composable.
pub fn add_numeric_values(base: &str, value: &str, delta: Option<(&str, u64)>) -> Option<String> {
    if let (Some((a,unit_a)),Some((b,unit_b)))=(numeric_unit(base),numeric_unit(value)) {
        if unit_a==unit_b {
            let extra=if let Some((delta,count))=delta {
                let (amount,unit)=numeric_unit(delta)?;
                if unit!=unit_a{return combine_numeric_values(&[(base,1.0),(value,1.0),(delta,count as f64)]);}
                amount*count as f64
            }else{0.0};
            return serialize_numeric(a+b+extra,unit_a);
        }
    }
    match delta {
        Some((delta,count))=>combine_numeric_values(&[(base,1.0),(value,1.0),(delta,count as f64)]),
        None=>combine_numeric_values(&[(base,1.0),(value,1.0)]),
    }
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
    keyframes: &[Keyframe],progress:f64,before:bool,
    include:impl FnMut(&str)->bool,
    mut compose:impl FnMut(&str,&Keyframe,&str)->Option<String>,
    interpolate:impl FnMut(&str,&str,&str,f64)->Option<String>,
)->Vec<(String,String)> {
    sample_keyframes_intervals(keyframes,progress,before,false,include,
        |property,frame,value|compose(property,frame?,value?),interpolate)
}

/// Web Animations' implicit boundary keyframes have a neutral value, which is
/// resolved against the lower effect stack at sampling time. They are not
/// authored frames and must never be inserted into the source keyframe array.
pub fn sample_keyframes_composed_with_neutral_property_filter(
    keyframes: &[Keyframe],progress:f64,before:bool,
    include:impl FnMut(&str)->bool,
    compose:impl FnMut(&str,Option<&Keyframe>,Option<&str>)->Option<String>,
    interpolate:impl FnMut(&str,&str,&str,f64)->Option<String>,
)->Vec<(String,String)> {
    sample_keyframes_intervals(keyframes,progress,before,true,include,compose,interpolate)
}

fn sample_keyframes_intervals(
    keyframes:&[Keyframe],progress:f64,before:bool,neutral:bool,
    mut include:impl FnMut(&str)->bool,
    mut compose:impl FnMut(&str,Option<&Keyframe>,Option<&str>)->Option<String>,
    mut interpolate:impl FnMut(&str,&str,&str,f64)->Option<String>,
)->Vec<(String,String)> {
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
            let mut indexes = keyframes
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
            if neutral {
                if keyframes[*indexes.first()?].offset>0.0{indexes.insert(0,keyframes.len());}
                if keyframes[indexes.iter().copied().rfind(|index|*index<keyframes.len())?].offset<1.0{indexes.push(keyframes.len()+1);}
            }
            let offset=|index:usize|if index==keyframes.len(){0.0}else if index==keyframes.len()+1{1.0}else{keyframes[index].offset};
            let first = *indexes.first()?;

            let last = *indexes.last()?;
            let left_index = indexes
                .iter()
                .copied()
                .take_while(|index| offset(*index) <= progress)
                .last();
            let right_index = indexes
                .iter()
                .copied()
                .find(|index| offset(*index) > progress);
            let (left_index, right_index) = match (left_index, right_index) {
                (Some(left), Some(right)) => (left, right),
                (None, Some(_)) if indexes.len() > 1 => (first, indexes[1]),
                (Some(_), None) if indexes.len() > 1 => (indexes[indexes.len() - 2], last),
                (None, Some(right)) => (right, right),
                (Some(left), None) => (left, left),
                (None, None) => (first, last),
            };
            let value_at = |index: usize| {
                keyframes.get(index)?.declarations
                    .iter()
                    .find(|(name, _)| name == property)
                    .map(|(_, value)| value)
            };
            let left_value = compose(property, keyframes.get(left_index), value_at(left_index).map(String::as_str))?;
            let right_value = compose(property, keyframes.get(right_index), value_at(right_index).map(String::as_str))?;
            if left_index == right_index {
                return Some((property.to_owned(), right_value));
            }
            let span = offset(right_index)-offset(left_index);
            let local=if span>0.0{(progress-offset(left_index))/span}else{1.0};
            let local=keyframes.get(left_index).and_then(|frame|frame.easing.as_deref())
                .and_then(|easing|ease_with_before(easing,local,before)).unwrap_or(local);
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
#[test]
fn specification_animation_context_numeric_math_preserves_percentage_basis_and_accumulation_order() {
    use crate::css::typed_numeric::{parse_numeric_expression,NumericDimension,NumericType,NumericUnit};
    assert_eq!(interpolate_numeric("calc(15% + 24px)","calc(7% + 13px)",0.0).as_deref(),Some("calc(15% + 24px)"));
    assert_eq!(interpolate_numeric("calc(15% + 24px)","calc(7% + 13px)",1.0).as_deref(),Some("calc(7% + 13px)"));
    let value=interpolate_numeric("10%","30px",0.5).expect("computed length-percentage interpolation");
    let expression=parse_numeric_expression(&value).expect("canonical serialized math");
    assert_eq!(expression.numeric_type(),Some(NumericType {percent_hint:Some(NumericDimension::Length),..NumericType::from_unit(NumericUnit::Px)}));
    assert!(expression.within_limits());
    assert_eq!(interpolate_numeric("1px","3px",0.5).as_deref(),Some("2px"));
    assert_eq!(interpolate_numeric("1s","3px",0.5),None,"unrelated dimensions remain incompatible");
    assert_eq!(add_numeric_values("10px","20px",Some(("30px",2))).as_deref(),Some("90px"));
    let accumulated=combine_numeric_values(&[("max(20px, 15%)",2.0),("min(10px, 5%)",1.0)]).unwrap();
    assert!(accumulated.find("max(").unwrap()<accumulated.find("min(").unwrap(),"V_A must precede V_B in nonsimplifiable math: {accumulated}");
    assert!(parse_numeric_expression(&accumulated).unwrap().within_limits());
    assert_eq!(combine_numeric_values(&[("10px",f64::INFINITY)]),None);
}

    #[test]
    fn specification_animation_neutral_boundaries_use_live_underlying_without_mutating_authored_frames() {
        let frames=vec![Keyframe { offset_is_specified: true,offset:0.0,declarations:vec![(String::from("left"),String::from("-50"))],easing:Some(String::from("linear")),composite:Some(CompositeMode::Add)}];
        let authored=frames.clone();
        let sample=|underlying:f64,progress:f64|sample_keyframes_composed_with_neutral_property_filter(&frames,progress,false,|_|true,
            |_,frame,value|Some(match (frame,value) {
                (Some(frame),Some(value)) if frame.composite==Some(CompositeMode::Add)=>format!("{}",underlying+value.parse::<f64>().unwrap()),
                (_,Some(value))=>String::from(value),
                _=>format!("{underlying}"),
            }),|_,left,right,progress|Some(format!("{}",left.parse::<f64>().unwrap()+(right.parse::<f64>().unwrap()-left.parse::<f64>().unwrap())*progress)));
        assert_eq!(sample(200.0,0.5),vec![(String::from("left"),String::from("175"))]);
        assert_eq!(sample(300.0,0.5),vec![(String::from("left"),String::from("275"))]);
        assert_eq!(sample(200.0,1.0),vec![(String::from("left"),String::from("200"))]);
        assert_eq!(frames,authored,"implicit neutral frames never become authored keyframes");
        let middle=vec![Keyframe{ offset_is_specified: true,offset:0.5,declarations:vec![(String::from("opacity"),String::from("1"))],easing:None,composite:None}];
        for (progress,expected) in [(0.0,"0"),(0.25,"0.5"),(0.5,"1"),(0.75,"0.5"),(1.0,"0")] {
            let sampled=sample_keyframes_composed_with_neutral_property_filter(&middle,progress,false,|_|true,
                |_,_,value|Some(String::from(value.unwrap_or("0"))),
                |_,left,right,progress|Some(format!("{}",left.parse::<f64>().unwrap()+(right.parse::<f64>().unwrap()-left.parse::<f64>().unwrap())*progress)));
            assert_eq!(sampled,vec![(String::from("opacity"),String::from(expected))]);
        }
    }

    #[test]
    fn specification_animation_progress_timing_normalizes_specified_delays_and_auto_duration(){
        let specified=super::Timing{duration_ms:2000.0,delay_ms:500.0,end_delay_ms:1500.0,iterations:2.0,..super::Timing::default()};
        let sampled=specified.normalized_for_progress(false,1000.0);
        assert_eq!(sampled.delay_ms,500.0/6.0);
        assert_eq!(sampled.duration_ms,2000.0/6.0);
        assert_eq!(sampled.end_delay_ms,250.0);
        assert_eq!(sampled.end_time(),1000.0);
        let auto=specified.normalized_for_progress(true,1000.0);
        assert_eq!(auto.delay_ms,0.0);assert_eq!(auto.end_delay_ms,0.0);
        assert_eq!(auto.duration_ms,500.0);
        assert_eq!(specified.delay_ms,500.0);assert_eq!(specified.end_delay_ms,1500.0);
    }

    #[test]
    fn view_progress_ranges_follow_subject_bounds_and_reject_inactive_ranges() {

        let range=super::ProgressRange::parse("exit").unwrap();
        assert_eq!(range.view_bounds(700.0,200.0,200.0),Some((700.0,900.0)));
        assert_eq!(range.view_bounds(800.0,100.0,200.0),Some((800.0,900.0)));
        assert_eq!(super::progress_fraction(800.0,700.0,900.0),Some(0.5));
        assert_eq!(super::progress_fraction(800.0,800.0,900.0),Some(0.0));
        let partial=super::ProgressRange::parse("entry 25% exit 75%").unwrap();
        assert_eq!(partial.view_bounds(700.0,200.0,200.0),Some((550.0,850.0)));
        assert!(super::ProgressRange::parse("exit nonsense").is_none());
        assert!(super::progress_fraction(10.0,0.0,0.0).is_none());
        assert!(super::progress_fraction(f64::NAN,0.0,100.0).is_none());
    }
    use super::*;
    use alloc::vec;

    #[test]
    fn filtered_keyframes_preserve_intervals_metadata_and_original_frames() {
        let frames = [0.0, 0.5, 0.5, 1.0].into_iter().enumerate().map(|(index, offset)| Keyframe { offset_is_specified: true,
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
    fn specification_linear_easing_fixes_stops_and_extrapolates_shared_samples() {
        assert_eq!(linear_easing_points("linear(0, .25, 1)"), Some(vec![(0.0,0.0),(0.5,0.25),(1.0,1.0)]));
        assert_eq!(linear_easing_points("linear(0 20%, .5 10%, 1)"), Some(vec![(0.2,0.0),(0.2,0.5),(1.0,1.0)]));
        assert_eq!(ease("linear(0, .25 25% 75%, 1)",0.5),Some(0.25));
        assert_eq!(ease("linear(0, .25 25% 75%, 1)",1.5),Some(2.5));
        assert_eq!(ease("linear(0 50%, 1 50%)",0.5),Some(1.0));
        assert_eq!(ease("linear(0, 1 100% 100%)",2.0),Some(1.0));
        assert!(ease("linear(0)",0.5).is_none());
        assert!(linear_easing_points("linear(0 0% 100%)").is_none());
        assert!(serialize_linear_easing("linear(0 0% 100%)").is_none());
        assert_eq!(ease("linear(0 0% 100%, 1)",0.5),Some(0.0));
        assert_eq!(serialize_linear_easing("linear(0, .25, 1)").as_deref(),Some("linear(0, 0.25, 1)"));
        assert_eq!(ease_with_before("linear(0 50%, 1 50%)",0.5,true),Some(0.0));
        assert!(ease("linear(0, 1px)",0.5).is_none());
        assert!(ease("linear(0, 1 10% 20% 30%)",0.5).is_none());
    }

    #[test]
    fn specification_css_transitions_retarget_reverse_and_preserve_running_timing() {
        let parameters = TransitionParameters { duration_ms: 1000.0, delay_ms: -100.0 };
        let TransitionAction::Start { state, .. } = transition_update(&0, &100, &0, None,
            None, Some(parameters), true, true, 0.0).action else { panic!("initial transition missing") };
        let TransitionAction::Start { state: reversed, parameters: reverse_timing } =
            transition_update(&100, &0, &25, Some(&state), None, Some(parameters), true, true, 0.25).action
            else { panic!("reverse transition missing") };
        assert_eq!(reverse_timing, TransitionParameters { duration_ms: 250.0, delay_ms: -25.0 });
        assert_eq!(reversed.reversing_adjusted_start_value, 100);
        let TransitionAction::Start { state: again, parameters: again_timing } =
            transition_update(&0, &100, &15, Some(&reversed), None, Some(parameters), true, true, 0.4).action
            else { panic!("second reverse missing") };
        assert!((again_timing.duration_ms-850.0).abs() <= 850.0*f64::EPSILON*2.0,
            "second reversal duration: {}",again_timing.duration_ms);
        assert!((again_timing.delay_ms+85.0).abs() <= 85.0*f64::EPSILON*2.0,
            "second reversal delay: {}",again_timing.delay_ms);
        assert_eq!(again.reversing_adjusted_start_value, 0);
        assert_eq!(transition_update(&0, &100, &25, Some(&state), None,
            Some(TransitionParameters { duration_ms: 0.0, delay_ms: 0.0 }), true, true, 0.25).action,
            TransitionAction::Keep);
        assert_eq!(transition_update(&0, &100, &25, Some(&state), None, None, true, true, 0.25).action,
            TransitionAction::Cancel);
    }

    #[test]
    fn specification_css_transitions_lazy_transitionability_preserves_lifecycle_and_unchanged_admission() {
        let parameters=TransitionParameters{duration_ms:1000.0,delay_ms:-100.0};
        let state=TransitionState{start_value:0,end_value:100,reversing_adjusted_start_value:0,reversing_shortening_factor:1.0};
        for (before,after,current,running,completed,timing) in [
            (10,10,10,None,None,Some(parameters)),
            (10,10,10,None,Some(20),Some(parameters)),
            (10,10,10,None,Some(10),None),
            (0,100,50,Some(&state),None,Some(parameters)),
            (50,100,50,None,Some(100),Some(parameters)),
            (0,100,0,None,None,Some(TransitionParameters{duration_ms:0.0,delay_ms:0.0})),
            (0,0,0,Some(&state),None,Some(parameters)),
            (0,50,25,Some(&state),None,None),
        ] {
            let lazy=transition_update_with(&before,&after,&current,running,completed.as_ref(),timing,
                |_|panic!("endpoint parsing was requested for a lifecycle-only branch"),0.25);
            let eager=transition_update(&before,&after,&current,running,completed.as_ref(),timing,true,true,0.25);
            assert_eq!(lazy.action,eager.action);assert_eq!(lazy.remove_completed,eager.remove_completed);
        }
        for retarget in [false,true] {
            for accepted in [false,true] {
                let mut requests=alloc::vec::Vec::new();
                let (before,after,current,running)=if retarget{(100,0,25,Some(&state))}else{(0,100,0,None)};
                let lazy=transition_update_with(&before,&after,&current,running,None,Some(parameters),
                    |requested|{requests.push(requested);accepted},0.25);
                let eager=transition_update(&before,&after,&current,running,None,Some(parameters),accepted,accepted,0.25);
                assert_eq!(requests,[retarget]);assert_eq!(lazy.action,eager.action);
            }
        }
    }

    #[test]
    fn specification_css_transitions_completed_state_and_delayed_zero_duration() {
        let parameters = TransitionParameters { duration_ms: 0.0, delay_ms: 100.0 };
        assert!(matches!(transition_update(&0, &100, &0, None, None, Some(parameters), true, true, 0.0).action,
            TransitionAction::Start { .. }));
        assert_eq!(transition_update(&50, &100, &50, None, Some(&100), Some(parameters), true, true, 0.0).action,
            TransitionAction::Keep);
        assert!(transition_update(&100, &100, &100, None, Some(&100), None, true, true, 0.0).remove_completed);
        let timing = Timing { duration_ms: 0.0, delay_ms: 100.0, fill: FillMode::Both, ..Timing::default() };
        assert_eq!(transition_phase(timing, Some(0.0), 1.0, false, TransitionPhase::Idle, true), TransitionPhase::Before);
        assert_eq!(transition_events(timing, TransitionPhase::Before, TransitionPhase::After, 0.0),
            [Some(CssEvent { kind: "transitionstart", elapsed_ms: 0.0 }), Some(CssEvent { kind: "transitionend", elapsed_ms: 0.0 }), None]);
    }

    #[test]
    fn specification_css_transition_events_preserve_skipped_boundaries_and_cancel_elapsed() {
        let timing = Timing { delay_ms: -400.0, duration_ms: 1000.0, fill: FillMode::Both, ..Timing::default() };
        assert_eq!(transition_events(timing, TransitionPhase::Idle, TransitionPhase::After, 0.0),
            [Some(CssEvent { kind: "transitionrun", elapsed_ms: 400.0 }), Some(CssEvent { kind: "transitionstart", elapsed_ms: 400.0 }), Some(CssEvent { kind: "transitionend", elapsed_ms: 1000.0 })]);
        assert_eq!(transition_events(timing, TransitionPhase::After, TransitionPhase::Before, 0.0),
            [Some(CssEvent { kind: "transitionstart", elapsed_ms: 1000.0 }), Some(CssEvent { kind: "transitionend", elapsed_ms: 400.0 }), None]);
        assert_eq!(transition_events(timing, TransitionPhase::Active, TransitionPhase::Idle, 700.0),
            [Some(CssEvent { kind: "transitioncancel", elapsed_ms: 700.0 }), None, None]);
        assert_eq!(transition_events(timing, TransitionPhase::After, TransitionPhase::Idle, 700.0), [None, None, None]);
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
            Keyframe { offset_is_specified: true,
                offset: 0.0,
                declarations: vec![
                    ("opacity".into(), "0".into()),
                    ("display".into(), "none".into()),
                ],
                easing: None,
                composite: None,
            },
            Keyframe { offset_is_specified: true,
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
            Keyframe { offset_is_specified: true,
                offset: 0.0,
                declarations: vec![
                    ("display".into(), "none".into()),
                    ("visibility".into(), "hidden".into()),
                    ("overflow".into(), "hidden".into()),
                ],
                easing: None,
                composite: None,
            },
            Keyframe { offset_is_specified: true,
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
            Keyframe { offset_is_specified: true,
                offset: 0.0,
                declarations: vec![("width".into(), "0px".into())],
                easing: Some("linear".into()),
                composite: None,
            },
            Keyframe { offset_is_specified: true,
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
            Keyframe { offset_is_specified: true,
                offset: 0.0,
                declarations: vec![("width".into(), "2px".into())],
                easing: None,
                composite: Some(CompositeMode::Add),
            },
            Keyframe { offset_is_specified: true,
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
            Keyframe { offset_is_specified: true,
                offset: 0.0,
                declarations: vec![("opacity".into(), "0".into())],
                easing: Some("ease-in".into()),
                composite: None,
            },
            Keyframe { offset_is_specified: true,
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
