//! The object-URL registry behind `URL.createObjectURL`, scoped to one interpreter.
//!
//! An entry holds the blob's [`Source`], never the `Blob` wrapper, so a registered URL does not
//! keep a JavaScript object (and whatever it references) alive; a file-backed source retains its
//! read callback until the URL is revoked.

use super::{blob_of, Blob, Bytes, Source};
use lumen::embed::{Ctx, OpError, OpResult, Value};
use std::{collections::HashMap, rc::Rc};

const MAX_ID_BYTES: usize = 128;
const MAX_CONTENT_TYPE_BYTES: usize = 255;
const URL_PREFIX: &str = "blob:nodedata:";

/// Resource limits a browser host puts on the registry. Without them the registry is unbounded,
/// as in Node.
#[derive(Clone, Copy)]
pub struct ObjectUrlLimits {
    pub max_url_bytes: usize,
    pub max_registry_bytes: usize,
    pub max_entries: usize,
}

impl Default for ObjectUrlLimits {
    fn default() -> Self {
        Self {
            max_url_bytes: 32 * 1024 * 1024,
            max_registry_bytes: 64 * 1024 * 1024,
            max_entries: 4096,
        }
    }
}

/// The bytes and media type an object URL names.
#[derive(Clone)]
pub struct ObjectUrlResource {
    pub bytes: Bytes,
    pub content_type: String,
}

struct Entry {
    source: Source,
    kind: String,
}

type TokenProvider = Rc<dyn Fn() -> Result<String, String>>;

#[derive(Default)]
struct Registry {
    entries: HashMap<String, Entry>,
    bytes: usize,
    limits: Option<ObjectUrlLimits>,
    provider: Option<TokenProvider>,
}

fn registry(ctx: &mut Ctx) -> &mut Registry {
    if !ctx.op_state().has::<Registry>() {
        ctx.op_state().put(Registry::default());
    }
    ctx.op_state()
        .get_mut::<Registry>()
        .expect("the object URL registry was just installed")
}

/// Bounds the registry (a browser host); `None` removes the bounds.
pub fn set_object_url_limits(ctx: &mut Ctx, limits: Option<ObjectUrlLimits>) {
    registry(ctx).limits = limits;
}

/// The host's cryptographic source of object-URL identifiers and multipart boundaries. A bounded
/// registry refuses to create URLs without one.
pub fn set_token_provider(ctx: &mut Ctx, provider: TokenProvider) {
    registry(ctx).provider = Some(provider);
}

/// A random token: the host's provider when set, else 128 bits from the operating system.
pub(crate) fn random_token(ctx: &mut Ctx) -> OpResult<String> {
    match registry(ctx).provider.clone() {
        Some(provider) => provider().map_err(|error| OpError::new("NotSupportedError", error)),
        None => crate::random::random_bytes(16).map(|bytes| bytes.iter().map(|byte| format!("{byte:02x}")).collect()),
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_~.".contains(&byte))
}

fn quota(message: &'static str) -> OpError {
    OpError::new("QuotaExceededError", message)
}

/// Registers `blob` and returns its `blob:nodedata:<id>` URL; `None` when `blob` is not a Blob.
pub fn create_object_url(ctx: &mut Ctx, blob: &Value) -> OpResult<Option<String>> {
    let Some(view) = blob_of(ctx, blob) else {
        return Ok(None);
    };
    let size = view.source.len();
    let (limits, provider) = {
        let registry = registry(ctx);
        (registry.limits, registry.provider.clone())
    };
    if let Some(limits) = limits {
        if size > limits.max_url_bytes {
            return Err(quota("Blob object URL exceeds the resource limit"));
        }
        if view.content_type.len() > MAX_CONTENT_TYPE_BYTES {
            return Err(OpError::new("TypeError", "Blob type is invalid"));
        }
        let registry = registry(ctx);
        if registry.entries.len() >= limits.max_entries
            || registry.bytes.saturating_add(size) > limits.max_registry_bytes
        {
            return Err(quota("Blob object URL registry is full"));
        }
        if provider.is_none() {
            return Err(OpError::new(
                "NotSupportedError",
                "secure Blob URL identifier source is unavailable",
            ));
        }
    }
    let mut unique = None;
    for _ in 0..8 {
        let id = match &provider {
            Some(provider) => {
                provider().map_err(|error| OpError::new("NotSupportedError", error))?
            }
            None => crate::random::random_uuid()?,
        };
        if valid_id(&id) && !registry(ctx).entries.contains_key(&id) {
            unique = Some(id);
            break;
        }
    }
    let id = unique.ok_or_else(|| {
        OpError::new(
            "OperationError",
            "could not allocate a unique Blob URL identifier",
        )
    })?;
    let registry = registry(ctx);
    registry.bytes = registry.bytes.saturating_add(size);
    registry.entries.insert(
        id.clone(),
        Entry {
            source: view.source,
            kind: view.content_type,
        },
    );
    Ok(Some(format!("{URL_PREFIX}{id}")))
}

/// Drops the entry for `id` (the part of the URL after `blob:nodedata:`).
pub fn revoke_object_url(ctx: &mut Ctx, id: &str) {
    let registry = registry(ctx);
    if let Some(removed) = registry.entries.remove(id) {
        registry.bytes = registry.bytes.saturating_sub(removed.source.len());
    }
}

fn lookup(ctx: &mut Ctx, id: &str) -> Option<(Source, String)> {
    let entry = registry(ctx).entries.get(id)?;
    Some((entry.source.clone(), entry.kind.clone()))
}

/// A new `Blob` over the registered bytes; Node's `resolveObjectURL`.
pub fn resolve_object_url(ctx: &mut Ctx, id: &str) -> Option<Value> {
    let (source, kind) = lookup(ctx, id)?;
    Some(ctx.new_instance(Blob::from_source(source, kind)))
}

/// The registered bytes and media type, reading a file-backed blob.
pub fn object_url_resource(ctx: &mut Ctx, id: &str) -> Option<ObjectUrlResource> {
    let (source, kind) = lookup(ctx, id)?;
    let bytes = source.bytes(ctx).ok()?;
    Some(ObjectUrlResource {
        bytes,
        content_type: kind,
    })
}
