//! Browser-owned byte views of Blob object URLs.
//!
//! `URL.createObjectURL` registers blobs in lumen-host's interpreter-scoped registry; installing
//! this module bounds that registry with the browser's resource limits. Managed resource loaders
//! read the registered bytes and media type through [`get`], without consulting mutable globals.
use crate::*;
use lumen_host::blob::ObjectUrlLimits;
use std::rc::Rc;
use crate::browsing_context::Origin;

struct BrowserEnvironment {
    origin: Origin,
    owner: usize,
    policy_container: crate::csp::PolicyContainer,
    creator: std::rc::Weak<DomRealm>,
}

impl lumen_host::blob::ObjectUrlEnvironment for BrowserEnvironment {
    fn serialized_origin(&self) -> String { self.origin.serialize() }
    fn owner_identity(&self) -> usize { self.owner }
    fn same_partition(&self, other: &dyn lumen_host::blob::ObjectUrlEnvironment) -> bool {
        // The standard storage key for non-storage purposes is keyed by origin.
        other.as_any().downcast_ref::<Self>().is_some_and(|other| self.origin.same_origin(&other.origin))
    }
    fn as_any(&self) -> &dyn std::any::Any { self }
    fn retained_bytes(&self) -> usize { self.policy_container.retained_bytes() }
}

pub use lumen_host::blob::ObjectUrlResource;

pub(crate) fn install(ctx: &mut Ctx) -> Result<(), Value> {
    lumen_host::blob::set_object_url_limits(ctx, Some(ObjectUrlLimits::default()));
    lumen_host::blob::ensure_random_token_provider(ctx);
    lumen_host::blob::set_object_url_environment_provider(ctx, Rc::new(|ctx| {
        let Some(realm) = crate::window_globals::current_dom_realm(ctx) else { return Ok(None); };
        let origin = realm.document_origin().or_else(|| realm.browsing_context().map(|context| context.root_or_child_origin()))
            .ok_or_else(|| OpError::new("InvalidStateError", "Blob URL creator has no origin"))?;
        let owner = Rc::as_ptr(&realm) as usize;
        Ok(Some(Rc::new(BrowserEnvironment { origin, owner, policy_container: realm.policy_container(), creator: Rc::downgrade(&realm) }) as Rc<dyn lumen_host::blob::ObjectUrlEnvironment>))
    }));
    Ok(())
}

/// Read an object URL resource through typed interpreter host state.
pub fn get(ctx: &mut Ctx, id: &str) -> Option<ObjectUrlResource> {
    lumen_host::blob::object_url_resource(ctx, id)
}

/// Navigation uses the initiator captured when the request was made, even
/// when a different document's owner loop later resolves its bytes.
pub fn get_for_origin(ctx: &mut Ctx, url: &str, origin: &Origin) -> Option<ObjectUrlResource> {
    lumen_host::blob::object_url_resource_for_environment(ctx, url,
        Some(Rc::new(BrowserEnvironment { origin:origin.clone(), owner:0, policy_container: Default::default(), creator: Default::default() })))
}
pub(crate) fn policy_container(ctx: &mut Ctx, url: &str) -> Option<crate::csp::PolicyContainer> {
    lumen_host::blob::object_url_environment(ctx, url)?.as_any()
        .downcast_ref::<BrowserEnvironment>().map(|environment| environment.creator.upgrade()
            .map_or_else(|| environment.policy_container.clone(), |creator|creator.policy_container()))
}

pub fn resource_origin(resource: &ObjectUrlResource) -> Option<Origin> {
    resource.environment.as_ref()?.as_any().downcast_ref::<BrowserEnvironment>().map(|environment| environment.origin.clone())
}

pub(crate) fn origin(ctx: &mut Ctx, url: &str) -> Option<Origin> {
    lumen_host::blob::object_url_environment(ctx, url)?.as_any()
        .downcast_ref::<BrowserEnvironment>().map(|environment| environment.origin.clone())
}

pub(crate) fn retire_document(ctx: &mut Ctx, document: &Rc<DomRealm>) {
    lumen_host::blob::revoke_object_urls_for_environment(ctx, Rc::as_ptr(document) as usize);
}

/// Install the browser transport's cryptographic token source for object URLs.
pub fn set_id_provider(ctx: &mut Ctx, provider: Rc<dyn Fn() -> Result<String, String>>) {
    lumen_host::blob::set_token_provider(ctx, provider);
}
