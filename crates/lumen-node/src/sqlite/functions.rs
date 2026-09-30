//! Synchronous scalar SQLite callbacks. SQLite owns each boxed registration until replacement
//! or close; the callback and native API remain alive, and no registry borrow crosses JS calls.
use super::*;
use std::cell::Cell;

type Scalar = unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_void);
type Destroy = unsafe extern "C" fn(*mut c_void);
#[derive(Clone, Copy)]
pub(super) struct Api {
    create: unsafe extern "C" fn(
        *mut c_void,
        *const c_char,
        c_int,
        c_int,
        *mut c_void,
        Option<Scalar>,
        Option<Scalar>,
        Option<Destroy>,
        Option<Destroy>,
    ) -> c_int,
    user_data: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    value_type: unsafe extern "C" fn(*mut c_void) -> c_int,
    value_integer: unsafe extern "C" fn(*mut c_void) -> i64,
    value_double: unsafe extern "C" fn(*mut c_void) -> f64,
    value_text: unsafe extern "C" fn(*mut c_void) -> *const u8,
    value_blob: unsafe extern "C" fn(*mut c_void) -> *const c_void,
    value_bytes: unsafe extern "C" fn(*mut c_void) -> c_int,
    result_integer: unsafe extern "C" fn(*mut c_void, i64),
    result_double: unsafe extern "C" fn(*mut c_void, f64),
    result_text: unsafe extern "C" fn(*mut c_void, *const c_char, c_int, *mut c_void),
    result_blob: unsafe extern "C" fn(*mut c_void, *const c_void, c_int, *mut c_void),
    result_null: unsafe extern "C" fn(*mut c_void),
    result_error: unsafe extern "C" fn(*mut c_void, *const c_char, c_int),
}
impl Api {
    pub(super) fn load(lib: &DynLib) -> Result<Self, String> {
        macro_rules! symbol {
            ($name:literal) => {{
                let pointer = lib
                    .symbol($name)
                    .ok_or_else(|| format!("missing {}", $name))?;
                unsafe { std::mem::transmute(pointer) }
            }};
        }
        Ok(Self {
            create: symbol!("sqlite3_create_function_v2"),
            user_data: symbol!("sqlite3_user_data"),
            value_type: symbol!("sqlite3_value_type"),
            value_integer: symbol!("sqlite3_value_int64"),
            value_double: symbol!("sqlite3_value_double"),
            value_text: symbol!("sqlite3_value_text"),
            value_blob: symbol!("sqlite3_value_blob"),
            value_bytes: symbol!("sqlite3_value_bytes"),
            result_integer: symbol!("sqlite3_result_int64"),
            result_double: symbol!("sqlite3_result_double"),
            result_text: symbol!("sqlite3_result_text"),
            result_blob: symbol!("sqlite3_result_blob"),
            result_null: symbol!("sqlite3_result_null"),
            result_error: symbol!("sqlite3_result_error"),
        })
    }
}
struct Function {
    api: Rc<super::Api>,
    callback: Value,
    bigints: bool,
    pending: Rc<RefCell<Option<Value>>>,
}
unsafe extern "C" fn destroy(data: *mut c_void) {
    drop(Box::from_raw(data as *mut Function));
}
// Bind the selected library and current engine location for each step/exec. A Runtime can
// move after registration, and pooled coroutines can execute on a different native thread.
type Binding = (
    Option<unsafe extern "C" fn(*mut c_void) -> *mut c_void>,
    *mut Ctx,
);
thread_local! {static CURRENT: Cell<Binding> = const {Cell::new((None, std::ptr::null_mut()))};}
pub(super) struct Enter(Binding);
impl Enter {
    pub(super) fn new(api: &super::Api, ctx: *mut Ctx) -> Self {
        Self(CURRENT.with(|slot| slot.replace((Some(api.functions.user_data), ctx))))
    }
}
impl Drop for Enter {
    fn drop(&mut self) {
        CURRENT.with(|slot| slot.set(self.0));
    }
}
unsafe extern "C" fn scalar(context: *mut c_void, count: c_int, values: *mut *mut c_void) {
    let (user_data, ctx) = CURRENT.with(|slot| slot.get());
    let data = user_data.expect("registered scalar API")(context);
    let function = &*(data as *const Function);
    let api = function.api.functions;
    let ctx = &mut *ctx;
    let result = (|| -> Result<Value, Value> {
        let mut arguments = Vec::with_capacity(count as usize);
        for index in 0..count as usize {
            let value = *values.add(index);
            arguments.push(match (api.value_type)(value) {
                SQLITE_INTEGER => {
                    let integer = (api.value_integer)(value);
                    if function.bigints {
                        Value::bigint_from_i64(integer)
                    } else {
                        if integer.unsigned_abs() > 9007199254740991 {
                            return Err(ctx.make_error(
                                "RangeError",
                                "SQLite integer exceeds JavaScript's safe integer range",
                            ));
                        }
                        Value::Num(integer as f64)
                    }
                }
                SQLITE_FLOAT => Value::Num((api.value_double)(value)),
                SQLITE_TEXT => {
                    let pointer = (api.value_text)(value);
                    let size = (api.value_bytes)(value) as usize;
                    let bytes = if size == 0 {
                        &[]
                    } else {
                        std::slice::from_raw_parts(pointer, size)
                    };
                    Value::from_string(String::from_utf8_lossy(bytes).into_owned())
                }
                SQLITE_BLOB => {
                    let pointer = (api.value_blob)(value);
                    let size = (api.value_bytes)(value) as usize;
                    let bytes = if size == 0 {
                        &[]
                    } else {
                        std::slice::from_raw_parts(pointer.cast::<u8>(), size)
                    };
                    ctx.make_uint8array(bytes)?
                }
                _ => Value::Null,
            });
        }
        ctx.invoke(function.callback.clone(), Value::Undefined, &arguments)
    })();
    let result = result.and_then(|value| {
        match value {
            Value::Null | Value::Undefined => (api.result_null)(context),
            Value::Num(value) => (api.result_double)(context, value),
            Value::Bool(value) => (api.result_integer)(context, i64::from(value)),
            Value::BigInt(ref value) => {
                let integer = value
                    .to_i128()
                    .and_then(|value| i64::try_from(value).ok())
                    .ok_or_else(|| {
                        ctx.make_error(
                            "RangeError",
                            "SQLite result BigInt exceeds signed 64-bit range",
                        )
                    })?;
                (api.result_integer)(context, integer);
            }
            Value::Str(value) => {
                let bytes = lumen_host::well_formed_utf8(&value);
                (api.result_text)(
                    context,
                    bytes.as_ptr().cast(),
                    c_int::try_from(bytes.len()).map_err(|_| {
                        ctx.make_error(
                            "RangeError",
                            "SQLite function result exceeds native byte limit",
                        )
                    })?,
                    SQLITE_TRANSIENT,
                );
            }
            ref value => {
                let bytes = ctx.typed_array_bytes(value).ok_or_else(|| {
                    ctx.make_error(
                        "TypeError",
                        "SQLite function result must be a scalar or typed array",
                    )
                })?;
                (api.result_blob)(
                    context,
                    bytes.as_ptr().cast(),
                    c_int::try_from(bytes.len()).map_err(|_| {
                        ctx.make_error(
                            "RangeError",
                            "SQLite function result exceeds native byte limit",
                        )
                    })?,
                    SQLITE_TRANSIENT,
                );
            }
        }
        Ok(())
    });
    if let Err(error) = result {
        *function.pending.borrow_mut() = Some(error);
        (api.result_error)(context, c"JavaScript SQLite function failed".as_ptr(), -1);
    }
}

pub(super) struct Active(Rc<Cell<usize>>);
impl Active {
    pub(super) fn new(count: Rc<Cell<usize>>) -> Self {
        count.set(count.get() + 1);
        Self(count)
    }
}
impl Drop for Active {
    fn drop(&mut self) {
        self.0.set(self.0.get() - 1)
    }
}
pub(super) fn busy_error(ctx: &mut Ctx) -> Value {
    let error = ctx.make_error(
        "Error",
        "Cannot modify or close an executing SQLite statement or close its database",
    );
    let _ = ctx.set_member(&error, "code", Value::str("ERR_INVALID_STATE"));
    error
}
pub(super) fn op_function(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let id = arg_u32(args, 0);
    let name = ctx
        .coerce_string(args.get(1).unwrap_or(&Value::Undefined))?
        .to_string();
    let callback = args.get(2).cloned().unwrap_or(Value::Undefined);
    let count = arg_i32(args, 3);
    let flags = arg_i32(args, 4);
    let bigints = arg_bool(args, 5);
    let db = db_ptr(ctx, id)?;
    let key = (name.to_ascii_lowercase(), count);
    if state(ctx).dbs.get(&id).unwrap().functions.len() >= 256
        && !state(ctx).dbs.get(&id).unwrap().functions.contains(&key)
    {
        return Err(ctx.make_error(
            "RangeError",
            "SQLite scalar function registry exceeds 256 entries",
        ));
    }
    let name = std::ffi::CString::new(name)
        .map_err(|_| ctx.make_error("TypeError", "SQLite function name contains NUL"))?;
    if name.as_bytes().len() > 255 || !(-1..=1000).contains(&count) {
        return Err(ctx.make_error(
            "RangeError",
            "Invalid SQLite function name or argument count",
        ));
    }
    let api = state(ctx).api.as_ref().unwrap().clone();
    let pending = state(ctx).dbs.get(&id).unwrap().pending.clone();
    let data = Box::into_raw(Box::new(Function {
        api: api.clone(),
        callback,
        bigints,
        pending,
    }));
    // SQLITE_UTF8 plus the supported deterministic/direct-only flags. SQLite destroys data
    // on registration failure too, as specified by sqlite3_create_function_v2.
    let rc = unsafe {
        (api.functions.create)(
            db,
            name.as_ptr(),
            count,
            1 | (flags & (0x800 | 0x80000)),
            data.cast(),
            Some(scalar),
            None,
            None,
            Some(destroy),
        )
    };
    if rc != SQLITE_OK {
        return Err(db_error(ctx, db, "function registration failed"));
    }
    state(ctx).dbs.get_mut(&id).unwrap().functions.insert(key);
    Ok(Value::Undefined)
}
