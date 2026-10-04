//! TLS transport for the runtime's `https`, `tls`, `fetch` and WebSocket clients.
//!
//! Desktop Unix loads system OpenSSL at runtime (see `openssl`). Android and Windows use rustls
//! on the ring provider, trusting the operating system's certificate store (see `rustls`). Both
//! backends expose the same [`TlsStream`] surface.

#[cfg(all(unix, not(target_os = "android")))]
mod openssl;
#[cfg(all(unix, not(target_os = "android")))]
pub use openssl::TlsStream;
#[cfg(all(unix, not(target_os = "android")))]
pub mod engine;

mod rustls;
pub use rustls::{
    client_config_with_roots, install_runtime_crypto, ClientSession, PlaintextRead,
    RuntimeRandomFill, RuntimeUnixTimeSource,
};
#[cfg(any(not(unix), target_os = "android"))]
pub use rustls::TlsStream;
