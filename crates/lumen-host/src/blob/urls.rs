//! The object-URL registry behind `URL.createObjectURL`, scoped to one interpreter.
//!
//! An entry holds the blob's [`Source`], never the `Blob` wrapper, so a registered URL does not
//! keep a JavaScript object (and whatever it references) alive; a file-backed source retains its
//! read callback until the URL is revoked.

use super::{blob_of, Blob, Bytes, Source};
use lumen::embed::{Ctx, OpError, OpResult, Value};
use std::{any::Any, borrow::Cow, cell::Cell, collections::HashMap, rc::Rc};

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
    pub environment: Option<Rc<dyn ObjectUrlEnvironment>>,
}

/// Embedding policy for browser object URLs. The byte store remains shared
/// with Node; browser origins and storage partitions belong to the embedding.
pub trait ObjectUrlEnvironment: Any {
    fn serialized_origin(&self) -> String;
    fn owner_identity(&self) -> usize;
    fn same_partition(&self, other: &dyn ObjectUrlEnvironment) -> bool;
    fn as_any(&self) -> &dyn Any;
    /// Pure retained embedding metadata participates in the registry budget.
    fn retained_bytes(&self) -> usize { 0 }
}

pub type ObjectUrlEnvironmentProvider = Rc<dyn Fn(&mut Ctx) -> OpResult<Option<Rc<dyn ObjectUrlEnvironment>>>>;

struct Entry {
    source: Source,
    kind: String,
    environment: Option<Rc<dyn ObjectUrlEnvironment>>,
    canonical: Cell<Option<bool>>,
}
impl Entry {
    fn retained_bytes(&self) -> usize {
        self.source.len().saturating_add(self.environment.as_ref().map_or(0, |environment|environment.retained_bytes()))
    }
}

type TokenProvider = Rc<dyn Fn() -> Result<String, String>>;

#[derive(Default)]
struct Registry {
    entries: HashMap<String, Entry>,
    bytes: usize,
    limits: Option<ObjectUrlLimits>,
    provider: Option<TokenProvider>,
    environment_provider: Option<ObjectUrlEnvironmentProvider>,
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

pub fn ensure_random_token_provider(ctx: &mut Ctx) {
    if registry(ctx).provider.is_none() {
        registry(ctx).provider = Some(Rc::new(|| crate::random::random_uuid().map_err(|error| format!("{error:?}"))));
    }
}

pub fn set_object_url_environment_provider(ctx: &mut Ctx, provider: ObjectUrlEnvironmentProvider) {
    registry(ctx).environment_provider = Some(provider);
}

fn current_environment(ctx: &mut Ctx) -> OpResult<Option<Rc<dyn ObjectUrlEnvironment>>> {
    match registry(ctx).environment_provider.clone() {
        Some(provider) => provider(ctx),
        None => Ok(None),
    }
}

fn canonical_key(input: &str) -> Option<String> {
    let mut url = lumen_common::url::parse(input, None).ok()?;
    if url.scheme != "blob" { return None; }
    url.fragment = None;
    Some(url.href())
}

fn resource_key<'a>(entries: &HashMap<String, Entry>, input: &'a str) -> Option<Cow<'a, str>> {
    if input.starts_with("blob:") {
        if !input.contains('#') {
            if let Some(entry) = entries.get(input) {
                let canonical = entry.canonical.get().unwrap_or_else(|| {
                    let canonical = canonical_key(input).as_deref() == Some(input);
                    entry.canonical.set(Some(canonical));
                    canonical
                });
                if canonical { return Some(Cow::Borrowed(input)); }
            }
        }
        canonical_key(input).map(Cow::Owned)
    } else { Some(Cow::Owned(format!("{URL_PREFIX}{input}"))) }
}

fn authorized(entry: &Option<Rc<dyn ObjectUrlEnvironment>>, caller: &Option<Rc<dyn ObjectUrlEnvironment>>) -> bool {
    match (entry, caller) {
        (Some(entry), Some(caller)) => entry.same_partition(caller.as_ref()),
        (None, None) => true,
        _ => false,
    }
}

/// The document-unloading hook removes registrations, without rooting its realm.
pub fn revoke_object_urls_for_environment(ctx: &mut Ctx, owner: usize) {
    let registry = registry(ctx);
    let mut removed = 0usize;
    registry.entries.retain(|_, entry| {
        let keep = !entry.environment.as_ref().is_some_and(|environment| environment.owner_identity() == owner);
        if !keep { removed = removed.saturating_add(entry.retained_bytes()); }
        keep
    });
    registry.bytes = registry.bytes.saturating_sub(removed);
    lumen_common::fasthash::shrink_sparse(&mut registry.entries);
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
    let environment = current_environment(ctx)?;
    let retained_size = size.saturating_add(environment.as_ref().map_or(0, |environment|environment.retained_bytes()));
    let origin = environment.as_ref().map(|environment| environment.serialized_origin());
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
            || registry.bytes.saturating_add(retained_size) > limits.max_registry_bytes
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
        let mut url = String::with_capacity(id.len() + origin.as_ref().map_or(URL_PREFIX.len(), |origin| origin.len() + 6));
        match &origin {
            Some(origin) => { url.push_str("blob:"); url.push_str(origin); url.push('/'); }
            None => url.push_str(URL_PREFIX),
        }
        url.push_str(&id);
        if valid_id(&id) && !registry(ctx).entries.contains_key(&url) {
            unique = Some(url);
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
    registry.bytes = registry.bytes.saturating_add(retained_size);
    registry.entries.insert(
        id.clone(),
        Entry {
            source: view.source,
            kind: view.content_type,
            environment,
            canonical: Cell::new(None),
        },
    );
    Ok(Some(id))
}

/// Drops the entry for `id` (the part of the URL after `blob:nodedata:`).
pub fn revoke_object_url(ctx: &mut Ctx, id: &str) {
    let Ok(caller) = current_environment(ctx) else { return; };
    let registry = registry(ctx);
    let Some(key) = resource_key(&registry.entries, id) else { return; };
    let key = key.as_ref();
    if !registry.entries.get(key).is_some_and(|entry| authorized(&entry.environment, &caller)) { return; }
    if let Some(removed) = registry.entries.remove(key) {
        registry.bytes = registry.bytes.saturating_sub(removed.retained_bytes());
        lumen_common::fasthash::shrink_sparse(&mut registry.entries);
    }
}

fn lookup(ctx: &mut Ctx, id: &str) -> Option<(Source, String, Option<Rc<dyn ObjectUrlEnvironment>>)> {
    let caller = current_environment(ctx).ok()?;
    lookup_for_environment(ctx, id, &caller)
}

fn lookup_for_environment(ctx: &mut Ctx, id: &str, caller: &Option<Rc<dyn ObjectUrlEnvironment>>) -> Option<(Source, String, Option<Rc<dyn ObjectUrlEnvironment>>)> {
    let entries = &registry(ctx).entries;
    let key = resource_key(entries, id)?;
    let entry = entries.get(key.as_ref())?;
    if !authorized(&entry.environment, caller) { return None; }
    Some((entry.source.clone(), entry.kind.clone(), entry.environment.clone()))
}

/// A new `Blob` over the registered bytes; Node's `resolveObjectURL`.
pub fn resolve_object_url(ctx: &mut Ctx, id: &str) -> Option<Value> {
    let (source, kind, _) = lookup(ctx, id)?;
    Some(ctx.new_instance(Blob::from_source(source, kind)))
}

/// The registered bytes and media type, reading a file-backed blob.
pub fn object_url_resource(ctx: &mut Ctx, id: &str) -> Option<ObjectUrlResource> {
    let caller = current_environment(ctx).ok()?;
    object_url_resource_for_environment(ctx, id, caller)
}

pub fn object_url_resource_for_environment(ctx: &mut Ctx, id: &str, caller: Option<Rc<dyn ObjectUrlEnvironment>>) -> Option<ObjectUrlResource> {
    let (source, kind, environment) = lookup_for_environment(ctx, id, &caller)?;
    let bytes = source.bytes(ctx).ok()?;
    Some(ObjectUrlResource {
        bytes,
        content_type: kind,
        environment,
    })
}

/// Creator metadata without reading a file-backed resource.
pub fn object_url_environment(ctx: &mut Ctx, id: &str) -> Option<Rc<dyn ObjectUrlEnvironment>> {
    lookup(ctx, id)?.2
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(keys: &[&str]) -> HashMap<String, Entry> {
        keys.iter().map(|key| ((*key).to_owned(), Entry {
            source: Source::Memory(Default::default()),
            kind: String::new(),
            environment: None,
            canonical: Cell::new(None),
        })).collect()
    }

    #[test]
    fn exact_canonical_key_skips_parsing_and_matches_the_slow_path() {
        let key = "blob:https://example.com/3f1c2a4e-0000-4000-8000-000000000000";
        let entries = entries(&[key]);
        let fast = resource_key(&entries, key).unwrap();
        assert!(matches!(fast, Cow::Borrowed(_)));
        assert_eq!(fast.as_ref(), canonical_key(key).unwrap());
    }

    #[test]
    fn fragment_is_stripped_and_unknown_keys_use_the_slow_path() {
        let key = "blob:https://example.com/abc";
        let entries = entries(&[key]);
        let with_fragment = format!("{key}#frag");
        assert_eq!(resource_key(&entries, &with_fragment).unwrap().as_ref(), key);
        assert_eq!(resource_key(&entries, "blob:https://other.example/abc").unwrap().as_ref(), "blob:https://other.example/abc");
        assert_eq!(resource_key(&entries, "abc").unwrap().as_ref(), "blob:nodedata:abc");
    }

    #[test]
    fn non_canonical_entry_keys_are_never_returned_verbatim() {
        let key = "blob:HTTPS://Example.com/abc";
        let entries = entries(&[key]);
        let resolved = resource_key(&entries, key);
        assert_eq!(resolved.as_deref(), canonical_key(key).as_deref());
    }
}
