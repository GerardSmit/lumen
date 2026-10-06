//! The native `WebAssembly` namespace: `Module`, `Instance`, `Memory`, `Table`, `Global`, the three
//! error classes and `validate` / `compile` / `instantiate` (`docs/native-wasm.md`). All wasm
//! entities live in one shared [`Store`] in OpState; `Memory`, `Table` and `Global` wrappers carry
//! integer *store addresses*, so entities can be imported and shared across instances
//! (cross-module linking). Imported JS functions are called back through [`CtxHost`].

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::MutexGuard;

use lumen::embed::{JsHost, OpResult, SharedBufferHandle, WeakValue};
use lumen_bind::Methods;
use lumen_host::{Ctx, OpError, Value};

use crate::wasm;
use crate::wasm::exec::{Host, Imports, MemEntity, Store, Val};
use crate::wasm::parse::{Module as Parsed, ValType};

pub(crate) struct WasmStore {
    store: Store,
    /// Imported JS callbacks, indexed by the host id stored in `FuncEntity::Host`.
    host_funcs: Vec<Value>,
    /// Memories JS has seen a `buffer` for: each buffer views the memory's bytes in place.
    bufs: Vec<MemBuf>,
    /// While wasm runs (and the store is moved out), the memories an import may grow.
    active_mems: *mut [MemEntity],
    /// The live JS wrapper of each store entity, so one entity has one object identity.
    objects: HashMap<(Kind, usize), WeakValue>,
    sweep_at: usize,
}

impl Default for WasmStore {
    fn default() -> WasmStore {
        WasmStore {
            store: Store::default(),
            host_funcs: Vec::new(),
            bufs: Vec::new(),
            active_mems: std::ptr::slice_from_raw_parts_mut(std::ptr::null_mut(), 0),
            objects: HashMap::new(),
            sweep_at: 256,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Kind {
    Func,
    Memory,
    Table,
    Global,
}

#[derive(Clone)]
struct MemBuf {
    addr: usize,
    buf: Value,
    shared: bool,
    generation: u64,
}

fn view(ctx: &mut Ctx, m: &MemEntity) -> Value {
    // SAFETY: the view is kept alive by the region's owner and never outlives its length:
    // memories only grow, and a grown memory gets a new view.
    unsafe { ctx.make_array_buffer_external(m.bytes.base(), m.bytes.len(), m.bytes.owner()) }
}

/// Replace the buffers of memories whose length changed (after wasm ran or grew them).
fn refresh(ctx: &mut Ctx, mems: &[MemEntity]) {
    let bufs = ctx
        .host_mut::<WasmStore>()
        .expect("wasm store")
        .bufs
        .clone();
    for (i, b) in bufs.into_iter().enumerate() {
        let Some(m) = mems.get(b.addr) else { continue };
        if b.shared && m.generation != b.generation {
            let handle = m
                .bytes
                .shared_handle()
                .expect("shared memory backing")
                .current_view()
                .expect("shared memory view");
            let buf = ctx.import_shared_array_buffer(&handle);
            let ws = ctx.host_mut::<WasmStore>().expect("wasm store");
            ws.bufs[i] = MemBuf {
                addr: b.addr,
                buf,
                shared: true,
                generation: m.generation,
            };
        } else if m.generation != b.generation {
            ctx.array_buffer_detach(&b.buf);
            let buf = view(ctx, m);
            let ws = ctx.host_mut::<WasmStore>().expect("wasm store");
            ws.bufs[i] = MemBuf {
                addr: b.addr,
                buf,
                shared: false,
                generation: m.generation,
            };
        }
    }
}

/// Run `f` on the store (moved out of OpState, so imports can re-enter JS), then refresh the
/// memory buffers.
struct SharedMemLock {
    // Drop the guard before its handle so the Arc-backed mutex outlives the guard.
    guard: Option<MutexGuard<'static, Vec<u8>>>,
    handle: SharedBufferHandle,
}

impl SharedMemLock {
    fn new(handle: SharedBufferHandle) -> SharedMemLock {
        let mut lock = SharedMemLock {
            guard: None,
            handle,
        };
        lock.resume();
        lock
    }

    fn suspend(&mut self) {
        self.guard.take();
    }

    fn resume(&mut self) {
        if self.guard.is_none() {
            let guard = self.handle.lock().expect("shared wasm memory poisoned");
            // SAFETY: `handle` is stored beside the guard and is dropped after it. The backing
            // Arc therefore outlives the guard for the entire lease.
            self.guard = Some(unsafe {
                std::mem::transmute::<MutexGuard<'_, Vec<u8>>, MutexGuard<'static, Vec<u8>>>(guard)
            });
        }
    }
}

struct SharedAccess {
    locks: Vec<SharedMemLock>,
}

impl SharedAccess {
    fn new(store: &Store) -> SharedAccess {
        let mut handles: Vec<SharedBufferHandle> = store
            .memories
            .iter()
            .filter_map(|memory| memory.bytes.shared_handle())
            .collect();
        handles.sort_by_key(SharedBufferHandle::lock_order_key);
        handles.dedup_by(|right, left| right.same_backing(left));
        SharedAccess {
            locks: handles.into_iter().map(SharedMemLock::new).collect(),
        }
    }

    fn suspend(&mut self) {
        for lock in self.locks.iter_mut().rev() {
            lock.suspend();
        }
    }

    fn resume(&mut self) {
        for lock in &mut self.locks {
            lock.resume();
        }
    }
}

fn with_store<R>(ctx: &mut Ctx, f: impl FnOnce(&mut Ctx, &mut Store, &mut SharedAccess) -> R) -> R {
    let mut store = std::mem::take(&mut ctx.host_mut::<WasmStore>().expect("wasm store").store);
    let mut shared = SharedAccess::new(&store);
    let r = f(ctx, &mut store, &mut shared);
    drop(shared);
    refresh(ctx, &store.memories);
    ctx.host_mut::<WasmStore>().expect("wasm store").store = store;
    r
}

// ---- value conversion -------------------------------------------------------------------------

fn val_to_js(v: Val) -> Value {
    match v {
        Val::I32(x) => Value::Num(x as f64),
        Val::I64(x) => Value::bigint_from_i64(x),
        Val::F32(x) => Value::Num(x as f64),
        Val::F64(x) => Value::Num(x),
        Val::Ref(_) => Value::Null,
    }
}

fn js_to_val(ctx: &mut Ctx, v: &Value, ty: ValType) -> Val {
    match ty {
        ValType::I32 => Val::I32(ctx.coerce_number(v).unwrap_or(0.0) as i64 as i32),
        ValType::I64 => Val::I64(
            v.bigint_as_i64()
                .unwrap_or_else(|| ctx.coerce_number(v).unwrap_or(0.0) as i64),
        ),
        ValType::F32 => Val::F32(ctx.coerce_number(v).unwrap_or(0.0) as f32),
        ValType::F64 => Val::F64(ctx.coerce_number(v).unwrap_or(0.0)),
        ValType::FuncRef | ValType::ExternRef => Val::Ref(None),
    }
}

// ---- host bridge ------------------------------------------------------------------------------

struct CtxHost<'a> {
    ctx: &'a mut Ctx,
    host_funcs: &'a [Value],
    shared: &'a mut SharedAccess,
    error: Option<Value>,
}

impl Host for CtxHost<'_> {
    fn call_host(
        &mut self,
        id: usize,
        args: &[Val],
        results: &[ValType],
        mems: &mut [MemEntity],
    ) -> Result<Vec<Val>, String> {
        self.shared.suspend();
        refresh(self.ctx, mems);
        let ws = self.ctx.host_mut::<WasmStore>().expect("wasm store");
        let prev = std::mem::replace(&mut ws.active_mems, mems);
        let r = self.call_js(id, args, results);
        self.ctx
            .host_mut::<WasmStore>()
            .expect("wasm store")
            .active_mems = prev;
        self.shared.resume();
        r
    }

    fn before_memory_grow(&mut self) {
        self.shared.suspend();
    }

    fn after_memory_grow(&mut self) {
        self.shared.resume();
    }
}

impl CtxHost<'_> {
    fn call_js(
        &mut self,
        id: usize,
        args: &[Val],
        results: &[ValType],
    ) -> Result<Vec<Val>, String> {
        let callback = self
            .host_funcs
            .get(id)
            .cloned()
            .ok_or("wasm: bad import id")?;
        let js_args: Vec<Value> = args.iter().map(|v| val_to_js(*v)).collect();
        let ret = match self.ctx.invoke(callback, Value::Undefined, &js_args) {
            Ok(v) => v,
            Err(e) => {
                self.error = Some(e);
                return Err("wasm: imported function threw".into());
            }
        };
        match results.len() {
            0 => Ok(vec![]),
            1 => Ok(vec![js_to_val(self.ctx, &ret, results[0])]),
            _ => {
                let mut out = Vec::with_capacity(results.len());
                for (i, &ty) in results.iter().enumerate() {
                    let el = self
                        .ctx
                        .get_member(&ret, &i.to_string())
                        .unwrap_or(Value::Undefined);
                    out.push(js_to_val(self.ctx, &el, ty));
                }
                Ok(out)
            }
        }
    }
}

/// Run store function `func_addr`, moving the store (and host callbacks) out of OpState so the
/// interpreter can borrow them mutably while `CtxHost` re-enters JS for imports; then move back.
fn run_func(ctx: &mut Ctx, func_addr: usize, args: Vec<Val>) -> Result<Vec<Val>, Value> {
    let ws = ctx.host_mut::<WasmStore>().expect("wasm store");
    let host_funcs = std::mem::take(&mut ws.host_funcs);
    let (result, host_err) = with_store(ctx, |ctx, store, shared| {
        let mut host = CtxHost {
            ctx,
            host_funcs: &host_funcs,
            shared,
            error: None,
        };
        let r = store.invoke(func_addr, args, &mut host, 0);
        (r, host.error)
    });
    ctx.host_mut::<WasmStore>().expect("wasm store").host_funcs = host_funcs;
    result.map_err(|msg| host_err.unwrap_or_else(|| error_value(ctx, api::RuntimeError, &msg)))
}


// ---- JS API support ---------------------------------------------------------------------------

const FUNC_SLOT: &str = "#\u{0}wasm_func_addr";
const EXPORTS_SLOT: &str = "#\u{0}wasm_exports";
const MAX_PAGES: f64 = 65536.0;
const SHARED_MEMORY_LIMIT: f64 = 256.0 * 1024.0 * 1024.0;

fn thrown(value: Value) -> OpError {
    OpError::thrown(value)
}

pub(crate) fn define_data(
    ctx: &mut Ctx,
    target: &Value,
    key: &str,
    value: Value,
    writable: bool,
    enumerable: bool,
    configurable: bool,
) -> Result<(), Value> {
    let descriptor = ctx.new_object_with_proto(&Value::Null);
    for (name, value) in [
        ("value", value),
        ("writable", Value::Bool(writable)),
        ("enumerable", Value::Bool(enumerable)),
        ("configurable", Value::Bool(configurable)),
    ] {
        ctx.member_set(&descriptor, name, value)?;
    }
    ctx.define_property_value(target, Value::str(key), &descriptor)
}

/// A new instance of the error class `T` whose own `message` is `message` (omitted when empty,
/// as `new Error()` omits it).
fn error_value<T: Methods<JsHost>>(ctx: &mut Ctx, class: T, message: &str) -> Value {
    let error = ctx.new_instance(class);
    if !message.is_empty() {
        let message = Value::from_string(message.to_string());
        if let Err(failure) = define_data(ctx, &error, "message", message, true, false, true) {
            return failure;
        }
    }
    error
}

fn compile_error(ctx: &mut Ctx, message: &str) -> OpError {
    thrown(error_value(ctx, api::CompileError, message))
}

fn link_error(ctx: &mut Ctx, message: &str) -> OpError {
    thrown(error_value(ctx, api::LinkError, message))
}

fn type_error(message: impl Into<std::borrow::Cow<'static, str>>) -> OpError {
    OpError::type_error(message)
}

fn field(ctx: &mut Ctx, object: &Value, name: &str) -> OpResult<Value> {
    ctx.member_get(object, name).map_err(thrown)
}

fn number(ctx: &mut Ctx, value: &Value) -> OpResult<f64> {
    ctx.coerce_number(value).map_err(thrown)
}

/// An enforced unsigned 32-bit integer: `ToNumber` (so BigInt throws), then truncation.
fn u32_value(ctx: &mut Ctx, value: &Value, what: &str) -> OpResult<f64> {
    let n = number(ctx, value)?;
    let integer = n.trunc();
    if !n.is_finite() || integer < 0.0 || integer > u32::MAX as f64 {
        return Err(type_error(format!(
            "{what} is outside the unsigned 32-bit range"
        )));
    }
    Ok(integer)
}

fn descriptor_object(value: &Value, what: &str) -> OpResult<()> {
    match value {
        Value::Obj(_) => Ok(()),
        _ => Err(type_error(format!("{what}: Argument 0 must be a descriptor object"))),
    }
}

fn val_type(ctx: &mut Ctx, value: &Value) -> OpResult<ValType> {
    if matches!(value, Value::Undefined) {
        return Ok(ValType::I32);
    }
    match &*ctx.coerce_string(value).map_err(thrown)? {
        "i32" => Ok(ValType::I32),
        "i64" => Ok(ValType::I64),
        "f32" => Ok(ValType::F32),
        "f64" => Ok(ValType::F64),
        _ => Err(type_error(
            "WebAssembly.Global(): Descriptor property 'value' must be a WebAssembly type",
        )),
    }
}

fn weak_lookup(ctx: &mut Ctx, kind: Kind, addr: usize) -> Option<Value> {
    ctx.host_mut::<WasmStore>()?
        .objects
        .get(&(kind, addr))
        .and_then(WeakValue::upgrade)
}

fn remember(ctx: &mut Ctx, kind: Kind, addr: usize, value: &Value) {
    let Some(weak) = ctx.weak_value(value) else {
        return;
    };
    let ws = ctx.host_mut::<WasmStore>().expect("wasm store");
    if ws.objects.len() >= ws.sweep_at {
        ws.objects.retain(|_, weak| weak.upgrade().is_some());
        ws.sweep_at = (ws.objects.len() * 2).max(256);
    }
    ws.objects.insert((kind, addr), weak);
}

/// The wrapper of store entity `(kind, addr)`, created by `make` when none is alive.
fn entity_object<T: Methods<JsHost>>(
    ctx: &mut Ctx,
    kind: Kind,
    addr: usize,
    make: impl FnOnce(usize) -> T,
) -> Value {
    if let Some(existing) = weak_lookup(ctx, kind, addr) {
        return existing;
    }
    let object = ctx.new_instance(make(addr));
    remember(ctx, kind, addr, &object);
    object
}

fn addr_of<T: lumen_bind::Class>(ctx: &Ctx, value: &Value, read: fn(&T) -> usize) -> Option<usize> {
    ctx.instance_data::<T>(value).map(|data| read(&data.borrow()))
}

/// The store function `func_addr` as a native JS function: arguments convert straight from the
/// call, a single result comes back as a value (none: `undefined`, several: an array), and traps
/// throw `WebAssembly.RuntimeError`. One function object per address while it is alive.
fn func_object(ctx: &mut Ctx, func_addr: usize) -> OpResult<Value> {
    if let Some(existing) = weak_lookup(ctx, Kind::Func, func_addr) {
        return Ok(existing);
    }
    let ty = {
        let ws = ctx.host_mut::<WasmStore>().expect("wasm store");
        match ws.store.funcs.get(func_addr) {
            Some(f) => f.ty().clone(),
            None => return Err(OpError::error("wasm: bad function address")),
        }
    };
    let arity = ty.params.len();
    let f = move |ctx: &mut Ctx, _this: Value, args: &[Value]| -> Result<Value, Value> {
        let vals = ty
            .params
            .iter()
            .enumerate()
            .map(|(i, &t)| js_to_val(ctx, args.get(i).unwrap_or(&Value::Undefined), t))
            .collect();
        let results = run_func(ctx, func_addr, vals)?;
        Ok(match results.len() {
            0 => Value::Undefined,
            1 => val_to_js(results[0]),
            _ => {
                let js = results.into_iter().map(val_to_js).collect();
                ctx.make_array(js)
            }
        })
    };
    let function = ctx.new_native_fn(&func_addr.to_string(), arity, Rc::new(f));
    ctx.define_native_private_value_slot(&function, FUNC_SLOT, Value::Num(func_addr as f64))
        .map_err(thrown)?;
    remember(ctx, Kind::Func, func_addr, &function);
    Ok(function)
}

fn func_addr_of(ctx: &Ctx, value: &Value) -> Option<usize> {
    ctx.native_private_value_slot(value, FUNC_SLOT)?
        .as_num_opt()
        .map(|n| n as usize)
}

fn memory_object(ctx: &mut Ctx, addr: usize) -> Value {
    entity_object(ctx, Kind::Memory, addr, |addr| api::MemoryObject { addr })
}

fn table_object(ctx: &mut Ctx, addr: usize) -> Value {
    entity_object(ctx, Kind::Table, addr, |addr| api::TableObject { addr })
}

fn global_object(ctx: &mut Ctx, addr: usize) -> Value {
    entity_object(ctx, Kind::Global, addr, |addr| api::GlobalObject { addr })
}

/// What a constructor of an entity wrapper returns: the instance, remembered as the wrapper of
/// its store address.
struct Tracked<T> {
    value: T,
    kind: Kind,
    addr: usize,
}

impl<T: lumen_bind::Class> lumen_bind::CtorRet<JsHost, T> for Tracked<T> {
    fn into_ctor(self, cx: &<JsHost as lumen_bind::Host>::Cx<'_>) -> Result<Value, Value> {
        let instance = <JsHost as lumen_bind::Host>::construct(cx, self.value)?;
        <JsHost as lumen_bind::Host>::with_ctx(cx, |ctx: &mut Ctx| {
            remember(ctx, self.kind, self.addr, &instance)
        });
        Ok(instance)
    }
}

/// What an error class constructor returns: the instance with its own `message`, when given.
struct ErrorInit<T> {
    value: T,
    message: Option<String>,
}

impl<T: lumen_bind::Class> lumen_bind::CtorRet<JsHost, T> for ErrorInit<T> {
    fn into_ctor(self, cx: &<JsHost as lumen_bind::Host>::Cx<'_>) -> Result<Value, Value> {
        let instance = <JsHost as lumen_bind::Host>::construct(cx, self.value)?;
        if let Some(message) = self.message {
            <JsHost as lumen_bind::Host>::with_ctx(cx, |ctx: &mut Ctx| {
                define_data(ctx, &instance, "message", Value::from_string(message), true, false, true)
            })?;
        }
        Ok(instance)
    }
}

fn error_init<T>(ctx: &mut Ctx, value: T, message: &Value) -> OpResult<ErrorInit<T>> {
    let message = match message {
        Value::Undefined => None,
        message => Some(ctx.coerce_string(message).map_err(thrown)?.to_string()),
    };
    Ok(ErrorInit { value, message })
}

/// What `new WebAssembly.Instance` returns: the instance holding its frozen `exports` object.
struct WithExports {
    value: api::InstanceObject,
    exports: Value,
}

impl lumen_bind::CtorRet<JsHost, api::InstanceObject> for WithExports {
    fn into_ctor(self, cx: &<JsHost as lumen_bind::Host>::Cx<'_>) -> Result<Value, Value> {
        let instance = <JsHost as lumen_bind::Host>::construct(cx, self.value)?;
        <JsHost as lumen_bind::Host>::with_ctx(cx, |ctx: &mut Ctx| {
            ctx.define_native_private_value_slot(&instance, EXPORTS_SLOT, self.exports)
        })?;
        Ok(instance)
    }
}

// ---- entity allocation ------------------------------------------------------------------------

/// A `{initial, maximum}` descriptor's limits: whole numbers in `0..=limit` with `maximum` (when
/// given) at least `initial`, else a `RangeError`.
fn alloc_limits(min: Option<f64>, max: Option<f64>, limit: u32) -> OpResult<(usize, Option<u32>)> {
    let ok = |n: f64| n >= 0.0 && n <= limit as f64;
    let min = min.unwrap_or(0.0).trunc();
    let max = max.map(f64::trunc);
    if !ok(min) || max.is_some_and(|m| !ok(m) || m < min) {
        return Err(OpError::range_error("WebAssembly: invalid initial or maximum size"));
    }
    Ok((min as usize, max.map(|m| m as u32)))
}

fn new_memory(ctx: &mut Ctx, descriptor: &Value) -> OpResult<usize> {
    descriptor_object(descriptor, "WebAssembly.Memory()")?;
    let initial = field(ctx, descriptor, "initial")?;
    let initial = u32_value(ctx, &initial, "WebAssembly.Memory page count")?;
    let maximum = match field(ctx, descriptor, "maximum")? {
        Value::Undefined => None,
        value => Some(u32_value(ctx, &value, "WebAssembly.Memory page count")?),
    };
    let shared = field(ctx, descriptor, "shared")?;
    let shared = ctx.to_boolean(&shared);
    if shared && maximum.is_none() {
        return Err(type_error("WebAssembly shared memory requires maximum"));
    }
    if maximum.is_some_and(|maximum| maximum < initial) {
        return Err(OpError::range_error("WebAssembly.Memory maximum is below initial"));
    }
    if initial > MAX_PAGES || maximum.is_some_and(|maximum| maximum > MAX_PAGES) {
        return Err(OpError::range_error(
            "WebAssembly.Memory page count exceeds the 4 GiB limit",
        ));
    }
    let buffer = if shared {
        if maximum.unwrap_or(0.0) * 65536.0 > SHARED_MEMORY_LIMIT {
            return Err(OpError::range_error(
                "WebAssembly shared memory exceeds the shared buffer limit",
            ));
        }
        let global = ctx.global_object();
        let constructor = field(ctx, &global, "SharedArrayBuffer")?;
        let length = Value::Num(initial * 65536.0);
        Some(ctx.construct_value(constructor, &[length]).map_err(thrown)?)
    } else {
        None
    };
    alloc_memory(ctx, Some(initial), maximum, buffer)
}

fn alloc_memory(
    ctx: &mut Ctx,
    initial: Option<f64>,
    maximum: Option<f64>,
    shared_buffer: Option<Value>,
) -> OpResult<usize> {
    let (min, max) = alloc_limits(initial, maximum, wasm::parse::MAX_MEMORY_PAGES)?;
    if let Some(buffer) = shared_buffer.filter(|value| !matches!(value, Value::Undefined)) {
        let max = max.ok_or_else(|| type_error("WebAssembly shared memory requires a maximum"))?;
        let bytes = (max as usize)
            .checked_mul(wasm::exec::PAGE_SIZE)
            .ok_or_else(|| OpError::range_error("WebAssembly shared memory is too large"))?;
        if bytes > lumen::embed::MAX_BUFFER_BYTES {
            return Err(OpError::range_error(
                "WebAssembly shared memory exceeds the shared buffer limit",
            ));
        }
        let mut handle = ctx
            .export_shared_array_buffer(&buffer)
            .map_err(thrown)?
            .ok_or_else(|| type_error("WebAssembly shared memory requires a SharedArrayBuffer"))?;
        let initial_bytes = min.saturating_mul(wasm::exec::PAGE_SIZE);
        if handle.byte_len() != initial_bytes {
            return Err(OpError::range_error(
                "WebAssembly shared memory buffer does not match its initial size",
            ));
        }
        handle
            .reserve_to(bytes)
            .map_err(|_| OpError::range_error("WebAssembly shared memory reservation failed"))?;
        let ws = ctx.host_mut::<WasmStore>().expect("wasm store");
        return match ws.store.alloc_shared_memory(min, max, handle) {
            Ok(addr) => {
                ws.bufs.push(MemBuf {
                    addr,
                    buf: buffer.clone(),
                    shared: true,
                    generation: 0,
                });
                Ok(addr)
            }
            Err(error) => Err(OpError::range_error(error)),
        };
    }
    let ws = ctx.host_mut::<WasmStore>().expect("wasm store");
    ws.store
        .alloc_memory(min, max)
        .map_err(OpError::range_error)
}

fn new_table(ctx: &mut Ctx, descriptor: &Value) -> OpResult<usize> {
    descriptor_object(descriptor, "WebAssembly.Table()")?;
    match field(ctx, descriptor, "element")? {
        Value::Undefined => {}
        element => {
            let element = ctx.coerce_string(&element).map_err(thrown)?;
            if !matches!(&*element, "anyfunc" | "funcref") {
                return Err(type_error(
                    "WebAssembly.Table(): Descriptor property 'element' must be 'anyfunc'",
                ));
            }
        }
    }
    let initial = match field(ctx, descriptor, "initial")? {
        Value::Undefined => None,
        value => Some(u32_value(ctx, &value, "WebAssembly.Table size")?),
    };
    let maximum = match field(ctx, descriptor, "maximum")? {
        Value::Undefined => None,
        value => Some(u32_value(ctx, &value, "WebAssembly.Table size")?),
    };
    let (min, max) = alloc_limits(initial, maximum, u32::MAX)?;
    if min > wasm::parse::MAX_TABLE_SIZE as usize {
        return Err(OpError::range_error("WebAssembly.Table: initial size too large"));
    }
    let ws = ctx.host_mut::<WasmStore>().expect("wasm store");
    ws.store.alloc_table(min, max).map_err(OpError::range_error)
}

fn new_global(ctx: &mut Ctx, descriptor: &Value, value: &Value) -> OpResult<usize> {
    descriptor_object(descriptor, "WebAssembly.Global()")?;
    let mutable = field(ctx, descriptor, "mutable")?;
    let mutable = ctx.to_boolean(&mutable);
    let ty = field(ctx, descriptor, "value")?;
    let ty = val_type(ctx, &ty)?;
    let initial = match value {
        Value::Undefined => Value::Num(0.0),
        value => value.clone(),
    };
    let val = js_to_val(ctx, &initial, ty);
    let ws = ctx.host_mut::<WasmStore>().expect("wasm store");
    Ok(ws.store.alloc_global(val, mutable))
}

fn memory_buffer(ctx: &mut Ctx, addr: usize) -> OpResult<Value> {
    let ws = ctx.host_mut::<WasmStore>().expect("wasm store");
    if let Some(b) = ws.bufs.iter().find(|b| b.addr == addr) {
        return Ok(b.buf.clone());
    }
    let store = std::mem::take(&mut ws.store);
    let r = match store.memories.get(addr) {
        Some(m) => {
            let (buf, shared) = if let Some(handle) = m
                .bytes
                .shared_handle()
                .and_then(|handle| handle.current_view().ok())
            {
                (ctx.import_shared_array_buffer(&handle), true)
            } else {
                (view(ctx, m), false)
            };
            let ws = ctx.host_mut::<WasmStore>().expect("wasm store");
            ws.bufs.push(MemBuf {
                addr,
                buf: buf.clone(),
                shared,
                generation: m.generation,
            });
            Ok(buf)
        }
        None => Err(OpError::error("wasm: bad memory address")),
    };
    ctx.host_mut::<WasmStore>().expect("wasm store").store = store;
    r
}

/// The previous page count, or `-1` when the memory cannot grow by `delta` pages. Works from
/// inside an import too (the memories are reached through `active_mems` while the store is moved
/// out).
fn memory_grow(ctx: &mut Ctx, addr: usize, delta: f64) -> OpResult<f64> {
    let delta = if (0.0..=MAX_PAGES).contains(&delta) {
        delta as i32
    } else {
        -1
    };
    let ws = ctx.host_mut::<WasmStore>().expect("wasm store");
    let active = ws.active_mems;
    // SAFETY: `active_mems` is set only while an import runs, and points at the running
    // store's memories, which no one else borrows until the import returns.
    let mems: &mut [MemEntity] = if active.is_null() || ws.store.memories.len() > addr {
        &mut ws.store.memories
    } else {
        unsafe { &mut *active }
    };
    let Some(m) = mems.get_mut(addr) else {
        return Err(OpError::error("wasm: bad memory address"));
    };
    let r = m.grow(delta);
    let mems: &[MemEntity] = if active.is_null() {
        &[]
    } else {
        unsafe { &*active }
    };
    let store = std::mem::take(&mut ctx.host_mut::<WasmStore>().expect("wasm store").store);
    refresh(
        ctx,
        if mems.is_empty() {
            &store.memories
        } else {
            mems
        },
    );
    ctx.host_mut::<WasmStore>().expect("wasm store").store = store;
    Ok(r as f64)
}

fn table_get(ctx: &mut Ctx, addr: usize, index: f64) -> OpResult<Value> {
    let ws = ctx.host_mut::<WasmStore>().expect("wasm store");
    let slot = ws
        .store
        .tables
        .get(addr)
        .and_then(|t| t.elems.get(index as usize))
        .copied();
    match slot {
        Some(Some(faddr)) => func_object(ctx, faddr),
        Some(None) => Ok(Value::Null),
        None => Err(OpError::range_error("WebAssembly.Table.get(): invalid address")),
    }
}

fn table_set(ctx: &mut Ctx, addr: usize, index: f64, faddr: Option<usize>) -> OpResult<()> {
    let ws = ctx.host_mut::<WasmStore>().expect("wasm store");
    match ws
        .store
        .tables
        .get_mut(addr)
        .and_then(|t| t.elems.get_mut(index as usize))
    {
        Some(slot) => {
            *slot = faddr;
            Ok(())
        }
        None => Err(OpError::range_error("WebAssembly.Table.set(): invalid address")),
    }
}

fn table_size(ctx: &mut Ctx, addr: usize) -> f64 {
    let ws = ctx.host_mut::<WasmStore>().expect("wasm store");
    ws.store.tables.get(addr).map(|t| t.elems.len()).unwrap_or(0) as f64
}

fn global_get(ctx: &mut Ctx, addr: usize) -> OpResult<Value> {
    let v = ctx
        .host_mut::<WasmStore>()
        .and_then(|ws| ws.store.globals.get(addr))
        .map(|g| g.get())
        .ok_or_else(|| OpError::error("wasm: bad global address"))?;
    Ok(val_to_js(v))
}

fn global_set(ctx: &mut Ctx, addr: usize, raw: &Value) -> OpResult<()> {
    let ty = {
        let ws = ctx.host_mut::<WasmStore>().expect("wasm store");
        match ws.store.globals.get(addr) {
            Some(g) => match g.ty() {
                ValType::I64 => ValType::I64,
                ValType::F32 => ValType::F32,
                ValType::F64 => ValType::F64,
                _ => ValType::I32,
            },
            None => return Err(OpError::error("wasm: bad global address")),
        }
    };
    if !ctx
        .host_mut::<WasmStore>()
        .and_then(|ws| ws.store.globals.get(addr))
        .is_some_and(|g| g.mutable)
    {
        return Err(type_error("Can't set the value of an immutable global."));
    }
    let val = js_to_val(ctx, raw, ty);
    if let Some(g) = ctx
        .host_mut::<WasmStore>()
        .and_then(|ws| ws.store.globals.get_mut(addr))
    {
        g.set(val);
    }
    Ok(())
}

// ---- modules and instances --------------------------------------------------------------------

fn compile_bytes(ctx: &mut Ctx, source: &Value) -> OpResult<Rc<Parsed>> {
    let Some(bytes) = ctx.buffer_source_bytes(source) else {
        return Err(type_error(
            "WebAssembly.Module(): Argument 0 must be a buffer source",
        ));
    };
    wasm::decode(&bytes).map_err(|message| compile_error(ctx, &message))
}

fn import_object(value: &Value) -> OpResult<Option<Value>> {
    match value {
        Value::Undefined => Ok(None),
        Value::Obj(_) => Ok(Some(value.clone())),
        _ => Err(type_error(
            "WebAssembly.Instance(): Argument 1 must be an object",
        )),
    }
}

enum Resolved {
    Func(Value, wasm::parse::FuncType),
    Mem(usize),
    Table(usize),
    Global(usize),
}

fn resolve_imports(
    ctx: &mut Ctx,
    module: &Parsed,
    imports: &Option<Value>,
) -> OpResult<Vec<Resolved>> {
    let mut resolved = Vec::with_capacity(module.imports.len());
    for (i, import) in module.imports.iter().enumerate() {
        let Some(imports) = imports else {
            return Err(type_error(
                "WebAssembly.Instance(): Imports argument must be present and must be an object",
            ));
        };
        let namespace = field(ctx, imports, &import.module)?;
        if !matches!(namespace, Value::Obj(_)) {
            return Err(type_error(format!(
                "WebAssembly.Instance(): Import #{i} \"{}\": module is not an object or function",
                import.module
            )));
        }
        let value = field(ctx, &namespace, &import.name)?;
        let fail = |ctx: &mut Ctx, what: &str| {
            link_error(
                ctx,
                &format!(
                    "WebAssembly.Instance(): Import #{i} \"{}\" \"{}\": {what}",
                    import.module, import.name
                ),
            )
        };
        resolved.push(match &import.kind {
            wasm::ImportKind::Func(type_index) => {
                if !value.is_callable() {
                    return Err(fail(ctx, "function import requires a callable"));
                }
                Resolved::Func(value, module.types[*type_index as usize].clone())
            }
            wasm::ImportKind::Memory(_) => {
                match addr_of::<api::MemoryObject>(ctx, &value, |m| m.addr) {
                    Some(addr) => Resolved::Mem(addr),
                    None => {
                        return Err(fail(ctx, "memory import must be a WebAssembly.Memory object"))
                    }
                }
            }
            wasm::ImportKind::Table(_) => {
                match addr_of::<api::TableObject>(ctx, &value, |t| t.addr) {
                    Some(addr) => Resolved::Table(addr),
                    None => {
                        return Err(fail(ctx, "table import requires a WebAssembly.Table"))
                    }
                }
            }
            wasm::ImportKind::Global(global_type) => {
                if let Some(addr) = addr_of::<api::GlobalObject>(ctx, &value, |g| g.addr) {
                    Resolved::Global(addr)
                } else if global_type.mutable {
                    return Err(fail(
                        ctx,
                        "imported mutable global must be a WebAssembly.Global object",
                    ));
                } else {
                    let is_bigint = value.bigint_as_i64().is_some();
                    let acceptable = match global_type.val {
                        ValType::I64 => is_bigint,
                        ValType::I32 | ValType::F32 | ValType::F64 => {
                            matches!(value, Value::Num(_))
                        }
                        _ => false,
                    };
                    if !acceptable {
                        return Err(fail(
                            ctx,
                            "global import must be a number, valid Wasm reference, or WebAssembly.Global object",
                        ));
                    }
                    let val = js_to_val(ctx, &value, global_type.val);
                    let ws = ctx.host_mut::<WasmStore>().expect("wasm store");
                    Resolved::Global(ws.store.alloc_global(val, false))
                }
            }
        });
    }
    Ok(resolved)
}

/// Link and start `module` against `imports`; the instance's frozen, null-prototype `exports`
/// object.
fn link(ctx: &mut Ctx, module: Rc<Parsed>, imports: Option<Value>) -> OpResult<Value> {
    let resolved = resolve_imports(ctx, &module, &imports)?;
    let linked = {
        let ws = ctx.host_mut::<WasmStore>().expect("wasm store");
        let mut linked = Imports::default();
        for entry in resolved {
            match entry {
                Resolved::Func(f, ty) => {
                    let id = ws.host_funcs.len();
                    ws.host_funcs.push(f);
                    linked.funcs.push((id, ty));
                }
                Resolved::Mem(addr) => linked.mem_addr = Some(addr),
                Resolved::Table(addr) => linked.table_addr = Some(addr),
                Resolved::Global(addr) => linked.global_addrs.push(addr),
            }
        }
        linked
    };
    // Data segments may write into an imported memory, which lives in its JS buffer.
    let instance = with_store(ctx, |_, store, _shared| {
        store.instantiate(Rc::clone(&module), linked)
    });
    let instance = match instance {
        Ok(instance) => instance,
        Err(message) => return Err(link_error(ctx, &message)),
    };

    if let Some(start) = module.start {
        let start_addr = {
            let ws = ctx.host_mut::<WasmStore>().expect("wasm store");
            ws.store.instances[instance].func_addrs[start as usize]
        };
        run_func(ctx, start_addr, Vec::new()).map_err(thrown)?;
    }

    let exported: Vec<(String, wasm::ExportKind, usize)> = {
        let ws = ctx.host_mut::<WasmStore>().expect("wasm store");
        module
            .exports
            .iter()
            .filter_map(|e| {
                ws.store
                    .export_addr(instance, &e.name)
                    .map(|(kind, addr)| (e.name.clone(), kind, addr))
            })
            .collect()
    };
    let exports = ctx.new_object_with_proto(&Value::Null);
    for (name, kind, addr) in exported {
        let value = match kind {
            wasm::ExportKind::Func => func_object(ctx, addr)?,
            wasm::ExportKind::Memory => memory_object(ctx, addr),
            wasm::ExportKind::Table => table_object(ctx, addr),
            wasm::ExportKind::Global => global_object(ctx, addr),
        };
        define_data(ctx, &exports, &name, value, true, true, true).map_err(thrown)?;
    }
    ctx.freeze_native_object(&exports);
    Ok(exports)
}

fn instance_value(ctx: &mut Ctx, module: Rc<Parsed>, imports: Option<Value>) -> OpResult<Value> {
    let exports = link(ctx, module, imports)?;
    let instance = ctx.new_instance(api::InstanceObject);
    ctx.define_native_private_value_slot(&instance, EXPORTS_SLOT, exports)
        .map_err(thrown)?;
    Ok(instance)
}

fn instantiate_source(ctx: &mut Ctx, source: &Value, imports: &Value) -> OpResult<Value> {
    let imports = import_object(imports)?;
    if let Some(module) = ctx.instance_data::<api::ModuleObject>(source) {
        let module = module.borrow().module.clone();
        return instance_value(ctx, module, imports);
    }
    let module = compile_bytes(ctx, source)?;
    let compiled = ctx.new_instance(api::ModuleObject { module: module.clone() });
    let instance = instance_value(ctx, module, imports)?;
    Ok(ctx.plain_object(&[("module", compiled), ("instance", instance)]))
}

/// Publish `WebAssembly` as a global built on first access, as the other lazy web globals are.
pub(crate) fn install(ctx: &mut Ctx) -> Result<(), Value> {
    ctx.install_lazy_global_group(
        &["WebAssembly"],
        Rc::new(|ctx, global| {
            if ctx.has_own_property_value(global, &Value::str("WebAssembly"))? {
                return Ok(());
            }
            let namespace = Value::Obj(ctx.new_object());
            ctx.install_module::<api::Module>(&namespace)?;
            define_data(ctx, global, "WebAssembly", namespace, true, false, true)
        }),
    );
    Ok(())
}

#[lumen_bind::module(name = "WebAssembly")]
pub(crate) mod api {
    use super::*;
    use lumen::embed::{Promise, This};

    #[class(name = "Module", hint(js(webidl, invalid_this)))]
    pub struct ModuleObject {
        pub(super) module: Rc<Parsed>,
    }

    #[class(name = "Instance", hint(js(webidl, invalid_this)))]
    pub struct InstanceObject;

    #[class(name = "Memory", hint(js(webidl, invalid_this)))]
    pub struct MemoryObject {
        pub(super) addr: usize,
    }

    #[class(name = "Table", hint(js(webidl, invalid_this)))]
    pub struct TableObject {
        pub(super) addr: usize,
    }

    #[class(name = "Global", hint(js(webidl, invalid_this)))]
    pub struct GlobalObject {
        pub(super) addr: usize,
    }

    #[class(name = "CompileError", hint(js(error)))]
    pub struct CompileError;

    #[class(name = "LinkError", hint(js(error)))]
    pub struct LinkError;

    #[class(name = "RuntimeError", hint(js(error)))]
    pub struct RuntimeError;

    #[methods]
    impl ModuleObject {
        #[constructor]
        fn constructor(ctx: &mut Ctx, bytes: Value) -> OpResult<ModuleObject> {
            Ok(ModuleObject {
                module: compile_bytes(ctx, &bytes)?,
            })
        }

        fn exports(ctx: &mut Ctx, module: &ModuleObject) -> Value {
            let items = wasm::export_descriptors(&module.module)
                .into_iter()
                .map(|(name, kind)| {
                    ctx.plain_object(&[
                        ("name", Value::from_string(name)),
                        ("kind", Value::str(kind)),
                    ])
                })
                .collect();
            ctx.make_array(items)
        }

        fn imports(ctx: &mut Ctx, module: &ModuleObject) -> Value {
            let items = wasm::import_descriptors(&module.module)
                .into_iter()
                .map(|(module, name, kind)| {
                    ctx.plain_object(&[
                        ("module", Value::from_string(module)),
                        ("name", Value::from_string(name)),
                        ("kind", Value::str(kind)),
                    ])
                })
                .collect();
            ctx.make_array(items)
        }

        fn custom_sections(ctx: &mut Ctx, _module: &ModuleObject, _name: &str) -> Value {
            ctx.make_array(Vec::new())
        }
    }

    #[methods]
    impl InstanceObject {
        #[constructor]
        fn constructor(
            ctx: &mut Ctx,
            module: &ModuleObject,
            #[default(Value::Undefined)] imports: Value,
        ) -> OpResult<WithExports> {
            let imports = import_object(&imports)?;
            let exports = link(ctx, module.module.clone(), imports)?;
            Ok(WithExports {
                value: InstanceObject,
                exports,
            })
        }

        #[getter]
        fn exports(this: This<Value>, ctx: &mut Ctx) -> OpResult<Value> {
            ctx.native_private_value_slot(&this, EXPORTS_SLOT)
                .filter(|_| ctx.instance_data::<InstanceObject>(&this).is_some())
                .ok_or_else(|| OpError::type_error("Illegal invocation"))
        }
    }

    #[methods]
    impl MemoryObject {
        #[constructor]
        fn constructor(ctx: &mut Ctx, descriptor: Value) -> OpResult<Tracked<MemoryObject>> {
            let addr = new_memory(ctx, &descriptor)?;
            Ok(Tracked {
                value: MemoryObject { addr },
                kind: Kind::Memory,
                addr,
            })
        }

        #[getter]
        fn buffer(&self, ctx: &mut Ctx) -> OpResult<Value> {
            memory_buffer(ctx, self.addr)
        }

        fn grow(&self, ctx: &mut Ctx, delta: Value) -> OpResult<f64> {
            let pages = u32_value(ctx, &delta, "WebAssembly.Memory.grow page count")?;
            let previous = memory_grow(ctx, self.addr, pages)?;
            if previous < 0.0 {
                return Err(OpError::range_error("WebAssembly.Memory.grow() failed"));
            }
            Ok(previous)
        }
    }

    #[methods]
    impl TableObject {
        #[constructor]
        fn constructor(ctx: &mut Ctx, descriptor: Value) -> OpResult<Tracked<TableObject>> {
            let addr = new_table(ctx, &descriptor)?;
            Ok(Tracked {
                value: TableObject { addr },
                kind: Kind::Table,
                addr,
            })
        }

        #[getter]
        fn length(&self, ctx: &mut Ctx) -> f64 {
            table_size(ctx, self.addr)
        }

        fn get(&self, ctx: &mut Ctx, index: f64) -> OpResult<Value> {
            table_get(ctx, self.addr, index)
        }

        fn set(
            &self,
            ctx: &mut Ctx,
            index: f64,
            #[default(Value::Undefined)] value: Value,
        ) -> OpResult<()> {
            let function = match &value {
                Value::Undefined | Value::Null => None,
                value => Some(func_addr_of(ctx, value).ok_or_else(|| {
                    type_error("WebAssembly.Table.set(): Argument 1 is invalid for table: function-typed object expected")
                })?),
            };
            table_set(ctx, self.addr, index, function)
        }
    }

    #[methods]
    impl GlobalObject {
        #[constructor]
        fn constructor(
            ctx: &mut Ctx,
            descriptor: Value,
            #[default(Value::Undefined)] value: Value,
        ) -> OpResult<Tracked<GlobalObject>> {
            let addr = new_global(ctx, &descriptor, &value)?;
            Ok(Tracked {
                value: GlobalObject { addr },
                kind: Kind::Global,
                addr,
            })
        }

        #[getter]
        fn value(&self, ctx: &mut Ctx) -> OpResult<Value> {
            global_get(ctx, self.addr)
        }

        #[setter]
        fn set_value(&self, ctx: &mut Ctx, value: Value) -> OpResult<()> {
            global_set(ctx, self.addr, &value)
        }

        fn value_of(&self, ctx: &mut Ctx) -> OpResult<Value> {
            global_get(ctx, self.addr)
        }
    }

    #[methods]
    impl CompileError {
        #[constructor]
        fn constructor(
            ctx: &mut Ctx,
            #[default(Value::Undefined)] message: Value,
        ) -> OpResult<ErrorInit<CompileError>> {
            error_init(ctx, CompileError, &message)
        }
    }

    #[methods]
    impl LinkError {
        #[constructor]
        fn constructor(
            ctx: &mut Ctx,
            #[default(Value::Undefined)] message: Value,
        ) -> OpResult<ErrorInit<LinkError>> {
            error_init(ctx, LinkError, &message)
        }
    }

    #[methods]
    impl RuntimeError {
        #[constructor]
        fn constructor(
            ctx: &mut Ctx,
            #[default(Value::Undefined)] message: Value,
        ) -> OpResult<ErrorInit<RuntimeError>> {
            error_init(ctx, RuntimeError, &message)
        }
    }

    /// The error classes' prototypes carry `name` and an empty `message`, as native errors do.
    #[init]
    fn init(ctx: &mut Ctx, _namespace: &Value) -> OpResult<()> {
        for (constructor, name) in [
            (ctx.class_constructor::<CompileError>(), "CompileError"),
            (ctx.class_constructor::<LinkError>(), "LinkError"),
            (ctx.class_constructor::<RuntimeError>(), "RuntimeError"),
        ] {
            let prototype = field(ctx, &constructor, "prototype")?;
            define_data(ctx, &prototype, "name", Value::str(name), true, false, true)
                .map_err(thrown)?;
            define_data(ctx, &prototype, "message", Value::str(""), true, false, true)
                .map_err(thrown)?;
        }
        Ok(())
    }

    #[op(hint(js(webidl)))]
    fn validate(bytes: &[u8]) -> bool {
        wasm::validate(bytes)
    }

    #[op(hint(js(webidl)))]
    fn compile(ctx: &mut Ctx, source: Value) -> Promise<Value> {
        Promise::ready(compile_bytes(ctx, &source).map(|module| ctx.new_instance(ModuleObject { module })))
    }

    #[op(hint(js(webidl)))]
    fn instantiate(
        ctx: &mut Ctx,
        source: Value,
        #[default(Value::Undefined)] imports: Value,
    ) -> Promise<Value> {
        Promise::ready(instantiate_source(ctx, &source, &imports))
    }
}
