//! Device-backed Web Audio graph bindings.
//!
//! Oscillator and buffer sources feed a context destination through a validated
//! graph of gain nodes. PCM generation runs in the owner-side runtime service.

use crate::events::HtmlTargetExt;
use crate::{DomDocument, DomRealm};
use lumen::embed::{Ctx, Deferred, JsFunction, JsObject, Nullable, OpError, OpResult, Value, WeakValue};
use std::{cell::RefCell, rc::Rc, sync::Arc};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextState {
    Suspended,
    Running,
    Closed,
}

#[derive(Clone, Debug)]
pub struct OscillatorSnapshot {
    pub id: u64,
    pub context: u64,
    pub context_time: f64,
    pub frequency_hz: f32,
    pub frequency_events: Vec<ParamEvent>,
    pub waveform: Waveform,
    pub connections: Vec<AudioTarget>,
    pub started: bool,
    pub stopped: bool,
    pub start_time: f64,
    pub stop_time: Option<f64>,
    pub phase: f64,
    pub buffer: Option<Arc<BufferSnapshot>>,
    pub buffer_source: bool,
    pub playback_rate: f32,
    pub playback_rate_events: Vec<ParamEvent>,
    pub buffer_offset: f64,
    pub loop_enabled: bool,
    pub loop_start: f64,
    pub loop_end: f64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioTarget {
    Gain(u64),
    Destination,
}

#[derive(Clone, Debug)]
pub struct GainSnapshot {
    pub id: u64,
    pub context_time: f64,
    pub value: f32,
    pub events: Vec<ParamEvent>,
    pub connections: Vec<AudioTarget>,
}

#[derive(Clone, Debug)]
pub struct BufferSnapshot {
    pub sample_rate: u32,
    pub length: usize,
    pub channels: Vec<Vec<f32>>,
}

#[derive(Default)]
struct AudioBufferData {
    sample_rate: u32,
    length: usize,
    channels: Vec<Value>,
}

#[derive(Clone, Copy, Debug)]
pub struct ParamEvent {
    pub time: f64,
    pub value: f32,
    pub linear: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Waveform {
    Sine,
    Square,
    Sawtooth,
    Triangle,
}

#[derive(Clone, Debug)]
pub struct ContextSnapshot {
    pub id: u64,
    pub state: ContextState,
    pub current_time: f64,
    pub sample_rate: u32,
    pub wants_running: bool,
    pub oscillators: Vec<OscillatorSnapshot>,
    pub gains: Vec<GainSnapshot>,
}

#[derive(Default)]
struct ContextRecord {
    state: ContextState,
    current_time: f64,
    sample_rate: u32,
    wants_running: bool,
    resume_waiters: Vec<Deferred>,
}

impl Default for ContextState {
    fn default() -> Self {
        Self::Suspended
    }
}

#[derive(Default)]
struct OscillatorRecord {
    id: u64,
    context: u64,
    frequency_hz: f32,
    frequency_events: Vec<ParamEvent>,
    waveform: Waveform,
    connections: Vec<AudioTarget>,
    started: bool,
    stopped: bool,
    start_time: f64,
    stop_time: Option<f64>,
    phase: f64,
    buffer: Option<Rc<RefCell<AudioBufferData>>>,
    render_buffer: Option<Arc<BufferSnapshot>>,
    buffer_source: bool,
    playback_rate: f32,
    playback_rate_events: Vec<ParamEvent>,
    buffer_offset: f64,
    loop_enabled: bool,
    loop_start: f64,
    loop_end: f64,
    wrapper: Option<WeakValue>,
}

impl Default for Waveform {
    fn default() -> Self {
        Self::Sine
    }
}

#[derive(Default)]
pub(crate) struct WebAudioController {
    next_context: u64,
    next_oscillator: u64,
    contexts: std::collections::BTreeMap<u64, ContextRecord>,
    oscillators: std::collections::BTreeMap<u64, OscillatorRecord>,
    gains: std::collections::BTreeMap<u64, GainRecord>,
}

#[derive(Default)]
struct GainRecord {
    context: u64,
    value: f32,
    events: Vec<ParamEvent>,
    connections: Vec<AudioTarget>,
}

impl WebAudioController {
    fn create_context(&mut self) -> u64 {
        if self.contexts.len() >= 16 {
            return 0;
        }
        self.next_context = self.next_context.wrapping_add(1).max(1);
        let id = self.next_context;
        self.contexts.insert(
            id,
            ContextRecord {
                sample_rate: 44_100,
                wants_running: false,
                ..ContextRecord::default()
            },
        );
        id
    }

    fn create_oscillator(&mut self, context: u64) -> OpResult<u64> {
        if !self.contexts.contains_key(&context) {
            return Err(OpError::new("InvalidStateError", "audio context is closed"));
        }
        if self.oscillators.len() >= 128 {
            return Err(OpError::new(
                "QuotaExceededError",
                "audio source limit exceeded",
            ));
        }
        self.next_oscillator = self.next_oscillator.wrapping_add(1).max(1);
        let id = self.next_oscillator;
        self.oscillators.insert(
            id,
            OscillatorRecord {
                id,
                context,
                frequency_hz: 440.0,
                playback_rate: 1.0,
                ..OscillatorRecord::default()
            },
        );
        Ok(id)
    }

    fn create_buffer_source(&mut self, context: u64) -> OpResult<u64> {
        let id = self.create_oscillator(context)?;
        if let Some(source) = self.oscillators.get_mut(&id) {
            source.buffer_source = true;
        }
        Ok(id)
    }

    fn create_gain(&mut self, context: u64) -> OpResult<u64> {
        if !self.contexts.contains_key(&context) {
            return Err(OpError::new("InvalidStateError", "audio context is closed"));
        }
        if self.oscillators.len() + self.gains.len() >= 128 {
            return Err(OpError::new(
                "QuotaExceededError",
                "audio node limit exceeded",
            ));
        }
        self.next_oscillator = self.next_oscillator.wrapping_add(1).max(1);
        let id = self.next_oscillator;
        self.gains.insert(
            id,
            GainRecord {
                context,
                value: 1.0,
                events: Vec::new(),
                connections: Vec::new(),
            },
        );
        Ok(id)
    }

    fn snapshots(&self) -> Vec<ContextSnapshot> {
        self.contexts
            .iter()
            .map(|(&id, context)| ContextSnapshot {
                id,
                state: context.state,
                current_time: context.current_time,
                sample_rate: context.sample_rate,
                wants_running: context.wants_running,
                gains: {
                    let mut gains = self
                        .gains
                        .iter()
                        .filter(|(_, gain)| gain.context == id)
                        .map(|(&gain_id, gain)| GainSnapshot {
                            id: gain_id,
                            context_time: context.current_time,
                            value: gain.value,
                            events: gain.events.clone(),
                            connections: gain.connections.clone(),
                        })
                        .collect::<Vec<_>>();
                    gains.sort_by_key(|gain| gain.id);
                    topological_gains(gains)
                },
                oscillators: self
                    .oscillators
                    .values()
                    .filter(|source| source.context == id)
                    .map(|source| OscillatorSnapshot {
                        id: source.id,
                        context: source.context,
                        context_time: context.current_time,
                        frequency_hz: source.frequency_hz,
                        frequency_events: source.frequency_events.clone(),
                        waveform: source.waveform,
                        connections: source.connections.clone(),
                        started: source.started,
                        stopped: source.stopped,
                        start_time: source.start_time,
                        stop_time: source.stop_time,
                        phase: source.phase,
                        buffer: source.render_buffer.clone(),
                        buffer_source: source.buffer_source,
                        playback_rate: source.playback_rate,
                        playback_rate_events: source.playback_rate_events.clone(),
                        buffer_offset: source.buffer_offset,
                        loop_enabled: source.loop_enabled,
                        loop_start: source.loop_start,
                        loop_end: source.loop_end,
                    })
                    .collect(),
            })
            .collect()
    }
}

fn topological_gains(mut gains: Vec<GainSnapshot>) -> Vec<GainSnapshot> {
    let mut emitted = Vec::with_capacity(gains.len());
    let mut remaining = std::mem::take(&mut gains);
    while !remaining.is_empty() {
        let Some(index) = remaining.iter().position(|gain| {
            !remaining.iter().any(|predecessor| {
                predecessor.id != gain.id
                    && predecessor
                        .connections
                        .contains(&AudioTarget::Gain(gain.id))
                    && !emitted
                        .iter()
                        .any(|done: &GainSnapshot| done.id == predecessor.id)
            })
        }) else {
            // Connection validation prevents cycles; retain remaining gains in
            // stable order if corrupted state is ever observed.
            emitted.extend(remaining);
            break;
        };
        emitted.push(remaining.remove(index));
    }
    emitted
}

fn graph_target(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    context: u64,
    destination: &Value,
    output: u32,
    input: u32,
) -> OpResult<AudioTarget> {
    if output != 0 || input != 0 {
        return Err(OpError::new(
            "IndexSizeError",
            "Web Audio nodes expose one output and one input",
        ));
    }
    if let Ok(same_context) = ctx
        .with_instance::<DomAudioDestinationNode, _>(destination, |target| {
            Rc::ptr_eq(realm, &target.realm) && target.context == context
        })
    {
        if same_context {
            return Ok(AudioTarget::Destination);
        }
        return Err(OpError::new(
            "InvalidAccessError",
            "audio nodes belong to different contexts",
        ));
    }
    let same_context = ctx
        .with_instance::<DomGainNode, _>(destination, |target| {
            Rc::ptr_eq(realm, &target.realm) && target.context == context
        })
        .map_err(|_| OpError::new("TypeError", "destination must be an AudioNode"))?;
    if !same_context {
        return Err(OpError::new(
            "InvalidAccessError",
            "audio nodes belong to different contexts",
        ));
    }
    let id = ctx
        .with_instance::<DomGainNode, _>(destination, |target| target.id)
        .map_err(|_| OpError::new("TypeError", "destination must be an AudioNode"))?;
    Ok(AudioTarget::Gain(id))
}

fn gain_reaches(controller: &WebAudioController, context: u64, from: u64, to: u64) -> bool {
    let mut pending = vec![from];
    let mut visited = std::collections::BTreeSet::new();
    while let Some(id) = pending.pop() {
        if id == to {
            return true;
        }
        if !visited.insert(id) {
            continue;
        }
        if let Some(gain) = controller
            .gains
            .get(&id)
            .filter(|gain| gain.context == context)
        {
            pending.extend(gain.connections.iter().filter_map(|target| match target {
                AudioTarget::Gain(next) => Some(*next),
                AudioTarget::Destination => None,
            }));
        }
    }
    false
}

fn connect_graph_node(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    context: u64,
    source: u64,
    destination: Value,
    output: u32,
    input: u32,
) -> OpResult<Value> {
    let target = graph_target(ctx, realm, context, &destination, output, input)?;
    let mut controller = realm.web_audio.borrow_mut();
    let source_is_gain = controller
        .gains
        .get(&source)
        .is_some_and(|gain| gain.context == context);
    let source_exists = source_is_gain
        || controller
            .oscillators
            .get(&source)
            .is_some_and(|source| source.context == context);
    if !source_exists {
        return Err(OpError::new(
            "InvalidStateError",
            "audio node is unavailable",
        ));
    }
    if source_is_gain {
        if let AudioTarget::Gain(next) = target {
            if gain_reaches(&controller, context, next, source) {
                return Err(OpError::new(
                    "InvalidAccessError",
                    "audio graph cycles require an unsupported DelayNode",
                ));
            }
        }
    }
    let connections = if source_is_gain {
        &mut controller
            .gains
            .get_mut(&source)
            .expect("gain exists")
            .connections
    } else {
        &mut controller
            .oscillators
            .get_mut(&source)
            .expect("source exists")
            .connections
    };
    if !connections.contains(&target) {
        connections.push(target);
    }
    Ok(destination)
}

fn disconnect_graph_node(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    context: u64,
    source: u64,
    destination: Option<Value>,
    output: Option<u32>,
    input: Option<u32>,
) -> OpResult<()> {
    let target = match destination {
        None if output.is_none() && input.is_none() => None,
        None => {
            return Err(OpError::new(
                "TypeError",
                "disconnect requires a destination or output index",
            ));
        }
        Some(Value::Num(index)) if output.is_none() && input.is_none() => {
            if !index.is_finite() || index.fract() != 0.0 || index < 0.0 || index > u32::MAX as f64
            {
                return Err(OpError::new("IndexSizeError", "output index is invalid"));
            }
            if index != 0.0 {
                return Err(OpError::new(
                    "IndexSizeError",
                    "audio node output index is out of range",
                ));
            }
            None
        }
        Some(destination) => Some(graph_target(
            ctx,
            realm,
            context,
            &destination,
            output.unwrap_or(0),
            input.unwrap_or(0),
        )?),
    };
    let mut controller = realm.web_audio.borrow_mut();
    let connections = if controller
        .gains
        .get(&source)
        .is_some_and(|gain| gain.context == context)
    {
        &mut controller
            .gains
            .get_mut(&source)
            .expect("gain exists")
            .connections
    } else if controller
        .oscillators
        .get(&source)
        .is_some_and(|node| node.context == context)
    {
        &mut controller
            .oscillators
            .get_mut(&source)
            .expect("source exists")
            .connections
    } else {
        return Err(OpError::new(
            "InvalidStateError",
            "audio node is unavailable",
        ));
    };
    if let Some(target) = target {
        connections.retain(|connection| *connection != target);
    } else {
        connections.clear();
    }
    Ok(())
}

impl DomRealm {
    pub fn webaudio_contexts(&self) -> Vec<ContextSnapshot> {
        self.web_audio.borrow().snapshots()
    }

    pub fn webaudio_set_running(&self, id: u64, sample_rate: u32, ctx: &mut Ctx) {
        let mut controller = self.web_audio.borrow_mut();
        let Some(context) = controller.contexts.get_mut(&id) else {
            return;
        };
        context.state = ContextState::Running;
        context.wants_running = true;
        if sample_rate != 0 {
            context.sample_rate = sample_rate;
        }
        for waiter in context.resume_waiters.drain(..) {
            waiter.resolve(ctx, Value::Undefined);
        }
        context.wants_running = false;
    }

    pub fn webaudio_reject_resume(&self, id: u64, ctx: &mut Ctx, message: &str) {
        let mut controller = self.web_audio.borrow_mut();
        let Some(context) = controller.contexts.get_mut(&id) else {
            return;
        };
        for waiter in context.resume_waiters.drain(..) {
            waiter.reject(ctx, OpError::new("NotSupportedError", message.to_owned()));
        }
    }

    pub fn webaudio_set_time(&self, id: u64, time: f64) {
        if let Some(context) = self.web_audio.borrow_mut().contexts.get_mut(&id) {
            context.current_time = time.max(context.current_time);
        }
    }

    pub fn webaudio_set_phases(&self, phases: &[(u64, f64)]) {
        let mut controller = self.web_audio.borrow_mut();
        for (id, phase) in phases {
            if let Some(source) = controller.oscillators.get_mut(id) {
                source.phase = if source.buffer_source {
                    *phase
                } else {
                    phase.fract()
                };
            }
        }
    }

    pub fn webaudio_mark_stopped(&self, source_ids: &[u64]) {
        let mut controller = self.web_audio.borrow_mut();
        for id in source_ids {
            if let Some(source) = controller.oscillators.get_mut(id) {
                source.stopped = true;
            }
        }
    }

    pub fn webaudio_queue_ended(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        source_ids: &[u64],
    ) -> OpResult<()> {
        for id in source_ids {
            let wrapper = {
                let controller = self.web_audio.borrow();
                let Some(source) = controller.oscillators.get(id) else {
                    continue;
                };
                let Some(wrapper) = source.wrapper.as_ref().and_then(WeakValue::upgrade) else {
                    continue;
                };
                wrapper
            };
            crate::scheduling::queue_task(ctx, move |ctx| {
                let global = ctx.global_object();
                let constructor = ctx
                    .get_member(&global, "Event")
                    .map_err(lumen::embed::abrupt_value)
                    .map_err(OpError::thrown)?;
                let event = ctx
                    .construct_value(constructor, &[Value::str("ended")])
                    .map_err(OpError::thrown)?;
                let event = JsObject::from_value(event).ok_or_else(|| {
                    OpError::new("TypeError", "Event constructor returned a non-object")
                })?;
                crate::events::dispatch_user_agent_event(ctx, lumen_bind::This(wrapper), event)
                    .map(|_| ())
            })?;
        }
        Ok(())
    }
}

fn active_realm(ctx: &mut Ctx) -> OpResult<Rc<DomRealm>> {
    let global = ctx.global_object();
    let document_value = ctx
        .get_member(&global, "document")
        .map_err(|_| OpError::new("TypeError", "AudioContext has no active document"))?;
    ctx.with_instance::<DomDocument, _>(&document_value, |document| document.realm.clone())
        .map_err(|_| OpError::new("TypeError", "AudioContext has no active document"))
}

fn snapshot_audio_buffer(ctx: &Ctx, data: &AudioBufferData) -> OpResult<Arc<BufferSnapshot>> {
    let channels = data
        .channels
        .iter()
        .map(|channel| {
            let bytes = ctx.typed_array_bytes(channel).ok_or_else(|| {
                OpError::new("InvalidStateError", "audio buffer channel is detached")
            })?;
            if bytes.len() != data.length.saturating_mul(4) {
                return Err(OpError::new(
                    "InvalidStateError",
                    "audio buffer channel length changed",
                ));
            }
            Ok(bytes
                .chunks_exact(4)
                .map(|sample| f32::from_ne_bytes([sample[0], sample[1], sample[2], sample[3]]))
                .collect())
        })
        .collect::<OpResult<Vec<Vec<f32>>>>()?;
    Ok(Arc::new(BufferSnapshot {
        sample_rate: data.sample_rate,
        length: data.length,
        channels,
    }))
}

#[lumen_bind::class(name = "AudioContext", hint(js(webidl)))]
pub struct DomAudioContext {
    realm: Rc<DomRealm>,
    id: u64,
}

#[lumen_bind::methods]
impl DomAudioContext {
    #[constructor]
    fn new(ctx: &mut Ctx) -> OpResult<Self> {
        let realm = active_realm(ctx)?;
        let id = realm.web_audio.borrow_mut().create_context();
        if id == 0 {
            return Err(OpError::new(
                "QuotaExceededError",
                "audio context limit exceeded",
            ));
        }
        Ok(Self { realm, id })
    }

    #[getter]
    fn state(&self) -> &'static str {
        match self
            .realm
            .web_audio
            .borrow()
            .contexts
            .get(&self.id)
            .map_or(ContextState::Closed, |context| context.state)
        {
            ContextState::Suspended => "suspended",
            ContextState::Running => "running",
            ContextState::Closed => "closed",
        }
    }

    #[getter(rename(js = "sampleRate"))]
    fn sample_rate(&self) -> f64 {
        self.realm
            .web_audio
            .borrow()
            .contexts
            .get(&self.id)
            .map_or(0, |context| context.sample_rate) as f64
    }

    #[getter(rename(js = "currentTime"))]
    fn current_time(&self) -> f64 {
        self.realm
            .web_audio
            .borrow()
            .contexts
            .get(&self.id)
            .map_or(0.0, |context| context.current_time)
    }

    #[getter]
    fn destination(&self) -> DomAudioDestinationNode {
        DomAudioDestinationNode {
            realm: self.realm.clone(),
            context: self.id,
        }
    }

    #[getter(rename(js = "baseLatency"))]
    fn base_latency(&self) -> f64 {
        0.0
    }

    fn create_oscillator(&self) -> OpResult<DomOscillatorNode> {
        let id = self
            .realm
            .web_audio
            .borrow_mut()
            .create_oscillator(self.id)?;
        let base = crate::events::DomEventTarget::independent(&self.realm);
        Ok(DomOscillatorNode {
            realm: self.realm.clone(),
            id,
            context: self.id,
            base,
        })
    }

    fn create_gain(&self) -> OpResult<DomGainNode> {
        let id = self.realm.web_audio.borrow_mut().create_gain(self.id)?;
        Ok(DomGainNode {
            realm: self.realm.clone(),
            id,
            context: self.id,
        })
    }

    fn create_buffer_source(&self) -> OpResult<DomAudioBufferSourceNode> {
        let id = self
            .realm
            .web_audio
            .borrow_mut()
            .create_buffer_source(self.id)?;
        let base = crate::events::DomEventTarget::independent(&self.realm);
        Ok(DomAudioBufferSourceNode {
            realm: self.realm.clone(),
            id,
            context: self.id,
            base,
        })
    }

    fn create_buffer(
        &self,
        ctx: &mut Ctx,
        channels: u8,
        length: u32,
        sample_rate: f64,
    ) -> OpResult<DomAudioBuffer> {
        if !(1..=2).contains(&channels)
            || length == 0
            || length > 1_000_000
            || !sample_rate.is_finite()
            || !(8_000.0..=192_000.0).contains(&sample_rate)
        {
            return Err(OpError::new(
                "NotSupportedError",
                "buffer dimensions or sample rate are unsupported",
            ));
        }
        let global = ctx.global_object();
        let constructor = ctx
            .get_member(&global, "Float32Array")
            .map_err(|_| OpError::new("NotSupportedError", "Float32Array is unavailable"))?;
        let mut data = AudioBufferData {
            sample_rate: sample_rate as u32,
            length: length as usize,
            channels: Vec::with_capacity(channels as usize),
        };
        for _ in 0..channels {
            let channel = ctx
                .construct_value(constructor.clone(), &[Value::Num(f64::from(length))])
                .map_err(OpError::thrown)?;
            data.channels.push(channel);
        }
        Ok(DomAudioBuffer {
            data: Rc::new(RefCell::new(data)),
        })
    }

    fn resume(&self, ctx: &mut Ctx) -> Value {
        let deferred = Deferred::new(ctx);
        let promise = deferred.promise();
        if let Some(context) = self.realm.web_audio.borrow_mut().contexts.get_mut(&self.id) {
            if context.state == ContextState::Running {
                deferred.resolve(ctx, Value::Undefined);
            } else if context.state == ContextState::Closed {
                deferred.reject(ctx, OpError::new("InvalidStateError", "context is closed"));
            } else {
                context.wants_running = true;
                context.resume_waiters.push(deferred);
            }
        } else {
            deferred.reject(ctx, OpError::new("InvalidStateError", "context is closed"));
        }
        promise
    }

    fn suspend(&self, ctx: &mut Ctx) -> Value {
        if let Some(context) = self.realm.web_audio.borrow_mut().contexts.get_mut(&self.id) {
            if context.state != ContextState::Closed {
                context.state = ContextState::Suspended;
                context.wants_running = false;
            }
        }
        let deferred = Deferred::new(ctx);
        let promise = deferred.promise();
        deferred.resolve(ctx, Value::Undefined);
        promise
    }

    fn close(&self, ctx: &mut Ctx) -> Value {
        if let Some(context) = self.realm.web_audio.borrow_mut().contexts.get_mut(&self.id) {
            context.state = ContextState::Closed;
            context.wants_running = false;
            for waiter in context.resume_waiters.drain(..) {
                waiter.reject(ctx, OpError::new("InvalidStateError", "context was closed"));
            }
        }
        let deferred = Deferred::new(ctx);
        let promise = deferred.promise();
        deferred.resolve(ctx, Value::Undefined);
        promise
    }
}

#[lumen_bind::class(name = "AudioBuffer", hint(js(webidl)))]
pub struct DomAudioBuffer {
    data: Rc<RefCell<AudioBufferData>>,
}

#[lumen_bind::methods]
impl DomAudioBuffer {
    #[getter]
    fn length(&self) -> u32 {
        self.data.borrow().length as u32
    }
    #[getter(rename(js = "sampleRate"))]
    fn sample_rate(&self) -> f64 {
        f64::from(self.data.borrow().sample_rate)
    }
    #[getter(rename(js = "numberOfChannels"))]
    fn number_of_channels(&self) -> u8 {
        self.data.borrow().channels.len() as u8
    }
    #[getter]
    fn duration(&self) -> f64 {
        let data = self.data.borrow();
        data.length as f64 / f64::from(data.sample_rate)
    }

    fn get_channel_data(&self, channel: u8) -> OpResult<Value> {
        self.data
            .borrow()
            .channels
            .get(channel as usize)
            .cloned()
            .ok_or_else(|| OpError::new("IndexSizeError", "audio buffer channel is out of range"))
    }

    fn copy_to_channel(
        &self,
        ctx: &mut Ctx,
        source: Value,
        channel: u8,
        #[default(0)] start: u32,
    ) -> OpResult<()> {
        if !ctx
            .typed_array_raw(&source)
            .is_some_and(|(kind, _, _)| kind == 7)
        {
            return Err(OpError::new("TypeError", "source must be a Float32Array"));
        }
        let bytes = ctx
            .typed_array_bytes(&source)
            .ok_or_else(|| OpError::new("TypeError", "source must be a typed array"))?;
        if bytes.len() % 4 != 0 {
            return Err(OpError::new(
                "TypeError",
                "source must contain Float32 samples",
            ));
        }
        let data = self.data.borrow();
        let target = data.channels.get(channel as usize).ok_or_else(|| {
            OpError::new("IndexSizeError", "audio buffer channel is out of range")
        })?;
        let byte_start = (start as usize)
            .checked_mul(4)
            .ok_or_else(|| OpError::new("RangeError", "channel offset is too large"))?;
        if byte_start > data.length * 4 {
            return Err(OpError::new(
                "RangeError",
                "channel offset is outside the buffer",
            ));
        }
        let count = bytes.len().min(data.length * 4 - byte_start);
        let mut target_bytes = ctx.typed_array_bytes(target).unwrap_or_default();
        target_bytes[byte_start..byte_start + count].copy_from_slice(&bytes[..count]);
        if !ctx.typed_array_set_bytes(target, &target_bytes) {
            return Err(OpError::new(
                "InvalidStateError",
                "audio buffer channel is detached",
            ));
        }
        Ok(())
    }

    fn copy_from_channel(
        &self,
        ctx: &mut Ctx,
        destination: Value,
        channel: u8,
        #[default(0)] start: u32,
    ) -> OpResult<()> {
        if !ctx
            .typed_array_raw(&destination)
            .is_some_and(|(kind, _, _)| kind == 7)
        {
            return Err(OpError::new(
                "TypeError",
                "destination must be a Float32Array",
            ));
        }
        let data = self.data.borrow();
        let source = data.channels.get(channel as usize).ok_or_else(|| {
            OpError::new("IndexSizeError", "audio buffer channel is out of range")
        })?;
        let destination_bytes = ctx
            .typed_array_bytes(&destination)
            .ok_or_else(|| OpError::new("TypeError", "destination must be a typed array"))?;
        if destination_bytes.len() % 4 != 0 {
            return Err(OpError::new(
                "TypeError",
                "destination must contain Float32 samples",
            ));
        }
        let source_bytes = ctx.typed_array_bytes(source).unwrap_or_default();
        let byte_start = (start as usize)
            .checked_mul(4)
            .ok_or_else(|| OpError::new("RangeError", "channel offset is too large"))?;
        if byte_start > source_bytes.len() {
            return Err(OpError::new(
                "RangeError",
                "channel offset is outside the buffer",
            ));
        }
        let count = destination_bytes.len().min(source_bytes.len() - byte_start);
        let mut bytes = destination_bytes;
        bytes[..count].copy_from_slice(&source_bytes[byte_start..byte_start + count]);
        if !ctx.typed_array_set_bytes(&destination, &bytes) {
            return Err(OpError::new(
                "InvalidStateError",
                "destination typed array is detached",
            ));
        }
        Ok(())
    }
}

#[lumen_bind::class(name = "AudioDestinationNode", hint(js(webidl)))]
pub struct DomAudioDestinationNode {
    realm: Rc<DomRealm>,
    context: u64,
}

#[lumen_bind::methods]
impl DomAudioDestinationNode {
    #[getter(rename(js = "maxChannelCount"))]
    fn max_channel_count(&self) -> u8 {
        1
    }

    #[getter(rename(js = "channelCount"))]
    fn channel_count(&self) -> u8 {
        1
    }
}

#[lumen_bind::class(name = "GainNode", hint(js(webidl)))]
pub struct DomGainNode {
    realm: Rc<DomRealm>,
    id: u64,
    context: u64,
}

#[lumen_bind::methods]
impl DomGainNode {
    #[getter]
    fn gain(&self) -> DomAudioParam {
        DomAudioParam {
            realm: self.realm.clone(),
            target: ParamTarget::Gain(self.id),
        }
    }

    fn connect(
        &self,
        ctx: &mut Ctx,
        destination: Value,
        #[default(0)] output: u32,
        #[default(0)] input: u32,
    ) -> OpResult<Value> {
        connect_graph_node(
            ctx,
            &self.realm,
            self.context,
            self.id,
            destination,
            output,
            input,
        )
    }

    fn disconnect(
        &self,
        ctx: &mut Ctx,
        destination: Option<Value>,
        output: Option<u32>,
        input: Option<u32>,
    ) -> OpResult<()> {
        disconnect_graph_node(
            ctx,
            &self.realm,
            self.context,
            self.id,
            destination,
            output,
            input,
        )
    }
}

#[lumen_bind::class(name = "AudioBufferSourceNode", extends = crate::events::DomEventTarget, hint(js(webidl)))]
pub struct DomAudioBufferSourceNode {
    base: crate::events::DomEventTarget,
    realm: Rc<DomRealm>,
    id: u64,
    context: u64,
}

#[lumen_bind::methods]
impl DomAudioBufferSourceNode {
    #[getter]
    fn onended(&self) -> Nullable<JsFunction> {
        Nullable(self.base.handler("ended"))
    }

    #[setter]
    fn set_onended(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<JsFunction>,
    ) {
        self.base.set_handler(ctx, &this.0, "ended", callback);
    }

    #[getter]
    fn buffer(&self) -> Nullable<DomAudioBuffer> {
        Nullable(self.realm
            .web_audio
            .borrow()
            .oscillators
            .get(&self.id)
            .and_then(|source| source.buffer.as_ref())
            .map(|data| DomAudioBuffer { data: data.clone() }))
    }

    #[setter]
    fn set_buffer(&self, ctx: &mut Ctx, value: Value) -> OpResult<()> {
        if let Value::Null = value {
            if let Some(source) = self
                .realm
                .web_audio
                .borrow_mut()
                .oscillators
                .get_mut(&self.id)
            {
                if source.started {
                    return Err(OpError::new(
                        "InvalidStateError",
                        "buffer cannot change after start",
                    ));
                }
                source.buffer = None;
                source.render_buffer = None;
            }
            return Ok(());
        }
        let buffer = ctx
            .with_instance::<DomAudioBuffer, _>(&value, |buffer| buffer.data.clone())
            .map_err(|_| OpError::new("TypeError", "buffer must be an AudioBuffer or null"))?;
        if let Some(source) = self
            .realm
            .web_audio
            .borrow_mut()
            .oscillators
            .get_mut(&self.id)
        {
            if source.started {
                return Err(OpError::new(
                    "InvalidStateError",
                    "buffer cannot change after start",
                ));
            }
            source.buffer = Some(buffer);
            source.render_buffer = None;
            return Ok(());
        }
        Err(OpError::new(
            "InvalidStateError",
            "buffer source is unavailable",
        ))
    }

    #[getter(rename(js = "playbackRate"))]
    fn playback_rate(&self) -> DomAudioParam {
        DomAudioParam {
            realm: self.realm.clone(),
            target: ParamTarget::PlaybackRate(self.id),
        }
    }

    #[getter(rename(js = "loop"))]
    fn loop_enabled(&self) -> bool {
        self.realm
            .web_audio
            .borrow()
            .oscillators
            .get(&self.id)
            .is_some_and(|source| source.loop_enabled)
    }

    #[setter(rename(js = "loop"))]
    fn set_loop_enabled(&self, enabled: bool) {
        if let Some(source) = self
            .realm
            .web_audio
            .borrow_mut()
            .oscillators
            .get_mut(&self.id)
        {
            source.loop_enabled = enabled;
        }
    }

    #[getter(rename(js = "loopStart"))]
    fn loop_start(&self) -> f64 {
        self.realm
            .web_audio
            .borrow()
            .oscillators
            .get(&self.id)
            .map_or(0.0, |source| source.loop_start)
    }
    #[setter(rename(js = "loopStart"), coerce)]
    fn set_loop_start(&self, value: f64) -> OpResult<()> {
        self.set_loop_point(value, true)
    }
    #[getter(rename(js = "loopEnd"))]
    fn loop_end(&self) -> f64 {
        self.realm
            .web_audio
            .borrow()
            .oscillators
            .get(&self.id)
            .map_or(0.0, |source| source.loop_end)
    }
    #[setter(rename(js = "loopEnd"), coerce)]
    fn set_loop_end(&self, value: f64) -> OpResult<()> {
        self.set_loop_point(value, false)
    }

    fn connect(
        &self,
        ctx: &mut Ctx,
        destination: Value,
        #[default(0)] output: u32,
        #[default(0)] input: u32,
    ) -> OpResult<Value> {
        connect_graph_node(
            ctx,
            &self.realm,
            self.context,
            self.id,
            destination,
            output,
            input,
        )
    }

    fn disconnect(
        &self,
        ctx: &mut Ctx,
        destination: Option<Value>,
        output: Option<u32>,
        input: Option<u32>,
    ) -> OpResult<()> {
        disconnect_graph_node(
            ctx,
            &self.realm,
            self.context,
            self.id,
            destination,
            output,
            input,
        )
    }

    fn start(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        #[default(0.0)] when: f64,
        #[default(0.0)] offset: f64,
        duration: Option<f64>,
    ) -> OpResult<()> {
        if !when.is_finite()
            || when < 0.0
            || !offset.is_finite()
            || offset < 0.0
            || duration.is_some_and(|duration| !duration.is_finite() || duration < 0.0)
        {
            return Err(OpError::new(
                "RangeError",
                "invalid buffer start time, offset, or duration",
            ));
        }
        let buffer_data = self
            .realm
            .web_audio
            .borrow()
            .oscillators
            .get(&self.id)
            .and_then(|source| source.buffer.clone());
        let render_buffer = buffer_data
            .as_ref()
            .map(|data| snapshot_audio_buffer(ctx, &data.borrow()))
            .transpose()?;
        let mut controller = self.realm.web_audio.borrow_mut();
        let Some(source) = controller.oscillators.get_mut(&self.id) else {
            return Err(OpError::new(
                "InvalidStateError",
                "buffer source is unavailable",
            ));
        };
        if source.started {
            return Err(OpError::new(
                "InvalidStateError",
                "buffer source already started",
            ));
        }
        source.started = true;
        source.stopped = false;
        source.wrapper = ctx.weak_value(&this.0);
        source.start_time = when;
        source.buffer_offset = offset;
        source.phase = offset;
        source.render_buffer = render_buffer;
        source.stop_time =
            duration.map(|duration| when + duration / f64::from(source.playback_rate.max(0.0001)));
        Ok(())
    }

    fn stop(&self, #[default(0.0)] when: f64) -> OpResult<()> {
        if !when.is_finite() || when < 0.0 {
            return Err(OpError::new(
                "RangeError",
                "stop time must be finite and non-negative",
            ));
        }
        if let Some(source) = self
            .realm
            .web_audio
            .borrow_mut()
            .oscillators
            .get_mut(&self.id)
        {
            source.stop_time = Some(when);
            return Ok(());
        }
        Err(OpError::new(
            "InvalidStateError",
            "buffer source is unavailable",
        ))
    }
}

impl DomAudioBufferSourceNode {
    fn set_loop_point(&self, value: f64, start: bool) -> OpResult<()> {
        if !value.is_finite() || value < 0.0 {
            return Err(OpError::new(
                "RangeError",
                "loop point must be finite and non-negative",
            ));
        }
        if let Some(source) = self
            .realm
            .web_audio
            .borrow_mut()
            .oscillators
            .get_mut(&self.id)
        {
            if start {
                source.loop_start = value;
            } else {
                source.loop_end = value;
            }
            return Ok(());
        }
        Err(OpError::new(
            "InvalidStateError",
            "buffer source is unavailable",
        ))
    }
}

#[derive(Clone, Copy)]
enum ParamTarget {
    Frequency(u64),
    Gain(u64),
    PlaybackRate(u64),
}

#[lumen_bind::class(name = "OscillatorNode", extends = crate::events::DomEventTarget, hint(js(webidl)))]
pub struct DomOscillatorNode {
    base: crate::events::DomEventTarget,
    realm: Rc<DomRealm>,
    id: u64,
    context: u64,
}

#[lumen_bind::methods]
impl DomOscillatorNode {
    #[getter]
    fn onended(&self) -> Nullable<JsFunction> {
        Nullable(self.base.handler("ended"))
    }

    #[setter]
    fn set_onended(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<JsFunction>,
    ) {
        self.base.set_handler(ctx, &this.0, "ended", callback);
    }

    #[getter]
    fn frequency(&self) -> DomAudioParam {
        DomAudioParam {
            realm: self.realm.clone(),
            target: ParamTarget::Frequency(self.id),
        }
    }

    #[getter(rename(js = "type"))]
    fn waveform(&self) -> &'static str {
        match self
            .realm
            .web_audio
            .borrow()
            .oscillators
            .get(&self.id)
            .map_or(Waveform::Sine, |source| source.waveform)
        {
            Waveform::Sine => "sine",
            Waveform::Square => "square",
            Waveform::Sawtooth => "sawtooth",
            Waveform::Triangle => "triangle",
        }
    }

    #[setter(rename(js = "type"))]
    fn set_waveform(&self, value: &str) -> OpResult<()> {
        let waveform = match value {
            "sine" => Waveform::Sine,
            "square" => Waveform::Square,
            "sawtooth" => Waveform::Sawtooth,
            "triangle" => Waveform::Triangle,
            _ => {
                return Err(OpError::new(
                    "NotSupportedError",
                    "unsupported oscillator type",
                ));
            }
        };
        if let Some(source) = self
            .realm
            .web_audio
            .borrow_mut()
            .oscillators
            .get_mut(&self.id)
        {
            source.waveform = waveform;
        }
        Ok(())
    }

    fn connect(
        &self,
        ctx: &mut Ctx,
        destination: Value,
        #[default(0)] output: u32,
        #[default(0)] input: u32,
    ) -> OpResult<Value> {
        connect_graph_node(
            ctx,
            &self.realm,
            self.context,
            self.id,
            destination,
            output,
            input,
        )
    }

    fn disconnect(
        &self,
        ctx: &mut Ctx,
        destination: Option<Value>,
        output: Option<u32>,
        input: Option<u32>,
    ) -> OpResult<()> {
        disconnect_graph_node(
            ctx,
            &self.realm,
            self.context,
            self.id,
            destination,
            output,
            input,
        )
    }

    fn start(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        #[default(0.0)] when: f64,
    ) -> OpResult<()> {
        if !when.is_finite() || when < 0.0 {
            return Err(OpError::new(
                "RangeError",
                "start time must be finite and non-negative",
            ));
        }
        if let Some(source) = self
            .realm
            .web_audio
            .borrow_mut()
            .oscillators
            .get_mut(&self.id)
        {
            if source.started {
                return Err(OpError::new(
                    "InvalidStateError",
                    "oscillator already started",
                ));
            }
            source.started = true;
            source.stopped = false;
            source.wrapper = ctx.weak_value(&this.0);
            source.start_time = when;
            source.stop_time = None;
            return Ok(());
        }
        Err(OpError::new(
            "InvalidStateError",
            "oscillator is unavailable",
        ))
    }

    fn stop(&self, #[default(0.0)] when: f64) -> OpResult<()> {
        if !when.is_finite() || when < 0.0 {
            return Err(OpError::new(
                "RangeError",
                "stop time must be finite and non-negative",
            ));
        }
        let current_time = self
            .realm
            .web_audio
            .borrow()
            .contexts
            .get(&self.context)
            .map_or(0.0, |context| context.current_time);
        if let Some(source) = self
            .realm
            .web_audio
            .borrow_mut()
            .oscillators
            .get_mut(&self.id)
        {
            source.stop_time = Some(when);
            if when <= current_time {
                source.stopped = true;
            }
            return Ok(());
        }
        Err(OpError::new(
            "InvalidStateError",
            "oscillator is unavailable",
        ))
    }
}

#[lumen_bind::class(name = "AudioParam", hint(js(webidl)))]
pub struct DomAudioParam {
    realm: Rc<DomRealm>,
    target: ParamTarget,
}

#[lumen_bind::methods]
impl DomAudioParam {
    #[getter]
    fn value(&self) -> f64 {
        let controller = self.realm.web_audio.borrow();
        match self.target {
            ParamTarget::Frequency(id) => controller
                .oscillators
                .get(&id)
                .map_or(0.0, |source| f64::from(source.frequency_hz)),
            ParamTarget::Gain(id) => controller
                .gains
                .get(&id)
                .map_or(0.0, |gain| f64::from(gain.value)),
            ParamTarget::PlaybackRate(id) => controller
                .oscillators
                .get(&id)
                .map_or(1.0, |source| f64::from(source.playback_rate)),
        }
    }

    #[setter]
    fn set_value(&self, value: f64) -> OpResult<()> {
        match self.target {
            ParamTarget::Frequency(id) => {
                if !value.is_finite() || !(0.0..=24_000.0).contains(&value) {
                    return Err(OpError::new(
                        "RangeError",
                        "frequency must be between 0 and 24000 Hz",
                    ));
                }
                if let Some(source) = self.realm.web_audio.borrow_mut().oscillators.get_mut(&id) {
                    source.frequency_hz = value as f32;
                    source.frequency_events.clear();
                    return Ok(());
                }
            }
            ParamTarget::Gain(id) => {
                if !value.is_finite() || !(-100.0..=100.0).contains(&value) {
                    return Err(OpError::new(
                        "RangeError",
                        "gain must be finite and within bounds",
                    ));
                }
                if let Some(gain) = self.realm.web_audio.borrow_mut().gains.get_mut(&id) {
                    gain.value = value as f32;
                    gain.events.clear();
                    return Ok(());
                }
            }
            ParamTarget::PlaybackRate(id) => {
                if !value.is_finite() || !(0.0..=16.0).contains(&value) {
                    return Err(OpError::new(
                        "RangeError",
                        "playback rate must be between 0 and 16",
                    ));
                }
                if let Some(source) = self.realm.web_audio.borrow_mut().oscillators.get_mut(&id) {
                    source.playback_rate = value as f32;
                    source.playback_rate_events.clear();
                    return Ok(());
                }
            }
        }
        Err(OpError::new(
            "InvalidStateError",
            "audio parameter is unavailable",
        ))
    }

    fn set_value_at_time(&self, value: f64, time: f64) -> OpResult<Value> {
        self.push_event(value, time, false)?;
        Ok(Value::Undefined)
    }

    fn linear_ramp_to_value_at_time(&self, value: f64, time: f64) -> OpResult<Value> {
        self.push_event(value, time, true)?;
        Ok(Value::Undefined)
    }

    fn cancel_scheduled_values(&self, start_time: f64) -> OpResult<Value> {
        if !start_time.is_finite() || start_time < 0.0 {
            return Err(OpError::new(
                "RangeError",
                "automation time must be finite and non-negative",
            ));
        }
        match self.target {
            ParamTarget::Frequency(id) => {
                let mut controller = self.realm.web_audio.borrow_mut();
                if let Some(source) = controller.oscillators.get_mut(&id) {
                    source
                        .frequency_events
                        .retain(|event| event.time < start_time);
                }
            }
            ParamTarget::Gain(id) => {
                let mut controller = self.realm.web_audio.borrow_mut();
                if let Some(gain) = controller.gains.get_mut(&id) {
                    gain.events.retain(|event| event.time < start_time);
                }
            }
            ParamTarget::PlaybackRate(id) => {
                if let Some(source) = self.realm.web_audio.borrow_mut().oscillators.get_mut(&id) {
                    source
                        .playback_rate_events
                        .retain(|event| event.time < start_time);
                }
            }
        }
        Ok(Value::Undefined)
    }

    #[getter(rename(js = "defaultValue"))]
    fn default_value(&self) -> f64 {
        match self.target {
            ParamTarget::Frequency(_) => 440.0,
            ParamTarget::Gain(_) => 1.0,
            ParamTarget::PlaybackRate(_) => 1.0,
        }
    }
}

impl DomAudioParam {
    fn push_event(&self, value: f64, time: f64, linear: bool) -> OpResult<()> {
        if !time.is_finite() || time < 0.0 {
            return Err(OpError::new(
                "RangeError",
                "automation time must be finite and non-negative",
            ));
        }
        let valid_value = match self.target {
            ParamTarget::Frequency(_) => (0.0..=24_000.0).contains(&value),
            ParamTarget::Gain(_) => (-100.0..=100.0).contains(&value),
            ParamTarget::PlaybackRate(_) => (0.0..=16.0).contains(&value),
        };
        if !value.is_finite() || !valid_value {
            return Err(OpError::new(
                "RangeError",
                "automation value is outside supported bounds",
            ));
        }
        let event = ParamEvent {
            time,
            value: value as f32,
            linear,
        };
        match self.target {
            ParamTarget::Frequency(id) => {
                let mut controller = self.realm.web_audio.borrow_mut();
                let source = controller.oscillators.get_mut(&id).ok_or_else(|| {
                    OpError::new("InvalidStateError", "audio parameter is unavailable")
                })?;
                source.frequency_events.push(event);
                source
                    .frequency_events
                    .sort_by(|left, right| left.time.total_cmp(&right.time));
            }
            ParamTarget::Gain(id) => {
                let mut controller = self.realm.web_audio.borrow_mut();
                let gain = controller.gains.get_mut(&id).ok_or_else(|| {
                    OpError::new("InvalidStateError", "audio parameter is unavailable")
                })?;
                gain.events.push(event);
                gain.events
                    .sort_by(|left, right| left.time.total_cmp(&right.time));
            }
            ParamTarget::PlaybackRate(id) => {
                let mut controller = self.realm.web_audio.borrow_mut();
                let source = controller.oscillators.get_mut(&id).ok_or_else(|| {
                    OpError::new("InvalidStateError", "audio parameter is unavailable")
                })?;
                source.playback_rate_events.push(event);
                source
                    .playback_rate_events
                    .sort_by(|left, right| left.time.total_cmp(&right.time));
            }
        }
        Ok(())
    }
}

pub fn install(ctx: &mut Ctx) {
    let global = ctx.global_object();
    for (name, constructor) in [
        ("AudioContext", ctx.class_constructor::<DomAudioContext>()),
        (
            "webkitAudioContext",
            ctx.class_constructor::<DomAudioContext>(),
        ),
        (
            "AudioDestinationNode",
            ctx.class_constructor::<DomAudioDestinationNode>(),
        ),
        (
            "OscillatorNode",
            ctx.class_constructor::<DomOscillatorNode>(),
        ),
        ("GainNode", ctx.class_constructor::<DomGainNode>()),
        ("AudioParam", ctx.class_constructor::<DomAudioParam>()),
        ("AudioBuffer", ctx.class_constructor::<DomAudioBuffer>()),
        (
            "AudioBufferSourceNode",
            ctx.class_constructor::<DomAudioBufferSourceNode>(),
        ),
    ] {
        let _ = ctx.set_member(&global, name, constructor);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oscillator_bindings_create_and_connect_a_real_graph_node() {
        let mut engine = lumen::Engine::new();
        crate::install(engine.ctx(), "<body></body>", 64).unwrap();
        let value = engine
            .eval_value(
                "(() => { const c=new AudioContext(); const o=c.createOscillator(); const g=c.createGain(); o.frequency.value=880; g.gain.value=.25; g.gain.setValueAtTime(.5,1); g.gain.linearRampToValueAtTime(0,2); o.frequency.linearRampToValueAtTime(1000,1); o.type='square'; o.connect(g); g.connect(c.destination); o.start(.25); o.stop(2); return c.state==='suspended' && c.sampleRate===44100 && o.frequency.value===880 && g.gain.value===.25 && o.type==='square'; })()",
            )
            .unwrap();
        let value = match value {
            Ok(value) => value,
            Err(_) => panic!("Web Audio graph setup threw"),
        };
        assert!(matches!(value, Value::Bool(true)));
        let realm = active_realm(engine.ctx()).unwrap();
        let contexts = realm.webaudio_contexts();
        assert_eq!(contexts.len(), 1);
        assert_eq!(contexts[0].oscillators.len(), 1);
        assert_eq!(
            contexts[0].oscillators[0].connections,
            vec![AudioTarget::Gain(contexts[0].gains[0].id)]
        );
        assert!(contexts[0].oscillators[0].started);
        assert_eq!(contexts[0].oscillators[0].frequency_hz, 880.0);
        assert_eq!(contexts[0].oscillators[0].frequency_events.len(), 1);
        assert_eq!(contexts[0].oscillators[0].start_time, 0.25);
        assert_eq!(contexts[0].oscillators[0].stop_time, Some(2.0));
    }

    #[test]
    fn buffer_source_freezes_channel_samples_and_playback_parameters_at_start() {
        let mut engine = lumen::Engine::new();
        crate::install(engine.ctx(), "<body></body>", 64).unwrap();
        let result = engine.eval_value(
            "(() => { const c=new AudioContext(); const b=c.createBuffer(1,4,8000); b.getChannelData(0).set([1,.5,-.5,-1]); let rejectsWrongArray=false; try { b.copyToChannel(new Uint32Array(1),0) } catch(e) { rejectsWrongArray=e.name==='TypeError' }; const g=c.createGain(); g.gain.value=.25; g.gain.linearRampToValueAtTime(0,1); g.connect(c.destination); const s=c.createBufferSource(); s.buffer=b; s.playbackRate.value=2; s.playbackRate.setValueAtTime(3,1); s.loop=true; s.loopStart=.000125; s.loopEnd=.000375; s.connect(g); s.start(0,.000125); return rejectsWrongArray && b.length===4 && b.numberOfChannels===1; })()",
        ).unwrap();
        assert!(matches!(result, Ok(Value::Bool(true))));
        let realm = active_realm(engine.ctx()).unwrap();
        let contexts = realm.webaudio_contexts();
        let source = &contexts[0].oscillators[0];
        assert!(source.buffer_source && !source.connections.is_empty() && source.started);
        assert_eq!(source.playback_rate, 2.0);
        assert_eq!(source.playback_rate_events.len(), 1);
        assert!(source.loop_enabled);
        assert_eq!(source.buffer_offset, 0.000125);
        assert_eq!(contexts[0].gains.len(), 1);
        assert_eq!(contexts[0].gains[0].value, 0.25);
        assert_eq!(contexts[0].gains[0].events.len(), 1);
        let buffer = source.buffer.as_ref().unwrap();
        assert_eq!(buffer.sample_rate, 8000);
        assert_eq!(buffer.channels[0], [1.0, 0.5, -0.5, -1.0]);
    }

    #[test]
    fn graph_supports_chains_fan_in_fan_out_and_disconnect_overloads() {
        let mut engine = lumen::Engine::new();
        crate::install(engine.ctx(), "<body></body>", 64).unwrap();
        let result = engine.eval_value(
            "(() => { const c=new AudioContext(); globalThis.a=c.createOscillator(); const b=c.createOscillator(); globalThis.g1=c.createGain(); globalThis.g2=c.createGain(); a.connect(g1); a.connect(g1); b.connect(g1); g1.connect(g2); g1.connect(c.destination); g2.connect(c.destination); let cycle=false, badIndex=false; try { g2.connect(g1) } catch(e) { cycle=e.name==='InvalidAccessError' }; try { a.connect(g2,1,0) } catch(e) { badIndex=e.name==='IndexSizeError' }; return cycle && badIndex; })()",
        ).unwrap();
        assert!(matches!(result, Ok(Value::Bool(true))));
        let realm = active_realm(engine.ctx()).unwrap();
        let contexts = realm.webaudio_contexts();
        let graph = &contexts[0];
        assert_eq!(graph.gains.len(), 2);
        assert_eq!(
            graph.oscillators[0].connections.len(),
            1,
            "duplicate edges are ignored"
        );
        assert_eq!(graph.oscillators[1].connections.len(), 1);
        assert_eq!(
            graph.gains[0].connections,
            vec![
                AudioTarget::Gain(graph.gains[1].id),
                AudioTarget::Destination
            ]
        );
        assert_eq!(graph.gains[1].connections, vec![AudioTarget::Destination]);
        assert!(graph.gains[0].id < graph.gains[1].id);

        let result = engine
            .eval_value("g1.disconnect(g2); a.disconnect(0)")
            .unwrap();
        assert!(matches!(result, Ok(Value::Undefined)));
        let graph = &realm.webaudio_contexts()[0];
        assert_eq!(graph.gains[0].connections, vec![AudioTarget::Destination]);
        assert!(graph.oscillators[0].connections.is_empty());
    }
}
