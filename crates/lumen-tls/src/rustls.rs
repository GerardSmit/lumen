//! TLS over rustls (ring provider).
//!
//! Same surface and I/O semantics as `openssl`: the handshake completes inside `connect`/`accept`,
//! reads block on the socket, a socket read timeout surfaces as `ErrorKind::Interrupted` (the
//! callers poll with short timeouts and retry on it), and a peer that closes without
//! `close_notify` reads as a clean EOF. Clients trust the operating system's certificate store,
//! falling back to the bundled Mozilla roots when that store yields nothing.

#[cfg(any(test, not(unix), target_os = "android"))]
use std::collections::HashMap;
use std::io::{Cursor, ErrorKind, Read, Write};
#[cfg(any(test, not(unix), target_os = "android"))]
use std::net::TcpStream;
use std::sync::{Arc, OnceLock};
#[cfg(any(test, not(unix), target_os = "android"))]
use std::sync::Mutex;
use std::time::Duration;
#[cfg(any(test, not(unix), target_os = "android"))]
use std::time::Instant;

#[cfg(any(test, not(unix), target_os = "android"))]
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{self, CryptoProvider};
#[cfg(any(test, not(unix), target_os = "android"))]
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
#[cfg(any(test, not(unix), target_os = "android"))]
use rustls::pki_types::PrivateKeyDer;
use rustls::time_provider::TimeProvider;
use rustls::{ClientConfig, ClientConnection, ProtocolVersion, RootCertStore};
#[cfg(any(test, not(unix), target_os = "android"))]
use rustls::{DigitallySignedStruct, SignatureScheme};
#[cfg(any(test, not(unix), target_os = "android"))]
use rustls::{CipherSuite, Connection, ServerConfig, ServerConnection};

pub type RuntimeRandomFill = fn(&mut [u8]) -> Result<(), String>;
pub type RuntimeUnixTimeSource = fn() -> Option<u64>;

#[derive(Clone, Copy)]
struct RuntimeCrypto {
    random_fill: RuntimeRandomFill,
    unix_time: RuntimeUnixTimeSource,
}

static RUNTIME_CRYPTO: OnceLock<RuntimeCrypto> = OnceLock::new();

/// Install strong entropy and synchronized wall-clock providers for transport-free TLS clients.
/// The entropy callback must fill the entire slice from a CSPRNG or return an error. The clock
/// must return Unix seconds only when certificate validity can be checked against real time.
pub fn install_runtime_crypto(
    random_fill: RuntimeRandomFill,
    unix_time: RuntimeUnixTimeSource,
) -> Result<(), String> {
    RUNTIME_CRYPTO
        .set(RuntimeCrypto {
            random_fill,
            unix_time,
        })
        .map_err(|_| String::from("TLS runtime crypto providers are already installed"))
}

#[derive(Debug)]
struct RuntimeSecureRandom;

impl rustls::crypto::SecureRandom for RuntimeSecureRandom {
    fn fill(&self, bytes: &mut [u8]) -> Result<(), rustls::crypto::GetRandomFailed> {
        let runtime = RUNTIME_CRYPTO
            .get()
            .ok_or(rustls::crypto::GetRandomFailed)?;
        (runtime.random_fill)(bytes).map_err(|_| rustls::crypto::GetRandomFailed)
    }
}

static RUNTIME_SECURE_RANDOM: RuntimeSecureRandom = RuntimeSecureRandom;

#[derive(Debug)]
struct RuntimeTimeProvider;

impl TimeProvider for RuntimeTimeProvider {
    fn current_time(&self) -> Option<UnixTime> {
        let seconds = (RUNTIME_CRYPTO.get()?.unix_time)()?;
        Some(UnixTime::since_unix_epoch(Duration::from_secs(seconds)))
    }
}

fn runtime_provider() -> Result<Arc<CryptoProvider>, String> {
    if RUNTIME_CRYPTO.get().is_none() {
        return Err(String::from(
            "TLS runtime entropy and clock providers are unavailable",
        ));
    }
    static PROVIDER: OnceLock<Arc<CryptoProvider>> = OnceLock::new();
    Ok(PROVIDER
        .get_or_init(|| {
            let mut provider = crypto::ring::default_provider();
            provider.secure_random = &RUNTIME_SECURE_RANDOM;
            Arc::new(provider)
        })
        .clone())
}

/// Construct a verified Rustls client config from the bundled Mozilla roots plus caller roots.
/// Certificate verification uses the clock installed by [`install_runtime_crypto`].
pub fn client_config_with_roots(
    protocols: &[String],
    extra_roots: &[Vec<u8>],
) -> Result<Arc<ClientConfig>, String> {
    validate_alpn(protocols)?;
    let provider = runtime_provider()?;
    let runtime = RUNTIME_CRYPTO
        .get()
        .ok_or_else(|| String::from("TLS runtime entropy and clock providers are unavailable"))?;
    if (runtime.unix_time)().is_none() {
        return Err(String::from("TLS requires a synchronized wall clock"));
    }
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    for root in extra_roots {
        roots
            .add(CertificateDer::from(root.clone()))
            .map_err(|error| format!("invalid TLS trust anchor: {error}"))?;
    }
    let builder = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| error.to_string())?;
    let mut config = builder.with_root_certificates(roots).with_no_client_auth();
    config.alpn_protocols = protocols
        .iter()
        .map(|protocol| protocol.as_bytes().to_vec())
        .collect();
    config.time_provider = Arc::new(RuntimeTimeProvider);
    Ok(Arc::new(config))
}

/// A nonblocking Rustls client session driven by an external transport.
///
/// `connect` queues its initial ClientHello. The owner exchanges bytes with its transport by
/// passing received TLS records to [`feed_encrypted`](Self::feed_encrypted) and sending the bytes
/// returned by [`take_encrypted`](Self::take_encrypted). Plaintext I/O never touches a socket.
pub struct ClientSession {
    connection: ClientConnection,
    transport_eof: bool,
}

/// Result of attempting a nonblocking plaintext read from [`ClientSession`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaintextRead {
    /// `length` bytes were decrypted and copied into the supplied buffer.
    Data(usize),
    /// No plaintext is buffered yet; the transport should wait for more encrypted bytes.
    Pending,
    /// The peer sent an authenticated TLS `close_notify` alert.
    Closed,
}

impl ClientSession {
    /// Start a verified client using bundled Mozilla roots and the installed runtime providers.
    pub fn connect(hostname: &str, protocols: &[String]) -> Result<Self, String> {
        let config = client_config_with_roots(protocols, &[])?;
        Self::with_config(config, hostname)
    }

    /// Start a verified client using the shared runtime crypto/time providers and extra roots.
    pub fn connect_with_roots(
        hostname: &str,
        protocols: &[String],
        extra_roots: &[Vec<u8>],
    ) -> Result<Self, String> {
        let config = client_config_with_roots(protocols, extra_roots)?;
        Self::with_config(config, hostname)
    }

    fn with_config(config: Arc<ClientConfig>, hostname: &str) -> Result<Self, String> {
        let server_name = ServerName::try_from(hostname.to_owned())
            .map_err(|error| format!("invalid TLS hostname {hostname:?}: {error}"))?;
        let connection = ClientConnection::new(config, server_name)
            .map_err(|error| format!("TLS client setup failed: {error}"))?;
        Ok(Self {
            connection,
            transport_eof: false,
        })
    }

    /// Whether the peer handshake has completed.
    pub fn is_handshaking(&self) -> bool {
        self.connection.is_handshaking()
    }

    /// Feed encrypted peer bytes, processing every complete TLS record they contain.
    pub fn feed_encrypted(&mut self, bytes: &[u8]) -> Result<usize, String> {
        if self.transport_eof {
            return Err(String::from(
                "TLS encrypted input arrived after transport EOF",
            ));
        }
        let mut input = Cursor::new(bytes);
        while (input.position() as usize) < bytes.len() {
            let consumed = self
                .connection
                .read_tls(&mut input)
                .map_err(|error| format!("TLS encrypted input failed: {error}"))?;
            if consumed == 0 {
                break;
            }
            self.connection
                .process_new_packets()
                .map_err(|error| format!("TLS peer data rejected: {error}"))?;
        }
        Ok(input.position() as usize)
    }

    /// Signal that the underlying byte transport reached EOF.
    ///
    /// Rustls treats this as an unauthenticated transport close. Any subsequent plaintext read
    /// without a previously received `close_notify` fails with an unexpected-EOF error, allowing
    /// close-delimited protocols to reject truncation instead of treating it as a clean TLS EOF.
    pub fn feed_eof(&mut self) -> Result<(), String> {
        if self.transport_eof {
            return Ok(());
        }
        self.transport_eof = true;
        let mut empty = Cursor::new(&[]);
        self.connection
            .read_tls(&mut empty)
            .map_err(|error| format!("TLS transport EOF failed: {error}"))?;
        self.connection
            .process_new_packets()
            .map_err(|error| format!("TLS peer data rejected at transport EOF: {error}"))?;
        Ok(())
    }

    /// Drain encrypted TLS records queued for the peer.
    pub fn take_encrypted(&mut self) -> Result<Vec<u8>, String> {
        let mut output = Vec::new();
        while self.connection.wants_write() {
            let written = self
                .connection
                .write_tls(&mut output)
                .map_err(|error| format!("TLS encrypted output failed: {error}"))?;
            if written == 0 {
                break;
            }
        }
        Ok(output)
    }

    /// Queue plaintext for encryption. Returns the number of bytes accepted by Rustls.
    pub fn write_plaintext(&mut self, bytes: &[u8]) -> Result<usize, String> {
        self.connection
            .writer()
            .write(bytes)
            .map_err(|error| format!("TLS plaintext write failed: {error}"))
    }

    /// Read currently available decrypted bytes without waiting for more transport input.
    ///
    /// A transport must call [`feed_eof`](Self::feed_eof) when its TCP-like stream closes. Rustls
    /// then distinguishes an authenticated `close_notify` (`Closed`) from abrupt truncation
    /// (an error).
    pub fn read_plaintext(&mut self, bytes: &mut [u8]) -> Result<PlaintextRead, String> {
        if bytes.is_empty() {
            return Ok(PlaintextRead::Data(0));
        }
        match self.connection.reader().read(bytes) {
            Ok(0) => Ok(PlaintextRead::Closed),
            Ok(length) => Ok(PlaintextRead::Data(length)),
            Err(error) if error.kind() == ErrorKind::WouldBlock => Ok(PlaintextRead::Pending),
            Err(error) => Err(format!("TLS plaintext read failed: {error}")),
        }
    }

    /// Queue TLS close_notify. The owner must then send bytes from `take_encrypted`.
    pub fn close_notify(&mut self) {
        self.connection.send_close_notify();
    }

    pub fn protocol(&self) -> String {
        match self.connection.protocol_version() {
            Some(ProtocolVersion::TLSv1_3) => "TLSv1.3".into(),
            Some(ProtocolVersion::TLSv1_2) => "TLSv1.2".into(),
            Some(other) => format!("{other:?}"),
            None => String::new(),
        }
    }

    pub fn cipher(&self) -> String {
        self.connection
            .negotiated_cipher_suite()
            .map(|suite| format!("{:?}", suite.suite()))
            .unwrap_or_default()
    }

    pub fn alpn_protocol(&self) -> String {
        self.connection
            .alpn_protocol()
            .map(|protocol| String::from_utf8_lossy(protocol).into_owned())
            .unwrap_or_default()
    }
}

/// How long a handshake may keep retrying reads that time out. The runtime's sockets carry short
/// poll-style read timeouts (100 ms), so a single timeout is not a failure.
#[cfg(any(test, not(unix), target_os = "android"))]
const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(30);

#[cfg(any(test, not(unix), target_os = "android"))]
pub struct TlsStream {
    connection: Connection,
    stream: TcpStream,
    nonblocking: bool,
}

#[cfg(any(test, not(unix), target_os = "android"))]
impl TlsStream {
    pub fn connect(stream: TcpStream, hostname: &str) -> Result<Self, String> {
        Self::connect_with_options(stream, hostname, &[], true)
    }

    #[cfg(any(not(unix), target_os = "android"))]
    pub fn connect_with_alpn(
        stream: TcpStream,
        hostname: &str,
        protocols: &[String],
    ) -> Result<Self, String> {
        Self::connect_with_options(stream, hostname, protocols, true)
    }

    /// Connect with the platform's normal roots plus certificate authorities supplied for this
    /// connection only. The hostname still controls both SNI and certificate name verification.
    pub fn connect_with_extra_roots(
        stream: TcpStream,
        hostname: &str,
        pem: &[u8],
    ) -> Result<Self, String> {
        let config = client_config_with_extra_pem(&[], pem)?;
        let server_name = ServerName::try_from(hostname.to_owned())
            .map_err(|error| format!("invalid TLS hostname {hostname:?}: {error}"))?;
        Self::connect_configured(stream, server_name, config)
    }

    pub fn connect_with_options(
        stream: TcpStream,
        hostname: &str,
        protocols: &[String],
        verify_peer: bool,
    ) -> Result<Self, String> {
        validate_alpn(protocols)?;
        let server_name = match ServerName::try_from(hostname.to_owned()) {
            Ok(name) => name,
            // Without verification the name only feeds SNI; fall back to the peer address (which
            // sends no SNI) so e.g. an empty servername still connects, as it does with OpenSSL.
            Err(_) if !verify_peer => ServerName::IpAddress(
                stream
                    .peer_addr()
                    .map_err(|error| error.to_string())?
                    .ip()
                    .into(),
            ),
            Err(error) => return Err(format!("invalid TLS hostname {hostname:?}: {error}")),
        };
        let config = client_config(verify_peer, protocols)?;
        Self::connect_configured(stream, server_name, config)
    }

    fn connect_configured(
        stream: TcpStream,
        server_name: ServerName<'static>,
        config: Arc<ClientConfig>,
    ) -> Result<Self, String> {
        let connection = ClientConnection::new(config, server_name)
            .map_err(|error| format!("TLS client setup failed: {error}"))?;
        let mut tls = Self {
            connection: connection.into(),
            stream,
            nonblocking: false,
        };
        tls.handshake()
            .map_err(|error| format!("TLS handshake failed: {error}"))?;
        Ok(tls)
    }

    pub fn accept(
        stream: TcpStream,
        certificate_pem: &[u8],
        private_key_pem: &[u8],
    ) -> Result<Self, String> {
        Self::accept_with_alpn(stream, certificate_pem, private_key_pem, &[])
    }

    pub fn accept_with_alpn(
        stream: TcpStream,
        certificate_pem: &[u8],
        private_key_pem: &[u8],
        protocols: &[String],
    ) -> Result<Self, String> {
        validate_alpn(protocols)?;
        let chain = CertificateDer::pem_slice_iter(certificate_pem)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("invalid PEM: {error}"))?;
        if chain.is_empty() {
            return Err("invalid PEM: no certificate found".into());
        }
        let key = PrivateKeyDer::from_pem_slice(private_key_pem)
            .map_err(|error| format!("invalid PEM: {error}"))?;
        let mut config = ServerConfig::builder_with_provider(provider().clone())
            .with_safe_default_protocol_versions()
            .map_err(|error| error.to_string())?
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .map_err(|error| {
                format!("TLS certificate/private key configuration failed: {error}")
            })?;
        config.alpn_protocols = protocols.iter().map(|p| p.as_bytes().to_vec()).collect();
        let connection = ServerConnection::new(Arc::new(config))
            .map_err(|error| format!("TLS server setup failed: {error}"))?;
        let mut tls = Self {
            connection: connection.into(),
            stream,
            nonblocking: false,
        };
        tls.handshake()
            .map_err(|error| format!("TLS server handshake failed: {error}"))?;
        Ok(tls)
    }

    #[cfg(any(not(unix), target_os = "android"))]
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.stream.set_read_timeout(timeout)
    }

    /// The TCP socket under the TLS session, for registering it with a readiness reactor.
    #[cfg(any(not(unix), target_os = "android"))]
    pub fn socket(&self) -> &TcpStream {
        &self.stream
    }

    /// Switches the socket to nonblocking mode. A read that needs the peer then fails with
    /// `WouldBlock` (wait for the socket, then repeat the call) instead of `Interrupted`. A write
    /// always accepts what it is given; records the socket would not take stay queued, and
    /// [`Write::flush`] fails with `WouldBlock` until they are out. Call it after the handshake,
    /// which always runs blocking.
    #[cfg(any(not(unix), target_os = "android"))]
    pub fn set_nonblocking(&mut self, nonblocking: bool) -> std::io::Result<()> {
        self.stream.set_nonblocking(nonblocking)?;
        self.nonblocking = nonblocking;
        Ok(())
    }

    /// The negotiated version in OpenSSL's `SSL_get_version` spelling ("TLSv1.3").
    pub fn protocol(&self) -> String {
        match self.connection.protocol_version() {
            Some(ProtocolVersion::TLSv1_3) => "TLSv1.3".into(),
            Some(ProtocolVersion::TLSv1_2) => "TLSv1.2".into(),
            Some(ProtocolVersion::TLSv1_1) => "TLSv1.1".into(),
            Some(ProtocolVersion::TLSv1_0) => "TLSv1".into(),
            Some(other) => format!("{other:?}"),
            None => String::new(),
        }
    }

    /// The negotiated suite in OpenSSL's `SSL_CIPHER_get_name` spelling.
    pub fn cipher(&self) -> String {
        let Some(suite) = self.connection.negotiated_cipher_suite() else {
            return String::new();
        };
        let name = match suite.suite() {
            CipherSuite::TLS13_AES_128_GCM_SHA256 => "TLS_AES_128_GCM_SHA256",
            CipherSuite::TLS13_AES_256_GCM_SHA384 => "TLS_AES_256_GCM_SHA384",
            CipherSuite::TLS13_CHACHA20_POLY1305_SHA256 => "TLS_CHACHA20_POLY1305_SHA256",
            CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256 => "ECDHE-ECDSA-AES128-GCM-SHA256",
            CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384 => "ECDHE-ECDSA-AES256-GCM-SHA384",
            CipherSuite::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256 => {
                "ECDHE-ECDSA-CHACHA20-POLY1305"
            }
            CipherSuite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256 => "ECDHE-RSA-AES128-GCM-SHA256",
            CipherSuite::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384 => "ECDHE-RSA-AES256-GCM-SHA384",
            CipherSuite::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256 => {
                "ECDHE-RSA-CHACHA20-POLY1305"
            }
            other => {
                return other
                    .as_str()
                    .map_or_else(|| format!("{other:?}"), str::to_owned)
            }
        };
        name.into()
    }

    pub fn alpn_protocol(&self) -> String {
        self.connection
            .alpn_protocol()
            .map(|protocol| String::from_utf8_lossy(protocol).into_owned())
            .unwrap_or_default()
    }

    /// Drives the handshake to completion, retrying reads that hit the socket's (short) read
    /// timeout until [`HANDSHAKE_DEADLINE`].
    fn handshake(&mut self) -> std::io::Result<()> {
        let deadline = Instant::now() + HANDSHAKE_DEADLINE;
        while self.connection.is_handshaking() {
            self.flush_tls()?;
            if !self.connection.is_handshaking() {
                break;
            }
            match self.connection.read_tls(&mut self.stream) {
                Ok(0) => {
                    return Err(std::io::Error::new(
                        ErrorKind::UnexpectedEof,
                        "peer closed the connection during the handshake",
                    ))
                }
                Ok(_) => self.process_packets()?,
                Err(error) if is_retry(&error) => {
                    if Instant::now() >= deadline {
                        return Err(std::io::Error::new(
                            ErrorKind::TimedOut,
                            "timed out waiting for the peer",
                        ));
                    }
                }
                Err(error) => return Err(error),
            }
        }
        // The final flight (client Finished, TLS 1.3 session tickets) may still be queued.
        self.flush_tls()
    }

    fn process_packets(&mut self) -> std::io::Result<()> {
        if let Err(error) = self.connection.process_new_packets() {
            // Deliver the alert rustls queued for the peer before failing.
            let _ = self.flush_tls();
            return Err(std::io::Error::new(ErrorKind::InvalidData, error));
        }
        Ok(())
    }

    /// Writes every queued TLS record to the socket.
    fn flush_tls(&mut self) -> std::io::Result<()> {
        while self.connection.wants_write() {
            match self.connection.write_tls(&mut self.stream) {
                Ok(0) => return Err(ErrorKind::WriteZero.into()),
                Ok(_) => {}
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

#[cfg(any(test, not(unix), target_os = "android"))]
fn is_retry(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
    )
}

fn validate_alpn(protocols: &[String]) -> Result<(), String> {
    if protocols
        .iter()
        .any(|protocol| protocol.is_empty() || protocol.len() > u8::MAX as usize)
    {
        return Err("TLS ALPN protocol names must contain 1 to 255 bytes".into());
    }
    Ok(())
}

#[cfg(any(test, not(unix), target_os = "android"))]
impl Read for TlsStream {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        loop {
            match self.connection.reader().read(buffer) {
                Ok(length) => return Ok(length),
                Err(error) if error.kind() == ErrorKind::WouldBlock => {}
                // The peer closed the TCP stream without close_notify: HTTP-style EOF.
                Err(error) if error.kind() == ErrorKind::UnexpectedEof => return Ok(0),
                Err(error) => return Err(error),
            }
            // Post-handshake messages (key updates, alerts) may have queued a reply.
            match self.flush_tls() {
                Err(error) if self.nonblocking && error.kind() == ErrorKind::WouldBlock => {}
                other => other?,
            }
            match self.connection.read_tls(&mut self.stream) {
                // EOF is recorded by rustls; the next reader() call reports it.
                Ok(_) => self.process_packets()?,
                Err(error) if self.nonblocking && error.kind() == ErrorKind::WouldBlock => {
                    return Err(ErrorKind::WouldBlock.into())
                }
                Err(error) if is_retry(&error) => {
                    return Err(std::io::Error::new(
                        ErrorKind::Interrupted,
                        "TLS operation should retry",
                    ))
                }
                Err(error) => return Err(error),
            }
        }
    }
}

#[cfg(any(test, not(unix), target_os = "android"))]
impl Write for TlsStream {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        // Drain earlier records first so the plaintext buffer has room for this one.
        self.flush_tls()?;
        let length = self.connection.writer().write(buffer)?;
        match self.flush_tls() {
            // The plaintext is accepted and its records are queued; `flush` sends them.
            Err(error) if self.nonblocking && error.kind() == ErrorKind::WouldBlock => {}
            other => other?,
        }
        Ok(length)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.connection.writer().flush()?;
        self.flush_tls()?;
        self.stream.flush()
    }
}

#[cfg(any(test, not(unix), target_os = "android"))]
impl Drop for TlsStream {
    fn drop(&mut self) {
        self.connection.send_close_notify();
        let _ = self.flush_tls();
    }
}

#[cfg(any(test, not(unix), target_os = "android"))]
fn provider() -> &'static Arc<CryptoProvider> {
    static PROVIDER: OnceLock<Arc<CryptoProvider>> = OnceLock::new();
    PROVIDER.get_or_init(|| Arc::new(crypto::ring::default_provider()))
}

/// The operating system's trust store, or the bundled Mozilla roots if it yields nothing.
#[cfg(any(test, not(unix), target_os = "android"))]
fn root_store() -> &'static Arc<RootCertStore> {
    static ROOTS: OnceLock<Arc<RootCertStore>> = OnceLock::new();
    ROOTS.get_or_init(|| {
        let mut roots = RootCertStore::empty();
        #[cfg(all(any(not(unix), target_os = "android"), not(target_os = "none")))]
        roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
        if roots.is_empty() {
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        }
        Arc::new(roots)
    })
}

/// Client configs are shared per (verify, ALPN) pair so connections reuse the session cache.
#[cfg(any(test, not(unix), target_os = "android"))]
fn client_config(verify_peer: bool, protocols: &[String]) -> Result<Arc<ClientConfig>, String> {
    type Key = (bool, Vec<String>);
    static CONFIGS: OnceLock<Mutex<HashMap<Key, Arc<ClientConfig>>>> = OnceLock::new();
    let configs = CONFIGS.get_or_init(Default::default);
    let key = (verify_peer, protocols.to_vec());
    if let Some(config) = configs.lock().unwrap().get(&key) {
        return Ok(config.clone());
    }
    let builder = ClientConfig::builder_with_provider(provider().clone())
        .with_safe_default_protocol_versions()
        .map_err(|error| error.to_string())?;
    let mut config = if verify_peer {
        builder
            .with_root_certificates(root_store().clone())
            .with_no_client_auth()
    } else {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyCertificate(provider().clone())))
            .with_no_client_auth()
    };
    config.alpn_protocols = protocols.iter().map(|p| p.as_bytes().to_vec()).collect();
    let config = Arc::new(config);
    configs.lock().unwrap().insert(key, config.clone());
    Ok(config)
}

/// Build a verified socket client using the normal root set plus connection-local PEM roots.
/// Configurations containing caller roots are deliberately not entered in the shared cache.
#[cfg(any(test, not(unix), target_os = "android"))]
fn client_config_with_extra_pem(
    protocols: &[String],
    pem: &[u8],
) -> Result<Arc<ClientConfig>, String> {
    validate_alpn(protocols)?;
    let certificates = CertificateDer::pem_slice_iter(pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("invalid TLS CA PEM: {error}"))?;
    if certificates.is_empty() {
        return Err("invalid TLS CA PEM: no certificate found".into());
    }
    let mut roots = root_store().as_ref().clone();
    for certificate in certificates {
        roots
            .add(certificate)
            .map_err(|error| format!("invalid TLS CA certificate: {error}"))?;
    }
    let builder = ClientConfig::builder_with_provider(provider().clone())
        .with_safe_default_protocol_versions()
        .map_err(|error| error.to_string())?;
    let mut config = builder.with_root_certificates(roots).with_no_client_auth();
    config.alpn_protocols = protocols.iter().map(|p| p.as_bytes().to_vec()).collect();
    Ok(Arc::new(config))
}

/// `rejectUnauthorized: false`: any certificate chain and name is accepted, but the handshake
/// signatures are still checked against the presented certificate.
#[cfg(any(test, not(unix), target_os = "android"))]
#[derive(Debug)]
struct AcceptAnyCertificate(Arc<CryptoProvider>);

#[cfg(any(test, not(unix), target_os = "android"))]
impl ServerCertVerifier for AcceptAnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::Once;

    static INSTALL_RUNTIME: Once = Once::new();

    fn install_runtime() {
        INSTALL_RUNTIME.call_once(|| {
            fn random(bytes: &mut [u8]) -> Result<(), String> {
                crypto::ring::default_provider()
                    .secure_random
                    .fill(bytes)
                    .map_err(|_| String::from("test entropy unavailable"))
            }
            fn now() -> Option<u64> {
                Some(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .ok()?
                        .as_secs(),
                )
            }
            install_runtime_crypto(random, now).unwrap();
        });
    }

    fn self_signed() -> (Vec<u8>, Vec<u8>) {
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        (
            certified.cert.pem().into_bytes(),
            certified.key_pair.serialize_pem().into_bytes(),
        )
    }

    fn der_pair(
        cert_pem: &[u8],
        key_pem: &[u8],
    ) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
        let certs = CertificateDer::pem_slice_iter(cert_pem)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let key = PrivateKeyDer::from_pem_slice(key_pem).unwrap();
        (certs, key)
    }

    fn local_server(cert_pem: &[u8], key_pem: &[u8], alpn: &[String]) -> ServerConnection {
        let (certs, key) = der_pair(cert_pem, key_pem);
        let config = ServerConfig::builder_with_provider(crypto::ring::default_provider().into())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .unwrap();
        let mut config = config;
        config.alpn_protocols = alpn.iter().map(|value| value.as_bytes().to_vec()).collect();
        ServerConnection::new(Arc::new(config)).unwrap()
    }

    fn pump(client: &mut ClientSession, server: &mut ServerConnection) -> Result<(), String> {
        let client_bytes = client.take_encrypted()?;
        if !client_bytes.is_empty() {
            server
                .read_tls(&mut Cursor::new(client_bytes))
                .map_err(|error| error.to_string())?;
            server
                .process_new_packets()
                .map_err(|error| error.to_string())?;
        }
        let mut server_bytes = Vec::new();
        while server.wants_write() {
            let written = server
                .write_tls(&mut server_bytes)
                .map_err(|error| error.to_string())?;
            if written == 0 {
                break;
            }
        }
        if !server_bytes.is_empty() {
            client.feed_encrypted(&server_bytes)?;
        }
        Ok(())
    }

    fn finish_handshake(
        client: &mut ClientSession,
        server: &mut ServerConnection,
    ) -> Result<(), String> {
        for _ in 0..16 {
            pump(client, server)?;
            if !client.is_handshaking() && !server.is_handshaking() {
                return Ok(());
            }
        }
        Err(String::from("in-memory TLS handshake did not complete"))
    }

    #[test]
    fn transport_free_session_verifies_and_reads_authenticated_eof() {
        install_runtime();
        let (cert_pem, key_pem) = self_signed();
        let (certs, _) = der_pair(&cert_pem, &key_pem);
        let roots = certs
            .iter()
            .map(|cert| cert.as_ref().to_vec())
            .collect::<Vec<_>>();
        let alpn = [String::from("http/1.1")];
        let mut client = ClientSession::connect_with_roots("localhost", &alpn, &roots).unwrap();
        let mut pending = [0u8; 1];
        assert_eq!(
            client.read_plaintext(&mut pending).unwrap(),
            PlaintextRead::Pending
        );
        let mut server = local_server(&cert_pem, &key_pem, &alpn);
        finish_handshake(&mut client, &mut server).unwrap();
        assert_eq!(client.protocol(), "TLSv1.3");
        assert_eq!(client.alpn_protocol(), "http/1.1");

        assert_eq!(client.write_plaintext(b"request").unwrap(), 7);
        pump(&mut client, &mut server).unwrap();
        let mut request = [0u8; 7];
        server.reader().read_exact(&mut request).unwrap();
        assert_eq!(&request, b"request");
        server.writer().write_all(b"response").unwrap();
        server.send_close_notify();
        let mut server_bytes = Vec::new();
        while server.wants_write() {
            let written = server.write_tls(&mut server_bytes).unwrap();
            assert_ne!(written, 0);
        }
        client.feed_encrypted(&server_bytes).unwrap();
        let mut response = [0u8; 16];
        assert_eq!(
            client.read_plaintext(&mut response).unwrap(),
            PlaintextRead::Data(8)
        );
        assert_eq!(&response[..8], b"response");
        assert_eq!(
            client.read_plaintext(&mut response).unwrap(),
            PlaintextRead::Closed
        );
    }

    #[test]
    fn transport_free_session_rejects_wrong_hostname_and_truncated_tls() {
        install_runtime();
        let (cert_pem, key_pem) = self_signed();
        let (certs, _) = der_pair(&cert_pem, &key_pem);
        let roots = certs
            .iter()
            .map(|cert| cert.as_ref().to_vec())
            .collect::<Vec<_>>();
        let mut wrong_name =
            ClientSession::connect_with_roots("not-localhost", &[], &roots).unwrap();
        let mut server = local_server(&cert_pem, &key_pem, &[]);
        let mut rejected = false;
        for _ in 0..8 {
            let bytes = wrong_name.take_encrypted().unwrap();
            if !bytes.is_empty() {
                server.read_tls(&mut Cursor::new(bytes)).unwrap();
                if server.process_new_packets().is_err() {
                    break;
                }
            }
            let mut response = Vec::new();
            while server.wants_write() {
                server.write_tls(&mut response).unwrap();
            }
            if !response.is_empty() && wrong_name.feed_encrypted(&response).is_err() {
                rejected = true;
                break;
            }
        }
        assert!(
            rejected,
            "a trusted certificate with the wrong hostname must fail verification"
        );

        let mut client = ClientSession::connect_with_roots("localhost", &[], &roots).unwrap();
        let mut server = local_server(&cert_pem, &key_pem, &[]);
        finish_handshake(&mut client, &mut server).unwrap();
        server.writer().write_all(b"partial response").unwrap();
        let mut encrypted = Vec::new();
        while server.wants_write() {
            server.write_tls(&mut encrypted).unwrap();
        }
        client.feed_encrypted(&encrypted).unwrap();
        client.feed_eof().unwrap();
        let mut plaintext = [0u8; 32];
        assert_eq!(
            client.read_plaintext(&mut plaintext).unwrap(),
            PlaintextRead::Data(16)
        );
        assert!(
            client.read_plaintext(&mut plaintext).is_err(),
            "abrupt TCP EOF must not be clean TLS EOF"
        );
    }

    fn round_trip(alpn_server: &[String], alpn_client: &[String]) -> (String, String, String) {
        let (cert, key) = self_signed();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server_alpn = alpn_server.to_vec();
        let server = std::thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            // Poll-style timeout like the runtime's accept loop.
            tcp.set_read_timeout(Some(Duration::from_millis(100)))
                .unwrap();
            let mut tls = TlsStream::accept_with_alpn(tcp, &cert, &key, &server_alpn).unwrap();
            let mut request = [0u8; 4];
            let mut filled = 0;
            while filled < 4 {
                match tls.read(&mut request[filled..]) {
                    Ok(0) => panic!("early EOF"),
                    Ok(n) => filled += n,
                    Err(e) if e.kind() == ErrorKind::Interrupted => {}
                    Err(e) => panic!("{e}"),
                }
            }
            assert_eq!(&request, b"ping");
            tls.write_all(b"pong").unwrap();
        });
        let tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
        tcp.set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut client =
            TlsStream::connect_with_options(tcp, "localhost", alpn_client, false).unwrap();
        client.write_all(b"ping").unwrap();
        let mut response = Vec::new();
        loop {
            let mut chunk = [0u8; 64];
            match client.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => response.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => panic!("{e}"),
            }
        }
        server.join().unwrap();
        assert_eq!(response, b"pong");
        (client.protocol(), client.cipher(), client.alpn_protocol())
    }

    #[test]
    fn unverified_client_round_trips_with_local_server() {
        let (protocol, cipher, alpn) = round_trip(&[], &[]);
        assert_eq!(protocol, "TLSv1.3");
        assert!(cipher.starts_with("TLS_"), "{cipher}");
        assert_eq!(alpn, "");
    }

    #[test]
    fn negotiates_alpn() {
        let server = ["h2".to_string(), "http/1.1".to_string()];
        let client = ["http/1.1".to_string()];
        let (_, _, alpn) = round_trip(&server, &client);
        assert_eq!(alpn, "http/1.1");
    }

    #[test]
    fn verified_client_rejects_self_signed_certificate() {
        let (cert, key) = self_signed();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            TlsStream::accept(tcp, &cert, &key).err()
        });
        let tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let error = TlsStream::connect(tcp, "localhost")
            .err()
            .expect("must fail");
        assert!(error.contains("certificate"), "{error}");
        assert!(server.join().unwrap().is_some());
    }

    #[test]
    fn socket_client_extra_roots_are_connection_scoped() {
        let (cert, key) = self_signed();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server_cert = cert.clone();
        let server_key = key.clone();
        let server = std::thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            let mut tls = TlsStream::accept(tcp, &server_cert, &server_key).unwrap();
            let mut request = [0u8; 4];
            tls.read_exact(&mut request).unwrap();
            assert_eq!(&request, b"ping");
            tls.write_all(b"pong").unwrap();
        });

        let tcp = TcpStream::connect(address).unwrap();
        let mut client = TlsStream::connect_with_extra_roots(tcp, "localhost", &cert).unwrap();
        client.write_all(b"ping").unwrap();
        let mut response = [0u8; 4];
        client.read_exact(&mut response).unwrap();
        assert_eq!(&response, b"pong");
        server.join().unwrap();

        // The additional CA must not enter the shared platform root store or later configs.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            TlsStream::accept(tcp, &cert, &key).err()
        });
        let tcp = TcpStream::connect(address).unwrap();
        let error = TlsStream::connect(tcp, "localhost")
            .err()
            .expect("the ordinary client must not inherit a connection-local root");
        assert!(error.contains("certificate"), "{error}");
        assert!(server.join().unwrap().is_some());
    }

    #[test]
    fn socket_client_rejects_invalid_extra_root_pem() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let tcp = TcpStream::connect(address).unwrap();
        let (_peer, _) = listener.accept().unwrap();
        let error = TlsStream::connect_with_extra_roots(tcp, "localhost", b"not a certificate")
            .err()
            .expect("invalid extra-root PEM must be rejected");
        assert!(error.contains("PEM"), "{error}");
    }

    #[test]
    fn rejects_bad_pem() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let _client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let (tcp, _) = listener.accept().unwrap();
        let error = TlsStream::accept(tcp, b"nope", b"nope").err().unwrap();
        assert!(error.contains("PEM"), "{error}");
    }
}
