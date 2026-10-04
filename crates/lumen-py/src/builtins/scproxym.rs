//! `_scproxy`, which `urllib.request` imports on macOS. CPython reads the proxy settings from
//! SystemConfiguration; this reports none configured, so only the `*_proxy` environment
//! variables apply.

#[lumen_bind::module(name = "_scproxy")]
pub mod _scproxy {
    use crate::object::*;
    use crate::vm::{dict_set_str, Interp};

    /// The proxy bypass settings: no simple-host exclusion and no exceptions.
    #[op]
    fn _get_proxy_settings(it: &mut Interp) -> Value {
        let d = it.new_dict();
        dict_set_str(&d, "exclude_simple", Value::Bool(false));
        dict_set_str(&d, "exceptions", Value::tuple(Vec::new()));
        Value::Obj(d)
    }

    /// The system proxies by scheme: none.
    #[op]
    fn _get_proxies(it: &mut Interp) -> Value {
        Value::Obj(it.new_dict())
    }
}
