//! The native `Navigator` interface and the `navigator` global.
//!
//! `Navigator` carries the properties every Lumen realm shares (`userAgent`); a DOM realm
//! extends it with `clipboard`, `permissions` and the other browsing-context members
//! (`DomNavigator` in `lumen-html-js`, `extends = Navigator`). The `navigator` global is a lazy
//! constant of the module, so a realm that already defines it keeps its own.

/// The value of `navigator.userAgent`.
pub const USER_AGENT: &str = "lumen";

#[lumen_bind::module(name = "navigator")]
pub mod bindings {
    use super::USER_AGENT;

    #[class(name = "Navigator", hint(js(webidl, invalid_this)))]
    pub struct Navigator;

    /// `globalThis.navigator`.
    #[constant(name = "navigator", enumerable)]
    const NAVIGATOR: Navigator = Navigator;

    #[methods]
    impl Navigator {
        #[getter]
        fn user_agent(&self) -> &'static str {
            USER_AGENT
        }
    }
}

pub use bindings::Navigator;
