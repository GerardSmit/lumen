//! Engine-independent shared-worker identity.
//!
//! A shared worker is reused only when its canonical script URL, origin, script
//! type, credentials mode, and name all match. Hosts provide their own
//! owner-local realm and port transports; the key remains common to every host.

extern crate alloc;

use alloc::string::String;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SharedWorkerKey {
    pub url: String,
    pub origin: String,
    pub is_module: bool,
    pub credentials: crate::cors::Credentials,
    pub name: String,
}

impl SharedWorkerKey {
    pub fn new(
        url: impl Into<String>,
        origin: impl Into<String>,
        is_module: bool,
        credentials: crate::cors::Credentials,
        name: impl Into<String>,
    ) -> Self {
        Self {
            url: url.into(),
            origin: origin.into(),
            is_module,
            credentials,
            name: name.into(),
        }
    }
}
