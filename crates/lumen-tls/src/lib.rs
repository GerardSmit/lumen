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

#[cfg(any(not(unix), target_os = "android"))]
mod rustls;
#[cfg(any(not(unix), target_os = "android"))]
pub use self::rustls::TlsStream;
