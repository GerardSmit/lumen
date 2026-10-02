//! `_xxinterpchannels`: process-global channels that interpreters (each on its own thread) send
//! shareable objects through. The ends, closing and release rules follow CPython 3.12's
//! `_xxinterpchannelsmodule.c`; the item queue is `lumen_os::channel::Queue`, the queue the JS
//! message ports use.

use super::subinterpm::current_interp_id;
use super::xid::Shared;
use lumen_os::channel::{Pop, Queue};
use std::sync::{Mutex, MutexGuard};

const SEND: i32 = 1;
const BOTH: i32 = 0;
const RECV: i32 = -1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChanErr {
    NotFound,
    Closed,
    InterpClosed,
    Empty,
    NotEmpty,
    NoNextId,
}

struct Item {
    interp: i64,
    data: Shared,
}

/// The interpreters associated with one side of a channel, and whether each is still open.
#[derive(Default)]
struct Ends {
    send: Vec<(i64, bool)>,
    recv: Vec<(i64, bool)>,
    numsendopen: i64,
    numrecvopen: i64,
}

impl Ends {
    fn side(&mut self, send: bool) -> (&mut Vec<(i64, bool)>, &mut i64) {
        if send {
            (&mut self.send, &mut self.numsendopen)
        } else {
            (&mut self.recv, &mut self.numrecvopen)
        }
    }

    /// Whether `interp` may use the side (`false`: it closed its end).
    fn associate(&mut self, interp: i64, send: bool) -> bool {
        let (ends, open) = self.side(send);
        match ends.iter().find(|e| e.0 == interp) {
            Some(e) => e.1,
            None => {
                ends.push((interp, true));
                *open += 1;
                true
            }
        }
    }

    fn is_open(&self) -> bool {
        self.numsendopen != 0 || self.numrecvopen != 0 || (self.send.is_empty() && self.recv.is_empty())
    }

    fn close_one(&mut self, interp: i64, send: bool) {
        let (ends, open) = self.side(send);
        let at = match ends.iter().position(|e| e.0 == interp) {
            Some(i) => i,
            None => {
                ends.push((interp, true));
                *open += 1;
                ends.len() - 1
            }
        };
        ends[at].1 = false;
        *open -= 1;
    }

    /// `which`: `SEND`, `RECV` or `BOTH`.
    fn close_interpreter(&mut self, interp: i64, which: i32) {
        if which >= 0 {
            self.close_one(interp, true);
        }
        if which <= 0 {
            self.close_one(interp, false);
        }
    }

    fn drop_interpreter(&mut self, interp: i64) {
        for send in [true, false] {
            let (ends, open) = self.side(send);
            if let Some(e) = ends.iter_mut().find(|e| e.0 == interp) {
                e.1 = false;
                *open -= 1;
            }
        }
    }

    fn close_all(&mut self) {
        for send in [true, false] {
            let (ends, open) = self.side(send);
            for e in ends.iter_mut() {
                e.1 = false;
                *open -= 1;
            }
        }
    }
}

struct Chan {
    queue: Queue<Item>,
    ends: Ends,
    open: bool,
    /// `close(send=True)` on a non-empty channel: it closes for good once drained.
    closing: bool,
}

impl Chan {
    fn new() -> Chan {
        Chan { queue: Queue::new(), ends: Ends::default(), open: true, closing: false }
    }

    fn add(&mut self, interp: i64, data: Shared) -> Result<(), ChanErr> {
        if !self.open {
            return Err(ChanErr::Closed);
        }
        if !self.ends.associate(interp, true) {
            return Err(ChanErr::InterpClosed);
        }
        let _ = self.queue.push(Item { interp, data });
        Ok(())
    }

    fn next(&mut self, interp: i64) -> Result<Option<Item>, ChanErr> {
        if !self.open {
            return Err(ChanErr::Closed);
        }
        if !self.ends.associate(interp, false) {
            return Err(ChanErr::InterpClosed);
        }
        let item = match self.queue.pop() {
            Pop::Message(i) => Some(i),
            _ => None,
        };
        if item.is_none() && self.closing {
            self.open = false;
        }
        Ok(item)
    }

    fn drop_interpreter(&mut self, interp: i64) {
        self.queue.remove_where(|i| i.interp == interp);
        self.ends.drop_interpreter(interp);
        self.open = self.ends.is_open();
    }

    fn close_all(&mut self, force: bool) -> Result<(), ChanErr> {
        if !self.open {
            return Err(ChanErr::Closed);
        }
        if !force && !self.queue.is_empty() {
            return Err(ChanErr::NotEmpty);
        }
        self.open = false;
        self.ends.close_all();
        Ok(())
    }
}

struct Ref {
    id: i64,
    /// `None` once closed; the ref stays until no ID object uses it.
    chan: Option<Chan>,
    objcount: i64,
}

struct Channels {
    refs: Vec<Ref>,
    next_id: i64,
}

static CHANNELS: Mutex<Channels> = Mutex::new(Channels { refs: Vec::new(), next_id: 0 });

fn channels() -> MutexGuard<'static, Channels> {
    CHANNELS.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn drop_interpreter(interp: i64) {
    let mut all = channels();
    for r in all.refs.iter_mut() {
        if let Some(c) = r.chan.as_mut() {
            c.drop_interpreter(interp);
        }
    }
}

impl Channels {
    fn index(&self, id: i64) -> Option<usize> {
        self.refs.iter().position(|r| r.id == id)
    }

    fn open_chan(&mut self, id: i64) -> Result<&mut Chan, ChanErr> {
        let at = self.index(id).ok_or(ChanErr::NotFound)?;
        let chan = self.refs[at].chan.as_mut().ok_or(ChanErr::Closed)?;
        if !chan.open {
            return Err(ChanErr::Closed);
        }
        Ok(chan)
    }

    fn create(&mut self) -> Result<i64, ChanErr> {
        let id = self.next_id;
        if id < 0 {
            return Err(ChanErr::NoNextId);
        }
        self.next_id += 1;
        self.refs.push(Ref { id, chan: Some(Chan::new()), objcount: 0 });
        Ok(id)
    }

    fn destroy(&mut self, id: i64) -> Result<(), ChanErr> {
        let at = self.index(id).ok_or(ChanErr::NotFound)?;
        self.refs.remove(at);
        Ok(())
    }

    fn check_sendable(&mut self, id: i64) -> Result<(), ChanErr> {
        if self.open_chan(id)?.closing {
            return Err(ChanErr::Closed);
        }
        Ok(())
    }

    fn send(&mut self, id: i64, interp: i64, data: Shared) -> Result<(), ChanErr> {
        let chan = self.open_chan(id)?;
        if chan.closing {
            return Err(ChanErr::Closed);
        }
        chan.add(interp, data)
    }

    fn recv(&mut self, id: i64, interp: i64) -> Result<Option<Shared>, ChanErr> {
        let chan = self.open_chan(id)?;
        let result = chan.next(interp);
        let drained = chan.closing && chan.queue.is_empty();
        if drained {
            if let Some(at) = self.index(id) {
                self.refs[at].chan = None;
            }
        }
        result.map(|o| o.map(|i| i.data))
    }

    fn close(&mut self, id: i64, end: i32, force: bool) -> Result<(), ChanErr> {
        let at = self.index(id).ok_or(ChanErr::NotFound)?;
        let Some(chan) = self.refs[at].chan.as_mut() else { return Err(ChanErr::Closed) };
        if !force && end == SEND && chan.closing {
            return Err(ChanErr::Closed);
        }
        match chan.close_all(force) {
            Ok(()) => {
                self.refs[at].chan = None;
                Ok(())
            }
            Err(ChanErr::NotEmpty) if end == SEND => {
                if chan.closing {
                    return Err(ChanErr::Closed);
                }
                chan.closing = true;
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    fn release(&mut self, id: i64, interp: i64, which: i32) -> Result<(), ChanErr> {
        let chan = self.open_chan(id)?;
        chan.ends.close_interpreter(interp, which);
        chan.open = chan.ends.is_open();
        Ok(())
    }

    fn is_associated(&mut self, id: i64, interp: i64, send: bool) -> Result<bool, ChanErr> {
        let chan = self.open_chan(id)?;
        if send && chan.closing {
            return Err(ChanErr::Closed);
        }
        let ends = if send { &chan.ends.send } else { &chan.ends.recv };
        Ok(ends.iter().any(|e| e.0 == interp && e.1))
    }

    fn add_id_object(&mut self, id: i64) -> Result<(), ChanErr> {
        let at = self.index(id).ok_or(ChanErr::NotFound)?;
        self.refs[at].objcount += 1;
        Ok(())
    }

    fn drop_id_object(&mut self, id: i64) {
        let Some(at) = self.index(id) else { return };
        self.refs[at].objcount -= 1;
        if self.refs[at].objcount == 0 {
            self.refs.remove(at);
        }
    }

    /// The channel IDs, most recently created first.
    fn list(&self) -> Vec<i64> {
        self.refs.iter().rev().map(|r| r.id).collect()
    }
}

/// This module provides primitive operations to manage Python interpreters.
/// The 'interpreters' module provides a more convenient interface.
#[lumen_bind::module(name = "_xxinterpchannels")]
pub mod _xxinterpchannels {
    use super::*;
    use crate::ast::CmpOp;
    use crate::bind::{type_object, Py, This};
    use crate::builtins::native::{new_type, with_opaque};
    use crate::builtins::subinterpm::{interpreter_ids, new_interp_id};
    use crate::builtins::xid;
    use crate::object::*;
    use crate::vm::{dict_set_str, Interp};

    #[derive(Default)]
    struct State {
        errors: Vec<Obj>,
    }

    const ERROR: usize = 0;
    const NOT_FOUND: usize = 1;
    const CLOSED: usize = 2;
    const EMPTY: usize = 3;
    const NOT_EMPTY: usize = 4;

    fn chan_error(it: &mut Interp, e: ChanErr, cid: i64) -> Obj {
        let (kind, msg) = match e {
            ChanErr::NotFound => (NOT_FOUND, format!("channel {cid} not found")),
            ChanErr::Closed => (CLOSED, format!("channel {cid} is closed")),
            ChanErr::InterpClosed => (CLOSED, format!("channel {cid} is already closed")),
            ChanErr::Empty => (EMPTY, format!("channel {cid} is empty")),
            ChanErr::NotEmpty => (NOT_EMPTY, format!("channel {cid} may not be closed if not empty (try force=True)")),
            ChanErr::NoNextId => (ERROR, "failed to get a channel ID".to_string()),
        };
        let cls = it.native_state::<State>().errors.get(kind).cloned().unwrap_or_else(|| it.exc_type("RuntimeError"));
        it.new_exc(&cls, vec![Value::string(msg)])
    }

    /// A channel ID object; `force` allows an ID whose channel does not exist.
    fn new_id(it: &mut Interp, cid: i64, end: i32, force: bool, resolve: bool) -> R<Py<ChannelID>> {
        let added = channels().add_id_object(cid);
        match added {
            Ok(()) => {}
            Err(ChanErr::NotFound) if force => {}
            Err(e) => return Err(chan_error(it, e, cid)),
        }
        Ok(Py::new(it, ChannelID { id: cid, end, resolve }))
    }

    /// Rebuilds a channel ID that was shared from another interpreter.
    pub(crate) fn channel_id_from_shared(it: &mut Interp, id: i64, end: i32, resolve: bool) -> R<Value> {
        let cid = new_id(it, id, end, false, false)?;
        if end == BOTH || !resolve {
            return Ok(cid.into_value());
        }
        let name = if end == RECV { "RecvChannel" } else { "SendChannel" };
        for module in ["interpreters", "test.support.interpreters"] {
            let Ok(m) = it.import_module(module) else { continue };
            let Ok(cls) = it.get_attr_str(&Value::Obj(m), name) else { return Ok(cid.into_value()) };
            return Ok(it.call(&cls, vec![cid.value().clone()], Vec::new()).unwrap_or_else(|_| cid.value().clone()));
        }
        Ok(cid.into_value())
    }

    fn channel_id(it: &mut Interp, v: &Value) -> R<i64> {
        if let Some(id) = with_opaque::<ChannelID, _>(v, |c| c.id) {
            return Ok(id);
        }
        if it.has_index(v) {
            let n = it.index_of(v)?;
            if n < 0 {
                let r = it.repr_of(v)?;
                return Err(it.value_error(&format!("channel ID must be a non-negative int, got {r}")));
            }
            return Ok(n);
        }
        let t = it.tp_name_of(v);
        Err(it.type_error(&format!("channel ID must be an int, got {t}")))
    }

    /// A channel ID identifies a channel and may be used as an int.
    #[class(name = "ChannelID", module = "_xxinterpchannels")]
    pub struct ChannelID {
        pub(crate) id: i64,
        pub(crate) end: i32,
        pub(crate) resolve: bool,
    }

    impl Drop for ChannelID {
        fn drop(&mut self) {
            channels().drop_id_object(self.id);
        }
    }

    impl ChannelID {
        fn equals(&self, it: &mut Interp, other: &Value, op: CmpOp) -> R<Value> {
            let equal = if let Some((id, end)) = with_opaque::<ChannelID, _>(other, |c| (c.id, c.end)) {
                self.end == end && self.id == id
            } else {
                match other {
                    Value::Int(i) => *i >= 0 && *i == self.id,
                    Value::Bool(b) => i64::from(*b) == self.id,
                    Value::Obj(o) if matches!(o.kind, Kind::Int(_)) => false,
                    Value::Float(_) => return it.rich_compare(op, &Value::Int(self.id), other),
                    Value::Obj(o) if matches!(o.kind, Kind::Complex(..)) => return it.rich_compare(op, &Value::Int(self.id), other),
                    _ => return Ok(Value::NotImplemented),
                }
            };
            Ok(Value::Bool(equal == (op == CmpOp::Eq)))
        }
    }

    #[methods]
    impl ChannelID {
        #[proto(repr)]
        fn __repr__(&self) -> String {
            match self.end {
                SEND => format!("ChannelID({}, send=True)", self.id),
                RECV => format!("ChannelID({}, recv=True)", self.id),
                _ => format!("ChannelID({})", self.id),
            }
        }

        #[proto(str)]
        fn __str__(&self) -> String {
            self.id.to_string()
        }

        #[proto(hash)]
        fn __hash__(&self) -> i64 {
            self.id
        }

        #[proto(int)]
        fn __int__(&self) -> i64 {
            self.id
        }

        #[proto(index)]
        fn __index__(&self) -> i64 {
            self.id
        }

        #[proto(eq)]
        fn __eq__(&self, it: &mut Interp, other: &Value) -> R<Value> {
            self.equals(it, other, CmpOp::Eq)
        }

        #[proto(ne)]
        fn __ne__(&self, it: &mut Interp, other: &Value) -> R<Value> {
            self.equals(it, other, CmpOp::NotEq)
        }

        /// 'send', 'recv', or 'both'
        #[getter]
        fn end(&self) -> String {
            match self.end {
                SEND => "send",
                RECV => "recv",
                _ => "both",
            }
            .to_string()
        }

        /// the 'send' end of the channel
        #[getter]
        fn send(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
            let (id, resolve) = with_opaque::<ChannelID, _>(slf.0.value(), |c| (c.id, c.resolve)).unwrap_or((0, false));
            Ok(new_id(it, id, SEND, true, resolve)?.into_value())
        }

        /// the 'recv' end of the channel
        #[getter]
        fn recv(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
            let (id, resolve) = with_opaque::<ChannelID, _>(slf.0.value(), |c| (c.id, c.resolve)).unwrap_or((0, false));
            Ok(new_id(it, id, RECV, true, resolve)?.into_value())
        }
    }

    /// create() -> cid
    ///
    /// Create a new cross-interpreter channel and return a unique generated ID.
    #[op]
    fn create(it: &mut Interp) -> R<Value> {
        let made = channels().create();
        let cid = match made {
            Ok(c) => c,
            Err(e) => return Err(chan_error(it, e, -1)),
        };
        match new_id(it, cid, BOTH, false, false) {
            Ok(id) => Ok(id.into_value()),
            Err(e) => {
                let _ = channels().destroy(cid);
                Err(e)
            }
        }
    }

    /// destroy(cid)
    ///
    /// Close and finalize the channel.  Afterward attempts to use the channel
    /// will behave as though it never existed.
    #[op]
    fn destroy(it: &mut Interp, #[kw] cid: &Value) -> R<()> {
        let cid = channel_id(it, cid)?;
        let r = channels().destroy(cid);
        r.map_err(|e| chan_error(it, e, cid))
    }

    /// list_all() -> [cid]
    ///
    /// Return the list of all IDs for active channels.
    #[op]
    fn list_all(it: &mut Interp) -> R<Value> {
        let ids = channels().list();
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            out.push(new_id(it, id, BOTH, false, false)?.into_value());
        }
        Ok(Value::list(out))
    }

    /// list_interpreters(cid, *, send) -> [id]
    ///
    /// Return the list of all interpreter IDs associated with an end of the channel.
    ///
    /// The 'send' argument should be a boolean indicating whether to use the send or
    /// receive end.
    #[op]
    fn list_interpreters(it: &mut Interp, #[kw] cid: &Value, #[kwonly] send: bool) -> R<Value> {
        let cid = channel_id(it, cid)?;
        let mut found = Vec::new();
        for id in interpreter_ids() {
            let r = channels().is_associated(cid, id, send);
            match r {
                Ok(true) => found.push(id),
                Ok(false) => {}
                Err(e) => return Err(chan_error(it, e, cid)),
            }
        }
        let mut out = Vec::with_capacity(found.len());
        for id in found.into_iter().rev() {
            out.push(new_interp_id(it, id)?);
        }
        Ok(Value::list(out))
    }

    /// send(cid, obj)
    ///
    /// Add the object's data to the channel's queue.
    #[op]
    fn send(it: &mut Interp, #[kw] cid: &Value, #[kw] obj: &Value) -> R<()> {
        let cid = channel_id(it, cid)?;
        let checked = channels().check_sendable(cid);
        checked.map_err(|e| chan_error(it, e, cid))?;
        let data = xid::share(it, obj)?;
        let sent = channels().send(cid, current_interp_id(), data);
        sent.map_err(|e| chan_error(it, e, cid))
    }

    /// recv(cid, [default]) -> obj
    ///
    /// Return a new object from the data at the front of the channel's queue.
    ///
    /// If there is nothing to receive then raise ChannelEmptyError, unless
    /// a default value is provided.  In that case return it.
    #[op]
    fn recv(it: &mut Interp, #[kw] cid: &Value, #[kw] dflt: Option<&Value>) -> R<Value> {
        let cid = channel_id(it, cid)?;
        let got = channels().recv(cid, current_interp_id());
        match got.map_err(|e| chan_error(it, e, cid))? {
            Some(data) => xid::unshare(it, &data),
            None => match dflt {
                Some(d) => Ok(d.clone()),
                None => Err(chan_error(it, ChanErr::Empty, cid)),
            },
        }
    }

    /// close(cid, *, send=None, recv=None, force=False)
    ///
    /// Close the channel for all interpreters.
    ///
    /// If the channel is empty then the keyword args are ignored and both
    /// ends are immediately closed.  Otherwise, if 'force' is True then
    /// all queued items are released and both ends are immediately
    /// closed.
    ///
    /// If the channel is not empty *and* 'force' is False then following
    /// happens:
    ///
    ///  * recv is True (regardless of send):
    ///    - raise ChannelNotEmptyError
    ///  * recv is None and send is None:
    ///    - raise ChannelNotEmptyError
    ///  * send is True and recv is not True:
    ///    - fully close the 'send' end
    ///    - close the 'recv' end to interpreters not already receiving
    ///    - fully close it once empty
    ///
    /// Closing an already closed channel results in a ChannelClosedError.
    ///
    /// Once the channel's ID has no more ref counts in any interpreter
    /// the channel will be destroyed.
    #[op]
    fn close(
        it: &mut Interp,
        #[kw] cid: &Value,
        #[kwonly] #[default(false)] send: bool,
        #[kwonly] #[default(false)] recv: bool,
        #[kwonly] #[default(false)] force: bool,
    ) -> R<()> {
        let cid = channel_id(it, cid)?;
        let r = channels().close(cid, i32::from(send) - i32::from(recv), force);
        r.map_err(|e| chan_error(it, e, cid))
    }

    /// release(cid, *, send=None, recv=None, force=True)
    ///
    /// Close the channel for the current interpreter.  'send' and 'recv'
    /// (bool) may be used to indicate the ends to close.  By default both
    /// ends are closed.  Closing an already closed end is a noop.
    #[op]
    fn release(
        it: &mut Interp,
        #[kw] cid: &Value,
        #[kwonly] #[default(false)] send: bool,
        #[kwonly] #[default(false)] recv: bool,
        #[kwonly] #[default(false)] force: bool,
    ) -> R<()> {
        let _ = force;
        let cid = channel_id(it, cid)?;
        let (send, recv) = if !send && !recv { (true, true) } else { (send, recv) };
        let r = channels().release(cid, current_interp_id(), i32::from(send) - i32::from(recv));
        r.map_err(|e| chan_error(it, e, cid))
    }

    fn flag(it: &mut Interp, v: Option<&Value>) -> R<i32> {
        match v {
            None => Ok(-1),
            Some(v) => Ok(i32::from(it.truthy(v)?)),
        }
    }

    #[op(name = "_channel_id")]
    fn channel_id_op(
        it: &mut Interp,
        #[kw] id: &Value,
        #[kwonly] send: Option<&Value>,
        #[kwonly] recv: Option<&Value>,
        #[kwonly] force: Option<&Value>,
        #[kwonly] _resolve: Option<&Value>,
    ) -> R<Value> {
        let cid = channel_id(it, id)?;
        let send = flag(it, send)?;
        let recv = flag(it, recv)?;
        let force = flag(it, force)? == 1;
        let resolve = flag(it, _resolve)? == 1;
        if send == 0 && recv == 0 {
            return Err(it.value_error("'send' and 'recv' cannot both be False"));
        }
        let mut end = BOTH;
        if send == 1 {
            if recv == 0 || recv == -1 {
                end = SEND;
            }
        } else if recv == 1 {
            end = RECV;
        }
        Ok(new_id(it, cid, end, force, resolve)?.into_value())
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        let runtime = it.exc_type("RuntimeError");
        let base = new_type(it, "_xxinterpchannels", "ChannelError", Some(&runtime), Layout::Exception);
        let mut errors = vec![base.clone()];
        dict_set_str(&d, "ChannelError", Value::Obj(base.clone()));
        for name in ["ChannelNotFoundError", "ChannelClosedError", "ChannelEmptyError", "ChannelNotEmptyError"] {
            let t = new_type(it, "_xxinterpchannels", name, Some(&base), Layout::Exception);
            dict_set_str(&d, name, Value::Obj(t.clone()));
            errors.push(t);
        }
        it.native_state::<State>().errors = errors;
        let id = type_object::<ChannelID>(it);
        dict_set_str(&d, "ChannelID", Value::Obj(id));
    }
}
