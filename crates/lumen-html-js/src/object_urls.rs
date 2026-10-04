//! Browser-owned byte snapshots for Blob object URLs.
//!
//! The JavaScript Blob and URL implementations retain their ordinary Node
//! compatibility objects. This typed host registry gives managed browser
//! resource loaders a private, per-interpreter byte/MIME view without reading
//! mutable globals or duplicating URL ownership.
use crate::*;
use std::{collections::HashMap, rc::Rc};

const MAX_OBJECT_URL_BYTES: usize = 32 * 1024 * 1024;
const MAX_REGISTRY_BYTES: usize = 64 * 1024 * 1024;
const MAX_REGISTRY_ENTRIES: usize = 4096;
const MAX_ID_BYTES: usize = 128;
const MAX_CONTENT_TYPE_BYTES: usize = 255;

#[derive(Clone)]
pub struct ObjectUrlResource {
    pub bytes: Rc<[u8]>,
    pub content_type: String,
}

#[derive(Default)]
struct Registry {
    entries: HashMap<String, ObjectUrlResource>,
    bytes: usize,
    id_provider: Option<Rc<dyn Fn() -> Result<String, String>>>,
}

#[lumen_bind::module(name = "blob_object_urls")]
mod globals {
    use super::*;

    #[op(name = "__lumenCreateObjectURL")]
    fn create(ctx: &mut Ctx, bytes: &[u8], content_type: String) -> OpResult<String> {
        if bytes.len() > MAX_OBJECT_URL_BYTES {
            return Err(OpError::new(
                "QuotaExceededError",
                "Blob object URL exceeds the resource limit",
            ));
        }
        if content_type.len() > MAX_CONTENT_TYPE_BYTES
            || !content_type
                .bytes()
                .all(|byte| (0x20..=0x7e).contains(&byte))
        {
            return Err(OpError::new("TypeError", "Blob type is invalid"));
        }
        let registry = ctx.host_mut::<Registry>().ok_or_else(|| {
            OpError::new(
                "InvalidStateError",
                "Blob object URL registry is unavailable",
            )
        })?;
        let id_provider = registry.id_provider.clone().ok_or_else(|| {
            OpError::new(
                "NotSupportedError",
                "secure Blob URL identifier source is unavailable",
            )
        })?;
        if registry.entries.len() >= MAX_REGISTRY_ENTRIES {
            return Err(OpError::new(
                "QuotaExceededError",
                "Blob object URL registry is full",
            ));
        }
        let next_total = registry.bytes.saturating_add(bytes.len());
        if next_total > MAX_REGISTRY_BYTES {
            return Err(OpError::new(
                "QuotaExceededError",
                "Blob object URL registry is full",
            ));
        }
        let mut unique_id = None;
        for _ in 0..8 {
            let id = id_provider().map_err(|error| OpError::new("NotSupportedError", error))?;
            if !id.is_empty()
                && id.len() <= MAX_ID_BYTES
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_~.".contains(&byte))
                && !registry.entries.contains_key(&id)
            {
                unique_id = Some(id);
                break;
            }
        }
        let id = unique_id.ok_or_else(|| {
            OpError::new(
                "OperationError",
                "could not allocate a unique Blob URL identifier",
            )
        })?;
        registry.bytes = next_total;
        registry.entries.insert(
            id.clone(),
            ObjectUrlResource {
                bytes: Rc::from(bytes),
                content_type: content_type.to_ascii_lowercase(),
            },
        );
        Ok(id)
    }

    #[op(name = "__lumenRevokeObjectURL")]
    fn revoke(ctx: &mut Ctx, id: String) {
        if let Some(registry) = ctx.host_mut::<Registry>() {
            if let Some(removed) = registry.entries.remove(&id) {
                registry.bytes = registry.bytes.saturating_sub(removed.bytes.len());
            }
        }
    }
}

pub(crate) fn install(ctx: &mut Ctx) -> Result<(), Value> {
    if !ctx.op_state().has::<Registry>() {
        ctx.op_state().put(Registry::default());
        ctx.install_module::<globals::Module>(&ctx.global_object())?;
    }
    Ok(())
}

/// Read an object URL resource through typed interpreter host state.
pub fn get(ctx: &mut Ctx, id: &str) -> Option<ObjectUrlResource> {
    ctx.host_mut::<Registry>()?.entries.get(id).cloned()
}

/// Install the browser transport's cryptographic token source for object URLs.
pub fn set_id_provider(ctx: &mut Ctx, provider: Rc<dyn Fn() -> Result<String, String>>) {
    if let Some(registry) = ctx.host_mut::<Registry>() {
        registry.id_provider = Some(provider);
    }
}
