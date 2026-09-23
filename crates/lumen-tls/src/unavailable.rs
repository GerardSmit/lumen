//! The placeholder backend for targets without one: same surface as the OpenSSL stream, and
//! nothing ever constructs it.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const UNAVAILABLE: &str = "TLS is not available in this build of the runtime on this platform";

pub enum TlsStream {}

impl TlsStream {
    pub fn connect(_stream: TcpStream, _hostname: &str) -> Result<Self, String> {
        Err(UNAVAILABLE.into())
    }

    pub fn connect_with_alpn(
        _stream: TcpStream,
        _hostname: &str,
        _protocols: &[String],
    ) -> Result<Self, String> {
        Err(UNAVAILABLE.into())
    }

    pub fn connect_with_options(
        _stream: TcpStream,
        _hostname: &str,
        _protocols: &[String],
        _verify_peer: bool,
    ) -> Result<Self, String> {
        Err(UNAVAILABLE.into())
    }

    pub fn accept(
        _stream: TcpStream,
        _certificate_pem: &[u8],
        _private_key_pem: &[u8],
    ) -> Result<Self, String> {
        Err(UNAVAILABLE.into())
    }

    pub fn accept_with_alpn(
        _stream: TcpStream,
        _certificate_pem: &[u8],
        _private_key_pem: &[u8],
        _protocols: &[String],
    ) -> Result<Self, String> {
        Err(UNAVAILABLE.into())
    }

    pub fn set_read_timeout(&self, _timeout: Option<Duration>) -> std::io::Result<()> {
        match *self {}
    }

    pub fn protocol(&self) -> String {
        match *self {}
    }

    pub fn cipher(&self) -> String {
        match *self {}
    }

    pub fn alpn_protocol(&self) -> String {
        match *self {}
    }
}

impl Read for TlsStream {
    fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        match *self {}
    }
}

impl Write for TlsStream {
    fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
        match *self {}
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match *self {}
    }
}
