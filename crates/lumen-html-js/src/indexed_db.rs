//! Browser storage partition and task admission for the shared native IDB API.
use crate::*;
use lumen_common::indexed_db::MemoryBackend;
use std::sync::{Arc, Mutex};
use lumen_host::indexed_db::SharedBackend;

#[derive(Clone)]
struct DatabaseBackend(SharedBackend);

/// Supply a host profile backend before installing browser documents. The same
/// handle is shared across the interpreter's documents; the default below is
/// explicitly bounded memory storage, without durable browser persistence.
pub fn set_backend(ctx: &mut Ctx, backend: SharedBackend) {
    ctx.op_state().put(DatabaseBackend(backend));
}
pub fn backend(ctx: &mut Ctx) -> SharedBackend {
    if let Some(backend) = ctx.op_state().get::<DatabaseBackend>() { backend.0.clone() }
        else {
            let backend: SharedBackend = Arc::new(Mutex::new(MemoryBackend::new(256, 64 * 1024 * 1024)));
            set_backend(ctx, backend.clone()); backend
        }
}
pub(crate) fn install(ctx: &mut Ctx) -> Result<(), Value> {
    let backend = backend(ctx);
    lumen_host::indexed_db::install(ctx, lumen_host::indexed_db::Environment {
        backend,
        storage_key: Rc::new(|ctx| {
            let realm = window_globals::current_dom_realm(ctx).ok_or_else(|| OpError::new("SecurityError", "IndexedDB has no document storage key"))?;
            let origin = realm.document_origin().or_else(|| realm.browsing_context().map(|context| context.root_or_child_origin()))
                .ok_or_else(|| OpError::new("SecurityError", "IndexedDB has no document origin"))?;
            let serialized = origin.serialize();
            if serialized == "null" { return Err(OpError::thrown(lumen_host::events::dom_exception(ctx, "Opaque origin cannot use IndexedDB", "SecurityError"))); }
            Ok(serialized)
        }),
        queue: Rc::new(|ctx, task| scheduling::queue_task(ctx, task)),
    }).map_err(|failure| failure.to_value(ctx))
}

/// Worker callers pass the immutable creator storage key and profile backend;
/// author replacements of `location` never alter the storage partition.
pub fn install_worker(ctx: &mut Ctx, storage_key: String, backend: SharedBackend) -> Result<(), Value> {
    set_backend(ctx, backend.clone());
    lumen_host::indexed_db::install(ctx, lumen_host::indexed_db::Environment {
        backend,
        storage_key: Rc::new(move |ctx| {
            if storage_key == "null" { return Err(OpError::thrown(lumen_host::events::dom_exception(ctx, "Opaque worker cannot use IndexedDB", "SecurityError"))); }
            Ok(storage_key.clone())
        }),
        queue: Rc::new(|ctx, task| scheduling::queue_task(ctx, task)),
    }).map_err(|failure| failure.to_value(ctx))
}
