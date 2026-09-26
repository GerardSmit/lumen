//! TLS over rustls (ring provider) for targets without the system-OpenSSL backend (Windows).
//!
//! Same surface and I/O semantics as `openssl`: the handshake completes inside `connect`/`accept`,
//! reads block on the socket, a socket read timeout surfaces as `ErrorKind::Interrupted` (the
//! callers poll with short timeouts and retry on it), and a peer that closes without
//! `close_notify` reads as a clean EOF. Clients trust the operating system's certificate store,
//! falling back to the bundled Mozilla roots when that store yields nothing.

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{self, CryptoProvider};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{
    CipherSuite, ClientConfig, ClientConnection, Connection, DigitallySignedStruct,
    ProtocolVersion, RootCertStore, ServerConfig, ServerConnection, SignatureScheme,
};

/// How long a handshake may keep retrying reads that time out. The runtime's sockets carry short
/// poll-style read timeouts (100 ms), so a single timeout is not a failure.
const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(30);

pub struct TlsStream {
    connection: Connection,
    stream: TcpStream,
}

impl TlsStream {
    pub fn connect(stream: TcpStream, hostname: &str) -> Result<Self, String> {
        Self::connect_with_options(stream, hostname, &[], true)
    }

    pub fn connect_with_alpn(
        stream: TcpStream,
        hostname: &str,
        protocols: &[String],
    ) -> Result<Self, String> {
        Self::connect_with_options(stream, hostname, protocols, true)
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
        let connection = ClientConnection::new(config, server_name)
            .map_err(|error| format!("TLS client setup failed: {error}"))?;
        let mut tls = Self {
            connection: connection.into(),
            stream,
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
        };
        tls.handshake()
            .map_err(|error| format!("TLS server handshake failed: {error}"))?;
        Ok(tls)
    }

    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.stream.set_read_timeout(timeout)
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
            self.flush_tls()?;
            match self.connection.read_tls(&mut self.stream) {
                // EOF is recorded by rustls; the next reader() call reports it.
                Ok(_) => self.process_packets()?,
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

impl Write for TlsStream {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        // Drain earlier records first so the plaintext buffer has room for this one.
        self.flush_tls()?;
        let length = self.connection.writer().write(buffer)?;
        self.flush_tls()?;
        Ok(length)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.connection.writer().flush()?;
        self.flush_tls()?;
        self.stream.flush()
    }
}

impl Drop for TlsStream {
    fn drop(&mut self) {
        self.connection.send_close_notify();
        let _ = self.flush_tls();
    }
}

fn provider() -> &'static Arc<CryptoProvider> {
    static PROVIDER: OnceLock<Arc<CryptoProvider>> = OnceLock::new();
    PROVIDER.get_or_init(|| Arc::new(crypto::ring::default_provider()))
}

/// The operating system's trust store, or the bundled Mozilla roots if it yields nothing.
fn root_store() -> &'static Arc<RootCertStore> {
    static ROOTS: OnceLock<Arc<RootCertStore>> = OnceLock::new();
    ROOTS.get_or_init(|| {
        let mut roots = RootCertStore::empty();
        roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
        if roots.is_empty() {
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        }
        Arc::new(roots)
    })
}

/// Client configs are shared per (verify, ALPN) pair so connections reuse the session cache.
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

/// `rejectUnauthorized: false`: any certificate chain and name is accepted, but the handshake
/// signatures are still checked against the presented certificate.
#[derive(Debug)]
struct AcceptAnyCertificate(Arc<CryptoProvider>);

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

    fn self_signed() -> (Vec<u8>, Vec<u8>) {
        let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        (
            certified.cert.pem().into_bytes(),
            certified.key_pair.serialize_pem().into_bytes(),
        )
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
    fn rejects_bad_pem() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let _client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let (tcp, _) = listener.accept().unwrap();
        let error = TlsStream::accept(tcp, b"nope", b"nope").err().unwrap();
        assert!(error.contains("PEM"), "{error}");
    }
}
