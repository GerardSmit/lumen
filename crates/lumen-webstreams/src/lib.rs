//! Host-independent WHATWG Streams installation.
//!
//! Stream algorithms are generated from the same vendored Node 20.11 sources used by
//! `lumen-node`, but this extension installs only the browser-compatible `stream/web` surface.
//! It has no Node, filesystem, networking, TLS, or cryptography runtime dependency. Compression
//! streams are omitted because their upstream implementation requires Node's zlib/Duplex backend.

use lumen_host::Extension;

const JS_GLUE: &str = include_str!(concat!(env!("OUT_DIR"), "/webstreams_browser.js"));
const JS_GLUE_AOT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/webstreams_browser.aot"));

/// Extension installing Readable/Writable/Transform streams, byte/BYOB readers, queuing
/// strategies, and text encoder/decoder streams. Install after host globals such as
/// `TextEncoder`, `TextDecoder`, `AbortController`, `DOMException`, and `queueMicrotask` are
/// available.
pub fn extension() -> Extension {
    Extension {
        name: "web-streams",
        modules: &[],
        state_init: None,
        js_init: Some(JS_GLUE),
        js_init_snapshot: Some(JS_GLUE_AOT),
    }
}

/// Source used by the extension's compiler-mode fallback and diagnostics.
pub fn source() -> &'static str {
    JS_GLUE
}
