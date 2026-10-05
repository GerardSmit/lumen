//! MediaStream primitives and real canvas-backed video capture.
//!
//! Device-backed camera and microphone capture is provided by an embedder
//! `MediaDevicesHost`; the default host has no inputs and rejects requests.

use crate::{media::VideoFrameSnapshot, DomRealm};
use lumen::embed::{Ctx, JsObject, OpError, OpResult, Promise, Value, WeakValue};
use lumen_bind::This;
use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

type VideoFrameSource = Rc<dyn Fn() -> OpResult<VideoFrameSnapshot>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaCapturePermission {
    Granted,
    Prompt,
    Denied,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaDeviceKind {
    AudioInput,
    VideoInput,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MediaDeviceDescription {
    pub device_id: String,
    pub kind: MediaDeviceKind,
    pub label: String,
    pub group_id: String,
}

/// A host-provided camera. `next_frame` must return captured pixels from the
/// real device and must report unavailable frames as errors.
#[derive(Clone)]
pub struct MediaCaptureVideoSource {
    pub device: MediaDeviceDescription,
    pub next_frame: Rc<dyn Fn() -> Result<VideoFrameSnapshot, String>>,
    pub stop: Rc<dyn Fn()>,
}

/// Explicit embedder hooks for actual input devices. An unset hook means the
/// environment has no input source; the DOM rejects rather than fabricating a stream.
#[derive(Clone)]
pub struct MediaDevicesHost {
    pub permission: Rc<dyn Fn(MediaDeviceKind) -> MediaCapturePermission>,
    /// Complete a pending permission prompt. A denied or failed response must
    /// not be followed by opening the device.
    pub request_permission: Rc<dyn Fn(MediaDeviceKind) -> Result<bool, String>>,
    pub enumerate: Rc<dyn Fn() -> Vec<MediaDeviceDescription>>,
    pub open_video: Rc<dyn Fn() -> Result<MediaCaptureVideoSource, String>>,
}

struct TrackData {
    id: String,
    kind: &'static str,
    label: String,
    enabled: Cell<bool>,
    muted: Cell<bool>,
    ended: Cell<bool>,
    source: Option<VideoFrameSource>,
    last_frame: RefCell<Option<VideoFrameSnapshot>>,
    frame_rate: f64,
    next_frame: Cell<Instant>,
    started: Instant,
    canvas_capture: bool,
    realm: Weak<DomRealm>,
    wrapper: RefCell<Option<WeakValue>>,
    lease: Option<Rc<CaptureLease>>,
    source_released: Cell<bool>,
}

struct CaptureLease {
    tracks: Cell<usize>,
    stop: Rc<dyn Fn()>,
}

impl CaptureLease {
    fn new(stop: Rc<dyn Fn()>) -> Rc<Self> {
        Rc::new(Self {
            tracks: Cell::new(1),
            stop,
        })
    }

    fn retain(&self) {
        self.tracks.set(self.tracks.get().saturating_add(1));
    }

    fn release(&self) {
        let count = self.tracks.get();
        if count == 0 {
            return;
        }
        self.tracks.set(count - 1);
        if count == 1 {
            (self.stop)();
        }
    }
}

impl TrackData {
    fn new(
        kind: &'static str,
        label: String,
        source: Option<VideoFrameSource>,
        frame_rate: f64,
        canvas_capture: bool,
        realm: Weak<DomRealm>,
        lease: Option<Rc<CaptureLease>>,
    ) -> Rc<Self> {
        let now = Instant::now();
        let period = frame_period(frame_rate);
        Rc::new(Self {
            id: format!("media-track-{}", NEXT_ID.fetch_add(1, Ordering::Relaxed)),
            kind,
            label,
            enabled: Cell::new(true),
            muted: Cell::new(false),
            ended: Cell::new(false),
            source,
            last_frame: RefCell::new(None),
            frame_rate,
            next_frame: Cell::new(now + period),
            started: now,
            canvas_capture,
            realm,
            wrapper: RefCell::new(None),
            lease,
            source_released: Cell::new(false),
        })
    }

    fn clone_track(self: &Rc<Self>) -> Rc<Self> {
        let lease = if self.ended.get() {
            None
        } else {
            self.lease.clone()
        };
        if let Some(lease) = &lease {
            lease.retain();
        }
        let clone = Self::new(
            self.kind,
            self.label.clone(),
            if self.ended.get() {
                None
            } else {
                self.source.clone()
            },
            self.frame_rate,
            self.canvas_capture,
            self.realm.clone(),
            lease,
        );
        clone.enabled.set(self.enabled.get());
        clone.muted.set(self.muted.get());
        clone.ended.set(self.ended.get());
        *clone.last_frame.borrow_mut() = self.last_frame.borrow().clone();
        if let Some(realm) = clone.realm.upgrade() {
            realm.media_capture.register(&clone);
        }
        clone
    }

    fn capture(&self, ctx: &mut Ctx) -> OpResult<()> {
        if self.ended.get() {
            return Ok(());
        }
        if let Some(source) = &self.source {
            match source() {
                Ok(mut frame) => {
                    frame.presentation_time_micros = self.started.elapsed().as_micros() as u64;
                    if !self.enabled.get() && self.kind == "video" {
                        for pixel in std::sync::Arc::make_mut(&mut frame.rgba).chunks_exact_mut(4) {
                            pixel.copy_from_slice(&[0, 0, 0, 255]);
                        }
                    }
                    *self.last_frame.borrow_mut() = Some(frame);
                    if self.muted.replace(false) {
                        dispatch_track_event(ctx, self, "unmute")?;
                    }
                }
                Err(error) => {
                    if !self.muted.replace(true) {
                        dispatch_track_event(ctx, self, "mute")?;
                    }
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    fn capture_if_due(&self, ctx: &mut Ctx, now: Instant) {
        if self.frame_rate <= 0.0 || self.ended.get() || now < self.next_frame.get() {
            return;
        }
        let _ = self.capture(ctx);
        // Avoid a burst of stale frames after an owner turn was delayed.
        self.next_frame.set(now + frame_period(self.frame_rate));
    }

    fn stop(&self) -> bool {
        let changed = !self.ended.replace(true);
        self.release_source();
        changed
    }

    fn release_source(&self) {
        if !self.source_released.replace(true) {
            if let Some(lease) = &self.lease {
                lease.release();
            }
        }
    }
}

impl Drop for TrackData {
    fn drop(&mut self) {
        if !self.source_released.replace(true) {
            if let Some(lease) = &self.lease {
                lease.release();
            }
        }
    }
}

#[derive(Default)]
pub(crate) struct RealmMediaCapture {
    tracks: RefCell<Vec<Weak<TrackData>>>,
    host: RefCell<Option<MediaDevicesHost>>,
}

impl RealmMediaCapture {
    fn set_host(&self, host: Option<MediaDevicesHost>) {
        *self.host.borrow_mut() = host;
    }

    fn host(&self) -> Option<MediaDevicesHost> {
        self.host.borrow().clone()
    }

    fn register(&self, track: &Rc<TrackData>) {
        if track.frame_rate > 0.0 {
            self.tracks.borrow_mut().push(Rc::downgrade(track));
        }
    }

    pub(crate) fn pump(&self, ctx: &mut Ctx) -> OpResult<()> {
        let now = Instant::now();
        let live = {
            let mut tracks = self.tracks.borrow_mut();
            let live: Vec<_> = tracks.iter().filter_map(Weak::upgrade).collect();
            tracks.retain(|track| {
                track
                    .upgrade()
                    .is_some_and(|track| !track.ended.get() && track.frame_rate > 0.0)
            });
            live
        };
        for track in live {
            track.capture_if_due(ctx, now);
        }
        Ok(())
    }

    pub(crate) fn animation_pending(&self) -> bool {
        self.tracks
            .borrow()
            .iter()
            .filter_map(Weak::upgrade)
            .any(|track| !track.ended.get())
    }

    pub(crate) fn next_delay_ms(&self) -> Option<u64> {
        let now = Instant::now();
        let mut tracks = self.tracks.borrow_mut();
        tracks.retain(|track| track.strong_count() > 0);
        tracks
            .iter()
            .filter_map(Weak::upgrade)
            .filter(|track| !track.ended.get())
            .map(|track| {
                let remaining = track.next_frame.get().saturating_duration_since(now);
                remaining.as_millis() as u64 + u64::from(remaining.subsec_nanos() % 1_000_000 != 0)
            })
            .min()
    }
}

impl DomRealm {
    /// Install or remove the real input-device callbacks supplied by an embedder.
    pub fn set_media_devices_host(&self, host: Option<MediaDevicesHost>) {
        self.media_capture.set_host(host);
    }

    pub fn media_capture_permission(&self, kind: MediaDeviceKind) -> MediaCapturePermission {
        self.media_capture
            .host()
            .map_or(MediaCapturePermission::Denied, |host| {
                (host.permission)(kind)
            })
    }

    /// Sample due canvas capture tracks from the realm's native owner pump.
    pub fn pump_media_capture(&self, ctx: &mut Ctx) -> OpResult<()> {
        self.media_capture.pump(ctx)
    }

    /// Keep the native owner awake while an automatic canvas capture is active.
    pub fn media_capture_animation_pending(&self) -> bool {
        self.media_capture.animation_pending()
    }

    /// Delay to the next scheduled canvas capture frame.
    pub fn media_capture_next_delay_ms(&self) -> Option<u64> {
        self.media_capture.next_delay_ms()
    }
}

fn frame_period(frame_rate: f64) -> Duration {
    Duration::from_secs_f64((1.0 / frame_rate).clamp(0.001, 86_400.0))
}

#[lumen_bind::class(
    name = "MediaStreamTrack",
    extends = crate::events::DomEventTarget,
    hint(js(webidl))
)]
pub struct DomMediaStreamTrack {
    base: crate::events::DomEventTarget,
    data: Rc<TrackData>,
}

#[lumen_bind::methods]
impl DomMediaStreamTrack {
    #[getter]
    fn id(&self) -> String {
        self.data.id.clone()
    }

    #[getter]
    fn kind(&self) -> &'static str {
        self.data.kind
    }

    #[getter]
    fn label(&self) -> String {
        self.data.label.clone()
    }

    #[getter]
    fn enabled(&self) -> bool {
        self.data.enabled.get()
    }

    #[setter]
    fn set_enabled(&self, value: bool) {
        self.data.enabled.set(value);
    }

    #[getter]
    fn muted(&self) -> bool {
        self.data.muted.get()
    }

    #[getter]
    fn ready_state(&self) -> &'static str {
        if self.data.ended.get() {
            "ended"
        } else {
            "live"
        }
    }

    fn stop(&self) {
        // `MediaStreamTrack.stop()` ends this track without firing `ended`.
        self.data.stop();
    }

    fn clone(&self, ctx: &mut Ctx) -> Value {
        make_track(ctx, self.data.clone_track())
    }
}

#[lumen_bind::class(
    name = "CanvasCaptureMediaStreamTrack",
    extends = DomMediaStreamTrack,
    hint(js(webidl))
)]
pub struct DomCanvasCaptureMediaStreamTrack {
    base: DomMediaStreamTrack,
}

#[lumen_bind::class(
    name = "MediaStreamTrackEvent",
    extends = crate::events::DomEvent,
    hint(js(webidl))
)]
pub struct DomMediaStreamTrackEvent {
    base: crate::events::DomEvent,
    track: Value,
}

#[lumen_bind::methods]
impl DomMediaStreamTrackEvent {
    #[constructor(coerce)]
    fn new(ctx: &mut Ctx, event_type: &str, init: Option<Value>) -> OpResult<Self> {
        let track = match init.as_ref() {
            None | Some(Value::Null | Value::Undefined) => Value::Null,
            Some(init) => match ctx.member_get(init, "track") {
                Ok(track) => track,
                Err(error) => return Err(OpError::thrown(error)),
            },
        };
        let base = crate::events::DomEvent::new(ctx, event_type, init)?;
        Ok(Self { base, track })
    }

    #[getter]
    fn track(&self) -> Value {
        self.track.clone()
    }
}

#[lumen_bind::methods]
impl DomCanvasCaptureMediaStreamTrack {
    /// Capture the current real canvas bitmap into this track's frame queue.
    /// This is the manual-frame mode selected by `captureStream(0)`.
    fn request_frame(&self, ctx: &mut Ctx) -> OpResult<()> {
        if self.base.data.ended.get() {
            return Ok(());
        }
        self.base.data.capture(ctx)
    }
}

#[lumen_bind::class(
    name = "MediaStream",
    extends = crate::events::DomEventTarget,
    hint(js(webidl))
)]
pub struct DomMediaStream {
    base: crate::events::DomEventTarget,
    id: String,
    tracks: RefCell<Vec<Value>>,
}

#[lumen_bind::methods]
impl DomMediaStream {
    #[constructor]
    fn new(ctx: &mut Ctx, #[default(Value::Undefined)] input: Value) -> OpResult<Self> {
        let tracks = match input {
            Value::Undefined => Vec::new(),
            input => {
                if let Ok(tracks) = ctx.with_instance::<DomMediaStream, _>(&input, |stream| {
                    stream.tracks.borrow().clone()
                }) {
                    tracks
                } else {
                    let converted = ctx.convert_iterable(&input, 65_536, |ctx, track| {
                        ctx.with_instance::<DomMediaStreamTrack, _>(&track, |_| ())
                            .map_err(|_| {
                                OpError::new(
                                    "TypeError",
                                    "MediaStream expects MediaStreamTrack values",
                                )
                            })?;
                        Ok(track)
                    })?;
                    let mut tracks = Vec::new();
                    for track in converted {
                        let id = ctx.with_instance::<DomMediaStreamTrack, _>(&track, |track| {
                            track.data.id.clone()
                        })?;
                        if !tracks.iter().any(|existing| {
                            ctx.with_instance::<DomMediaStreamTrack, _>(existing, |track| {
                                track.data.id == id
                            })
                            .unwrap_or(false)
                        }) {
                            tracks.push(track);
                        }
                    }
                    tracks
                }
            }
        };
        Ok(Self {
            base: crate::events::DomEventTarget::new(),
            id: stream_id(),
            tracks: RefCell::new(tracks),
        })
    }

    #[getter]
    fn id(&self) -> String {
        self.id.clone()
    }

    #[getter]
    fn active(&self, ctx: &mut Ctx) -> bool {
        self.tracks.borrow().iter().any(|track| {
            ctx.with_instance::<DomMediaStreamTrack, _>(track, |track| !track.data.ended.get())
                .unwrap_or(false)
        })
    }

    fn get_tracks(&self) -> Vec<Value> {
        self.tracks.borrow().clone()
    }

    fn get_audio_tracks(&self, ctx: &mut Ctx) -> Vec<Value> {
        self.tracks
            .borrow()
            .iter()
            .filter(|track| {
                ctx.with_instance::<DomMediaStreamTrack, _>(track, |track| {
                    track.data.kind == "audio"
                })
                .unwrap_or(false)
            })
            .cloned()
            .collect()
    }

    fn get_video_tracks(&self, ctx: &mut Ctx) -> Vec<Value> {
        self.tracks
            .borrow()
            .iter()
            .filter(|track| {
                ctx.with_instance::<DomMediaStreamTrack, _>(track, |track| {
                    track.data.kind == "video"
                })
                .unwrap_or(false)
            })
            .cloned()
            .collect()
    }

    fn get_track_by_id(&self, ctx: &mut Ctx, id: String) -> Value {
        self.tracks
            .borrow()
            .iter()
            .find(|track| {
                ctx.with_instance::<DomMediaStreamTrack, _>(track, |track| track.data.id == id)
                    .unwrap_or(false)
            })
            .cloned()
            .unwrap_or(Value::Null)
    }

    fn add_track(&self, ctx: &mut Ctx, track: Value) -> OpResult<()> {
        let track_id = ctx
            .with_instance::<DomMediaStreamTrack, _>(&track, |track| track.data.id.clone())
            .map_err(|_| OpError::new("TypeError", "addTrack expects a MediaStreamTrack"))?;
        let mut tracks = self.tracks.borrow_mut();
        let mut already_present = false;
        for existing in tracks.iter() {
            let existing_id = ctx
                .with_instance::<DomMediaStreamTrack, _>(existing, |existing| {
                    existing.data.id.clone()
                })
                .map_err(|_| OpError::new("InvalidStateError", "stream track was released"))?;
            already_present |= existing_id == track_id;
        }
        if !already_present {
            tracks.push(track.clone());
        }
        drop(tracks);
        Ok(())
    }

    fn remove_track(&self, ctx: &mut Ctx, track: Value) -> OpResult<()> {
        let track_id = ctx
            .with_instance::<DomMediaStreamTrack, _>(&track, |track| track.data.id.clone())
            .map_err(|_| OpError::new("TypeError", "removeTrack expects a MediaStreamTrack"))?;
        let mut retained = Vec::new();
        let current = self.tracks.borrow().clone();
        for existing in current.iter() {
            let existing_id = ctx
                .with_instance::<DomMediaStreamTrack, _>(existing, |existing| {
                    existing.data.id.clone()
                })
                .map_err(|_| OpError::new("InvalidStateError", "stream track was released"))?;
            if existing_id != track_id {
                retained.push(existing.clone());
            }
        }
        *self.tracks.borrow_mut() = retained;
        Ok(())
    }

    fn clone(&self, ctx: &mut Ctx) -> OpResult<Self> {
        let mut tracks = Vec::new();
        for value in self.tracks.borrow().iter() {
            let data = ctx
                .with_instance::<DomMediaStreamTrack, _>(value, |track| track.data.clone_track())
                .map_err(|_| OpError::new("InvalidStateError", "stream track was released"))?;
            tracks.push(make_track(ctx, data));
        }
        Ok(Self {
            base: crate::events::DomEventTarget::new(),
            id: stream_id(),
            tracks: RefCell::new(tracks),
        })
    }
}

#[lumen_bind::class(name = "MediaDevices", hint(js(webidl)))]
pub struct DomMediaDevices {
    realm: Weak<DomRealm>,
}

#[lumen_bind::class(name = "MediaDeviceInfo", hint(js(webidl)))]
pub struct DomMediaDeviceInfo {
    description: MediaDeviceDescription,
}

#[lumen_bind::methods]
impl DomMediaDeviceInfo {
    #[getter]
    fn device_id(&self) -> String {
        self.description.device_id.clone()
    }

    #[getter]
    fn kind(&self) -> &'static str {
        match self.description.kind {
            MediaDeviceKind::AudioInput => "audioinput",
            MediaDeviceKind::VideoInput => "videoinput",
        }
    }

    #[getter]
    fn label(&self) -> String {
        self.description.label.clone()
    }

    #[getter]
    fn group_id(&self) -> String {
        self.description.group_id.clone()
    }
}

#[lumen_bind::methods]
impl DomMediaDevices {
    fn enumerate_devices(&self, ctx: &mut Ctx) -> Promise<Vec<Value>> {
        let Some(realm) = self.realm.upgrade() else {
            return Promise::ready(Err(OpError::new(
                "InvalidStateError",
                "media device realm has been destroyed",
            )));
        };
        let descriptions = realm.media_capture.host().map_or_else(Vec::new, |host| {
            (host.enumerate)()
                .into_iter()
                .filter(|device| device.kind == MediaDeviceKind::VideoInput)
                .collect()
        });
        Promise::ready(Ok::<Vec<Value>, OpError>(
            descriptions
                .into_iter()
                .map(|description| ctx.new_instance(DomMediaDeviceInfo { description }))
                .collect(),
        ))
    }

    fn get_user_media(&self, ctx: &mut Ctx, constraints: Value) -> Promise<Value> {
        let Some(realm) = self.realm.upgrade() else {
            return Promise::ready(Err(OpError::new(
                "InvalidStateError",
                "media device realm has been destroyed",
            )));
        };
        let audio_value = match ctx.member_get(&constraints, "audio") {
            Ok(value) => value,
            Err(error) => return Promise::ready(Err(OpError::thrown(error))),
        };
        let video_value = match ctx.member_get(&constraints, "video") {
            Ok(value) => value,
            Err(error) => return Promise::ready(Err(OpError::thrown(error))),
        };
        let audio = ctx.to_boolean(&audio_value);
        let video = ctx.to_boolean(&video_value);
        if !audio && !video {
            return Promise::ready(Err(OpError::new(
                "TypeError",
                "at least one media kind must be requested",
            )));
        }
        if audio {
            return Promise::ready(Err(OpError::new(
                "NotFoundError",
                "the host has no AudioInput source",
            )));
        }
        if matches!(video_value, Value::Obj(_)) {
            return Promise::ready(Err(OpError::new(
                "NotSupportedError",
                "the host camera service does not accept video constraint dictionaries",
            )));
        }
        let Some(host) = realm.media_capture.host() else {
            return Promise::ready(Err(OpError::new(
                "NotFoundError",
                "the host has no camera capture source",
            )));
        };
        match (host.permission)(MediaDeviceKind::VideoInput) {
            MediaCapturePermission::Denied => {
                return Promise::ready(Err(OpError::new(
                    "NotAllowedError",
                    "camera permission was denied by the host",
                )));
            }
            MediaCapturePermission::Granted => {}
            MediaCapturePermission::Prompt => {
                match (host.request_permission)(MediaDeviceKind::VideoInput) {
                    Ok(true) => {}
                    Ok(false) => {
                        return Promise::ready(Err(OpError::new(
                            "NotAllowedError",
                            "camera permission was denied by the host",
                        )));
                    }
                    Err(message) => {
                        return Promise::ready(Err(OpError::new(
                            "NotAllowedError",
                            format!("camera permission request failed: {message}"),
                        )));
                    }
                }
            }
        }
        let source = match (host.open_video)() {
            Ok(source) if source.device.kind == MediaDeviceKind::VideoInput => source,
            Ok(_) => {
                return Promise::ready(Err(OpError::new(
                    "NotReadableError",
                    "the host returned a non-video source for a camera request",
                )));
            }
            Err(message) => {
                return Promise::ready(Err(OpError::new(
                    "NotReadableError",
                    format!("camera capture failed: {message}"),
                )));
            }
        };
        let source_label = source.device.label;
        let frame = source.next_frame;
        let lease = CaptureLease::new(source.stop);
        let source: VideoFrameSource = Rc::new(move || {
            frame().map_err(|message| {
                OpError::new(
                    "NotReadableError",
                    format!("camera frame failed: {message}"),
                )
            })
        });
        let data = TrackData::new(
            "video",
            source_label,
            Some(source),
            30.0,
            false,
            Rc::downgrade(&realm),
            Some(lease),
        );
        realm.media_capture.register(&data);
        let track = make_track(ctx, data);
        let stream = ctx.new_instance(DomMediaStream {
            base: crate::events::DomEventTarget::independent(&realm),
            id: stream_id(),
            tracks: RefCell::new(vec![track]),
        });
        Promise::ready(Ok::<Value, OpError>(stream))
    }
}

fn stream_id() -> String {
    format!("media-stream-{}", NEXT_ID.fetch_add(1, Ordering::Relaxed))
}

fn stream_with_canvas_track(
    ctx: &mut Ctx,
    source: VideoFrameSource,
    frame_rate: f64,
    realm: &Rc<DomRealm>,
) -> OpResult<Value> {
    let data = TrackData::new(
        "video",
        "Canvas".into(),
        Some(source),
        frame_rate,
        true,
        Rc::downgrade(realm),
        None,
    );
    realm.media_capture.register(&data);
    let track = make_track(ctx, data);
    Ok(ctx.new_instance(DomMediaStream {
        base: crate::events::DomEventTarget::independent(realm),
        id: stream_id(),
        tracks: RefCell::new(vec![track]),
    }))
}

fn make_track(ctx: &mut Ctx, data: Rc<TrackData>) -> Value {
    let value = if data.canvas_capture {
        ctx.new_instance(DomCanvasCaptureMediaStreamTrack {
            base: DomMediaStreamTrack {
                base: data
                    .realm
                    .upgrade()
                    .map_or_else(crate::events::DomEventTarget::new, |realm| {
                        crate::events::DomEventTarget::independent(&realm)
                    }),
                data,
            },
        })
    } else {
        ctx.new_instance(DomMediaStreamTrack {
            base: crate::events::DomEventTarget::new(),
            data,
        })
    };
    let track = ctx
        .with_instance::<DomMediaStreamTrack, _>(&value, |track| track.data.clone())
        .expect("new media track has its declared native type");
    *track.wrapper.borrow_mut() = ctx.weak_value(&value);
    value
}

fn dispatch_track_event(ctx: &mut Ctx, track: &TrackData, kind: &str) -> OpResult<()> {
    let Some(target) = track.wrapper.borrow().as_ref().and_then(WeakValue::upgrade) else {
        return Ok(());
    };
    let event = crate::events::DomEvent::new(ctx, kind, None)?;
    let event = JsObject::from_value(ctx.new_instance(event))
        .ok_or_else(|| OpError::new("TypeError", "could not construct a track event"))?;
    crate::events::DomEventTarget::dispatch_event(ctx, This(target), event).map(|_| ())
}

pub(crate) fn canvas_capture_stream(
    ctx: &mut Ctx,
    source: VideoFrameSource,
    frame_rate: Option<f64>,
    realm: &Rc<DomRealm>,
) -> OpResult<Value> {
    if frame_rate.is_some_and(|rate| !rate.is_finite() || rate < 0.0) {
        return Err(OpError::new(
            "NotSupportedError",
            "frame rate must be non-negative",
        ));
    }
    stream_with_canvas_track(ctx, source, frame_rate.unwrap_or(0.0), realm)
}

pub(crate) fn install(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> OpResult<()> {
    let global = ctx.global_object();
    for (name, constructor) in [
        ("MediaStream", ctx.class_constructor::<DomMediaStream>()),
        (
            "MediaStreamTrack",
            ctx.class_constructor::<DomMediaStreamTrack>(),
        ),
        (
            "CanvasCaptureMediaStreamTrack",
            ctx.class_constructor::<DomCanvasCaptureMediaStreamTrack>(),
        ),
        ("MediaDevices", ctx.class_constructor::<DomMediaDevices>()),
        (
            "MediaStreamTrackEvent",
            ctx.class_constructor::<DomMediaStreamTrackEvent>(),
        ),
        (
            "MediaDeviceInfo",
            ctx.class_constructor::<DomMediaDeviceInfo>(),
        ),
    ] {
        crate::install_interface(ctx, &global, name, constructor)
            .map_err(|_| OpError::new("Error", "media capture interface installation failed"))?;
    }
    let navigator = ctx
        .member_get(&global, "navigator")
        .map_err(OpError::thrown)?;
    let media_devices = devices(ctx, realm);
    ctx.member_set(&navigator, "mediaDevices", media_devices)
        .map_err(OpError::thrown)?;
    Ok(())
}

pub(crate) fn devices(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> Value {
    ctx.new_instance(DomMediaDevices {
        realm: Rc::downgrade(realm),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen::Engine;

    fn eval_bool(engine: &mut Engine, source: &str) -> bool {
        match engine.eval_value(source) {
            Ok(Ok(Value::Bool(value))) => value,
            Ok(Err(error)) => panic!(
                "JavaScript evaluation ended abruptly: {}",
                engine
                    .ctx()
                    .to_string(&error)
                    .map(|text| text.to_string())
                    .unwrap_or_else(|_| "<unprintable>".into())
            ),
            Err(_) => panic!("JavaScript evaluation could not start"),
            Ok(Ok(_)) => panic!("JavaScript assertion did not return a boolean"),
        }
    }

    fn eval_string(engine: &mut Engine, source: &str) -> String {
        match engine.eval_value(source) {
            Ok(Ok(Value::Str(value))) => value.as_str().to_owned(),
            Ok(Err(_)) => panic!("JavaScript string evaluation ended abruptly: {source}"),
            Err(_) => panic!("JavaScript string evaluation could not start: {source}"),
            Ok(Ok(_)) => panic!("JavaScript string evaluation returned non-string: {source}"),
        }
    }

    #[test]
    fn canvas_capture_stream_retains_requested_real_frame_and_track_lifecycle() {
        let mut engine = Engine::new();
        let realm =
            crate::install(engine.ctx(), "<canvas width='2' height='1'></canvas>", 64).unwrap();
        assert!(eval_bool(
            &mut engine,
            "(()=>{const c=document.querySelector('canvas');const x=c.getContext('2d');x.fillStyle='rgb(12,34,56)';x.fillRect(0,0,2,1);const s=c.captureStream(0),t=s.getVideoTracks()[0];globalThis.__captureTestStream=s;if(!(s instanceof MediaStream)||!(t instanceof CanvasCaptureMediaStreamTrack)||t.kind!=='video'||!s.active)return false;t.requestFrame();return s.getTracks().length===1&&t.readyState==='live'})()"
        ));
        let stream = match engine.eval_value("__captureTestStream") {
            Ok(Ok(stream)) => stream,
            Ok(Err(_)) => panic!("captureStream evaluation ended abruptly"),
            Err(_) => panic!("captureStream evaluation could not start"),
        };
        let track = engine
            .ctx()
            .with_instance::<DomMediaStream, _>(&stream, |stream| stream.tracks.borrow()[0].clone())
            .unwrap();
        let pixels = engine
            .ctx()
            .with_instance::<DomCanvasCaptureMediaStreamTrack, _>(&track, |track| {
                track
                    .base
                    .data
                    .last_frame
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .rgba
                    .as_ref()
                    .clone()
            })
            .unwrap();
        assert_eq!(pixels, [12, 34, 56, 255, 12, 34, 56, 255]);
        assert!(eval_bool(
            &mut engine,
            "(()=>{const t=document.querySelector('canvas').captureStream(0).getVideoTracks()[0];let ended=0;t.addEventListener('ended',()=>ended++);t.stop();t.stop();return t.readyState==='ended'&&ended===0})()"
        ));
        drop(realm);
    }

    #[test]
    fn media_devices_rejects_capture_without_a_real_host_source() {
        let mut engine = Engine::new();
        let _realm = crate::install(engine.ctx(), "<main></main>", 64).unwrap();
        assert!(eval_bool(
            &mut engine,
            "(()=>{globalThis.__mediaDeviceResult='pending';navigator.mediaDevices.getUserMedia({video:true}).catch(e=>globalThis.__mediaDeviceResult=e.name);return navigator.mediaDevices instanceof MediaDevices&&typeof navigator.mediaDevices.enumerateDevices==='function'&&globalThis.__mediaDeviceResult==='pending'})()"
        ));
        engine.run_microtasks();
        assert!(eval_bool(
            &mut engine,
            "globalThis.__mediaDeviceResult==='NotFoundError'"
        ));
    }

    #[test]
    fn media_devices_host_grants_camera_and_streams_real_frame_pixels() {
        let mut engine = Engine::new();
        let realm = crate::install(engine.ctx(), "<main></main>", 64).unwrap();
        let permission_requests = Rc::new(Cell::new(0));
        let permission_requests_host = permission_requests.clone();
        let opens = Rc::new(Cell::new(0));
        let opens_host = opens.clone();
        let stops = Rc::new(Cell::new(0));
        let stops_host = stops.clone();
        let device = MediaDeviceDescription {
            device_id: "camera-0".into(),
            kind: MediaDeviceKind::VideoInput,
            label: "Test camera".into(),
            group_id: "group-0".into(),
        };
        let frame_device = device.clone();
        realm.set_media_devices_host(Some(MediaDevicesHost {
            permission: Rc::new(|_| MediaCapturePermission::Prompt),
            request_permission: Rc::new(move |_| {
                permission_requests_host.set(permission_requests_host.get() + 1);
                Ok(true)
            }),
            enumerate: Rc::new(move || vec![device.clone()]),
            open_video: Rc::new(move || {
                opens_host.set(opens_host.get() + 1);
                Ok(MediaCaptureVideoSource {
                    device: frame_device.clone(),
                    next_frame: Rc::new(|| {
                        Ok(VideoFrameSnapshot {
                            presentation_time_micros: 0,
                            width: 1,
                            height: 1,
                            rgba: std::sync::Arc::new(vec![90, 80, 70, 255]),
                        })
                    }),
                    stop: {
                        let stops_host = stops_host.clone();
                        Rc::new(move || stops_host.set(stops_host.get() + 1))
                    },
                })
            }),
        }));
        assert!(eval_bool(
            &mut engine,
            "globalThis.__captureResult='pending';globalThis.__deviceResult='pending';navigator.mediaDevices.enumerateDevices().then(devices=>globalThis.__deviceResult=devices.length+':'+devices[0].kind+':'+devices[0].label);navigator.mediaDevices.getUserMedia({video:true}).then(stream=>{globalThis.__cameraStream=stream;globalThis.__captureResult=stream instanceof MediaStream&&stream.active&&stream.getVideoTracks()[0].label==='Test camera'?'ready':'bad'},e=>globalThis.__captureResult=e.name);true"
        ));
        engine.run_microtasks();
        assert_eq!(
            eval_string(&mut engine, "String(globalThis.__captureResult)"),
            "ready"
        );
        assert!(eval_bool(
            &mut engine,
            "globalThis.__deviceResult==='1:videoinput:Test camera'"
        ));
        assert_eq!(permission_requests.get(), 1);
        assert_eq!(opens.get(), 1);
        assert!(eval_bool(
            &mut engine,
            "globalThis.__cameraTrack=globalThis.__cameraStream.getVideoTracks()[0];globalThis.__cameraClone=globalThis.__cameraTrack.clone();globalThis.__cameraClone.stop();globalThis.__cameraTrack.readyState==='live'&&globalThis.__cameraClone.readyState==='ended'"
        ));
        assert_eq!(stops.get(), 0, "a cloned live track keeps its input open");
        let track = match engine.eval_value("__cameraStream.getVideoTracks()[0]") {
            Ok(Ok(track)) => track,
            _ => panic!("camera track was not returned"),
        };
        let data = match engine
            .ctx()
            .with_instance::<DomMediaStreamTrack, _>(&track, |track| track.data.clone())
        {
            Ok(data) => data,
            Err(_) => panic!("camera track has its declared native type"),
        };
        data.next_frame
            .set(Instant::now() - Duration::from_millis(1));
        assert!(realm.pump_media_capture(engine.ctx()).is_ok());
        assert_eq!(
            data.last_frame.borrow().as_ref().unwrap().rgba.as_ref(),
            &[90, 80, 70, 255]
        );
        assert!(eval_bool(
            &mut engine,
            "globalThis.__cameraTrack.stop();globalThis.__cameraTrack.readyState==='ended'"
        ));
        assert_eq!(stops.get(), 1, "last track release closes the camera once");
    }

    #[test]
    fn automatic_capture_pump_samples_due_frames_and_stops_waking_after_end() {
        let captured = Rc::new(Cell::new(0));
        let capture_count = captured.clone();
        let source: VideoFrameSource = Rc::new(move || {
            capture_count.set(capture_count.get() + 1);
            Ok(VideoFrameSnapshot {
                presentation_time_micros: 0,
                width: 1,
                height: 1,
                rgba: std::sync::Arc::new(vec![1, 2, 3, 255]),
            })
        });
        let track = TrackData::new(
            "video",
            "Canvas".into(),
            Some(source),
            30.0,
            true,
            Weak::new(),
            None,
        );
        let registry = RealmMediaCapture::default();
        let mut engine = Engine::new();
        registry.register(&track);
        assert!(registry.animation_pending());
        assert!(registry.next_delay_ms().is_some());
        track
            .next_frame
            .set(Instant::now() - Duration::from_millis(1));
        assert!(registry.pump(engine.ctx()).is_ok());
        assert_eq!(captured.get(), 1);
        assert_eq!(
            track.last_frame.borrow().as_ref().unwrap().rgba.as_ref(),
            &[1, 2, 3, 255]
        );
        track.stop();
        assert!(registry.pump(engine.ctx()).is_ok());
        assert!(!registry.animation_pending());
        assert_eq!(registry.next_delay_ms(), None);
    }

    #[test]
    fn stream_script_mutation_is_silent_and_clone_preserves_canvas_track_type() {
        let mut engine = Engine::new();
        let _realm = crate::install(engine.ctx(), "<canvas></canvas>", 64).unwrap();
        assert!(eval_bool(
            &mut engine,
            "(()=>{const source=document.querySelector('canvas').captureStream(0).getVideoTracks()[0];const s=new MediaStream();let added=0,removed=0;s.addEventListener('addtrack',e=>added+=e.track===source?1:10);s.addEventListener('removetrack',e=>removed+=e.track===source?1:10);s.addTrack(source);const copy=s.clone(),cloneTrack=copy.getVideoTracks()[0];s.removeTrack(source);return added===0&&removed===0&&cloneTrack instanceof CanvasCaptureMediaStreamTrack&&cloneTrack.readyState==='live'})()"
        ));
    }

    #[test]
    fn media_stream_constructor_uses_iterables_and_preserves_track_identity() {
        let mut engine = Engine::new();
        let _realm = crate::install(engine.ctx(), "<canvas></canvas>", 64).unwrap();
        assert!(eval_bool(
            &mut engine,
            r#"(()=>{
            const original=document.querySelector('canvas').captureStream(0);
            const t=original.getVideoTracks()[0];
            const iterable=new MediaStream(new Set([t]));
            const duplicate=new MediaStream([t,t]);
            const copied=new MediaStream(original);
            if(iterable.getTrackById(t.id)!==t||duplicate.getTracks().length!==1||copied.getTracks()[0]!==t||copied.id===original.id)throw new Error('stream assertion');
            if(copied.getTrackById('missing')!==null||new MediaStream().active||new MediaStream(undefined).getTracks().length!==0)throw new Error('empty assertion');
            let closed=0, error='';
            const invalid={ [Symbol.iterator](){return {next(){return {done:false,value:{}}},return(){closed++;return {done:true}}}}};
            try{new MediaStream(invalid)}catch(e){error=e.name}
            if(error!=='TypeError'||closed!==1)throw new Error('iterator close '+error+':'+closed);
            let nullError='',arrayLikeError='';
            try{new MediaStream(null)}catch(e){nullError=e.name}
            try{new MediaStream({0:t,length:1})}catch(e){arrayLikeError=e.name}
            if(nullError!=='TypeError'||arrayLikeError!=='TypeError')throw new Error('invalid args '+nullError+':'+arrayLikeError);
            copied.removeTrack(t);
            if(!original.active||copied.active)throw new Error('ownership assertion');
            t.stop();
            return !original.active&&!iterable.active&&duplicate.getTrackById(t.id)===t;
        })()"#
        ));
    }

    #[test]
    fn source_failure_and_recovery_dispatch_mute_and_unmute_events() {
        let mut engine = Engine::new();
        let _realm = crate::install(engine.ctx(), "<main></main>", 64).unwrap();
        let available = Rc::new(Cell::new(false));
        let source_available = available.clone();
        let source: VideoFrameSource = Rc::new(move || {
            if !source_available.get() {
                return Err(OpError::new("SecurityError", "canvas snapshot unavailable"));
            }
            Ok(VideoFrameSnapshot {
                presentation_time_micros: 0,
                width: 1,
                height: 1,
                rgba: std::sync::Arc::new(vec![4, 5, 6, 255]),
            })
        });
        let data = TrackData::new(
            "video",
            "Canvas".into(),
            Some(source),
            0.0,
            true,
            Weak::new(),
            None,
        );
        let track = make_track(engine.ctx(), data.clone());
        let global = engine.ctx().global_object();
        engine
            .ctx()
            .member_set(&global, "__muteTestTrack", track)
            .unwrap_or_else(|_| panic!("could not retain the muted track in the test realm"));
        assert!(eval_bool(
            &mut engine,
            "globalThis.__muteEvents=[];__muteTestTrack.addEventListener('mute',()=>__muteEvents.push('mute'));__muteTestTrack.addEventListener('unmute',()=>__muteEvents.push('unmute'));true"
        ));
        assert!(data.capture(engine.ctx()).is_err());
        assert!(eval_bool(&mut engine, "__muteEvents.join(',')==='mute'"));
        available.set(true);
        assert!(data.capture(engine.ctx()).is_ok());
        assert!(eval_bool(
            &mut engine,
            "__muteEvents.join(',')==='mute,unmute'"
        ));
    }
}
