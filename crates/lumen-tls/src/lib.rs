//! TLS transport for the runtime's `https`, `tls`, `fetch` and WebSocket clients.
//!
//! Unix loads the system OpenSSL at runtime (see `openssl`). Other targets have no backend yet:
//! [`TlsStream`] exists so the runtime builds, and every connect or accept fails with a plain
//! error the JS side reports like any failed connection.

#[cfg(unix)]
mod openssl;
#[cfg(unix)]
pub use openssl::TlsStream;

#[cfg(not(unix))]
mod unavailable;
#[cfg(not(unix))]
pub use unavailable::TlsStream;
