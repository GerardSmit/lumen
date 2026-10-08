//! HTML audio element state and its owner-local media command queue.

use crate::events::HtmlTargetExt;
use lumen::embed::{Ctx, Deferred, OpError, OpResult, Value};
use std::{
    collections::{HashMap, VecDeque},
    rc::Rc,
};

use crate::{DomHtmlElement, DomNode, DomRealm};
use lumen_html::{Namespace, NodeId, NodeKind};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ReadyState {
    #[default]
    HaveNothing,
    HaveMetadata,
    HaveCurrentData,
    HaveFutureData,
    HaveEnoughData,
}

#[derive(Clone, Debug)]
pub struct MediaSnapshot {
    pub generation: u64,
    pub source: String,
    pub current_time: f64,
    pub duration: f64,
    pub volume: f64,
    pub playback_rate: f64,
    pub default_playback_rate: f64,
    pub muted: bool,
    pub paused: bool,
    pub ended: bool,
    pub ready_state: ReadyState,
    pub network_state: u16,
    pub error: Option<String>,
    pub wants_play: bool,
    pub seek_generation: u64,
    pub video_width: u32,
    pub video_height: u32,
    pub origin_clean: bool,
}

#[derive(Clone, Debug)]
pub struct VideoFrameSnapshot {
    pub presentation_time_micros: u64,
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<Vec<u8>>,
}

impl Default for MediaSnapshot {
    fn default() -> Self {
        Self {
            generation: 0,
            source: String::new(),
            current_time: 0.0,
            duration: f64::NAN,
            volume: 1.0,
            playback_rate: 1.0,
            default_playback_rate: 1.0,
            muted: false,
            paused: true,
            ended: false,
            ready_state: ReadyState::HaveNothing,
            network_state: 0,
            error: None,
            wants_play: false,
            seek_generation: 0,
            video_width: 0,
            video_height: 0,
            origin_clean: false,
        }
    }
}

#[derive(Default)]
struct State {
    snapshot: MediaSnapshot,
    muted_override: Option<bool>,
    promises: Vec<Deferred>,
    video_frame: Option<VideoFrameSnapshot>,
}

#[derive(Default)]
pub(crate) struct MediaController {
    states: HashMap<NodeId, State>,
    events: VecDeque<(NodeId, u64, String)>,
    detached: std::collections::HashSet<NodeId>,
}

impl MediaController {
    pub(crate) fn register_detached(&mut self, node: NodeId) {
        self.detached.insert(node);
    }

    pub(crate) fn detached_nodes(&self) -> Vec<NodeId> {
        self.detached.iter().copied().collect()
    }

    pub(crate) fn snapshot(&self, node: NodeId) -> MediaSnapshot {
        self.states
            .get(&node)
            .map(|state| state.snapshot.clone())
            .unwrap_or_default()
    }

    pub(crate) fn muted(&self, node: NodeId, default_muted: bool) -> bool {
        self.states
            .get(&node)
            .and_then(|state| state.muted_override)
            .unwrap_or(default_muted)
    }

    pub(crate) fn adopt_nodes_into(&mut self, target: &mut Self, mapping: &[(NodeId, NodeId)]) {
        if self.states.is_empty() && self.detached.is_empty() && self.events.is_empty() {
            return;
        }
        for &(old, new) in mapping {
            if let Some(state) = self.states.remove(&old) {
                target.states.insert(new, state);
            }
            if self.detached.remove(&old) {
                target.detached.insert(new);
            }
        }
        if !self.events.is_empty() {
            let nodes = mapping.iter().copied().collect::<HashMap<_, _>>();
            self.events.retain_mut(|(node, generation, kind)| {
                if let Some(&new) = nodes.get(node) {
                    target
                        .events
                        .push_back((new, *generation, std::mem::take(kind)));
                    false
                } else {
                    true
                }
            });
        }
    }

    pub(crate) fn video_frame(&self, node: NodeId) -> Option<VideoFrameSnapshot> {
        self.states.get(&node)?.video_frame.clone()
    }

    pub(crate) fn set_video_frame(
        &mut self,
        node: NodeId,
        generation: u64,
        frame: Option<VideoFrameSnapshot>,
    ) {
        if let Some(state) = self.current_mut(node, generation) {
            state.video_frame = frame;
        }
    }

    pub(crate) fn video_loaded(
        &mut self,
        node: NodeId,
        generation: u64,
        duration: f64,
        width: u32,
        height: u32,
        origin_clean: bool,
    ) {
        if let Some(state) = self.current_mut(node, generation) {
            state.snapshot.video_width = width;
            state.snapshot.video_height = height;
            state.snapshot.origin_clean = origin_clean;
        }
        self.loaded(node, generation, duration);
    }

    pub(crate) fn play(&mut self, ctx: &mut Ctx, node: NodeId) -> Value {
        let deferred = Deferred::new(ctx);
        let promise = deferred.promise();
        let state = self.states.entry(node).or_default();
        if state.snapshot.ended {
            state.snapshot.current_time = 0.0;
            state.snapshot.seek_generation = state.snapshot.seek_generation.wrapping_add(1).max(1);
        }
        state.snapshot.wants_play = true;
        state.snapshot.ended = false;
        state.promises.push(deferred);
        promise
    }

    pub(crate) fn pause(&mut self, ctx: &mut Ctx, node: NodeId) {
        let state = self.states.entry(node).or_default();
        state.snapshot.wants_play = false;
        for promise in state.promises.drain(..) {
            promise.reject(
                ctx,
                OpError::new("AbortError", "playback was paused before it started"),
            );
        }
        if !state.snapshot.paused {
            state.snapshot.paused = true;
            self.events
                .push_back((node, state.snapshot.generation, "pause".into()));
        }
    }

    pub(crate) fn seek(&mut self, node: NodeId, time: f64) {
        let state = self.states.entry(node).or_default();
        let limit = state.snapshot.duration;
        state.snapshot.current_time = if time.is_finite() && time > 0.0 {
            if limit.is_finite() {
                time.min(limit)
            } else {
                time
            }
        } else {
            0.0
        };
        state.snapshot.seek_generation = state.snapshot.seek_generation.wrapping_add(1).max(1);
    }

    pub(crate) fn set_volume(&mut self, node: NodeId, volume: f64) -> OpResult<()> {
        if !volume.is_finite() || !(0.0..=1.0).contains(&volume) {
            return Err(OpError::new(
                "IndexSizeError",
                "volume must be between 0 and 1",
            ));
        }
        let state = self.states.entry(node).or_default();
        if state.snapshot.volume != volume {
            state.snapshot.volume = volume;
            self.events
                .push_back((node, state.snapshot.generation, "volumechange".into()));
        }
        Ok(())
    }

    pub(crate) fn set_default_playback_rate(&mut self, node: NodeId, rate: f64) -> OpResult<()> {
        if !rate.is_finite() {
            return Err(OpError::type_error("defaultPlaybackRate must be a finite number"));
        }
        let state = self.states.entry(node).or_default();
        if state.snapshot.default_playback_rate != rate {
            state.snapshot.default_playback_rate = rate;
            self.events
                .push_back((node, state.snapshot.generation, "ratechange".into()));
        }
        Ok(())
    }

    pub(crate) fn set_playback_rate(&mut self, node: NodeId, rate: f64) -> OpResult<()> {
        if !rate.is_finite() {
            return Err(OpError::type_error("playbackRate must be a finite number"));
        }
        let state = self.states.entry(node).or_default();
        if state.snapshot.playback_rate != rate {
            state.snapshot.playback_rate = rate;
            self.events
                .push_back((node, state.snapshot.generation, "ratechange".into()));
        }
        Ok(())
    }

    pub(crate) fn set_muted(&mut self, node: NodeId, muted: bool, previous: bool) {
        let state = self.states.entry(node).or_default();
        state.muted_override = Some(muted);
        state.snapshot.muted = muted;
        if previous != muted {
            self.events
                .push_back((node, state.snapshot.generation, "volumechange".into()));
        }
    }

    pub(crate) fn set_source(&mut self, node: NodeId, source: String) -> u64 {
        let state = self.states.entry(node).or_default();
        if state.snapshot.source != source {
            state.snapshot.generation = state.snapshot.generation.wrapping_add(1).max(1);
            state.snapshot.source = source;
            state.snapshot.current_time = 0.0;
            state.snapshot.duration = f64::NAN;
            state.snapshot.ready_state = ReadyState::HaveNothing;
            state.snapshot.network_state = 2;
            state.snapshot.video_width = 0;
            state.snapshot.video_height = 0;
            state.snapshot.origin_clean = false;
            state.video_frame = None;
            state.snapshot.error = None;
            state.snapshot.ended = false;
            state.snapshot.paused = true;
            if state.snapshot.playback_rate != state.snapshot.default_playback_rate {
                state.snapshot.playback_rate = state.snapshot.default_playback_rate;
                self.events
                    .push_back((node, state.snapshot.generation, "ratechange".into()));
            }
            self.events
                .push_back((node, state.snapshot.generation, "loadstart".into()));
        }
        state.snapshot.generation
    }

    pub(crate) fn reload(&mut self, ctx: &mut Ctx, node: NodeId) -> u64 {
        let state = self.states.entry(node).or_default();
        for promise in state.promises.drain(..) {
            promise.reject(
                ctx,
                OpError::new("AbortError", "the media resource was reloaded"),
            );
        }
        state.snapshot.generation = state.snapshot.generation.wrapping_add(1).max(1);
        state.snapshot.current_time = 0.0;
        state.snapshot.duration = f64::NAN;
        state.snapshot.ready_state = ReadyState::HaveNothing;
        state.snapshot.network_state = 2;
        state.snapshot.video_width = 0;
        state.snapshot.video_height = 0;
        state.snapshot.origin_clean = false;
        state.video_frame = None;
        state.snapshot.error = None;
        state.snapshot.ended = false;
        state.snapshot.paused = true;
        state.snapshot.wants_play = false;
        if state.snapshot.playback_rate != state.snapshot.default_playback_rate {
            state.snapshot.playback_rate = state.snapshot.default_playback_rate;
            self.events
                .push_back((node, state.snapshot.generation, "ratechange".into()));
        }
        self.events
            .push_back((node, state.snapshot.generation, "loadstart".into()));
        state.snapshot.generation
    }

    pub(crate) fn loaded(&mut self, node: NodeId, generation: u64, duration: f64) {
        if let Some(state) = self.current_mut(node, generation) {
            state.snapshot.duration = duration;
            state.snapshot.ready_state = ReadyState::HaveEnoughData;
            state.snapshot.network_state = 1;
            state.snapshot.error = None;
            let generation = state.snapshot.generation;
            self.events
                .push_back((node, generation, "loadedmetadata".into()));
            self.events
                .push_back((node, generation, "loadeddata".into()));
            self.events.push_back((node, generation, "canplay".into()));
            self.events
                .push_back((node, generation, "canplaythrough".into()));
        }
    }

    pub(crate) fn failed(&mut self, node: NodeId, generation: u64, message: String) {
        if let Some(state) = self.current_mut(node, generation) {
            state.snapshot.error = Some(message);
            state.snapshot.ready_state = ReadyState::HaveNothing;
            state.snapshot.network_state = 3;
            state.snapshot.paused = true;
            state.snapshot.wants_play = false;
            let generation = state.snapshot.generation;
            self.events.push_back((node, generation, "error".into()));
        }
    }

    pub(crate) fn playing(&mut self, ctx: &mut Ctx, node: NodeId, generation: u64) {
        if let Some(state) = self.current_mut(node, generation) {
            let was_paused = state.snapshot.paused;
            state.snapshot.paused = false;
            state.snapshot.ended = false;
            for promise in state.promises.drain(..) {
                promise.resolve(ctx, Value::Undefined);
            }
            if was_paused {
                self.events.push_back((node, generation, "play".into()));
                self.events.push_back((node, generation, "playing".into()));
            }
        }
    }

    pub(crate) fn progress(&mut self, node: NodeId, generation: u64, current_time: f64) {
        if let Some(state) = self.current_mut(node, generation) {
            let changed = (state.snapshot.current_time - current_time).abs() >= 0.25;
            state.snapshot.current_time = current_time;
            if changed {
                self.events
                    .push_back((node, generation, "timeupdate".into()));
            }
        }
    }

    pub(crate) fn ended(&mut self, node: NodeId, generation: u64, duration: f64) {
        if let Some(state) = self.current_mut(node, generation) {
            state.snapshot.current_time = duration;
            state.snapshot.ended = true;
            state.snapshot.paused = true;
            state.snapshot.wants_play = false;
            self.events
                .push_back((node, generation, "timeupdate".into()));
            self.events.push_back((node, generation, "ended".into()));
        }
    }

    pub(crate) fn reject_play(&mut self, ctx: &mut Ctx, node: NodeId, message: &str) {
        if let Some(state) = self.states.get_mut(&node) {
            for promise in state.promises.drain(..) {
                promise.reject(ctx, OpError::new("NotSupportedError", message.to_owned()));
            }
        }
    }

    pub(crate) fn take_events(&mut self) -> Vec<(NodeId, u64, String)> {
        self.events.drain(..).collect()
    }

    fn current_mut(&mut self, node: NodeId, generation: u64) -> Option<&mut State> {
        self.states
            .get_mut(&node)
            .filter(|state| state.snapshot.generation == generation)
    }
}

#[lumen_bind::class(name = "HTMLMediaElement", extends = DomHtmlElement, hint(js(webidl)))]
pub struct DomHtmlMediaElement {
    pub(super) base: DomHtmlElement,
}

impl DomHtmlMediaElement {
    fn node(&self) -> &DomNode {
        &self.base.base.base
    }
}

#[lumen_bind::methods]
impl DomHtmlMediaElement {
    // HTML media IDL: a CEReactions boolean reflection. The DOM attribute
    // remains the single source for the control presentation policy.
    #[getter]
    fn controls(&self) -> OpResult<bool> { self.node().has_null_attribute("controls") }

    #[setter(coerce, hint(js(ce_reactions)))]
    fn set_controls(&self, value: bool) -> OpResult<()> {
        if value { self.node().set_attribute_core("controls", "") }
        else { self.node().remove_attribute_core("controls") }
    }

    #[getter(rename(js = "defaultPlaybackRate"))]
    fn default_playback_rate(&self) -> f64 {
        let (realm, node) = self.node().realm.resolve_adopted_node(self.node().id);
        realm.media_snapshot(node).default_playback_rate
    }

    #[setter(rename(js = "defaultPlaybackRate"), coerce)]
    fn set_default_playback_rate(&self, value: f64) -> OpResult<()> {
        let (realm, node) = self.node().realm.resolve_adopted_node(self.node().id);
        let result = realm.media.borrow_mut().set_default_playback_rate(node, value);
        result
    }

    #[getter(rename(js = "playbackRate"))]
    fn playback_rate(&self) -> f64 {
        let (realm, node) = self.node().realm.resolve_adopted_node(self.node().id);
        realm.media_snapshot(node).playback_rate
    }

    #[setter(rename(js = "playbackRate"), coerce)]
    fn set_playback_rate(&self, value: f64) -> OpResult<()> {
        let (realm, node) = self.node().realm.resolve_adopted_node(self.node().id);
        let result = realm.media.borrow_mut().set_playback_rate(node, value);
        result
    }
}

#[lumen_bind::class(name = "HTMLAudioElement", extends = DomHtmlMediaElement, hint(js(webidl)))]
pub struct DomHtmlAudioElement {
    pub(super) base: DomHtmlMediaElement,
}

impl DomHtmlAudioElement {
    pub(crate) fn from_html(base: DomHtmlElement) -> Self {
        base.base.base.realm.media.borrow_mut().register_detached(base.base.base.id);
        Self { base: DomHtmlMediaElement { base } }
    }

    fn node(&self) -> &DomNode {
        &self.base.base.base.base
    }

    fn snapshot(&self) -> MediaSnapshot {
        let (realm, node) = self.node().realm.resolve_adopted_node(self.node().id);
        realm.media_snapshot(node)
    }

    fn create(realm: Rc<DomRealm>, source: Option<&str>) -> OpResult<Self> {
        let mut attributes = vec![("preload".into(), "auto".into())];
        if let Some(source) = source { attributes.push(("src".into(), source.to_owned())); }
        let id = realm
            .session
            .borrow_mut()
            .document_mut()
            .create(NodeKind::Element {
                namespace: Namespace::Html,
                name: "audio".into(),
                attributes,
            })
            .map_err(crate::dom_error)?;
        let node = DomNode {
            base: crate::DomEventTarget::node(&realm, id),
            realm: realm.clone(),
            id,
            collections: std::cell::RefCell::new(std::collections::HashMap::new()),
        };
        realm.media.borrow_mut().register_detached(id);
        Ok(Self {
            base: DomHtmlMediaElement {
                base: DomHtmlElement {
                    base: crate::DomElement { base: node },
                },
            },
        })
    }
}

#[lumen_bind::class(name = "HTMLVideoElement", extends = DomHtmlMediaElement, hint(js(webidl)))]
pub struct DomHtmlVideoElement {
    pub(super) base: DomHtmlMediaElement,
}

impl DomHtmlVideoElement {
    pub(crate) fn from_html(base: DomHtmlElement) -> Self {
        base.base.base.realm.media.borrow_mut().register_detached(base.base.base.id);
        Self { base: DomHtmlMediaElement { base } }
    }

    pub(super) fn node(&self) -> &DomNode {
        &self.base.base.base.base
    }

    fn snapshot(&self) -> MediaSnapshot {
        let (realm, node) = self.node().realm.resolve_adopted_node(self.node().id);
        realm.media_snapshot(node)
    }
}

#[lumen_bind::methods]
impl DomHtmlVideoElement {
    #[constructor]
    fn new(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<crate::custom_elements::HtmlElementCtor> {
        crate::custom_elements::construct_customized_class::<Self>(ctx, this.0)
    }

    #[getter]
    fn src(&self) -> OpResult<String> {
        let Some(source) = self.node().get_attribute("src")?.0 else {
            return Ok(String::new());
        };
        let base = self.node().realm.base_url();
        Ok(lumen_common::url::parse(&source, Some(&base))
            .map(|url| url.href())
            .unwrap_or(source))
    }

    #[setter(coerce, hint(js(ce_reactions)))]
    fn set_src(&self, ctx: &mut Ctx, source: &str) -> OpResult<()> {
        self.node().set_attribute_core("src", source)?;
        self.node().realm.media_reload(ctx, self.node().id);
        Ok(())
    }

    #[getter(rename(js = "currentSrc"))]
    fn current_src(&self) -> String {
        self.snapshot().source
    }

    fn play(&self, ctx: &mut Ctx) -> OpResult<Value> {
        if self
            .node()
            .get_attribute("src")?
            .0
            .as_deref()
            .unwrap_or("")
            .is_empty()
        {
            let deferred = Deferred::new(ctx);
            let promise = deferred.promise();
            deferred.reject(
                ctx,
                OpError::new("NotSupportedError", "video element has no source"),
            );
            return Ok(promise);
        }
        Ok(self
            .node()
            .realm
            .media
            .borrow_mut()
            .play(ctx, self.node().id))
    }

    fn pause(&self, ctx: &mut Ctx) {
        self.node()
            .realm
            .media
            .borrow_mut()
            .pause(ctx, self.node().id);
    }
    fn load(&self, ctx: &mut Ctx) {
        self.node().realm.media_reload(ctx, self.node().id);
    }

    #[getter]
    fn paused(&self) -> bool {
        self.snapshot().paused
    }
    #[getter]
    fn ended(&self) -> bool {
        self.snapshot().ended
    }
    #[getter(rename(js = "currentTime"))]
    fn current_time(&self) -> f64 {
        self.snapshot().current_time
    }
    #[setter(rename(js = "currentTime"), coerce)]
    fn set_current_time(&self, value: f64) {
        self.node()
            .realm
            .media
            .borrow_mut()
            .seek(self.node().id, value);
    }
    #[getter]
    fn duration(&self) -> f64 {
        self.snapshot().duration
    }
    #[getter(rename(js = "readyState"))]
    fn ready_state(&self) -> u8 {
        self.snapshot().ready_state as u8
    }
    #[getter(rename(js = "networkState"))]
    fn network_state(&self) -> u16 {
        self.snapshot().network_state
    }
    #[getter(rename(js = "videoWidth"))]
    fn video_width(&self) -> u32 {
        self.snapshot().video_width
    }
    #[getter(rename(js = "videoHeight"))]
    fn video_height(&self) -> u32 {
        self.snapshot().video_height
    }
    #[getter]
    fn volume(&self) -> f64 {
        self.snapshot().volume
    }
    #[setter(coerce)]
    fn set_volume(&self, value: f64) -> OpResult<()> {
        self.node()
            .realm
            .media
            .borrow_mut()
            .set_volume(self.node().id, value)
    }
    #[getter]
    fn muted(&self) -> bool {
        self.snapshot().muted
    }
    #[setter(coerce)]
    fn set_muted(&self, value: bool) {
        let (realm, node) = self.node().realm.resolve_adopted_node(self.node().id);
        realm.media_set_muted(node, value);
    }
    #[getter]
    fn error(&self) -> Value {
        self.snapshot()
            .error
            .map_or(Value::Null, Value::from_string)
    }

    #[method(coerce)]
    fn can_play_type(&self, mime: &str) -> &'static str {
        let mime = mime.trim().to_ascii_lowercase();
        let Some((essence, parameters)) = mime.split_once(';') else {
            return if mime == "video/mp4" { "maybe" } else { "" };
        };
        if essence.trim() != "video/mp4" {
            return "";
        }
        let Some((name, value)) = parameters.trim().split_once('=') else {
            return "";
        };
        if name.trim() != "codecs" {
            return "";
        }
        let codecs = value.trim().trim_matches('"');
        let mut has_avc = false;
        for codec in codecs.split(',').map(str::trim) {
            if codec.starts_with("avc1.") {
                has_avc = true;
            } else if codec == "mp4a.40.2" {
                // AAC-LC is the only MP4 audio codec decoded by the current
                // video pipeline.
            } else {
                // Other AAC profiles and all other audio/video codecs are unsupported. In
                // particular, do not promise playback then silently drop an
                // MP4 audio track.
                return "";
            }
        }
        if has_avc {
            "maybe"
        } else {
            ""
        }
    }
}

pub(crate) fn legacy_audio_factory(
    ctx: &mut Ctx,
    source: Option<&str>,
) -> OpResult<crate::NodeConstructorResult<DomHtmlAudioElement>> {
    let realm = crate::window_globals::current_dom_realm(ctx)
        .ok_or_else(|| OpError::new("TypeError", "Audio has no active document"))?;
    let audio = DomHtmlAudioElement::create(realm, source)?;
    let node = audio.node();
    let realm = node.realm.clone();
    let id = node.id;
    Ok(crate::NodeConstructorResult::from_native(audio, realm, id))
}

#[lumen_bind::methods]
impl DomHtmlAudioElement {
    #[constructor]
    fn new(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<crate::custom_elements::HtmlElementCtor> {
        crate::custom_elements::construct_customized_class::<Self>(ctx, this.0)
    }

    #[getter]
    fn src(&self) -> OpResult<String> {
        let node = self.node();
        let Some(source) = node.get_attribute("src")?.0 else {
            return Ok(String::new());
        };
        let base = node.realm.base_url();
        Ok(lumen_common::url::parse(&source, Some(&base))
            .map(|url| url.href())
            .unwrap_or(source))
    }

    #[setter(coerce, hint(js(ce_reactions)))]
    fn set_src(&self, ctx: &mut Ctx, source: &str) -> OpResult<()> {
        self.node().set_attribute_core("src", source)?;
        self.node()
            .realm
            .media
            .borrow_mut()
            .reload(ctx, self.node().id);
        Ok(())
    }

    #[getter(rename(js = "currentSrc"))]
    fn current_src(&self) -> String {
        self.snapshot().source
    }

    fn play(&self, ctx: &mut Ctx) -> OpResult<Value> {
        if self
            .node()
            .get_attribute("src")?
            .0
            .as_deref()
            .unwrap_or("")
            .is_empty()
        {
            let deferred = Deferred::new(ctx);
            let promise = deferred.promise();
            deferred.reject(
                ctx,
                OpError::new("NotSupportedError", "audio element has no source"),
            );
            return Ok(promise);
        }
        Ok(self
            .node()
            .realm
            .media
            .borrow_mut()
            .play(ctx, self.node().id))
    }

    fn pause(&self, ctx: &mut Ctx) {
        self.node()
            .realm
            .media
            .borrow_mut()
            .pause(ctx, self.node().id);
    }

    fn load(&self, ctx: &mut Ctx) {
        self.node()
            .realm
            .media
            .borrow_mut()
            .reload(ctx, self.node().id);
    }

    #[getter]
    fn paused(&self) -> bool {
        self.snapshot().paused
    }

    #[getter]
    fn ended(&self) -> bool {
        self.snapshot().ended
    }

    #[getter(rename(js = "currentTime"))]
    fn current_time(&self) -> f64 {
        self.snapshot().current_time
    }

    #[setter(rename(js = "currentTime"), coerce)]
    fn set_current_time(&self, value: f64) {
        self.node()
            .realm
            .media
            .borrow_mut()
            .seek(self.node().id, value);
    }

    #[getter]
    fn duration(&self) -> f64 {
        self.snapshot().duration
    }

    #[getter(rename(js = "readyState"))]
    fn ready_state(&self) -> u8 {
        self.snapshot().ready_state as u8
    }

    #[getter(rename(js = "networkState"))]
    fn network_state(&self) -> u16 {
        self.snapshot().network_state
    }

    #[getter]
    fn volume(&self) -> f64 {
        self.snapshot().volume
    }

    #[setter(coerce)]
    fn set_volume(&self, value: f64) -> OpResult<()> {
        self.node()
            .realm
            .media
            .borrow_mut()
            .set_volume(self.node().id, value)
    }

    #[getter]
    fn muted(&self) -> bool {
        self.snapshot().muted
    }

    #[setter(coerce)]
    fn set_muted(&self, value: bool) {
        let (realm, node) = self.node().realm.resolve_adopted_node(self.node().id);
        realm.media_set_muted(node, value);
    }

    #[getter]
    fn error(&self) -> Value {
        self.snapshot()
            .error
            .map_or(Value::Null, Value::from_string)
    }

    #[method(coerce)]
    fn can_play_type(&self, mime: &str) -> &'static str {
        let mime = mime.split(';').next().unwrap_or("").trim();
        if ["audio/wav", "audio/wave", "audio/x-wav", "audio/vnd.wave"]
            .iter()
            .any(|supported| mime.eq_ignore_ascii_case(supported))
        {
            "maybe"
        } else {
            ""
        }
    }
}

#[cfg(test)]
mod tests {
    use lumen::{embed::Value, Engine};

    #[test]
    fn muted_defaults_follow_content_attributes_until_explicit_override_and_survive_adoption() {
        let mut engine = Engine::new();
        let realm = crate::install(
            engine.ctx(),
            "<audio id='audio' muted></audio><video id='video' muted></video>",
            128,
        )
        .unwrap();
        let evaluate = |engine: &mut Engine, source: &str| {
            engine
                .eval_value(source)
                .expect("valid muted-state script")
                .ok()
                .expect("muted-state script threw")
        };
        let result = evaluate(
            &mut engine,
            r#"
            globalThis.mediaElements = [document.getElementById('audio'), document.getElementById('video')];
            globalThis.volumeChanges = 0;
            for (const element of mediaElements) {
                element.onvolumechange = () => volumeChanges++;
                if (!element.muted) throw new Error('parser default missing');
                element.volume = 0.5;
                element.removeAttribute('muted');
                if (element.muted) throw new Error('default ignored after volume state allocation');
                element.setAttribute('muted', '');
                if (!element.muted) throw new Error('default missing');
                element.muted = true;
                element.removeAttribute('muted');
                if (!element.muted) throw new Error('same-value IDL setter did not preserve override');
                element.muted = false;
                element.setAttribute('muted', '');
                if (element.muted) throw new Error('content attribute overwrote explicit false');
                element.load();
                if (element.muted) throw new Error('load reset explicit override');
                const destination = document.implementation.createHTMLDocument('adopted');
                destination.adoptNode(element);
                if (element.muted) throw new Error('adoption lost explicit false');
                element.muted = true;
                element.removeAttribute('muted');
                if (!element.muted) throw new Error('adopted setter lost explicit true');
            }
            true;
        "#,
        );
        assert!(matches!(result, Value::Bool(true)));
        // Adoption moves controller state rather than leaving an old-owner copy.
        assert!(realm.media.borrow().states.is_empty());
    }

    #[test]
    fn muted_content_changes_are_silent_and_native_snapshots_match_effective_gain() {
        let mut engine = Engine::new();
        let realm = crate::install(engine.ctx(), "<audio id='audio' muted></audio>", 64).unwrap();
        let evaluate = |engine: &mut Engine, source: &str| {
            engine
                .eval_value(source)
                .expect("valid muted snapshot script")
                .ok()
                .expect("muted snapshot script threw")
        };
        let element = evaluate(
            &mut engine,
            "globalThis.audio=document.getElementById('audio');audio",
        );
        let node = engine
            .ctx()
            .with_instance::<crate::DomNode, _>(&element, |node| node.id)
            .unwrap();
        assert!(realm.media_snapshot(node).muted);
        evaluate(&mut engine,
            "globalThis.events=0;audio.onvolumechange=()=>events++;audio.removeAttribute('muted');audio.setAttribute('muted','');audio.muted=true");
        assert_eq!(realm.queue_media_tasks(engine.ctx()).unwrap(), 0);
        assert!(realm.media_snapshot(node).muted);
        evaluate(
            &mut engine,
            "audio.muted=false;audio.setAttribute('muted','')",
        );
        assert!(!realm.media_snapshot(node).muted);
        assert_eq!(realm.queue_media_tasks(engine.ctx()).unwrap(), 1);
        assert!(crate::scheduling::run_tasks(&mut engine, 64).is_empty());
        assert!(matches!(
            evaluate(&mut engine, "events===1"),
            Value::Bool(true)
        ));
    }

    #[test]
    fn audio_elements_inherit_media_interface_and_report_only_wav_support() {
        let mut engine = Engine::new();
        crate::install(engine.ctx(), "<body></body>", 64).unwrap();
        let value = engine
            .eval_value(
                r#"(() => {
                    const audio = new Audio('tone.wav');
                    return audio instanceof HTMLAudioElement &&
                        audio instanceof HTMLMediaElement &&
                        audio instanceof HTMLElement &&
                        audio.canPlayType('audio/wav') === 'maybe' &&
                        audio.canPlayType('audio/mpeg') === '' &&
                        audio.paused && audio.readyState === 0 && Number.isNaN(audio.duration);
                })()"#,
            )
            .unwrap();
        let value = match value {
            Ok(value) => value,
            Err(_) => panic!("media expression threw"),
        };
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn video_elements_expose_intrinsic_dimensions_and_only_avc_mp4_support() {
        let mut engine = Engine::new();
        crate::install(engine.ctx(), "<body><video></video></body>", 64).unwrap();
        let value = engine.eval_value(
            "(()=>{const video=document.querySelector('video');return video instanceof HTMLVideoElement && video instanceof HTMLMediaElement && video.videoWidth===0 && video.videoHeight===0 && video.canPlayType('video/mp4; codecs=\"avc1.42E01E\"')==='maybe' && video.canPlayType('video/mp4; codecs=\"avc1.42E01E, mp4a.40.2\"')==='maybe' && video.canPlayType('video/mp4; codecs=\"avc1.42E01E, mp4a.40.5\"')==='' && video.canPlayType('video/webm; codecs=\"vp9\"')==='' && video.paused && Number.isNaN(video.duration)})()",
        ).unwrap();
        assert!(matches!(value, Ok(Value::Bool(true))));
    }
}
