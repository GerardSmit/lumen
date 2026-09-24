//! An example lumen extension written the way a third-party crate would: plain Rust fns and
//! structs, bound with `#[lumen::op]`, `#[lumen::class]` and `#[lumen::methods]`.

use lumen::embed::{
    BigI64, BigU64, Ctx, Deferred, JsArrayBuffer, JsFunction, JsObject, OpError, Promise, SendError,
    State, This, Value,
};
use lumen::Engine;
use std::borrow::Cow;
use std::cell::Cell;

// ---- ops: numbers -----------------------------------------------------------------------------

/// Strict f64 arguments; `fast` also emits an unboxed `extern "C"` entry for the JIT.
#[lumen::op(fast)]
pub fn clamp(x: f64, lo: f64, hi: f64) -> f64 {
    x.max(lo).min(hi)
}

/// ToInt32/ToUint32 wrapping, 64-bit safe integers, BigInt in/out; tuples become arrays.
#[lumen::op]
pub fn ints(a: i32, b: u32, c: i64, d: BigU64) -> (i32, u32, i64, BigU64) {
    (a, b, c, BigU64(d.0.wrapping_add(1)))
}

#[lumen::op]
pub fn big(x: BigI64) -> BigI64 {
    BigI64(x.0.wrapping_mul(2))
}

/// Returns an i64 that does not fit a Number -> RangeError.
#[lumen::op]
pub fn too_big() -> i64 {
    i64::MAX
}

/// `#[op(coerce)]`: JS ToNumber / ToString / ToBoolean instead of type errors.
#[lumen::op(coerce)]
pub fn coerced(x: f64, s: String, b: bool) -> String {
    format!("{x}|{s}|{b}")
}

// ---- ops: strings -----------------------------------------------------------------------------

/// `&str` borrows the JS string; the trailing `Option` is optional (so `length` is 1).
#[lumen::op(name = "greet")]
pub fn greet_op(name: &str, greeting: Option<String>) -> String {
    format!("{}, {name}!", greeting.as_deref().unwrap_or("Hello"))
}

#[lumen::op]
pub fn shout(s: Cow<str>) -> Cow<'static, str> {
    Cow::Owned(s.to_uppercase())
}

// ---- ops: bytes -------------------------------------------------------------------------------

/// Zero-copy: `bytes` points into the ArrayBuffer backing store.
#[lumen::op]
pub fn sum(bytes: &[u8]) -> f64 {
    bytes.iter().map(|&b| b as u64).sum::<u64>() as f64
}

#[lumen::op]
pub fn blen(bytes: &[u8]) -> u32 {
    bytes.len() as u32
}

/// The `aes_ecb` shape: borrowed inputs, owned output moved into a Uint8Array (no copy).
#[lumen::op]
pub fn xor_bytes(encrypt: bool, key: &[u8], data: &[u8]) -> Result<Vec<u8>, OpError> {
    if key.is_empty() {
        return Err(OpError::range_error("key must not be empty").with_code("ERR_CRYPTO_INVALID_KEYLEN"));
    }
    let _ = encrypt;
    Ok(data.iter().zip(key.iter().cycle()).map(|(d, k)| d ^ k).collect())
}

/// `&mut [u8]` writes land in the JS buffer; overlapping `dst`/`src` is a TypeError.
#[lumen::op]
pub fn copy_into(dst: &mut [u8], src: &[u8]) -> u32 {
    let n = dst.len().min(src.len());
    dst[..n].copy_from_slice(&src[..n]);
    n as u32
}

/// `&mut Ctx` + a borrowed buffer: the buffer is lent for the call, so JS run by `cb` sees it
/// detached (length 0) instead of pulling the bytes out from under the slice.
#[lumen::op]
pub fn fill_with_callback(ctx: &mut Ctx, data: &mut [u8], cb: JsFunction) -> Result<f64, OpError> {
    let seen = cb.call(ctx, Value::Undefined, &[])?;
    data.fill(7);
    Ok(match seen {
        Value::Num(n) => n,
        _ => -1.0,
    })
}

#[lumen::op]
pub fn owned_copy(bytes: Vec<u8>) -> usize {
    bytes.len()
}

#[lumen::op]
pub fn make_buffer(n: u32) -> JsArrayBuffer {
    JsArrayBuffer(vec![1; n as usize])
}

// ---- ops: objects, functions, arrays, this, state -----------------------------------------------

#[lumen::op]
pub fn call_twice(ctx: &mut Ctx, f: JsFunction, x: f64) -> Result<Value, OpError> {
    let once = f.call(ctx, Value::Undefined, &[Value::Num(x)])?;
    Ok(f.call(ctx, Value::Undefined, &[once])?)
}

#[lumen::op]
pub fn get_field(ctx: &mut Ctx, obj: JsObject, key: &str) -> Result<Value, OpError> {
    obj.get(ctx, key)
}

#[lumen::op]
pub fn vec_sum(xs: Vec<f64>) -> f64 {
    xs.iter().sum()
}

#[lumen::op]
pub fn range(n: u32) -> Vec<u32> {
    (0..n).collect()
}

#[lumen::op]
pub fn words(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_owned).collect()
}

#[lumen::op]
pub fn this_is_object(this: This<Option<JsObject>>) -> bool {
    this.0.is_some()
}

#[lumen::op]
pub fn identity(v: Value) -> Value {
    v
}

#[derive(Default)]
pub struct Counter {
    pub total: u32,
}

#[lumen::op]
pub fn counter_add(st: &mut State<Counter>, n: u32) -> u32 {
    st.total += n;
    st.total
}

#[lumen::op]
pub fn maybe(n: Option<f64>) -> Option<f64> {
    n.map(|n| n * 2.0)
}

#[lumen::op]
pub fn parse_int(s: &str) -> Result<i32, OpError> {
    Ok(s.trim().parse::<i32>()?)
}

// ---- promises -----------------------------------------------------------------------------------

/// Ops the host settles later: the demo's "event loop" drains this queue.
#[derive(Default)]
pub struct PendingQueue(pub Vec<(Deferred, f64)>);

#[lumen::op(name = "delayedDouble")]
pub fn delayed_double(ctx: &mut Ctx, v: f64) -> Promise<f64> {
    let d = Deferred::new(ctx);
    let p = Promise::pending(&d);
    ctx.op_state().get_mut::<PendingQueue>().unwrap().0.push((d, v));
    p
}

/// Host side of the event loop: settle everything queued.
pub fn settle_pending(engine: &mut Engine) -> usize {
    let ctx = engine.ctx();
    let items = std::mem::take(&mut ctx.op_state().get_mut::<PendingQueue>().unwrap().0);
    let n = items.len();
    for (d, v) in items {
        if v < 0.0 {
            d.reject(ctx, OpError::range_error("negative"));
        } else {
            d.resolve(ctx, v * 2.0);
        }
    }
    engine.run_microtasks();
    n
}

// ---- classes: a fake fetch ----------------------------------------------------------------------

#[lumen::class]
pub struct Headers {
    entries: Vec<(String, String)>,
}

#[lumen::methods]
impl Headers {
    /// No `#[constructor]`: `new Headers()` throws "Illegal constructor".
    fn get(&self, name: &str) -> Option<String> {
        self.entries
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    }

    fn has(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    #[getter]
    fn size(&self) -> u32 {
        self.entries.len() as u32
    }
}

thread_local! {
    pub static RESPONSES_DROPPED: Cell<usize> = const { Cell::new(0) };
}

#[lumen::class]
pub struct Response {
    status: u16,
    body: Vec<u8>,
    headers: Vec<(String, String)>,
}

impl Drop for Response {
    fn drop(&mut self) {
        RESPONSES_DROPPED.with(|c| c.set(c.get() + 1));
    }
}

#[lumen::methods]
impl Response {
    #[constructor]
    fn new(body: Option<String>, status: Option<u16>) -> Result<Self, OpError> {
        let status = status.unwrap_or(200);
        if !(200..=599).contains(&status) {
            return Err(OpError::range_error(format!("status {status} out of range")));
        }
        Ok(Response {
            status,
            body: body.unwrap_or_default().into_bytes(),
            headers: vec![("content-type".into(), "text/plain".into())],
        })
    }

    #[getter]
    fn status(&self) -> u16 {
        self.status
    }

    #[setter]
    fn set_status(&mut self, status: u16) {
        self.status = status;
    }

    #[getter]
    fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    #[getter]
    fn headers(&self) -> Headers {
        Headers {
            entries: self.headers.clone(),
        }
    }

    fn text(&self) -> Promise<String> {
        Promise::ready(String::from_utf8(self.body.clone()))
    }

    fn json(&self, ctx: &mut Ctx) -> Promise<Value> {
        let text = String::from_utf8_lossy(&self.body).into_owned();
        Promise::ready(ctx.json_parse(&text))
    }

    #[method(name = "arrayBuffer")]
    fn array_buffer(&self) -> Promise<JsArrayBuffer> {
        Promise::resolved(JsArrayBuffer(self.body.clone()))
    }

    /// Synchronous body access: a Uint8Array adopting a fresh Vec.
    fn bytes_sync(&self) -> Vec<u8> {
        self.body.clone()
    }

    /// `&mut self` + a JS callback: a callback that touches this same instance hits the
    /// RefCell guard and throws instead of aliasing.
    fn update(&mut self, ctx: &mut Ctx, f: JsFunction) -> Result<u16, OpError> {
        let v = f.call(ctx, Value::Undefined, &[Value::Num(self.status as f64)])?;
        if let Value::Num(n) = v {
            self.status = n as u16;
        }
        Ok(self.status)
    }

    /// Static: `Response.error()`.
    fn error() -> Response {
        Response {
            status: 500,
            body: Vec::new(),
            headers: Vec::new(),
        }
    }

    #[skip]
    #[allow(dead_code)]
    fn internal_helper(&self) -> usize {
        self.body.len()
    }
}

/// `fetch(url)` against an in-memory table. `async` runs the body on the host's worker pool (a
/// runtime with an event loop, e.g. lumen-runtime) and settles the returned promise on the JS
/// thread when it finishes; a bare `Engine` without an async host runs it inline. The arguments
/// and result cross threads, hence owned `String` in and `SendError` (not `OpError`) out.
#[lumen::op(async)]
pub fn fetch(url: String) -> Result<Response, SendError> {
    let (status, body) = match url.as_str() {
        "https://example.test/data.json" => (200, r#"{"answer":42,"list":[1,2,3]}"#),
        "https://example.test/hello" => (200, "hello world"),
        "https://example.test/missing" => (404, "not found"),
        _ => return Err(SendError::new("TypeError", format!("fetch failed: unknown host in {url}"))),
    };
    Ok(Response {
        status,
        body: body.as_bytes().to_vec(),
        headers: vec![
            ("content-type".into(), "application/json".into()),
            ("x-demo".into(), "1".into()),
        ],
    })
}

/// A class instance as an op argument.
#[lumen::op]
pub fn response_status(r: &Response) -> u16 {
    r.status
}

/// Install everything on an engine.
pub fn install(engine: &mut Engine) {
    engine.ctx().op_state().put(Counter::default());
    engine.ctx().op_state().put(PendingQueue::default());
    engine.define_ops(
        "ext",
        lumen::ops![
            clamp, ints, big, too_big, coerced, greet_op, shout, sum, blen, xor_bytes, copy_into,
            fill_with_callback, owned_copy, make_buffer, call_twice, get_field, vec_sum, range,
            words, this_is_object, identity, counter_add, maybe, parse_int, delayed_double,
            response_status,
        ],
    );
    engine.define_op(&fetch::DESC);
    engine.define_class::<Response>();
    engine.define_class::<Headers>();
}
