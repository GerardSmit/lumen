//! Browser-owned byte views of Blob object URLs.
//!
//! `URL.createObjectURL` registers blobs in lumen-host's interpreter-scoped registry; installing
//! this module bounds that registry with the browser's resource limits. Managed resource loaders
//! read the registered bytes and media type through [`get`], without consulting mutable globals.
use crate::*;
use lumen_host::blob::ObjectUrlLimits;
use std::rc::Rc;

pub use lumen_host::blob::ObjectUrlResource;

pub(crate) fn install(ctx: &mut Ctx) -> Result<(), Value> {
    lumen_host::blob::set_object_url_limits(ctx, Some(ObjectUrlLimits::default()));
    Ok(())
}

/// Read an object URL resource through typed interpreter host state.
pub fn get(ctx: &mut Ctx, id: &str) -> Option<ObjectUrlResource> {
    lumen_host::blob::object_url_resource(ctx, id)
}

/// Install the browser transport's cryptographic token source for object URLs.
pub fn set_id_provider(ctx: &mut Ctx, provider: Rc<dyn Fn() -> Result<String, String>>) {
    lumen_host::blob::set_token_provider(ctx, provider);
}
