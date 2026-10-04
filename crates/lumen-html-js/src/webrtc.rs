//! Typed DOM facade for the host-owned WebRTC protocol driver.
//!
//! The host implementation is backed by str0m and real UDP sockets. With no
//! host attached, construction fails instead of returning a fake connection.

use super::*;
use lumen::embed::{JsObject, Promise, This};
use std::{cell::RefCell, collections::HashMap};

#[derive(Clone, Debug)]
pub enum WebRtcHostEvent {
    IceGatheringState(&'static str),
    Connected,
    Disconnected,
    DataChannelOpen {
        channel: u64,
        label: String,
        remote: bool,
    },
    DataChannelClose {
        channel: u64,
    },
    Message {
        channel: u64,
        binary: bool,
        bytes: Vec<u8>,
    },
    IceCandidate { candidate: Option<String> },
}

/// Adapter implemented by the browser runtime. Session descriptions are SDP
/// text produced and parsed by the pinned str0m implementation.
pub trait WebRtcHost {
    fn pump(&self) -> Result<(), String>;
    fn next_delay_ms(&self) -> Option<u64>;
    fn create_peer(&self) -> Result<u64, String>;
    fn create_data_channel(&self, peer: u64, label: String) -> Result<u64, String>;
    fn create_offer(&self, peer: u64) -> Result<String, String>;
    fn accept_offer(&self, peer: u64, offer: &str) -> Result<String, String>;
    fn accept_answer(&self, peer: u64, answer: &str) -> Result<(), String>;
    fn start_ice_gathering(&self, peer: u64) -> Result<(), String>;
    fn add_ice_candidate(&self, peer: u64, candidate: Option<&str>) -> Result<(), String>;
    fn send_data(
        &self,
        peer: u64,
        channel: u64,
        binary: bool,
        bytes: &[u8],
    ) -> Result<bool, String>;
    fn close_data_channel(&self, peer: u64, channel: u64) -> Result<(), String>;
    fn connected(&self, peer: u64) -> Result<bool, String>;
    fn channel_open(&self, peer: u64, channel: u64) -> Result<bool, String>;
    fn take_events(&self, peer: u64) -> Result<Vec<WebRtcHostEvent>, String>;
    fn close_peer(&self, peer: u64) -> Result<(), String>;
}

#[derive(Clone)]
struct HostSlot {
    host: RefCell<Option<Rc<dyn WebRtcHost>>>,
    peers: RefCell<HashMap<u64, WeakValue>>,
    channels: RefCell<HashMap<(u64, u64), WeakValue>>,
}

impl Default for HostSlot {
    fn default() -> Self {
        Self {
            host: RefCell::new(None),
            peers: RefCell::new(HashMap::new()),
            channels: RefCell::new(HashMap::new()),
        }
    }
}

pub fn set_host(ctx: &mut Ctx, host: Option<Rc<dyn WebRtcHost>>) {
    if ctx.op_state().get::<HostSlot>().is_none() {
        ctx.op_state().put(HostSlot::default());
    }
    if let Some(slot) = ctx.op_state().get_mut::<HostSlot>() {
        *slot.host.borrow_mut() = host;
    }
}

fn host(ctx: &mut Ctx) -> OpResult<Rc<dyn WebRtcHost>> {
    ctx.op_state()
        .get::<HostSlot>()
        .and_then(|slot| slot.host.borrow().clone())
        .ok_or_else(|| OpError::new("NotSupportedError", "WebRTC UDP service is unavailable"))
}

struct PeerData {
    host: Rc<dyn WebRtcHost>,
    id: u64,
    current_local_description: RefCell<Option<(String, String)>>,
    pending_local_description: RefCell<Option<(String, String)>>,
    current_remote_description: RefCell<Option<(String, String)>>,
    pending_remote_description: RefCell<Option<(String, String)>>,
    pending_answer: RefCell<Option<String>>,
    created_offer: RefCell<Option<String>>,
    signaling_state: Cell<&'static str>,
    ice_gathering_state: Cell<&'static str>,
    closed: Cell<bool>,
}

#[lumen_bind::class(
    name = "RTCPeerConnection",
    extends = crate::events::DomEventTarget,
    hint(js(webidl))
)]
pub struct DomRTCPeerConnection {
    base: crate::events::DomEventTarget,
    data: Rc<PeerData>,
}

#[lumen_bind::methods]
impl DomRTCPeerConnection {
    #[constructor]
    fn new(ctx: &mut Ctx, this: This<Value>, configuration: Option<Value>) -> OpResult<Self> {
        if let Some(configuration) = configuration.filter(|value| !matches!(value, Value::Null)) {
            let ice_servers = ctx
                .member_get(&configuration, "iceServers")
                .map_err(OpError::thrown)?;
            if !matches!(ice_servers, Value::Undefined | Value::Null) {
                let length = ctx.member_get(&ice_servers, "length").map_err(OpError::thrown)?;
                match length {
                    Value::Num(length) if length == 0.0 => {}
                    Value::Num(_) => {
                        return Err(OpError::new(
                            "NotSupportedError",
                            "STUN and TURN ICE servers are not supported by this runtime",
                        ));
                    }
                    _ => return Err(OpError::type_error("iceServers must be an array")),
                }
            }
        }
        let host = host(ctx)?;
        let id = host
            .create_peer()
            .map_err(|_| OpError::new("NetworkError", "could not allocate a WebRTC peer"))?;
        register_peer(ctx, id, &this.0);
        Ok(Self {
            base: crate::events::DomEventTarget::new(),
            data: Rc::new(PeerData {
                host,
                id,
                current_local_description: RefCell::new(None),
                pending_local_description: RefCell::new(None),
                current_remote_description: RefCell::new(None),
                pending_remote_description: RefCell::new(None),
                pending_answer: RefCell::new(None),
                created_offer: RefCell::new(None),
                signaling_state: Cell::new("stable"),
                ice_gathering_state: Cell::new("new"),
                closed: Cell::new(false),
            }),
        })
    }

    #[getter]
    fn connection_state(&self) -> &'static str {
        if self.data.closed.get() {
            "closed"
        } else if self.data.host.connected(self.data.id).unwrap_or(false) {
            "connected"
        } else {
            "new"
        }
    }

    #[getter]
    fn signaling_state(&self) -> &'static str {
        if self.data.closed.get() {
            "closed"
        } else {
            self.data.signaling_state.get()
        }
    }

    #[getter]
    fn ice_gathering_state(&self) -> &'static str {
        self.data.ice_gathering_state.get()
    }

    fn add_ice_candidate(&self, ctx: &mut Ctx, candidate: Option<Value>) -> Promise<()> {
        let result = if self.data.closed.get() {
            Err(OpError::new("InvalidStateError", "peer connection is closed"))
        } else if self.data.pending_remote_description.borrow().is_none()
            && self.data.current_remote_description.borrow().is_none() {
            Err(OpError::new("InvalidStateError", "remote description has not been set"))
        } else {
            parse_ice_candidate(ctx, candidate.as_ref()).and_then(|candidate| {
                self.data.host.add_ice_candidate(self.data.id, candidate.as_deref())
                    .map_err(|_| OpError::new("OperationError", "ICE candidate was rejected"))
            })
        };
        Promise::ready(result)
    }

    #[getter]
    fn local_description(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let pending = self.data.pending_local_description.borrow();
        let current = self.data.current_local_description.borrow();
        description_or_null(ctx, pending.as_ref().or(current.as_ref()))
    }

    #[getter]
    fn remote_description(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let pending = self.data.pending_remote_description.borrow();
        let current = self.data.current_remote_description.borrow();
        description_or_null(ctx, pending.as_ref().or(current.as_ref()))
    }

    #[getter]
    fn current_local_description(&self, ctx: &mut Ctx) -> OpResult<Value> {
        description_or_null(ctx, self.data.current_local_description.borrow().as_ref())
    }

    #[getter]
    fn pending_local_description(&self, ctx: &mut Ctx) -> OpResult<Value> {
        description_or_null(ctx, self.data.pending_local_description.borrow().as_ref())
    }

    #[getter]
    fn current_remote_description(&self, ctx: &mut Ctx) -> OpResult<Value> {
        description_or_null(ctx, self.data.current_remote_description.borrow().as_ref())
    }

    #[getter]
    fn pending_remote_description(&self, ctx: &mut Ctx) -> OpResult<Value> {
        description_or_null(ctx, self.data.pending_remote_description.borrow().as_ref())
    }

    fn create_data_channel(
        &self,
        ctx: &mut Ctx,
        this: This<Value>,
        label: String,
    ) -> OpResult<Value> {
        register_peer(ctx, self.data.id, &this.0);
        if self.data.closed.get() {
            return Err(OpError::new(
                "InvalidStateError",
                "peer connection is closed",
            ));
        }
        if label.len() > 256 {
            return Err(OpError::new(
                "TypeError",
                "data channel label exceeds 256 bytes",
            ));
        }
        let channel = self
            .data
            .host
            .create_data_channel(self.data.id, label.clone())
            .map_err(|_| OpError::new("OperationError", "could not create data channel"))?;
        let value = ctx.new_instance(DomRTCDataChannel {
            base: crate::events::DomEventTarget::new(),
            peer: self.data.clone(),
            id: channel,
            label,
            closed: Cell::new(false),
        });
        register_channel(ctx, self.data.id, channel, &value);
        Ok(value)
    }

    fn create_offer(&self, ctx: &mut Ctx, this: This<Value>) -> Promise<Value> {
        register_peer(ctx, self.data.id, &this.0);
        let result = self
            .data
            .host
            .create_offer(self.data.id)
            .and_then(|sdp| {
                *self.data.created_offer.borrow_mut() = Some(sdp.clone());
                description(ctx, "offer", &sdp)
                    .map_err(|_| String::from("could not create offer object"))
            })
            .map_err(|_| OpError::new("OperationError", "could not create WebRTC offer"));
        Promise::ready(result)
    }

    fn create_answer(&self, ctx: &mut Ctx) -> Promise<Value> {
        let result = self
            .data
            .pending_answer
            .borrow()
            .as_ref()
            .ok_or_else(|| OpError::new("InvalidStateError", "no remote offer has been set"))
            .and_then(|sdp| description(ctx, "answer", sdp));
        Promise::ready(result)
    }

    fn set_remote_description(
        &self,
        ctx: &mut Ctx,
        this: This<Value>,
        description_value: Value,
    ) -> Promise<()> {
        register_peer(ctx, self.data.id, &this.0);
        let result = parse_description(ctx, &description_value).and_then(|(kind, sdp)| {
            let accepted = match kind.as_str() {
                "offer" => self
                    .data
                    .host
                    .accept_offer(self.data.id, &sdp)
                    .map(|answer| *self.data.pending_answer.borrow_mut() = Some(answer))
                    .map_err(|_| {
                        OpError::new("OperationError", "remote WebRTC offer was rejected")
                    }),
                "answer" => self
                    .data
                    .host
                    .accept_answer(self.data.id, &sdp)
                    .map_err(|_| {
                        OpError::new("OperationError", "remote WebRTC answer was rejected")
                    }),
                _ => {
                    return Err(OpError::new(
                        "TypeError",
                        "description type must be offer or answer",
                    ))
                }
            };
            accepted.map(|()| {
                if kind == "offer" {
                    self.data.signaling_state.set("have-remote-offer");
                    *self.data.pending_remote_description.borrow_mut() = Some((kind, sdp));
                } else {
                    *self.data.pending_remote_description.borrow_mut() = Some((kind, sdp));
                    self.commit_descriptions();
                }
            })
        });
        Promise::ready(result)
    }

    fn set_local_description(&self, ctx: &mut Ctx, description_value: Value) -> Promise<()> {
        let result = parse_description(ctx, &description_value).and_then(|(kind, sdp)| {
            let matches_created = match kind.as_str() {
                "offer" => self.data.created_offer.borrow().as_deref() == Some(sdp.as_str()),
                "answer" => self.data.pending_answer.borrow().as_deref() == Some(sdp.as_str()),
                _ => false,
            };
            if !matches_created {
                return Err(OpError::new(
                    "InvalidStateError",
                    "description was not created for this peer",
                ));
            }
            self.data.host.start_ice_gathering(self.data.id).map_err(|_| {
                OpError::new("OperationError", "could not start local ICE gathering")
            })?;
            *self.data.pending_local_description.borrow_mut() = Some((kind.clone(), sdp));
            if kind == "offer" {
                self.data.signaling_state.set("have-local-offer");
            } else {
                self.commit_descriptions();
            }
            Ok(())
        });
        Promise::ready(result)
    }

    fn commit_descriptions(&self) {
        if let Some(description) = self.data.pending_local_description.borrow_mut().take() {
            *self.data.current_local_description.borrow_mut() = Some(description);
        }
        if let Some(description) = self.data.pending_remote_description.borrow_mut().take() {
            *self.data.current_remote_description.borrow_mut() = Some(description);
        }
        self.data.signaling_state.set("stable");
    }

    fn close(&self) {
        if !self.data.closed.replace(true) {
            let _ = self.data.host.close_peer(self.data.id);
        }
    }
}

#[lumen_bind::class(
    name = "RTCDataChannel",
    extends = crate::events::DomEventTarget,
    hint(js(webidl))
)]
pub struct DomRTCDataChannel {
    base: crate::events::DomEventTarget,
    peer: Rc<PeerData>,
    id: u64,
    label: String,
    closed: Cell<bool>,
}

#[lumen_bind::methods]
impl DomRTCDataChannel {
    #[getter]
    fn label(&self) -> String {
        self.label.clone()
    }

    #[getter]
    fn ready_state(&self) -> &'static str {
        if self.closed.get() || self.peer.closed.get() {
            "closed"
        } else if self
            .peer
            .host
            .channel_open(self.peer.id, self.id)
            .unwrap_or(false)
        {
            "open"
        } else {
            "connecting"
        }
    }

    fn send(&self, data: Value, ctx: &mut Ctx) -> OpResult<()> {
        if self.closed.get() || self.peer.closed.get() {
            return Err(OpError::new("InvalidStateError", "data channel is closed"));
        }
        if !self.peer.host.connected(self.peer.id).unwrap_or(false) {
            return Err(OpError::new(
                "InvalidStateError",
                "data channel is not open",
            ));
        }
        let (binary, bytes) = match &data {
            Value::Str(text) => (false, text.as_bytes().to_vec()),
            Value::Obj(_) => (
                true,
                ctx.typed_array_bytes(&data).ok_or_else(|| {
                    OpError::type_error(
                        "data channel send accepts strings or attached TypedArray views",
                    )
                })?,
            ),
            _ => {
                return Err(OpError::type_error(
                    "data channel send accepts strings or TypedArray views",
                ))
            }
        };
        match self
            .peer
            .host
            .send_data(self.peer.id, self.id, binary, &bytes)
        {
            Ok(true) => Ok(()),
            Ok(false) => Err(OpError::new(
                "OperationError",
                "data channel send buffer is full",
            )),
            Err(_) => Err(OpError::new("NetworkError", "data channel send failed")),
        }
    }

    fn close(&self) -> OpResult<()> {
        if !self.closed.get() {
            self.peer
                .host
                .close_data_channel(self.peer.id, self.id)
                .map_err(|_| OpError::new("NetworkError", "data channel close failed"))?;
            self.closed.set(true);
        }
        Ok(())
    }
}

fn description(ctx: &mut Ctx, kind: &str, sdp: &str) -> OpResult<Value> {
    let value = ctx.new_object();
    let value = Value::Obj(value);
    ctx.member_set(&value, "type", Value::str(kind))
        .map_err(OpError::thrown)?;
    ctx.member_set(&value, "sdp", Value::str(sdp))
        .map_err(OpError::thrown)?;
    Ok(value)
}

fn description_or_null(ctx: &mut Ctx, value: Option<&(String, String)>) -> OpResult<Value> {
    value
        .map(|(kind, sdp)| description(ctx, kind, sdp))
        .transpose()
        .map(|description| description.unwrap_or(Value::Null))
}

fn parse_description(ctx: &mut Ctx, value: &Value) -> OpResult<(String, String)> {
    let kind = ctx.member_get(value, "type").map_err(OpError::thrown)?;
    let sdp = ctx.member_get(value, "sdp").map_err(OpError::thrown)?;
    let Value::Str(kind) = kind else {
        return Err(OpError::type_error(
            "session description type must be a string",
        ));
    };
    let Value::Str(sdp) = sdp else {
        return Err(OpError::type_error(
            "session description sdp must be a string",
        ));
    };
    Ok((kind.as_str().to_owned(), sdp.as_str().to_owned()))
}

fn parse_ice_candidate(ctx: &mut Ctx, value: Option<&Value>) -> OpResult<Option<String>> {
    let Some(value) = value.filter(|value| !matches!(value, Value::Null)) else { return Ok(None); };
    let candidate = ctx.member_get(value, "candidate").map_err(OpError::thrown)?;
    let Value::Str(candidate) = candidate else { return Err(OpError::type_error("ICE candidate candidate must be a string")); };
    if candidate.as_str().is_empty() { return Ok(None); }
    if !candidate.as_str().starts_with("candidate:") {
        return Err(OpError::new("TypeError", "ICE candidate must use candidate:<foundation> syntax"));
    }
    Ok(Some(candidate.as_str().to_owned()))
}

fn register_peer(ctx: &mut Ctx, id: u64, value: &Value) {
    let weak = ctx.weak_value(value);
    if let (Some(weak), Some(slot)) = (weak, ctx.op_state().get_mut::<HostSlot>()) {
        slot.peers.borrow_mut().insert(id, weak);
    }
}

fn register_channel(ctx: &mut Ctx, peer: u64, channel: u64, value: &Value) {
    let weak = ctx.weak_value(value);
    if let (Some(weak), Some(slot)) = (weak, ctx.op_state().get_mut::<HostSlot>()) {
        slot.channels.borrow_mut().insert((peer, channel), weak);
    }
}

fn dispatch_host_event(
    ctx: &mut Ctx,
    target: Value,
    kind: &str,
    property: Option<(&str, Value)>,
) -> OpResult<()> {
    let event = crate::events::DomEvent::new(ctx, kind, None)?;
    let event = ctx.new_instance(event);
    if let Some((name, value)) = property {
        ctx.member_set(&event, name, value)
            .map_err(OpError::thrown)?;
    }
    let event = JsObject::from_value(event)
        .ok_or_else(|| OpError::new("TypeError", "could not construct WebRTC event"))?;
    crate::events::DomEventTarget::dispatch_event(ctx, This(target), event).map(|_| ())
}

/// Deliver protocol events on the owner turn. Packets and timers are pumped
/// by the service before this adapter is called, so listeners never run in a
/// socket callback or outside the page's JavaScript owner.
pub(crate) fn pump(ctx: &mut Ctx) -> OpResult<()> {
    let Some((host, peers, channels)) = ctx.op_state().get::<HostSlot>().map(|slot| {
        (
            slot.host.borrow().clone(),
            slot.peers
                .borrow()
                .iter()
                .map(|(id, weak)| (*id, weak.clone()))
                .collect::<Vec<_>>(),
            slot.channels
                .borrow()
                .iter()
                .map(|(key, weak)| (*key, weak.clone()))
                .collect::<Vec<_>>(),
        )
    }) else {
        return Ok(());
    };
    let Some(host) = host else { return Ok(()) };
    let peers: HashMap<_, _> = peers.into_iter().collect();
    let channels: HashMap<_, _> = channels.into_iter().collect();
    for (peer_id, peer_weak) in peers {
        let Some(peer) = peer_weak.upgrade() else {
            continue;
        };
        let events = host
            .take_events(peer_id)
            .map_err(|_| OpError::new("NetworkError", "WebRTC event delivery failed"))?;
        for event in events {
            match event {
                WebRtcHostEvent::IceGatheringState(state) => {
                    let changed = ctx.with_instance::<DomRTCPeerConnection, _>(&peer, |pc| {
                        pc.data.ice_gathering_state.replace(state) != state
                    })?;
                    if changed { dispatch_host_event(ctx, peer.clone(), "icegatheringstatechange", None)?; }
                }
                WebRtcHostEvent::Connected => {
                    dispatch_host_event(ctx, peer.clone(), "connectionstatechange", None)?;
                    dispatch_host_event(ctx, peer.clone(), "iceconnectionstatechange", None)?;
                }
                WebRtcHostEvent::IceCandidate { candidate } => {
                    if let Some(candidate) = candidate {
                        let init = ctx.new_object();
                        let init = Value::Obj(init);
                        ctx.member_set(&init, "candidate", Value::str(candidate)).map_err(OpError::thrown)?;
                        let dom_event = crate::events::DomEvent::new(ctx, "icecandidate", None)?;
                        let event = ctx.new_instance(dom_event);
                        ctx.member_set(&event, "candidate", init).map_err(OpError::thrown)?;
                        let event = JsObject::from_value(event).ok_or_else(|| OpError::new("TypeError", "could not construct ICE candidate event"))?;
                        crate::events::DomEventTarget::dispatch_event(ctx, This(peer.clone()), event)?;
                    } else {
                        dispatch_host_event(ctx, peer.clone(), "icecandidate", Some(("candidate", Value::Null)))?;
                    }
                }
                WebRtcHostEvent::Disconnected => {
                    dispatch_host_event(ctx, peer.clone(), "connectionstatechange", None)?;
                    dispatch_host_event(ctx, peer.clone(), "iceconnectionstatechange", None)?;
                }
                WebRtcHostEvent::DataChannelOpen {
                    channel,
                    label,
                    remote,
                } => {
                    let channel_value = match channels
                        .get(&(peer_id, channel))
                        .and_then(WeakValue::upgrade)
                    {
                        Some(channel) => channel,
                        None => {
                            let peer_data = ctx
                                .with_instance::<DomRTCPeerConnection, _>(&peer, |peer| {
                                    peer.data.clone()
                                })?;
                            let channel_value = ctx.new_instance(DomRTCDataChannel {
                                base: crate::events::DomEventTarget::new(),
                                peer: peer_data,
                                id: channel,
                                label,
                                closed: Cell::new(false),
                            });
                            register_channel(ctx, peer_id, channel, &channel_value);
                            channel_value
                        }
                    };
                    dispatch_host_event(ctx, channel_value.clone(), "open", None)?;
                    if remote {
                        dispatch_host_event(
                            ctx,
                            peer.clone(),
                            "datachannel",
                            Some(("channel", channel_value)),
                        )?;
                    }
                }
                WebRtcHostEvent::DataChannelClose { channel } => {
                    if let Some(channel) = channels
                        .get(&(peer_id, channel))
                        .and_then(WeakValue::upgrade)
                    {
                        dispatch_host_event(ctx, channel, "close", None)?;
                    }
                }
                WebRtcHostEvent::Message {
                    channel,
                    binary,
                    bytes,
                } => {
                    let Some(channel) = channels
                        .get(&(peer_id, channel))
                        .and_then(WeakValue::upgrade)
                    else {
                        continue;
                    };
                    let data = if binary {
                        ctx.make_array_buffer_from(bytes)
                    } else {
                        let text = String::from_utf8(bytes).map_err(|_| {
                            OpError::new("EncodingError", "invalid UTF-8 data-channel message")
                        })?;
                        Value::from_string(text)
                    };
                    dispatch_host_event(ctx, channel, "message", Some(("data", data)))?;
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn install(ctx: &mut Ctx) -> OpResult<()> {
    let global = ctx.global_object();
    for (name, constructor) in [
        (
            "RTCPeerConnection",
            ctx.class_constructor::<DomRTCPeerConnection>(),
        ),
        (
            "RTCDataChannel",
            ctx.class_constructor::<DomRTCDataChannel>(),
        ),
    ] {
        crate::install_interface(ctx, &global, name, constructor)
            .map_err(|_| OpError::new("Error", "WebRTC interface installation failed"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen::Engine;

    #[derive(Default)]
    struct FakeHost {
        next_peer: Cell<u64>,
        next_channel: Cell<u64>,
        connected: Cell<bool>,
        open: Cell<bool>,
        events: RefCell<HashMap<u64, Vec<WebRtcHostEvent>>>,
    }

    impl WebRtcHost for FakeHost {
        fn pump(&self) -> Result<(), String> {
            Ok(())
        }
        fn next_delay_ms(&self) -> Option<u64> {
            None
        }
        fn create_peer(&self) -> Result<u64, String> {
            let id = self.next_peer.get() + 1;
            self.next_peer.set(id);
            Ok(id)
        }
        fn create_data_channel(&self, peer: u64, _label: String) -> Result<u64, String> {
            let id = self.next_channel.get() + 1;
            self.next_channel.set(id);
            self.events.borrow_mut().entry(peer).or_default();
            Ok(id)
        }
        fn create_offer(&self, _peer: u64) -> Result<String, String> {
            Ok("v=0\r\n".into())
        }
        fn accept_offer(&self, _peer: u64, _offer: &str) -> Result<String, String> {
            Ok("v=0\r\n".into())
        }
        fn accept_answer(&self, _peer: u64, _answer: &str) -> Result<(), String> {
            Ok(())
        }
        fn start_ice_gathering(&self, _peer: u64) -> Result<(), String> { Ok(()) }
        fn add_ice_candidate(&self, _peer: u64, _candidate: Option<&str>) -> Result<(), String> { Ok(()) }
        fn send_data(
            &self,
            _peer: u64,
            _channel: u64,
            _binary: bool,
            _bytes: &[u8],
        ) -> Result<bool, String> {
            Ok(true)
        }
        fn close_data_channel(&self, _peer: u64, _channel: u64) -> Result<(), String> {
            self.open.set(false);
            Ok(())
        }
        fn connected(&self, _peer: u64) -> Result<bool, String> {
            Ok(self.connected.get())
        }
        fn channel_open(&self, _peer: u64, _channel: u64) -> Result<bool, String> {
            Ok(self.open.get())
        }
        fn take_events(&self, peer: u64) -> Result<Vec<WebRtcHostEvent>, String> {
            Ok(std::mem::take(
                self.events.borrow_mut().entry(peer).or_default(),
            ))
        }
        fn close_peer(&self, _peer: u64) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn dom_webrtc_state_and_owner_turn_events_follow_host_state() {
        let mut engine = Engine::new();
        let host = Rc::new(FakeHost::default());
        let _realm = crate::install(engine.ctx(), "", 32).expect("install HTML realm");
        set_host(engine.ctx(), Some(host.clone()));
        match engine.eval_value(
            "globalThis.rtcEvents=[]; globalThis.pc=new RTCPeerConnection(); pc.addEventListener('connectionstatechange',()=>rtcEvents.push(pc.connectionState)); globalThis.dc=pc.createDataChannel('test'); dc.addEventListener('open',()=>rtcEvents.push(dc.readyState));",
        ).expect("run WebRTC setup") {
            Ok(_) => {}
            Err(_) => panic!("WebRTC setup script threw"),
        }
        let slot = engine
            .ctx()
            .op_state()
            .get::<HostSlot>()
            .expect("WebRTC host slot");
        assert_eq!(slot.peers.borrow().len(), 1);
        assert_eq!(slot.channels.borrow().len(), 1);
        assert!(!host.connected.get());
        assert!(!host.open.get());
        host.connected.set(true);
        host.open.set(true);
        host.events.borrow_mut().insert(
            1,
            vec![
                WebRtcHostEvent::Connected,
                WebRtcHostEvent::DataChannelOpen {
                    channel: 1,
                    label: "test".into(),
                    remote: false,
                },
            ],
        );
        let state = engine
            .eval_value("globalThis.pc.connectionState === 'connected' && globalThis.dc.readyState === 'open'")
            .expect("read states");
        assert!(matches!(state, Ok(Value::Bool(true))));
        pump(engine.ctx()).expect("deliver owner-turn protocol events");
        let delivered = engine
            .eval_value("globalThis.rtcEvents.length === 2 && globalThis.rtcEvents[0] === 'connected' && globalThis.rtcEvents[1] === 'open'")
            .expect("read events");
        assert!(matches!(delivered, Ok(Value::Bool(true))));

        match engine
            .eval_value("globalThis.offerResult=null; pc.createOffer().then(value=>globalThis.offerResult=value);")
            .expect("create offer")
        {
            Ok(_) => {}
            Err(_) => panic!("createOffer script threw"),
        }
        engine.run_microtasks();
        let before_local = engine
            .eval_value("offerResult.sdp === 'v=0\\r\\n' && pc.localDescription === null")
            .expect("check uncommitted offer");
        assert!(matches!(before_local, Ok(Value::Bool(true))));

        match engine
            .eval_value("pc.setLocalDescription(offerResult).then(()=>{});")
            .expect("set local offer")
        {
            Ok(_) => {}
            Err(_) => panic!("setLocalDescription script threw"),
        }
        engine.run_microtasks();
        let pending_offer = engine
            .eval_value("pc.signalingState === 'have-local-offer' && pc.pendingLocalDescription.type === 'offer' && pc.currentLocalDescription === null")
            .expect("check pending local offer");
        assert!(matches!(pending_offer, Ok(Value::Bool(true))));

        match engine
            .eval_value(
                "pc.setRemoteDescription({type:'answer',sdp:offerResult.sdp}).then(()=>{});",
            )
            .expect("set remote answer")
        {
            Ok(_) => {}
            Err(_) => panic!("setRemoteDescription script threw"),
        }
        engine.run_microtasks();
        let committed_offer = engine
            .eval_value("pc.signalingState === 'stable' && pc.pendingLocalDescription === null && pc.currentLocalDescription.type === 'offer' && pc.currentRemoteDescription.type === 'answer'")
            .expect("check committed offer");
        assert!(matches!(committed_offer, Ok(Value::Bool(true))));
    }
}
