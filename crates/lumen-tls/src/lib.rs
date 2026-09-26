//! TLS transport for the runtime's `https`, `tls`, `fetch` and WebSocket clients.
//!
//! Unix loads the system OpenSSL at runtime (see `openssl`). Other targets (Windows) use rustls
//! on the ring provider, trusting the operating system's certificate store (see `rustls`). Both
//! backends expose the same [`TlsStream`] surface.

#[cfg(unix)]
mod openssl;
#[cfg(unix)]
pub use openssl::TlsStream;

#[cfg(not(unix))]
mod rustls;
#[cfg(not(unix))]
pub use self::rustls::TlsStream;
