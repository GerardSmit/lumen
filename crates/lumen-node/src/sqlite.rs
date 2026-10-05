//! `bun:sqlite` / `node:sqlite` backend. With `bundled-sqlite`, use the statically linked
//! portable SQLite implementation. Otherwise resolve the OS library through [`crate::dylib`].
//! An explicit custom library selects that implementation for the complete API table.
//!
//! The JS surface (Bun's `Database`/`Statement` API) lives in `js/bun_sqlite.js`; this module is
//! the thin native op layer it drives: open/close, prepare/finalize, bind, step, and column
//! reads. Handles cross the FFI boundary as small integer ids (never raw pointers in JS): a
//! [`SqliteState`] in `OpState` maps each id to its `sqlite3*` / `sqlite3_stmt*`.
//!
//! # Safety
//!
//! Every pointer here is owned by [`SqliteState`] and never escapes to another thread — the engine
//! is single-threaded and `!Send`, and every op runs synchronously on the loop thread. The two
//! lifetime invariants SQLite imposes are enforced by [`SqliteState`]: a `sqlite3_stmt*` must be
//! finalized before its owning `sqlite3*` is closed (so `close` finalizes the db's live statements
//! first), and text/blob bindings must outlive the `bind` call — we pass `SQLITE_TRANSIENT`, so
//! SQLite copies the bytes before `bind_*` returns and our Rust buffer can drop immediately.

use std::cell::{Cell as ActiveCell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
mod functions;
mod library;
use library::Library;
use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_void};

use lumen_bind::NativeError;
use lumen_host::OpError;
use lumen_host::{Ctx, Value};

use crate::dylib::DynLib;

// ---- SQLite result / type constants ----------------------------------------------------------

const SQLITE_OK: c_int = 0;
const SQLITE_ROW: c_int = 100;
const SQLITE_DONE: c_int = 101;

const SQLITE_INTEGER: c_int = 1;
const SQLITE_FLOAT: c_int = 2;
const SQLITE_TEXT: c_int = 3;
const SQLITE_BLOB: c_int = 4;
// SQLITE_NULL is 5 — the default arm.

/// `SQLITE_TRANSIENT` — the special destructor telling SQLite to copy the bound bytes immediately.
/// It is `(void*)-1`; see the module safety note for why this is what makes `bind` sound.
const SQLITE_TRANSIENT: *mut c_void = usize::MAX as *mut c_void;

// ---- the dlsym'd C API -----------------------------------------------------------------------
//
// Pointers to `sqlite3` / `sqlite3_stmt` are opaque, carried as `*mut c_void`. Signatures follow
// the SQLite C API exactly (the calling convention is the platform C ABI; i64 params/returns are
// `sqlite3_int64`).

type OpenV2 = unsafe extern "C" fn(*const c_char, *mut *mut c_void, c_int, *const c_char) -> c_int;
type CloseV2 = unsafe extern "C" fn(*mut c_void) -> c_int;
type PrepareV2 = unsafe extern "C" fn(
    *mut c_void,
    *const c_char,
    c_int,
    *mut *mut c_void,
    *mut *const c_char,
) -> c_int;
type StepFn = unsafe extern "C" fn(*mut c_void) -> c_int;
type ResetFn = unsafe extern "C" fn(*mut c_void) -> c_int;
type FinalizeFn = unsafe extern "C" fn(*mut c_void) -> c_int;
type ClearBindings = unsafe extern "C" fn(*mut c_void) -> c_int;
type ExecFn = unsafe extern "C" fn(
    *mut c_void,
    *const c_char,
    *mut c_void,
    *mut c_void,
    *mut *mut c_char,
) -> c_int;

type BindInt64 = unsafe extern "C" fn(*mut c_void, c_int, i64) -> c_int;
type BindDouble = unsafe extern "C" fn(*mut c_void, c_int, f64) -> c_int;
type BindText =
    unsafe extern "C" fn(*mut c_void, c_int, *const c_char, c_int, *mut c_void) -> c_int;
type BindBlob =
    unsafe extern "C" fn(*mut c_void, c_int, *const c_void, c_int, *mut c_void) -> c_int;
type BindNull = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
type BindParameterCount = unsafe extern "C" fn(*mut c_void) -> c_int;
type BindParameterIndex = unsafe extern "C" fn(*mut c_void, *const c_char) -> c_int;
type BindParameterName = unsafe extern "C" fn(*mut c_void, c_int) -> *const c_char;

type ColumnCount = unsafe extern "C" fn(*mut c_void) -> c_int;
type ColumnName = unsafe extern "C" fn(*mut c_void, c_int) -> *const c_char;
type ColumnType = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
type ColumnInt64 = unsafe extern "C" fn(*mut c_void, c_int) -> i64;
type ColumnDouble = unsafe extern "C" fn(*mut c_void, c_int) -> f64;
type ColumnText = unsafe extern "C" fn(*mut c_void, c_int) -> *const u8;
type ColumnBytes = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
type ColumnBlob = unsafe extern "C" fn(*mut c_void, c_int) -> *const c_void;

type ExpandedSql = unsafe extern "C" fn(*mut c_void) -> *mut c_char;
type Errmsg = unsafe extern "C" fn(*mut c_void) -> *const c_char;
type ExtendedErrcode = unsafe extern "C" fn(*mut c_void) -> c_int;
type LastInsertRowid = unsafe extern "C" fn(*mut c_void) -> i64;
type Changes = unsafe extern "C" fn(*mut c_void) -> c_int;
type TotalChanges = unsafe extern "C" fn(*mut c_void) -> c_int;
type Libversion = unsafe extern "C" fn() -> *const c_char;
type FreeFn = unsafe extern "C" fn(*mut c_void);

// Optional (newer-SQLite) entry points, resolved best-effort so `db.serialize()` /
// `Database.deserialize()` work when the system library has them and throw honestly when not.
type SerializeFn = unsafe extern "C" fn(*mut c_void, *const c_char, *mut i64, c_int) -> *mut u8;
type DeserializeFn =
    unsafe extern "C" fn(*mut c_void, *const c_char, *mut u8, i64, i64, c_int) -> c_int;
type Malloc64 = unsafe extern "C" fn(u64) -> *mut c_void;
type LoadExtension =
    unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char, *mut *mut c_char) -> c_int;
type FileControl = unsafe extern "C" fn(*mut c_void, *const c_char, c_int, *mut c_void) -> c_int;
type DbConfig = unsafe extern "C" fn(*mut c_void, c_int, ...) -> c_int;
type DbFilename = unsafe extern "C" fn(*mut c_void, *const c_char) -> *const c_char;

/// Every SQLite entry point, kept alongside its selected library. A dynamic library must
/// live as long as the API to keep its function pointers valid.
struct Api {
    functions: functions::Api,
    _lib: Library,
    open_v2: OpenV2,
    close_v2: CloseV2,
    prepare_v2: PrepareV2,
    step: StepFn,
    reset: ResetFn,
    finalize: FinalizeFn,
    clear_bindings: ClearBindings,
    exec: ExecFn,
    bind_int64: BindInt64,
    bind_double: BindDouble,
    bind_text: BindText,
    bind_blob: BindBlob,
    bind_null: BindNull,
    bind_parameter_count: BindParameterCount,
    bind_parameter_index: BindParameterIndex,
    bind_parameter_name: BindParameterName,
    column_count: ColumnCount,
    column_name: ColumnName,
    column_decltype: ColumnName,
    column_database_name: Option<ColumnName>,
    column_table_name: Option<ColumnName>,
    column_origin_name: Option<ColumnName>,
    column_type: ColumnType,
    column_int64: ColumnInt64,
    column_double: ColumnDouble,
    column_text: ColumnText,
    column_bytes: ColumnBytes,
    column_blob: ColumnBlob,
    expanded_sql: ExpandedSql,
    errmsg: Errmsg,
    extended_errcode: ExtendedErrcode,
    last_insert_rowid: LastInsertRowid,
    changes: Changes,
    total_changes: TotalChanges,
    get_autocommit: unsafe extern "C" fn(*mut c_void) -> c_int,
    db_config: DbConfig,
    db_filename: DbFilename,
    libversion: Libversion,
    free: FreeFn,
    serialize: Option<SerializeFn>,
    deserialize: Option<DeserializeFn>,
    malloc64: Option<Malloc64>,
    compileoption_used: Option<unsafe extern "C" fn(*const c_char) -> c_int>,
    load_extension: Option<LoadExtension>,
    file_control: FileControl,
}

/// Resolve `$name` from `$lib` and `transmute` it to type `$t`, or bail with a message naming the
/// missing symbol (a too-old libsqlite3).
macro_rules! sym {
    ($lib:expr, $name:literal, $t:ty) => {{
        let raw = $lib
            .symbol($name)
            .ok_or_else(|| format!("libsqlite3 is missing the {} symbol", $name))?;
        // SAFETY: the symbol resolves to a C function with exactly the SQLite ABI of `$t`.
        unsafe { std::mem::transmute::<*mut c_void, $t>(raw) }
    }};
}

impl Api {
    /// Select the bundled/system/custom implementation and resolve one complete API table.
    fn load(custom_path: Option<&str>) -> Result<Api, String> {
        let lib = Library::load(custom_path)?;

        Ok(Api {
            functions: functions::Api::load(&lib)?,
            open_v2: sym!(lib, "sqlite3_open_v2", OpenV2),
            close_v2: sym!(lib, "sqlite3_close_v2", CloseV2),
            prepare_v2: sym!(lib, "sqlite3_prepare_v2", PrepareV2),
            step: sym!(lib, "sqlite3_step", StepFn),
            reset: sym!(lib, "sqlite3_reset", ResetFn),
            finalize: sym!(lib, "sqlite3_finalize", FinalizeFn),
            clear_bindings: sym!(lib, "sqlite3_clear_bindings", ClearBindings),
            exec: sym!(lib, "sqlite3_exec", ExecFn),
            bind_int64: sym!(lib, "sqlite3_bind_int64", BindInt64),
            bind_double: sym!(lib, "sqlite3_bind_double", BindDouble),
            bind_text: sym!(lib, "sqlite3_bind_text", BindText),
            bind_blob: sym!(lib, "sqlite3_bind_blob", BindBlob),
            bind_null: sym!(lib, "sqlite3_bind_null", BindNull),
            bind_parameter_count: sym!(lib, "sqlite3_bind_parameter_count", BindParameterCount),
            bind_parameter_index: sym!(lib, "sqlite3_bind_parameter_index", BindParameterIndex),
            bind_parameter_name: sym!(lib, "sqlite3_bind_parameter_name", BindParameterName),
            column_count: sym!(lib, "sqlite3_column_count", ColumnCount),
            column_name: sym!(lib, "sqlite3_column_name", ColumnName),
            column_decltype: sym!(lib, "sqlite3_column_decltype", ColumnName),
            column_database_name: lib
                .symbol("sqlite3_column_database_name")
                .map(|p| unsafe { std::mem::transmute::<*mut c_void, ColumnName>(p) }),
            column_table_name: lib
                .symbol("sqlite3_column_table_name")
                .map(|p| unsafe { std::mem::transmute::<*mut c_void, ColumnName>(p) }),
            column_origin_name: lib
                .symbol("sqlite3_column_origin_name")
                .map(|p| unsafe { std::mem::transmute::<*mut c_void, ColumnName>(p) }),
            column_type: sym!(lib, "sqlite3_column_type", ColumnType),
            column_int64: sym!(lib, "sqlite3_column_int64", ColumnInt64),
            column_double: sym!(lib, "sqlite3_column_double", ColumnDouble),
            column_text: sym!(lib, "sqlite3_column_text", ColumnText),
            column_bytes: sym!(lib, "sqlite3_column_bytes", ColumnBytes),
            column_blob: sym!(lib, "sqlite3_column_blob", ColumnBlob),
            expanded_sql: sym!(lib, "sqlite3_expanded_sql", ExpandedSql),
            errmsg: sym!(lib, "sqlite3_errmsg", Errmsg),
            extended_errcode: sym!(lib, "sqlite3_extended_errcode", ExtendedErrcode),
            last_insert_rowid: sym!(lib, "sqlite3_last_insert_rowid", LastInsertRowid),
            changes: sym!(lib, "sqlite3_changes", Changes),
            total_changes: sym!(lib, "sqlite3_total_changes", TotalChanges),
            db_config: sym!(lib, "sqlite3_db_config", DbConfig),
            db_filename: sym!(lib, "sqlite3_db_filename", DbFilename),
            get_autocommit: sym!(
                lib,
                "sqlite3_get_autocommit",
                unsafe extern "C" fn(*mut c_void) -> c_int
            ),
            libversion: sym!(lib, "sqlite3_libversion", Libversion),
            free: sym!(lib, "sqlite3_free", FreeFn),
            // SAFETY (all three): when present, each symbol is the documented SQLite function
            // with exactly this ABI.
            serialize: lib
                .symbol("sqlite3_serialize")
                .map(|p| unsafe { std::mem::transmute::<*mut c_void, SerializeFn>(p) }),
            deserialize: lib
                .symbol("sqlite3_deserialize")
                .map(|p| unsafe { std::mem::transmute::<*mut c_void, DeserializeFn>(p) }),
            malloc64: lib
                .symbol("sqlite3_malloc64")
                .map(|p| unsafe { std::mem::transmute::<*mut c_void, Malloc64>(p) }),
            compileoption_used: lib.symbol("sqlite3_compileoption_used").map(|p| unsafe {
                std::mem::transmute::<*mut c_void, unsafe extern "C" fn(*const c_char) -> c_int>(p)
            }),
            load_extension: lib
                .symbol("sqlite3_load_extension")
                .map(|p| unsafe { std::mem::transmute::<*mut c_void, LoadExtension>(p) }),
            file_control: sym!(lib, "sqlite3_file_control", FileControl),
            _lib: lib,
        })
    }
}

// ---- host-side registry ----------------------------------------------------------------------

struct Db {
    active: Rc<ActiveCell<usize>>,
    pending: Rc<RefCell<Option<Value>>>,
    functions: std::collections::HashSet<(String, i32)>,
    ptr: *mut c_void,
}

struct Stmt {
    active: Rc<ActiveCell<usize>>,
    ptr: *mut c_void,
    db_id: u32,
}

/// All live SQLite state for the process, in `OpState`. `api` is loaded lazily on the first
/// `Database` construction so a program that never touches `bun:sqlite` pays nothing (and never
/// fails for a missing libsqlite3).
#[derive(Default)]
struct SqliteState {
    api: Option<Rc<Api>>,
    custom_path: Option<String>,
    next: u32,
    dbs: HashMap<u32, Db>,
    stmts: HashMap<u32, Stmt>,
}

impl Drop for SqliteState {
    fn drop(&mut self) {
        if let Some(api) = self.api.as_ref() {
            for (_, statement) in self.stmts.drain() {
                unsafe { (api.finalize)(statement.ptr) };
            }
            for (_, database) in self.dbs.drain() {
                unsafe { (api.close_v2)(database.ptr) };
            }
        }
    }
}

impl SqliteState {
    fn next_id(&mut self) -> u32 {
        self.next += 1;
        self.next
    }
}

/// Close every open database now, as dropping the state would.
pub(crate) fn close_all(ctx: &mut Ctx) {
    if let Some(state) = ctx.host_mut::<SqliteState>() {
        *state = SqliteState::default();
    }
}

/// Fetch (or lazily install) the `SqliteState`.
fn state(ctx: &mut Ctx) -> &mut SqliteState {
    if ctx.host_mut::<SqliteState>().is_none() {
        ctx.op_state().put(SqliteState::default());
    }
    ctx.host_mut::<SqliteState>().unwrap()
}

/// Ensure the libsqlite3 API is loaded, returning an honest JS error if it cannot be.
fn ensure_api(ctx: &mut Ctx) -> Result<(), NativeError> {
    if state(ctx).api.is_none() {
        let custom_path = state(ctx).custom_path.clone();
        match Api::load(custom_path.as_deref()) {
            Ok(api) => state(ctx).api = Some(Rc::new(api)),
            Err(e) => return Err(NativeError::runtime(format!("bun:sqlite unavailable: {e}"))),
        }
    }
    Ok(())
}

// ---- helpers ---------------------------------------------------------------------------------

/// Read a C string pointer as an owned Rust `String` (lossy UTF-8); a null pointer reads as empty.
unsafe fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

/// The error a failed SQLite call throws. It carries `__sqlite = true` (so the JS glue rethrows it
/// as a real `SQLiteError`), the `errno` (SQLite's extended result code), and, for every code
/// except the generic `SQLITE_ERROR` (1), the `code` name string, matching Bun.
fn sqlite_err(errno: i32, message: String) -> NativeError {
    let err = NativeError::runtime(message)
        .with_prop("__sqlite", true)
        .with_prop("errno", errno);
    match result_code_name(errno) {
        Some(code) => err.with_prop("code", code),
        None => err,
    }
}

/// The current error (extended code + message) for a db handle, as the JS error value. The `api`
/// borrow is confined to a short block so the caller keeps `&mut ctx` for `sqlite_err`.
fn db_error(ctx: &mut Ctx, db: *mut c_void, fallback: &str) -> NativeError {
    let (errno, msg) = {
        let api = state(ctx).api.as_ref().unwrap().clone();
        unsafe { ((api.extended_errcode)(db), cstr((api.errmsg)(db))) }
    };
    let message = if msg.is_empty() {
        fallback.to_string()
    } else {
        msg
    };
    sqlite_err(errno, message)
}

/// Look up the db pointer for a handle, or throw "closed database" — matching Bun's message.
fn db_ptr(ctx: &mut Ctx, id: u32) -> Result<*mut c_void, NativeError> {
    match state(ctx).dbs.get(&id) {
        Some(db) => Ok(db.ptr),
        None => Err(NativeError::runtime("Cannot use a closed database")),
    }
}

fn stmt_ptr(ctx: &mut Ctx, id: u32) -> Result<*mut c_void, NativeError> {
    match state(ctx).stmts.get(&id) {
        Some(s) if s.active.get() == 0 => Ok(s.ptr),
        Some(_) => Err(functions::busy_error()),
        // Bun's exact message for a use-after-finalize.
        None => Err(NativeError::runtime("Statement has finalized")),
    }
}

// ---- ops -------------------------------------------------------------------------------------

pub(crate) use bindings::Module;

#[lumen_bind::module(name = "__sqlite")]
pub(crate) mod bindings {
    use super::*;

    /// `open(path, flags) -> dbId`. Lazily loads libsqlite3; an absent/too-old library is the honest
    /// error the `Database` constructor throws.
    #[op(coerce)]
    pub fn open(ctx: &mut Ctx, path: String, flags: i32) -> Result<Value, OpError> {
        let path = if !path.is_empty() && path != ":memory:" && !path.starts_with("file:") {
            ctx.op_state()
                .get::<lumen_host::RealmProcess>()
                .map(|realm| realm.resolve(&path).to_string_lossy().into_owned())
                .unwrap_or(path)
        } else {
            path
        };
        ensure_api(ctx)?;

        let c_path = std::ffi::CString::new(path.clone())
            .map_err(|_| NativeError::runtime("database path contains a NUL byte"))?;

        let mut db: *mut c_void = std::ptr::null_mut();
        let rc = {
            let api = state(ctx).api.as_ref().unwrap().clone();
            // SAFETY: valid NUL-terminated path; `&mut db` receives the handle.
            unsafe { (api.open_v2)(c_path.as_ptr(), &mut db, flags, std::ptr::null()) }
        };
        if rc != SQLITE_OK {
            // On failure SQLite may still allocate the handle; read its error, then close it.
            let err = if db.is_null() {
                sqlite_err(rc, format!("unable to open database file: {path}"))
            } else {
                let e = db_error(ctx, db, "unable to open database file");
                let api = state(ctx).api.as_ref().unwrap().clone();
                unsafe { (api.close_v2)(db) };
                e
            };
            return Err(err.into());
        }

        let st = state(ctx);
        let id = st.next_id();
        st.dbs.insert(
            id,
            Db {
                ptr: db,
                active: Rc::new(ActiveCell::new(0)),
                pending: Rc::new(RefCell::new(None)),
                functions: Default::default(),
            },
        );
        Ok(Value::Num(id as f64))
    }

    #[op(coerce)]
    pub fn function(
        ctx: &mut Ctx,
        id: u32,
        name: String,
        callback: Value,
        count: i32,
        flags: i32,
        bigints: Option<bool>,
    ) -> Result<Value, OpError> {
        functions::register(
            ctx,
            id,
            name,
            callback,
            count,
            flags,
            bigints.unwrap_or(false),
        )
    }

    /// `close(dbId)`. Finalizes every live statement owned by this db first (SQLite requires it),
    /// then closes the connection.
    #[op]
    pub fn close(ctx: &mut Ctx, id: u32) -> Result<Value, OpError> {
        if state(ctx)
            .dbs
            .get(&id)
            .is_some_and(|db| db.active.get() != 0)
        {
            return Err(functions::busy_error().into());
        }
        let st = state(ctx);
        if st.api.is_none() {
            return Ok(Value::Undefined);
        }
        let owned: Vec<u32> = st
            .stmts
            .iter()
            .filter(|(_, s)| s.db_id == id)
            .map(|(k, _)| *k)
            .collect();
        for sid in owned {
            if let Some(s) = st.stmts.remove(&sid) {
                let api = st.api.as_ref().unwrap();
                unsafe { (api.finalize)(s.ptr) };
            }
        }
        if let Some(db) = st.dbs.remove(&id) {
            let api = st.api.as_ref().unwrap();
            unsafe { (api.close_v2)(db.ptr) };
        }
        Ok(Value::Undefined)
    }

    /// `prepare(dbId, sql) -> stmtId`. Compiles the first statement in `sql`.
    #[op(coerce)]
    pub fn prepare(ctx: &mut Ctx, id: u32, sql: String) -> Result<Value, OpError> {
        let db = db_ptr(ctx, id)?;

        let mut stmt: *mut c_void = std::ptr::null_mut();
        let rc = {
            let api = state(ctx).api.as_ref().unwrap().clone();
            // SAFETY: `db` is a live connection; the SQL slice is passed with an explicit byte length
            // (need not be NUL-terminated); `&mut stmt` receives the compiled statement.
            unsafe {
                (api.prepare_v2)(
                    db,
                    sql.as_ptr() as *const c_char,
                    sql.len() as c_int,
                    &mut stmt,
                    std::ptr::null_mut(),
                )
            }
        };
        if rc != SQLITE_OK {
            return Err(db_error(ctx, db, "prepare failed").into());
        }
        if stmt.is_null() {
            // Empty statement (whitespace/comment only): nothing to run.
            return Err(NativeError::runtime("no SQL statement to prepare").into());
        }
        let st = state(ctx);
        let sid = st.next_id();
        st.stmts.insert(
            sid,
            Stmt {
                active: Rc::new(ActiveCell::new(0)),
                ptr: stmt,
                db_id: id,
            },
        );
        Ok(Value::Num(sid as f64))
    }

    #[op]
    pub fn finalize(ctx: &mut Ctx, sid: u32) -> Result<Value, OpError> {
        if state(ctx)
            .stmts
            .get(&sid)
            .is_some_and(|s| s.active.get() != 0)
        {
            return Err(functions::busy_error().into());
        }
        let st = state(ctx);
        if let Some(s) = st.stmts.remove(&sid) {
            if let Some(api) = st.api.as_ref() {
                unsafe { (api.finalize)(s.ptr) };
            }
        }
        Ok(Value::Undefined)
    }

    /// `reset(stmtId, clearBindings)` — rewind the statement so it can run again. Bindings are
    /// cleared only when `clearBindings` is true (before a fresh parameter set is bound); a plain
    /// post-run reset keeps them so a later no-argument call reuses them and `toString()` shows the
    /// last-bound SQL — both verified Bun behaviors.
    #[op]
    pub fn reset(ctx: &mut Ctx, sid: u32, clear: Option<bool>) -> Result<Value, OpError> {
        let clear = clear.unwrap_or(false);
        let stmt = stmt_ptr(ctx, sid)?;
        let api = state(ctx).api.as_ref().unwrap().clone();
        unsafe {
            (api.reset)(stmt);
            if clear {
                (api.clear_bindings)(stmt);
            }
        }
        Ok(Value::Undefined)
    }

    /// `bind(stmtId, index, value)` — bind one value at the 1-based parameter index, dispatching on
    /// the JS value's type to the matching `sqlite3_bind_*`.
    #[op]
    pub fn bind(ctx: &mut Ctx, sid: u32, index: i32, value: Value) -> Result<Value, OpError> {
        let stmt = stmt_ptr(ctx, sid)?;

        // For an object argument, the only bindable shape is a TypedArray (→ BLOB). Extract its bytes
        // now, while we still hold `&mut ctx`; reject any other object before borrowing the api.
        let blob: Option<Vec<u8>> = if matches!(value, Value::Obj(_)) {
            match ctx.typed_array_raw(&value) {
                Some((_, len, ptr)) => Some(if ptr.is_null() || len == 0 {
                    Vec::new()
                } else {
                    unsafe { std::slice::from_raw_parts(ptr, len).to_vec() }
                }),
                None => {
                    // Bun's exact TypeError message for an unbindable value.
                    return Err(NativeError::type_error(
                        "Binding expected string, TypedArray, boolean, number, bigint or null",
                    )
                    .into());
                }
            }
        } else {
            None
        };

        let rc = {
            let api = state(ctx).api.as_ref().unwrap().clone();
            unsafe {
                match &value {
                    Value::Undefined | Value::Null => (api.bind_null)(stmt, index),
                    Value::Bool(b) => (api.bind_int64)(stmt, index, if *b { 1 } else { 0 }),
                    Value::Num(n) => {
                        // Bun (JSC) binds an integral number as INTEGER only within the int52 range
                        // [-2^51, 2^51); anything outside — and every fractional/NaN/±Inf value —
                        // goes through bind_double (verified against bun v1.2.21: 2^50 → integer,
                        // 2^51 → real, -2^51 → integer). SQLite itself stores a NaN double as NULL.
                        // (-0.0 is REAL in Bun — JSC boxes it as a double — hence the sign check.)
                        const INT52: f64 = 2251799813685248.0; // 2^51
                        if n.fract() == 0.0
                            && *n >= -INT52
                            && *n < INT52
                            && !(*n == 0.0 && n.is_sign_negative())
                        {
                            (api.bind_int64)(stmt, index, *n as i64)
                        } else {
                            (api.bind_double)(stmt, index, *n)
                        }
                    }
                    Value::BigInt(_) => {
                        (api.bind_int64)(stmt, index, value.bigint_as_i64().unwrap_or(0))
                    }
                    Value::Str(s) => {
                        let text = s.to_string();
                        (api.bind_text)(
                            stmt,
                            index,
                            text.as_ptr() as *const c_char,
                            text.len() as c_int,
                            SQLITE_TRANSIENT,
                        )
                    }
                    Value::Obj(_) => {
                        let bytes = blob.as_deref().unwrap_or(&[]);
                        (api.bind_blob)(
                            stmt,
                            index,
                            bytes.as_ptr() as *const c_void,
                            bytes.len() as c_int,
                            SQLITE_TRANSIENT,
                        )
                    }
                    // Symbols (and any other engine-internal variant) are not bindable — Bun's
                    // exact TypeError. Break out of the api borrow to build the error.
                    _ => -1,
                }
            }
        };
        if rc == -1 {
            return Err(NativeError::type_error(
                "Binding expected string, TypedArray, boolean, number, bigint or null",
            )
            .into());
        }
        if rc != SQLITE_OK {
            let db_id = state(ctx).stmts.get(&sid).map(|s| s.db_id).unwrap_or(0);
            let db = db_ptr(ctx, db_id)?;
            return Err(db_error(ctx, db, "bind failed").into());
        }
        Ok(Value::Undefined)
    }

    /// `step(stmtId) -> 1 (a row is available) | 0 (done)`. Any other result is a run-time error.
    #[op]
    pub fn step(ctx: &mut Ctx, sid: u32) -> Result<Value, OpError> {
        let stmt = stmt_ptr(ctx, sid)?;
        let db_id = state(ctx).stmts.get(&sid).unwrap().db_id;
        let pending = state(ctx).dbs.get(&db_id).unwrap().pending.clone();
        let rc = {
            let _db_guard =
                functions::Active::new(state(ctx).dbs.get(&db_id).unwrap().active.clone());
            let _statement_guard =
                functions::Active::new(state(ctx).stmts.get(&sid).unwrap().active.clone());
            let api = state(ctx).api.as_ref().unwrap().clone();
            let _library = functions::Enter::new(&api, ctx);
            unsafe { (api.step)(stmt) }
        };
        if let Some(error) = pending.borrow_mut().take() {
            return Err(error.into());
        }
        match rc {
            SQLITE_ROW => Ok(Value::Num(1.0)),
            SQLITE_DONE => Ok(Value::Num(0.0)),
            _ => {
                let db_id = state(ctx).stmts.get(&sid).map(|s| s.db_id).unwrap_or(0);
                let db = db_ptr(ctx, db_id)?;
                Err(db_error(ctx, db, "step failed").into())
            }
        }
    }

    #[op(name = "columnCount")]
    pub fn column_count(ctx: &mut Ctx, sid: u32) -> Result<Value, OpError> {
        let stmt = stmt_ptr(ctx, sid)?;
        let api = state(ctx).api.as_ref().unwrap().clone();
        let n = unsafe { (api.column_count)(stmt) };
        Ok(Value::Num(n as f64))
    }

    /// `columnNames(stmtId) -> [name, ...]` — the result columns' names, in order.
    #[op(name = "columnNames")]
    pub fn column_names(ctx: &mut Ctx, sid: u32) -> Result<Value, OpError> {
        let stmt = stmt_ptr(ctx, sid)?;
        let names = {
            let api = state(ctx).api.as_ref().unwrap().clone();
            let n = unsafe { (api.column_count)(stmt) };
            let mut names = Vec::with_capacity(n as usize);
            for i in 0..n {
                names.push(Value::from_string(unsafe {
                    cstr((api.column_name)(stmt, i))
                }));
            }
            names
        };
        Ok(ctx.make_array(names))
    }

    /// Result metadata copied from the live prepared statement; origin fields are optional.
    #[op]
    pub fn columns(ctx: &mut Ctx, sid: u32) -> Result<Value, OpError> {
        let stmt = stmt_ptr(ctx, sid)?;
        let metadata = {
            let api = state(ctx).api.as_ref().unwrap().clone();
            // SAFETY: the statement is live. Copy all SQLite-owned strings before allocating JS
            // objects; optional origin functions are absent in builds without column metadata.
            (0..unsafe { (api.column_count)(stmt) })
                .map(|index| {
                    [
                        ("name", Some(api.column_name)),
                        ("type", Some(api.column_decltype)),
                        ("database", api.column_database_name),
                        ("table", api.column_table_name),
                        ("column", api.column_origin_name),
                    ]
                    .map(|(name, function)| {
                        let text = function.and_then(|function| {
                            let ptr = unsafe { function(stmt, index) };
                            (!ptr.is_null()).then(|| {
                                unsafe { CStr::from_ptr(ptr) }
                                    .to_string_lossy()
                                    .into_owned()
                            })
                        });
                        (name, text)
                    })
                })
                .collect::<Vec<_>>()
        };
        let rows = metadata
            .into_iter()
            .map(|fields| {
                let row = Value::Obj(ctx.new_object());
                for (name, text) in fields {
                    let value = text.map(Value::from_string).unwrap_or(Value::Null);
                    let _ = ctx.set_member(&row, name, value);
                }
                row
            })
            .collect();
        Ok(ctx.make_array(rows))
    }

    /// A column's value read out of SQLite, before it becomes a `Value` (BLOB needs `&mut ctx` to
    /// allocate a Uint8Array, which we can't do while the api is borrowed — so read raw first).
    enum Cell {
        Int(i64),
        Float(f64),
        Text(String),
        Blob(Vec<u8>),
        Null,
    }

    /// `row(stmtId, safeIntegers) -> [value, ...]` — the current row's column values, typed per Bun:
    /// INTEGER → number (bigint when `safeIntegers`), REAL → number, TEXT → string, BLOB → Uint8Array,
    /// NULL → null. Assumes the last `step` returned a row.
    #[op]
    pub fn row(ctx: &mut Ctx, sid: u32, safe_integers: Option<bool>) -> Result<Value, OpError> {
        let safe = safe_integers.unwrap_or(false);
        let stmt = stmt_ptr(ctx, sid)?;

        // Read every column into owned `Cell`s while the api is borrowed; the borrow ends with this
        // block, freeing `ctx` for the Uint8Array allocations below.
        let cells: Vec<Cell> = {
            let api = state(ctx).api.as_ref().unwrap().clone();
            let n = unsafe { (api.column_count)(stmt) };
            let mut cells = Vec::with_capacity(n as usize);
            for i in 0..n {
                let ty = unsafe { (api.column_type)(stmt, i) };
                let cell = match ty {
                    SQLITE_INTEGER => Cell::Int(unsafe { (api.column_int64)(stmt, i) }),
                    SQLITE_FLOAT => Cell::Float(unsafe { (api.column_double)(stmt, i) }),
                    SQLITE_TEXT => {
                        let len = unsafe { (api.column_bytes)(stmt, i) } as usize;
                        let ptr = unsafe { (api.column_text)(stmt, i) };
                        let s = if ptr.is_null() || len == 0 {
                            String::new()
                        } else {
                            String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(ptr, len) })
                                .into_owned()
                        };
                        Cell::Text(s)
                    }
                    SQLITE_BLOB => {
                        let len = unsafe { (api.column_bytes)(stmt, i) } as usize;
                        let ptr = unsafe { (api.column_blob)(stmt, i) };
                        let bytes = if ptr.is_null() || len == 0 {
                            Vec::new()
                        } else {
                            unsafe { std::slice::from_raw_parts(ptr as *const u8, len).to_vec() }
                        };
                        Cell::Blob(bytes)
                    }
                    _ => Cell::Null,
                };
                cells.push(cell);
            }
            cells
        };

        let mut values = Vec::with_capacity(cells.len());
        for cell in cells {
            let v = match cell {
                Cell::Int(n) => {
                    if safe {
                        Value::bigint_from_i64(n)
                    } else {
                        Value::Num(n as f64)
                    }
                }
                Cell::Float(f) => Value::Num(f),
                Cell::Text(s) => Value::from_string(s),
                Cell::Blob(b) => ctx.make_uint8array(&b)?,
                Cell::Null => Value::Null,
            };
            values.push(v);
        }
        Ok(ctx.make_array(values))
    }

    #[op(name = "bindParameterCount")]
    pub fn bind_parameter_count(ctx: &mut Ctx, sid: u32) -> Result<Value, OpError> {
        let stmt = stmt_ptr(ctx, sid)?;
        let api = state(ctx).api.as_ref().unwrap().clone();
        let n = unsafe { (api.bind_parameter_count)(stmt) };
        Ok(Value::Num(n as f64))
    }

    /// `bindParameterName(stmtId, index) -> name | null` — the source name of the 1-based parameter
    /// (`$a`/`:a`/`@a`), or null for a positional `?`.
    #[op(name = "bindParameterName")]
    pub fn bind_parameter_name(ctx: &mut Ctx, sid: u32, index: i32) -> Result<Value, OpError> {
        let stmt = stmt_ptr(ctx, sid)?;
        let api = state(ctx).api.as_ref().unwrap().clone();
        let p = unsafe { (api.bind_parameter_name)(stmt, index) };
        if p.is_null() {
            Ok(Value::Null)
        } else {
            Ok(Value::from_string(unsafe { cstr(p) }))
        }
    }

    /// `bindParameterIndex(stmtId, name) -> index` — the 1-based index of a named parameter, or 0 if
    /// there is no such parameter.
    #[op(coerce, name = "bindParameterIndex")]
    pub fn bind_parameter_index(ctx: &mut Ctx, sid: u32, name: String) -> Result<Value, OpError> {
        let stmt = stmt_ptr(ctx, sid)?;
        let c_name = match std::ffi::CString::new(name) {
            Ok(c) => c,
            Err(_) => return Ok(Value::Num(0.0)),
        };
        let api = state(ctx).api.as_ref().unwrap().clone();
        let idx = unsafe { (api.bind_parameter_index)(stmt, c_name.as_ptr()) };
        Ok(Value::Num(idx as f64))
    }

    /// `exec(dbId, sql) -> { changes, lastInsertRowid }` — run one or more statements with no
    /// bindings (Bun's `db.exec` / the no-parameter `db.run`). `changes` is the delta in the
    /// connection's total change count across the whole batch.
    #[op(coerce)]
    pub fn exec(ctx: &mut Ctx, id: u32, sql: String) -> Result<Value, OpError> {
        let db = db_ptr(ctx, id)?;
        let c_sql = std::ffi::CString::new(sql)
            .map_err(|_| NativeError::runtime("SQL contains a NUL byte"))?;

        let pending = state(ctx).dbs.get(&id).unwrap().pending.clone();
        let (rc, errmsg, before) = {
            let _db_guard = functions::Active::new(state(ctx).dbs.get(&id).unwrap().active.clone());
            let api = state(ctx).api.as_ref().unwrap().clone();
            let _library = functions::Enter::new(&api, ctx);
            let before = unsafe { (api.total_changes)(db) };
            let mut errmsg: *mut c_char = std::ptr::null_mut();
            // SAFETY: `db` is live; `c_sql` is NUL-terminated; no callback. `errmsg`, if set, is
            // SQLite-allocated and must be freed with `sqlite3_free`.
            let rc = unsafe {
                (api.exec)(
                    db,
                    c_sql.as_ptr(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut errmsg,
                )
            };
            (rc, errmsg, before)
        };
        if let Some(error) = pending.borrow_mut().take() {
            if !errmsg.is_null() {
                unsafe { (state(ctx).api.as_ref().unwrap().free)(errmsg.cast()) };
            }
            return Err(error.into());
        }
        if rc != SQLITE_OK {
            let (msg, errno) = {
                let api = state(ctx).api.as_ref().unwrap().clone();
                let msg = unsafe { cstr(errmsg) };
                let errno = unsafe { (api.extended_errcode)(db) };
                if !errmsg.is_null() {
                    unsafe { (api.free)(errmsg as *mut c_void) };
                }
                (msg, errno)
            };
            return Err(sqlite_err(errno, msg).into());
        }
        let (changes, rowid) = {
            let api = state(ctx).api.as_ref().unwrap().clone();
            let changes = unsafe { (api.total_changes)(db) } - before;
            let rowid = unsafe { (api.last_insert_rowid)(db) };
            (changes, rowid)
        };
        let obj = Value::Obj(ctx.new_object());
        let _ = ctx.set_member(&obj, "changes", Value::Num(changes as f64));
        let _ = ctx.set_member(&obj, "lastInsertRowid", Value::Num(rowid as f64));
        Ok(obj)
    }

    #[op(name = "isTransaction")]
    pub fn is_transaction(ctx: &mut Ctx, id: u32) -> Result<Value, OpError> {
        let db = db_ptr(ctx, id)?;
        let api = state(ctx).api.as_ref().unwrap().clone();
        // SAFETY: db_ptr validated the live connection; SQLite owns its transaction state.
        Ok(Value::Bool(unsafe { (api.get_autocommit)(db) } == 0))
    }

    #[op(coerce)]
    pub fn location(ctx: &mut Ctx, id: u32, schema: String) -> Result<Value, OpError> {
        let db = db_ptr(ctx, id)?;
        let schema = std::ffi::CString::new(schema)
            .map_err(|_| NativeError::type_error("schema name contains a NUL byte"))?;
        let api = state(ctx).api.as_ref().unwrap().clone();
        // SAFETY: the connection and NUL-terminated schema are live; copy SQLite's borrowed
        // result before another operation can alter an attachment.
        let filename = unsafe { (api.db_filename)(db, schema.as_ptr()) };
        if filename.is_null() {
            return Ok(Value::Null);
        }
        let filename = unsafe { CStr::from_ptr(filename) }.to_string_lossy();
        Ok(if filename.is_empty() {
            Value::Null
        } else {
            Value::from_string(filename.into_owned())
        })
    }

    #[op(name = "doubleQuotedStringLiterals")]
    pub fn double_quoted_string_literals(
        ctx: &mut Ctx,
        id: u32,
        enabled: Value,
    ) -> Result<Value, OpError> {
        configure_db_flags(ctx, id, &enabled, &[1013, 1014]) // DQS_DML / DQS_DDL
    }

    #[op]
    pub fn defensive(ctx: &mut Ctx, id: u32, enabled: Value) -> Result<Value, OpError> {
        configure_db_flags(ctx, id, &enabled, &[1010]) // SQLITE_DBCONFIG_DEFENSIVE
    }

    fn configure_db_flags(
        ctx: &mut Ctx,
        id: u32,
        enabled: &Value,
        operations: &[c_int],
    ) -> Result<Value, OpError> {
        let db = db_ptr(ctx, id)?;
        let enabled: c_int = i32::from(matches!(enabled, Value::Bool(true)));
        for &operation in operations {
            let api = state(ctx).api.as_ref().unwrap().clone();
            let mut actual: c_int = 0;
            // SAFETY: SQLite documents this variadic configuration as (int, int*); the
            // function pointer keeps the C variadic ABI, including arm64 argument placement.
            let rc = unsafe { (api.db_config)(db, operation, enabled, &mut actual as *mut c_int) };
            if rc != SQLITE_OK {
                return Err(db_error(ctx, db, "could not configure SQLite connection").into());
            }
        }
        Ok(Value::Undefined)
    }

    #[op]
    pub fn changes(ctx: &mut Ctx, id: u32) -> Result<Value, OpError> {
        let db = db_ptr(ctx, id)?;
        let api = state(ctx).api.as_ref().unwrap().clone();
        let n = unsafe { (api.changes)(db) };
        Ok(Value::Num(n as f64))
    }

    /// `totalChanges(dbId)` — the connection's cumulative change count. The JS glue diffs it around a
    /// statement execution so a read-only statement reports `changes: 0` (Bun's semantics), which
    /// plain `sqlite3_changes` (last write, however long ago) cannot express.
    #[op(name = "totalChanges")]
    pub fn total_changes(ctx: &mut Ctx, id: u32) -> Result<Value, OpError> {
        let db = db_ptr(ctx, id)?;
        let api = state(ctx).api.as_ref().unwrap().clone();
        let n = unsafe { (api.total_changes)(db) };
        Ok(Value::Num(n as f64))
    }

    /// `lastInsertRowid(dbId, safeIntegers) -> number | bigint`.
    #[op(name = "lastInsertRowid")]
    pub fn last_insert_rowid(
        ctx: &mut Ctx,
        id: u32,
        safe_integers: Option<bool>,
    ) -> Result<Value, OpError> {
        let safe = safe_integers.unwrap_or(false);
        let db = db_ptr(ctx, id)?;
        let api = state(ctx).api.as_ref().unwrap().clone();
        let n = unsafe { (api.last_insert_rowid)(db) };
        if safe {
            Ok(Value::bigint_from_i64(n))
        } else {
            Ok(Value::Num(n as f64))
        }
    }

    /// `expandedSql(stmtId) -> string` — the SQL with its current bindings inlined (Bun's
    /// `Statement.toString()`).
    #[op(name = "expandedSql")]
    pub fn expanded_sql(ctx: &mut Ctx, sid: u32) -> Result<Value, OpError> {
        let stmt = stmt_ptr(ctx, sid)?;
        let api = state(ctx).api.as_ref().unwrap().clone();
        let p = unsafe { (api.expanded_sql)(stmt) };
        if p.is_null() {
            return Ok(Value::from_string(String::new()));
        }
        let s = unsafe { cstr(p) };
        unsafe { (api.free)(p as *mut c_void) };
        Ok(Value::from_string(s))
    }

    #[op]
    pub fn libversion(ctx: &mut Ctx) -> Result<Value, OpError> {
        ensure_api(ctx)?;
        let api = state(ctx).api.as_ref().unwrap().clone();
        let v = unsafe { cstr((api.libversion)()) };
        Ok(Value::from_string(v))
    }

    #[op(coerce, name = "setCustomSQLite")]
    pub fn set_custom_sqlite(ctx: &mut Ctx, path: String) -> Result<Value, OpError> {
        if !std::path::Path::new(&path).is_file() {
            return Err(NativeError::runtime(format!(
                "Database.setCustomSQLite library not found: {path}"
            ))
            .into());
        }
        let sqlite = state(ctx);
        if sqlite.api.is_some() || !sqlite.dbs.is_empty() {
            return Err(NativeError::runtime(
                "Database.setCustomSQLite must be called before opening a database",
            )
            .into());
        }
        sqlite.custom_path = Some(path);
        Ok(Value::Undefined)
    }

    /// `SQLITE_DBCONFIG_ENABLE_LOAD_EXTENSION`: allows only the C `sqlite3_load_extension` interface,
    /// as Node does. `sqlite3_enable_load_extension` would also switch on the SQL `load_extension()`
    /// function, which lets any SQL text load native code.
    const SQLITE_DBCONFIG_ENABLE_LOAD_EXTENSION: c_int = 1005;

    #[op(name = "enableLoadExtension")]
    pub fn enable_load_extension(ctx: &mut Ctx, id: u32, allow: Value) -> Result<Value, OpError> {
        let db = db_ptr(ctx, id)?;
        let enabled = match allow {
            Value::Bool(value) => value,
            _ => return Err(NativeError::type_error("allow must be a boolean").into()),
        };
        let api = state(ctx).api.as_ref().unwrap().clone();
        let mut actual: c_int = 0;
        // SAFETY: the documented (int, int*) shape of the variadic configuration call.
        let rc = unsafe {
            (api.db_config)(
                db,
                SQLITE_DBCONFIG_ENABLE_LOAD_EXTENSION,
                i32::from(enabled),
                &mut actual as *mut c_int,
            )
        };
        if rc != SQLITE_OK {
            // A library built without extension loading has nothing to disable.
            let omitted = api
                .compileoption_used
                .is_some_and(|used| unsafe { used(c"OMIT_LOAD_EXTENSION".as_ptr()) != 0 });
            if omitted && !enabled {
                return Ok(Value::Undefined);
            }
            return Err(NativeError::runtime(if omitted {
                "the loaded SQLite library does not support extensions".to_string()
            } else {
                "unable to configure SQLite extension loading".to_string()
            })
            .into());
        }
        Ok(Value::Undefined)
    }

    #[op(coerce, name = "loadExtension")]
    pub fn load_extension(ctx: &mut Ctx, id: u32, path: String) -> Result<Value, OpError> {
        let db = db_ptr(ctx, id)?;
        let path = std::ffi::CString::new(path)
            .map_err(|_| NativeError::type_error("extension path contains a null byte"))?;
        let (load, free) = {
            let api = state(ctx).api.as_ref().unwrap().clone();
            (api.load_extension, api.free)
        };
        let Some(load) = load else {
            return Err(NativeError::runtime(
                "the loaded SQLite library does not support extensions",
            )
            .into());
        };
        let mut message: *mut c_char = std::ptr::null_mut();
        let rc = unsafe { load(db, path.as_ptr(), std::ptr::null(), &mut message) };
        if rc != SQLITE_OK {
            let text = unsafe { cstr(message) };
            if !message.is_null() {
                unsafe { free(message as *mut c_void) };
            }
            return Err(sqlite_err(
                rc,
                if text.is_empty() {
                    "unable to load SQLite extension".to_string()
                } else {
                    text
                },
            )
            .into());
        }
        Ok(Value::Undefined)
    }

    #[op(name = "fileControl")]
    pub fn file_control(
        ctx: &mut Ctx,
        id: u32,
        command: i32,
        value: Option<Value>,
    ) -> Result<Value, OpError> {
        let db = db_ptr(ctx, id)?;
        let file_control = state(ctx).api.as_ref().unwrap().file_control;
        let value = value.as_ref().unwrap_or(&Value::Undefined);
        let mut number = value.as_num_opt().unwrap_or(0.0) as i32;
        let pointer = if matches!(value, Value::Undefined | Value::Null) {
            std::ptr::null_mut()
        } else if value.as_num_opt().is_some() {
            &mut number as *mut i32 as *mut c_void
        } else if let Some((_kind, _len, ptr)) = ctx.typed_array_raw(value) {
            ptr as *mut c_void
        } else {
            return Err(NativeError::type_error(
                "Database.fileControl value must be a number, TypedArray, null, or undefined",
            )
            .into());
        };
        let rc = unsafe { file_control(db, std::ptr::null(), command, pointer) };
        if rc != SQLITE_OK {
            return Err(db_error(ctx, db, "sqlite3_file_control failed").into());
        }
        Ok(Value::Undefined)
    }

    /// `serialize(dbId) -> Uint8Array` — a byte-for-byte snapshot of the main database (what a
    /// `VACUUM INTO` of it would contain). Honest throw when the system libsqlite3 predates
    /// `sqlite3_serialize`.
    #[op]
    pub fn serialize(ctx: &mut Ctx, id: u32) -> Result<Value, OpError> {
        let db = db_ptr(ctx, id)?;
        let bytes: Vec<u8> = {
            let api = state(ctx).api.as_ref().unwrap().clone();
            let Some(serialize) = api.serialize else {
                return Err(NativeError::runtime("Database.serialize is not supported in lumen (system libsqlite3 lacks sqlite3_serialize)",
            ).into());
            };
            let schema = b"main\0";
            let mut size: i64 = 0;
            // SAFETY: `db` is live; `schema` is NUL-terminated; `size` receives the byte count. With
            // flags=0 the returned buffer is sqlite3_malloc'd and ours to free.
            let p = unsafe { serialize(db, schema.as_ptr() as *const c_char, &mut size, 0) };
            if p.is_null() {
                return Err(
                    NativeError::runtime("sqlite3_serialize failed (out of memory?)").into(),
                );
            }
            let bytes = unsafe { std::slice::from_raw_parts(p, size as usize).to_vec() };
            unsafe { (api.free)(p as *mut c_void) };
            bytes
        };
        Ok(ctx.make_uint8array(&bytes)?)
    }

    /// `deserialize(bytes) -> dbId` — open a fresh in-memory database whose contents are the given
    /// serialized image (read-write; SQLite owns a resizable copy of the bytes).
    #[op]
    pub fn deserialize(ctx: &mut Ctx, buf: Value) -> Result<Value, OpError> {
        ensure_api(ctx)?;
        let bytes: Vec<u8> = match ctx.typed_array_raw(&buf) {
            Some((_, len, ptr)) if !ptr.is_null() && len > 0 => unsafe {
                std::slice::from_raw_parts(ptr, len).to_vec()
            },
            Some(_) => Vec::new(),
            None => {
                return Err(NativeError::type_error(
                    "Database.deserialize expects a TypedArray or Buffer",
                )
                .into())
            }
        };

        {
            let api = state(ctx).api.as_ref().unwrap().clone();
            if api.deserialize.is_none() || api.malloc64.is_none() {
                return Err(NativeError::runtime("Database.deserialize is not supported in lumen (system libsqlite3 lacks sqlite3_deserialize)",
            ).into());
            }
        }

        // Open the shell :memory: db that will adopt the image.
        let mut db: *mut c_void = std::ptr::null_mut();
        let rc = {
            let api = state(ctx).api.as_ref().unwrap().clone();
            let path = b":memory:\0";
            // SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE
            unsafe {
                (api.open_v2)(
                    path.as_ptr() as *const c_char,
                    &mut db,
                    2 | 4,
                    std::ptr::null(),
                )
            }
        };
        if rc != SQLITE_OK || db.is_null() {
            return Err(NativeError::runtime("unable to open in-memory database").into());
        }

        let rc = {
            let api = state(ctx).api.as_ref().unwrap().clone();
            let deserialize = api.deserialize.unwrap();
            let malloc64 = api.malloc64.unwrap();
            // SQLite requires the image in memory it can manage: copy into sqlite3_malloc'd space and
            // hand ownership over with FREEONCLOSE (1) | RESIZEABLE (2).
            let p = unsafe { malloc64(bytes.len().max(1) as u64) } as *mut u8;
            if p.is_null() {
                unsafe { (api.close_v2)(db) };
                return Err(NativeError::runtime("sqlite3_malloc64 failed").into());
            }
            // SAFETY: `p` has capacity for `bytes.len()` bytes; src/dst don't overlap.
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
                let schema = b"main\0";
                deserialize(
                    db,
                    schema.as_ptr() as *const c_char,
                    p,
                    bytes.len() as i64,
                    bytes.len() as i64,
                    1 | 2, // SQLITE_DESERIALIZE_FREEONCLOSE | SQLITE_DESERIALIZE_RESIZEABLE
                )
            }
        };
        if rc != SQLITE_OK {
            let err = db_error(ctx, db, "sqlite3_deserialize failed");
            let api = state(ctx).api.as_ref().unwrap().clone();
            unsafe { (api.close_v2)(db) };
            return Err(err.into());
        }

        let st = state(ctx);
        let id = st.next_id();
        st.dbs.insert(
            id,
            Db {
                ptr: db,
                active: Rc::new(ActiveCell::new(0)),
                pending: Rc::new(RefCell::new(None)),
                functions: Default::default(),
            },
        );
        Ok(Value::Num(id as f64))
    }
}

/// Map a SQLite result code (primary or extended) to its constant name, as Bun exposes on
/// `SQLiteError.code`. The generic `SQLITE_ERROR` (1) has no `code` in Bun (verified against
/// v1.2.21), so it — and any code not listed — returns `None`.
fn result_code_name(code: i32) -> Option<&'static str> {
    let name = match code {
        // primary codes
        2 => "SQLITE_INTERNAL",
        3 => "SQLITE_PERM",
        4 => "SQLITE_ABORT",
        5 => "SQLITE_BUSY",
        6 => "SQLITE_LOCKED",
        7 => "SQLITE_NOMEM",
        8 => "SQLITE_READONLY",
        9 => "SQLITE_INTERRUPT",
        10 => "SQLITE_IOERR",
        11 => "SQLITE_CORRUPT",
        12 => "SQLITE_NOTFOUND",
        13 => "SQLITE_FULL",
        14 => "SQLITE_CANTOPEN",
        15 => "SQLITE_PROTOCOL",
        16 => "SQLITE_EMPTY",
        17 => "SQLITE_SCHEMA",
        18 => "SQLITE_TOOBIG",
        19 => "SQLITE_CONSTRAINT",
        20 => "SQLITE_MISMATCH",
        21 => "SQLITE_MISUSE",
        22 => "SQLITE_NOLFS",
        23 => "SQLITE_AUTH",
        24 => "SQLITE_FORMAT",
        25 => "SQLITE_RANGE",
        26 => "SQLITE_NOTADB",
        27 => "SQLITE_NOTICE",
        28 => "SQLITE_WARNING",
        // extended constraint codes (Bun surfaces these — verified: 1555, 1299, ...)
        275 => "SQLITE_CONSTRAINT_CHECK",
        531 => "SQLITE_CONSTRAINT_COMMITHOOK",
        787 => "SQLITE_CONSTRAINT_FOREIGNKEY",
        1043 => "SQLITE_CONSTRAINT_FUNCTION",
        1299 => "SQLITE_CONSTRAINT_NOTNULL",
        1555 => "SQLITE_CONSTRAINT_PRIMARYKEY",
        1811 => "SQLITE_CONSTRAINT_TRIGGER",
        2067 => "SQLITE_CONSTRAINT_UNIQUE",
        2323 => "SQLITE_CONSTRAINT_VTAB",
        2579 => "SQLITE_CONSTRAINT_ROWID",
        2835 => "SQLITE_CONSTRAINT_PINNED",
        3091 => "SQLITE_CONSTRAINT_DATATYPE",
        // common extended IOERR codes
        266 => "SQLITE_IOERR_READ",
        522 => "SQLITE_IOERR_WRITE",
        1032 => "SQLITE_READONLY_RECOVERY",
        _ => return None,
    };
    Some(name)
}
