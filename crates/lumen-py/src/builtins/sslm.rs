//! `_ssl` on the TLS engine (`lumen_tls::engine`) and the X.509 code (`lumen_common::x509`) that
//! `lumen-node`'s `tls` and `crypto` use: Python-side argument handling, OpenSSL-compatible
//! errors, non-blocking sockets and memory BIOs over one shared TLS session type
//! (`Modules/_ssl.c`).

use crate::bind::{Exc, This};
use crate::object::*;
use crate::vm::*;

/// `ssl.SSLError.__str__`: the message when `strerror` is a string, else `OSError`'s text.
#[lumen_bind::class(name = "SSLError")]
pub struct SslErrorExt;

#[lumen_bind::methods]
impl SslErrorExt {
    #[proto(str)]
    fn str(slf: This<Exc<'_>>, it: &mut Interp) -> R<String> {
        let e = slf.0 .0;
        let strerror = e.dict.borrow().as_ref().and_then(|d| dict_get_str(d, "strerror"));
        if let Some(text) = strerror.as_ref().and_then(|v| v.as_str()) {
            return Ok(text.to_string());
        }
        let args = match &e.kind {
            Kind::Exception(d) => d.borrow().args.clone(),
            _ => Value::tuple(Vec::new()),
        };
        it.str_of(&args)
    }
}

#[lumen_bind::module(name = "_ssl")]
pub mod _ssl {
    #![allow(clippy::new_ret_no_self)]

    use super::SslErrorExt;
    use crate::bind::{opaque_instance, Py, This};
    use crate::builtins::socketm::_socket::{fd_and_timeout, nosignal, now, wait_ready};
    use crate::object::*;
    use crate::vm::{dict_set_str, Interp};
    use lumen_common::x509::{self, AltName, NameTuples};
    use lumen_os::net;
    use lumen_tls::engine::{self, Event, Io};
    use std::os::unix::ffi::OsStrExt;

    const SSL_ERROR_WANT_CONNECT: i32 = 7;
    const SSL_ERROR_EOF: i32 = 8;
    const SSL_ERROR_INVALID_ERROR_CODE: i32 = 10;

    const CERT_NONE: i64 = 0;
    const CERT_OPTIONAL: i64 = 1;
    const CERT_REQUIRED: i64 = 2;

    const PROTOCOL_TLS: i32 = 2;
    const PROTOCOL_TLSV1: i32 = 3;
    const PROTOCOL_TLSV1_1: i32 = 4;
    const PROTOCOL_TLSV1_2: i32 = 5;
    const PROTOCOL_TLS_CLIENT: i32 = 16;
    const PROTOCOL_TLS_SERVER: i32 = 17;

    const TLS1_VERSION: i32 = 0x301;
    const TLS1_1_VERSION: i32 = 0x302;
    const TLS1_2_VERSION: i32 = 0x303;

    const SSL_VERIFY_PEER: i32 = 1;
    const SSL_VERIFY_FAIL_IF_NO_PEER_CERT: i32 = 2;
    const SSL_VERIFY_POST_HANDSHAKE: i32 = 8;

    const OP_ALL: u64 = 0x8000_0050;
    const OP_NO_SSLV3: u64 = 0x0200_0000;
    const OP_NO_COMPRESSION: u64 = 0x2_0000;
    const OP_CIPHER_SERVER_PREFERENCE: u64 = 0x40_0000;
    const OP_ALLOW_CLIENT_RENEGOTIATION: u64 = 0x100;

    const X509_CHECK_FLAG_NO_PARTIAL_WILDCARDS: u32 = 0x4;
    const X509_CHECK_FLAG_NEVER_CHECK_SUBJECT: u32 = 0x20;
    const X509_V_FLAG_TRUSTED_FIRST: u64 = 0x8000;
    const X509_V_ERR_HOSTNAME_MISMATCH: i64 = 62;
    const X509_V_ERR_IP_ADDRESS_MISMATCH: i64 = 64;

    const ALERT_HANDSHAKE_FAILURE: i32 = 40;
    const ALERT_INTERNAL_ERROR: i32 = 80;

    const PEM_BUFSIZE: usize = 1024;
    const READ_CHUNK: usize = 16 * 1024;
    const EINTR: i32 = 4;

    /// `ERR_PACK(ERR_LIB_SSL, 0, ERR_R_PEM_LIB)`.
    const SSL_PEM_LIB: u64 = (20 << 23) | 9;
    const ERR_LIB_PEM: u64 = 9;

    /// The line number reported in `(_ssl.c:NNN)`.
    const LINE: u32 = 1000;

    const DEFAULT_CIPHERS: &str = "@SECLEVEL=2:ECDH+AESGCM:ECDH+CHACHA20:ECDH+AES:DHE+AES:!aNULL:!eNULL:!aDSS:!SHA1:!AESCCM";

    #[derive(Default)]
    pub struct State {
        ssl: Option<Obj>,
        zero_return: Option<Obj>,
        want_read: Option<Obj>,
        want_write: Option<Obj>,
        syscall: Option<Obj>,
        eof: Option<Obj>,
        cert_verification: Option<Obj>,
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Class {
        Ssl,
        ZeroReturn,
        WantRead,
        WantWrite,
        Syscall,
        Eof,
        CertVerification,
    }

    fn class_of(it: &mut Interp, class: Class) -> Obj {
        let st = it.native_state::<State>();
        let found = match class {
            Class::Ssl => &st.ssl,
            Class::ZeroReturn => &st.zero_return,
            Class::WantRead => &st.want_read,
            Class::WantWrite => &st.want_write,
            Class::Syscall => &st.syscall,
            Class::Eof => &st.eof,
            Class::CertVerification => &st.cert_verification,
        }
        .clone();
        match found {
            Some(c) => c,
            None => it.exc_type("OSError"),
        }
    }

    /// An `SSLError` (or subclass) carrying just a message.
    fn plain_error(it: &mut Interp, class: Class, msg: &str) -> Obj {
        let cls = class_of(it, class);
        it.os_error_of(&cls, vec![Value::str(msg)])
    }

    fn ssl_error(it: &mut Interp, msg: &str) -> Obj {
        plain_error(it, Class::Ssl, msg)
    }

    fn timeout_error(it: &mut Interp, msg: &str) -> Obj {
        it.new_exc_str("TimeoutError", msg)
    }

    /// `fill_and_set_sslerror`: `[LIB: REASON] text (_ssl.c:LINE)` with the `library`, `reason`
    /// and, for verification failures, `verify_code` / `verify_message` attributes.
    fn fill_error(it: &mut Interp, class: Class, errno: i32, errstr: Option<String>, raw: u64, verify: Option<(i64, String)>) -> Obj {
        let details = engine::error_details(raw);
        let errstr = errstr.or_else(|| details.text.clone()).unwrap_or_else(|| "unknown error".to_string());
        let body = match (&details.library, &details.reason) {
            (Some(lib), Some(reason)) => format!("[{lib}: {reason}] {errstr}"),
            (Some(lib), None) => format!("[{lib}] {errstr}"),
            _ => errstr,
        };
        let msg = match &verify {
            Some((_, text)) => format!("{body}: {text} (_ssl.c:{LINE})"),
            None => format!("{body} (_ssl.c:{LINE})"),
        };
        let cls = class_of(it, class);
        let exc = it.os_error_of(&cls, vec![Value::Int(errno as i64), Value::string(msg)]);
        let d = it.instance_dict(&exc);
        dict_set_str(&d, "library", details.library.map_or(Value::None, Value::string));
        dict_set_str(&d, "reason", details.reason.map_or(Value::None, Value::string));
        if let Some((code, text)) = verify {
            dict_set_str(&d, "verify_code", Value::Int(code));
            dict_set_str(&d, "verify_message", Value::string(text));
        }
        exc
    }

    /// `_setSSLError`: an error raised outside a connection, from OpenSSL's newest queue entry
    /// (or the engine's own message).
    fn engine_error(it: &mut Interp, e: &engine::EngineError) -> Obj {
        let message = (e.raw_last == 0).then(|| e.message.clone());
        fill_error(it, Class::Ssl, 0, message, e.raw_last, None)
    }

    /// As [`engine_error`] for reading PEM files, which OpenSSL reports as `[SSL] PEM lib`.
    fn pem_error(it: &mut Interp, e: &engine::EngineError) -> Obj {
        if e.raw_last != 0 && engine::error_parts(e.raw_last).0 == ERR_LIB_PEM {
            return fill_error(it, Class::Ssl, 0, None, SSL_PEM_LIB, None);
        }
        engine_error(it, e)
    }

    /// What a failed SSL call left behind.
    struct Fail {
        code: i32,
        ret: i64,
        raw: u64,
        verify: i64,
        host: Option<String>,
    }

    /// `PySSL_SetError`.
    fn session_error(it: &mut Interp, f: &Fail) -> Obj {
        let (mut class, mut errno, mut errstr) = (Class::Ssl, f.code, None::<&str>);
        match f.code {
            engine::SSL_ERROR_ZERO_RETURN => {
                class = Class::ZeroReturn;
                errstr = Some("TLS/SSL connection has been closed (EOF)");
            }
            engine::SSL_ERROR_WANT_READ => {
                class = Class::WantRead;
                errstr = Some("The operation did not complete (read)");
            }
            engine::SSL_ERROR_WANT_WRITE => {
                class = Class::WantWrite;
                errstr = Some("The operation did not complete (write)");
            }
            engine::SSL_ERROR_WANT_X509_LOOKUP => errstr = Some("The operation did not complete (X509 lookup)"),
            SSL_ERROR_WANT_CONNECT => errstr = Some("The operation did not complete (connect)"),
            engine::SSL_ERROR_SYSCALL => {
                if f.raw == 0 {
                    if f.ret == 0 {
                        class = Class::Eof;
                        errno = SSL_ERROR_EOF;
                        errstr = Some("EOF occurred in violation of protocol");
                    } else {
                        class = Class::Syscall;
                        errstr = Some("Some I/O error occurred");
                    }
                }
            }
            engine::SSL_ERROR_SSL => {
                if f.raw == 0 {
                    errstr = Some("A failure in the SSL library occurred");
                } else {
                    let d = engine::error_details(f.raw);
                    if d.library.as_deref() == Some("SSL") {
                        match d.reason.as_deref() {
                            Some("CERTIFICATE_VERIFY_FAILED") => class = Class::CertVerification,
                            Some("UNEXPECTED_EOF_WHILE_READING") => {
                                class = Class::Eof;
                                errno = SSL_ERROR_EOF;
                                errstr = Some("EOF occurred in violation of protocol");
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {
                errno = SSL_ERROR_INVALID_ERROR_CODE;
                errstr = Some("Invalid error code");
            }
        }
        let verify = (class == Class::CertVerification).then(|| {
            let host = f.host.clone().unwrap_or_default();
            let text = match f.verify {
                X509_V_ERR_HOSTNAME_MISMATCH => format!("Hostname mismatch, certificate is not valid for '{host}'."),
                X509_V_ERR_IP_ADDRESS_MISMATCH => format!("IP address mismatch, certificate is not valid for '{host}'."),
                code => engine::verify_error_text(code).unwrap_or_default(),
            };
            (f.verify, text)
        });
        fill_error(it, class, errno, errstr.map(str::to_string), f.raw, verify)
    }

    fn write_unraisable(it: &mut Interp, f: &Value, e: &Obj) {
        it.flush_out();
        let repr = it.repr_of(f).unwrap_or_default();
        it.write_stderr(&format!("Exception ignored in: {repr}\n"));
        let text = it.format_exception(e);
        it.write_stderr(&text);
    }

    fn same(a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Obj(x), Value::Obj(y)) => std::rc::Rc::ptr_eq(x, y),
            _ => false,
        }
    }

    /// A reference that does not keep its target alive when the interpreter can make one.
    #[derive(Clone)]
    struct Ref {
        obj: Value,
        weak: bool,
    }

    impl Ref {
        fn new(it: &mut Interp, v: &Value) -> Ref {
            let made = it
                .import_module("_weakref")
                .and_then(|m| it.get_attr_str(&Value::Obj(m), "ref"))
                .and_then(|r| it.call(&r, vec![v.clone()], Vec::new()));
            match made {
                Ok(w) => Ref { obj: w, weak: true },
                Err(_) => Ref { obj: v.clone(), weak: false },
            }
        }

        fn get(&self, it: &mut Interp) -> R<Option<Value>> {
            if !self.weak {
                return Ok(Some(self.obj.clone()));
            }
            let v = it.call(&self.obj, Vec::new(), Vec::new())?;
            Ok(if v.is_none() { None } else { Some(v) })
        }
    }

    // ---- files, passwords, certificates -------------------------------------------------------

    fn path_bytes(it: &mut Interp, path: &Value) -> R<Vec<u8>> {
        crate::bind::path::fs_bytes(it, path)
    }

    fn path_text(it: &mut Interp, path: &Value) -> R<String> {
        Ok(String::from_utf8_lossy(&path_bytes(it, path)?).into_owned())
    }

    fn read_file(it: &mut Interp, path: &Value) -> R<Vec<u8>> {
        let raw = path_bytes(it, path)?;
        std::fs::read(std::ffi::OsStr::from_bytes(&raw)).map_err(|e| it.os_error_errno(e.raw_os_error().unwrap_or(5), Some(path), None))
    }

    /// The bytes of a `str` (UTF-8), `bytes` or `bytearray`.
    fn text_or_bytes(it: &mut Interp, v: &Value) -> R<Option<Vec<u8>>> {
        if let Some(s) = v.as_str() {
            return Ok(Some(s.as_bytes().to_vec()));
        }
        match v {
            Value::Obj(o) if matches!(o.kind, Kind::Bytes(_) | Kind::ByteArray(_)) => it.bytes_of(v).map(Some),
            _ => Ok(None),
        }
    }

    enum Password {
        Absent,
        Fixed(Vec<u8>),
        Callback(Value),
    }

    fn password_arg(it: &mut Interp, v: Option<&Value>) -> R<Password> {
        let Some(v) = v else { return Ok(Password::Absent) };
        if it.is_callable(v) {
            return Ok(Password::Callback(v.clone()));
        }
        match text_or_bytes(it, v)? {
            Some(bytes) if bytes.len() > PEM_BUFSIZE => Err(it.value_error(&format!("password cannot be longer than {PEM_BUFSIZE} bytes"))),
            Some(bytes) => Ok(Password::Fixed(bytes)),
            None => Err(it.type_error("password should be a string or callable")),
        }
    }

    fn tuple_of_strings(items: &[String]) -> Value {
        Value::tuple(items.iter().map(|s| Value::str(s)).collect())
    }

    fn name_value(name: &NameTuples) -> Value {
        Value::tuple(
            name.iter()
                .map(|rdn| Value::tuple(rdn.iter().map(|(k, v)| Value::tuple(vec![Value::str(k), Value::str(v)])).collect()))
                .collect(),
        )
    }

    /// `_decode_certificate`: the dict `getpeercert()` returns.
    fn cert_dict(it: &mut Interp, der: &[u8]) -> R<Value> {
        let Some(info) = x509::decode_cert(der) else {
            return Err(ssl_error(it, "Error decoding certificate"));
        };
        let d = it.new_dict();
        dict_set_str(&d, "subject", name_value(&info.subject));
        dict_set_str(&d, "issuer", name_value(&info.issuer));
        dict_set_str(&d, "version", Value::Int(info.version as i64));
        dict_set_str(&d, "serialNumber", Value::string(info.serial_number));
        dict_set_str(&d, "notBefore", Value::string(info.not_before));
        dict_set_str(&d, "notAfter", Value::string(info.not_after));
        if !info.subject_alt_name.is_empty() {
            let items = info
                .subject_alt_name
                .iter()
                .map(|n| match n {
                    AltName::Text(kind, value) => Value::tuple(vec![Value::str(kind), Value::str(value)]),
                    AltName::DirName(name) => Value::tuple(vec![Value::str("DirName"), name_value(name)]),
                })
                .collect();
            dict_set_str(&d, "subjectAltName", Value::tuple(items));
        }
        if !info.ocsp.is_empty() {
            dict_set_str(&d, "OCSP", tuple_of_strings(&info.ocsp));
        }
        if !info.ca_issuers.is_empty() {
            dict_set_str(&d, "caIssuers", tuple_of_strings(&info.ca_issuers));
        }
        if !info.crl_distribution_points.is_empty() {
            dict_set_str(&d, "crlDistributionPoints", tuple_of_strings(&info.crl_distribution_points));
        }
        Ok(Value::Obj(d))
    }

    fn certs_value(it: &mut Interp, certs: Vec<Vec<u8>>, binary: bool) -> R<Value> {
        let mut out = Vec::with_capacity(certs.len());
        for der in certs {
            out.push(if binary { Value::bytes(der) } else { cert_dict(it, &der)? });
        }
        Ok(Value::list(out))
    }

    fn cipher_dict(it: &mut Interp, c: &engine::CipherInfo) -> Value {
        let d = it.new_dict();
        let text = |s: &Option<String>| s.clone().map_or(Value::None, Value::string);
        dict_set_str(&d, "id", Value::Int(c.id as i64));
        dict_set_str(&d, "name", Value::str(&c.name));
        dict_set_str(&d, "protocol", Value::str(&c.protocol));
        dict_set_str(&d, "description", Value::str(&c.description));
        dict_set_str(&d, "strength_bits", Value::Int(c.strength_bits as i64));
        dict_set_str(&d, "alg_bits", Value::Int(c.alg_bits as i64));
        dict_set_str(&d, "aead", Value::Bool(c.aead));
        dict_set_str(&d, "symmetric", text(&c.symmetric));
        dict_set_str(&d, "digest", text(&c.digest));
        dict_set_str(&d, "kea", text(&c.kea));
        dict_set_str(&d, "auth", text(&c.auth));
        Value::Obj(d)
    }

    // ---- _SSLContext --------------------------------------------------------------------------

    #[class(name = "_SSLContext", module = "_ssl")]
    pub struct Context {
        engine: engine::Context,
        protocol: i32,
        verify_mode: i64,
        check_hostname: bool,
        post_handshake_auth: bool,
        hostflags: u32,
        alpn: Vec<u8>,
        sni: Option<Value>,
        msg: Option<Value>,
        keylog: Option<String>,
    }

    fn configure(engine: &mut engine::Context) -> Result<(), engine::EngineError> {
        engine.clear_options(OP_ALLOW_CLIENT_RENEGOTIATION)?;
        engine.set_options(OP_ALL | OP_NO_SSLV3 | OP_NO_COMPRESSION | OP_CIPHER_SERVER_PREFERENCE)?;
        engine.set_session_cache_mode(2)?;
        engine.set_session_id_context(b"Python")?;
        engine.set_hostflags(X509_CHECK_FLAG_NO_PARTIAL_WILDCARDS)?;
        engine.set_verify_flags(X509_V_FLAG_TRUSTED_FIRST)?;
        engine.set_dh_param(None)?;
        engine.set_ciphers(DEFAULT_CIPHERS)?;
        Ok(())
    }

    impl Context {
        fn verify_bits(&self) -> i32 {
            let mode = match self.verify_mode {
                CERT_NONE => return 0,
                CERT_OPTIONAL => SSL_VERIFY_PEER,
                _ => SSL_VERIFY_PEER | SSL_VERIFY_FAIL_IF_NO_PEER_CERT,
            };
            if self.protocol == PROTOCOL_TLS_SERVER && self.post_handshake_auth {
                mode | SSL_VERIFY_POST_HANDSHAKE
            } else {
                mode
            }
        }

        fn apply_verify(&mut self, it: &mut Interp) -> R<()> {
            let bits = self.verify_bits();
            self.engine.set_verify(bits).map_err(|e| engine_error(it, &e))
        }

        fn set_version(&mut self, it: &mut Interp, max: bool, v: i64) -> R<()> {
            if !matches!(self.protocol, PROTOCOL_TLS | PROTOCOL_TLS_CLIENT | PROTOCOL_TLS_SERVER) {
                return Err(it.value_error("The context's protocol doesn't support modification of highest and lowest version."));
            }
            let wire = match v {
                -2 | -1 => 0,
                0x300..=0x304 => v as i32,
                _ => return Err(it.value_error(&format!("Unsupported protocol version 0x{v:x}"))),
            };
            if !self.engine.try_set_proto(max, wire) {
                return Err(it.value_error(&format!("Unsupported protocol version 0x{wire:x}")));
            }
            Ok(())
        }
    }

    #[methods]
    impl Context {
        #[constructor]
        fn new(cls: This<Value>, it: &mut Interp, #[kw] protocol: i32) -> R<Value> {
            let (min, max) = match protocol {
                PROTOCOL_TLS | PROTOCOL_TLS_CLIENT | PROTOCOL_TLS_SERVER => (TLS1_2_VERSION, 0),
                PROTOCOL_TLSV1 => (TLS1_VERSION, TLS1_VERSION),
                PROTOCOL_TLSV1_1 => (TLS1_1_VERSION, TLS1_1_VERSION),
                PROTOCOL_TLSV1_2 => (TLS1_2_VERSION, TLS1_2_VERSION),
                _ => return Err(it.value_error(&format!("invalid or unsupported protocol version {protocol}"))),
            };
            let mut engine = engine::Context::new(None, min, max).map_err(|e| engine_error(it, &e))?;
            if max == 0 {
                engine.try_set_proto(true, 0);
            }
            configure(&mut engine).map_err(|e| {
                let _ = &e;
                engine_error(it, &e)
            })?;
            let client = protocol == PROTOCOL_TLS_CLIENT;
            let mut ctx = Context {
                engine,
                protocol,
                verify_mode: CERT_NONE,
                check_hostname: false,
                post_handshake_auth: false,
                hostflags: X509_CHECK_FLAG_NO_PARTIAL_WILDCARDS,
                alpn: Vec::new(),
                sni: None,
                msg: None,
                keylog: None,
            };
            if client {
                ctx.check_hostname = true;
                ctx.verify_mode = CERT_REQUIRED;
                ctx.apply_verify(it)?;
            }
            let Value::Obj(cls) = &cls.0 else { unreachable!("checked by the entry") };
            Ok(opaque_instance(cls, ctx))
        }

        fn _set_alpn_protocols(&mut self, protos: &[u8]) {
            self.alpn = protos.to_vec();
        }

        fn _wrap_socket(
            slf: This<Py<Self>>,
            it: &mut Interp,
            #[kw] sock: &Value,
            #[kw] server_side: bool,
            #[kw] server_hostname: Option<&Value>,
            #[kwonly] owner: Option<&Value>,
            #[kwonly] session: Option<&Value>,
        ) -> R<Value> {
            if fd_and_timeout(it, sock).is_none() {
                let t = it.type_name_of(sock);
                return Err(it.type_error(&format!("_wrap_socket() argument 'sock' must be _socket.socket, not {t}")));
            }
            let transport = Transport::Socket(Ref::new(it, sock));
            make_socket(it, &slf.0, transport, server_side, server_hostname, owner, session)
        }

        fn _wrap_bio(
            slf: This<Py<Self>>,
            it: &mut Interp,
            #[kw] incoming: Py<MemoryBio>,
            #[kw] outgoing: Py<MemoryBio>,
            #[kw] server_side: bool,
            #[kw] server_hostname: Option<&Value>,
            #[kwonly] owner: Option<&Value>,
            #[kwonly] session: Option<&Value>,
        ) -> R<Value> {
            let transport = Transport::Bio(incoming, outgoing);
            make_socket(it, &slf.0, transport, server_side, server_hostname, owner, session)
        }

        fn set_ciphers(&mut self, it: &mut Interp, cipherlist: &str) -> R<()> {
            if cipherlist.is_empty() || self.engine.set_ciphers(cipherlist).is_err() {
                return Err(ssl_error(it, "No cipher can be selected."));
            }
            Ok(())
        }

        fn get_ciphers(&self, it: &mut Interp) -> R<Value> {
            let ciphers = self.engine.ciphers().map_err(|e| engine_error(it, &e))?;
            Ok(Value::list(ciphers.iter().map(|c| cipher_dict(it, c)).collect()))
        }

        fn load_cert_chain(
            &mut self,
            it: &mut Interp,
            #[kw] certfile: &Value,
            #[kw] keyfile: Option<&Value>,
            #[kw] password: Option<&Value>,
        ) -> R<()> {
            let password = password_arg(it, password)?;
            let chain = read_file(it, certfile)?;
            self.engine.set_cert(&chain).map_err(|e| pem_error(it, &e))?;
            let key = match keyfile {
                Some(k) => read_file(it, k)?,
                None => chain,
            };
            let mut failure: Option<Obj> = None;
            let result = {
                let mut ask = |_size: usize| -> Result<Vec<u8>, ()> {
                    match &password {
                        Password::Fixed(bytes) => Ok(bytes.clone()),
                        Password::Callback(f) => {
                            let got = match it.call(f, Vec::new(), Vec::new()) {
                                Ok(v) => v,
                                Err(e) => {
                                    failure = Some(e);
                                    return Err(());
                                }
                            };
                            match text_or_bytes(it, &got) {
                                Ok(Some(bytes)) if bytes.len() > PEM_BUFSIZE => {
                                    failure = Some(it.value_error(&format!("password cannot be longer than {PEM_BUFSIZE} bytes")));
                                    Err(())
                                }
                                Ok(Some(bytes)) => Ok(bytes),
                                Ok(None) => {
                                    failure = Some(it.type_error("password callback must return a string"));
                                    Err(())
                                }
                                Err(e) => {
                                    failure = Some(e);
                                    Err(())
                                }
                            }
                        }
                        Password::Absent => Err(()),
                    }
                };
                match &password {
                    Password::Absent => self.engine.use_private_key(&key, None),
                    _ => self.engine.use_private_key(&key, Some(&mut ask as &mut dyn FnMut(usize) -> Result<Vec<u8>, ()>)),
                }
            };
            if let Err(e) = result {
                return Err(failure.unwrap_or_else(|| pem_error(it, &e)));
            }
            self.engine.verify_private_key().map_err(|e| engine_error(it, &e))
        }

        fn load_dh_params(&mut self, it: &mut Interp, path: &Value) -> R<()> {
            let pem = read_file(it, path)?;
            self.engine.load_dh_params(&pem).map_err(|e| engine_error(it, &e))
        }

        fn load_verify_locations(
            &mut self,
            it: &mut Interp,
            #[kw] cafile: Option<&Value>,
            #[kw] capath: Option<&Value>,
            #[kw] cadata: Option<&Value>,
        ) -> R<()> {
            if cafile.is_none() && capath.is_none() && cadata.is_none() {
                return Err(it.type_error("cafile, capath and cadata cannot be all omitted"));
            }
            let file = cafile.map(|v| path_text(it, v)).transpose()?;
            let dir = capath.map(|v| path_text(it, v)).transpose()?;
            if let Some(path) = &file {
                if let Err(e) = std::fs::metadata(path) {
                    return Err(it.os_error_errno(e.raw_os_error().unwrap_or(2), None, None));
                }
            }
            if file.is_some() || dir.is_some() {
                self.engine.load_verify_locations(file.as_deref(), dir.as_deref()).map_err(|e| engine_error(it, &e))?;
            }
            if let Some(data) = cadata {
                if let Some(text) = data.as_str() {
                    if !text.is_ascii() {
                        return Err(it.value_error("string argument should contain only ASCII characters"));
                    }
                    self.engine.add_ca_data(text.as_bytes(), false).map_err(|e| engine_error(it, &e))?;
                } else {
                    let Ok(bytes) = it.buffer_bytes(data) else {
                        return Err(it.type_error("cadata should be an ASCII string or a bytes-like object"));
                    };
                    self.engine.add_ca_data(&bytes, true).map_err(|e| engine_error(it, &e))?;
                }
            }
            Ok(())
        }

        fn session_stats(&self, it: &mut Interp) -> Value {
            let stats = self.engine.session_stats();
            let names = [
                "number",
                "connect",
                "connect_good",
                "connect_renegotiate",
                "accept",
                "accept_good",
                "accept_renegotiate",
                "hits",
                "cb_hits",
                "misses",
                "timeouts",
                "cache_full",
            ];
            let d = it.new_dict();
            for (name, value) in names.iter().zip(stats) {
                if *name != "cb_hits" {
                    dict_set_str(&d, name, Value::Int(value));
                }
            }
            Value::Obj(d)
        }

        fn set_default_verify_paths(&mut self, it: &mut Interp) -> R<()> {
            self.engine.set_default_verify_paths().map_err(|e| engine_error(it, &e))
        }

        fn set_ecdh_curve(&mut self, it: &mut Interp, name: &Value) -> R<()> {
            let curve = path_text(it, name)?;
            if !engine::short_name_known(&curve) {
                let shown = it.repr_of(name)?;
                return Err(it.value_error(&format!("unknown elliptic curve name {shown}")));
            }
            self.engine.set_ecdh_curve(&curve).map_err(|e| engine_error(it, &e))
        }

        fn cert_store_stats(&self, it: &mut Interp) -> Value {
            let (x509, crl, ca) = self.engine.store_stats();
            let d = it.new_dict();
            dict_set_str(&d, "x509_ca", Value::Int(ca as i64));
            dict_set_str(&d, "crl", Value::Int(crl as i64));
            dict_set_str(&d, "x509", Value::Int(x509 as i64));
            Value::Obj(d)
        }

        fn get_ca_certs(&self, it: &mut Interp, #[default(false)] binary_form: bool) -> R<Value> {
            certs_value(it, self.engine.ca_certs(), binary_form)
        }

        #[getter]
        fn protocol(&self) -> i32 {
            self.protocol
        }

        #[getter]
        fn options(&self) -> i64 {
            self.engine.options() as i64
        }

        #[setter]
        fn set_options(&mut self, it: &mut Interp, value: u64) -> R<()> {
            let current = self.engine.options();
            let clear = current & !value;
            let set = !current & value;
            if clear != 0 {
                self.engine.clear_options(clear).map_err(|e| engine_error(it, &e))?;
            }
            if set != 0 {
                self.engine.set_options(set).map_err(|e| engine_error(it, &e))?;
            }
            Ok(())
        }

        #[getter]
        fn verify_mode(&self) -> i64 {
            self.verify_mode
        }

        #[setter]
        fn set_verify_mode(&mut self, it: &mut Interp, value: i64) -> R<()> {
            if !(CERT_NONE..=CERT_REQUIRED).contains(&value) {
                return Err(it.value_error("invalid value for verify_mode"));
            }
            if value == CERT_NONE && self.check_hostname {
                return Err(it.value_error("Cannot set verify_mode to CERT_NONE when check_hostname is enabled."));
            }
            self.verify_mode = value;
            self.apply_verify(it)
        }

        #[getter]
        fn verify_flags(&self) -> i64 {
            self.engine.verify_flags() as i64
        }

        #[setter]
        fn set_verify_flags(&mut self, it: &mut Interp, value: u64) -> R<()> {
            self.engine.set_verify_flags(value).map_err(|e| engine_error(it, &e))
        }

        #[getter]
        fn check_hostname(&self) -> bool {
            self.check_hostname
        }

        #[setter]
        fn set_check_hostname(&mut self, it: &mut Interp, value: bool) -> R<()> {
            if value && self.verify_mode == CERT_NONE {
                self.verify_mode = CERT_REQUIRED;
                self.apply_verify(it)?;
            }
            self.check_hostname = value;
            Ok(())
        }

        #[getter]
        fn post_handshake_auth(&self) -> bool {
            self.post_handshake_auth
        }

        #[setter]
        fn set_post_handshake_auth(&mut self, it: &mut Interp, value: bool) -> R<()> {
            self.post_handshake_auth = value;
            self.apply_verify(it)
        }

        #[getter]
        fn hostname_checks_common_name(&self) -> bool {
            self.hostflags & X509_CHECK_FLAG_NEVER_CHECK_SUBJECT == 0
        }

        #[setter]
        fn set_hostname_checks_common_name(&mut self, it: &mut Interp, value: bool) -> R<()> {
            if value {
                self.hostflags &= !X509_CHECK_FLAG_NEVER_CHECK_SUBJECT;
            } else {
                self.hostflags |= X509_CHECK_FLAG_NEVER_CHECK_SUBJECT;
            }
            self.engine.set_hostflags(self.hostflags).map_err(|e| engine_error(it, &e))
        }

        #[getter]
        fn num_tickets(&self) -> usize {
            self.engine.num_tickets()
        }

        #[setter]
        fn set_num_tickets(&mut self, it: &mut Interp, value: i64) -> R<()> {
            if self.protocol != PROTOCOL_TLS_SERVER {
                return Err(it.value_error("SSLContext is not a server context."));
            }
            if value < 0 {
                return Err(it.value_error("value must be non-negative"));
            }
            if !self.engine.set_num_tickets(value as usize) {
                return Err(it.value_error("failed to set num tickets."));
            }
            Ok(())
        }

        #[getter]
        fn security_level(&self) -> i32 {
            self.engine.security_level()
        }

        #[getter]
        fn minimum_version(&self) -> i64 {
            match self.engine.min_proto() {
                0 => -2,
                v => v as i64,
            }
        }

        #[setter]
        fn set_minimum_version(&mut self, it: &mut Interp, value: i64) -> R<()> {
            self.set_version(it, false, value)
        }

        #[getter]
        fn maximum_version(&self) -> i64 {
            match self.engine.max_proto() {
                0 => -1,
                v => v as i64,
            }
        }

        #[setter]
        fn set_maximum_version(&mut self, it: &mut Interp, value: i64) -> R<()> {
            self.set_version(it, true, value)
        }

        #[getter]
        fn keylog_filename(&self) -> Option<String> {
            self.keylog.clone()
        }

        #[setter]
        fn set_keylog_filename(&mut self, it: &mut Interp, value: &Value) -> R<()> {
            if value.is_none() {
                self.keylog = None;
                return Ok(());
            }
            let path = path_text(it, value)?;
            if let Err(e) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
                return Err(it.os_error_errno(e.raw_os_error().unwrap_or(5), Some(value), None));
            }
            self.keylog = Some(path);
            Ok(())
        }

        #[getter]
        fn sni_callback(&self) -> Value {
            self.sni.clone().unwrap_or(Value::None)
        }

        #[setter]
        fn set_sni_callback(&mut self, it: &mut Interp, value: &Value) -> R<()> {
            if self.protocol == PROTOCOL_TLS_CLIENT {
                return Err(it.value_error("sni_callback cannot be set on TLS_CLIENT context"));
            }
            if value.is_none() {
                self.sni = None;
            } else if it.is_callable(value) {
                self.sni = Some(value.clone());
            } else {
                return Err(it.type_error("not a callable object"));
            }
            Ok(())
        }

        fn set_servername_callback(&mut self, it: &mut Interp, callback: &Value) -> R<()> {
            self.set_sni_callback(it, callback)
        }

        #[getter]
        fn _msg_callback(&self) -> Value {
            self.msg.clone().unwrap_or(Value::None)
        }

        #[setter]
        fn set__msg_callback(&mut self, it: &mut Interp, value: &Value) -> R<()> {
            if value.is_none() {
                self.msg = None;
            } else if it.is_callable(value) {
                self.msg = Some(value.clone());
            } else {
                return Err(it.type_error("not a callable object"));
            }
            Ok(())
        }
    }

    // ---- MemoryBIO and SSLSession -------------------------------------------------------------

    #[class(name = "MemoryBIO", module = "_ssl")]
    #[derive(Default)]
    pub struct MemoryBio {
        data: Vec<u8>,
        head: usize,
        eof: bool,
    }

    impl MemoryBio {
        fn pending_len(&self) -> usize {
            self.data.len() - self.head
        }

        fn take_all(&mut self) -> Vec<u8> {
            let out = self.data.split_off(self.head);
            self.data.clear();
            self.head = 0;
            out
        }

        fn push(&mut self, bytes: &[u8]) {
            self.data.extend_from_slice(bytes);
        }
    }

    #[methods]
    impl MemoryBio {
        #[constructor]
        fn new(cls: This<Value>) -> R<Value> {
            let Value::Obj(cls) = &cls.0 else { unreachable!("checked by the entry") };
            Ok(opaque_instance(cls, MemoryBio::default()))
        }

        /// The number of bytes currently in the memory BIO.
        #[getter]
        fn pending(&self) -> usize {
            self.pending_len()
        }

        /// Whether the memory BIO is at EOF.
        #[getter]
        fn eof(&self) -> bool {
            self.pending_len() == 0 && self.eof
        }

        /// Read up to size bytes from the memory BIO.
        ///
        /// If size is not specified, read the entire buffer.
        /// If the return value is an empty bytes instance, this means either
        /// EOF or that no data is available. Use the "eof" property to
        /// distinguish between the two.
        fn read(&mut self, #[default(-1)] size: i64) -> Vec<u8> {
            let avail = self.pending_len();
            let n = if size < 0 || size as usize > avail { avail } else { size as usize };
            let out = self.data[self.head..self.head + n].to_vec();
            self.head += n;
            if self.head == self.data.len() {
                self.data.clear();
                self.head = 0;
            }
            out
        }

        /// Writes the bytes b into the memory BIO.
        ///
        /// Returns the number of bytes written.
        fn write(&mut self, it: &mut Interp, b: &[u8]) -> R<usize> {
            if self.eof {
                return Err(ssl_error(it, "cannot write() after write_eof()"));
            }
            if b.len() > i32::MAX as usize {
                return Err(it.new_exc_str("OverflowError", &format!("string longer than {} bytes", i32::MAX)));
            }
            self.push(b);
            Ok(b.len())
        }

        /// Write an EOF marker to the memory BIO.
        ///
        /// When all data has been read, the "eof" property will be True.
        fn write_eof(&mut self) {
            self.eof = true;
        }
    }

    /// An SSL session, as a client keeps it for resumption.
    #[class(name = "SSLSession", module = "_ssl", hint(py(final, unhashable)))]
    pub struct SslSession {
        der: Vec<u8>,
        info: engine::SessionInfo,
        ctx: Py<Context>,
    }

    #[methods]
    impl SslSession {
        /// Session id
        #[getter]
        fn id(&self) -> Vec<u8> {
            self.info.id.clone()
        }

        /// Session creation time (seconds since epoch).
        #[getter]
        fn time(&self) -> i64 {
            self.info.time
        }

        /// Session timeout (delta in seconds).
        #[getter]
        fn timeout(&self) -> i64 {
            self.info.timeout
        }

        /// Ticket life time hint.
        #[getter]
        fn ticket_lifetime_hint(&self) -> u64 {
            self.info.ticket_lifetime_hint
        }

        /// Does the session contain a ticket?
        #[getter]
        fn has_ticket(&self) -> bool {
            self.info.has_ticket
        }

        #[proto(eq)]
        fn eq(slf: This<Py<Self>>, it: &mut Interp, other: &Value) -> R<Value> {
            let Some(other) = Py::<SslSession>::from_value(it, other) else {
                return Ok(Value::NotImplemented);
            };
            if same(slf.0.value(), other.value()) {
                return Ok(Value::Bool(true));
            }
            let a = slf.0.borrow(it)?.info.id.clone();
            let b = other.borrow(it)?.info.id.clone();
            Ok(Value::Bool(a == b))
        }
    }

    // ---- _SSLSocket ---------------------------------------------------------------------------

    #[derive(Clone)]
    enum Transport {
        Socket(Ref),
        Bio(Py<MemoryBio>, Py<MemoryBio>),
    }

    #[class(name = "_SSLSocket", module = "_ssl", hint(py(final)))]
    pub struct SslSocket {
        session: engine::Session,
        ctx: Py<Context>,
        transport: Transport,
        server_side: bool,
        server_hostname: Option<String>,
        owner: Option<Ref>,
        handshake_done: bool,
        outbuf: Vec<u8>,
        eof_fed: bool,
        messages: bool,
    }

    /// `_ssl_configure_hostname`: SNI and the certificate's host check.
    fn configure_hostname(it: &mut Interp, s: &mut engine::Session, host: &str, check: bool, flags: u32) -> R<()> {
        let ip = host.parse::<std::net::IpAddr>().is_ok();
        if !ip && !s.set_servername(host) {
            return Err(ssl_error(it, "invalid server_hostname"));
        }
        if check {
            s.set_host_check(host, flags, ip).map_err(|e| engine_error(it, &e))?;
        }
        Ok(())
    }

    fn make_socket(
        it: &mut Interp,
        ctx: &Py<Context>,
        transport: Transport,
        server_side: bool,
        hostname: Option<&Value>,
        owner: Option<&Value>,
        session: Option<&Value>,
    ) -> R<Value> {
        let hostname = match hostname {
            None => None,
            Some(h) if h.as_str().is_some() => {
                let encoded = it.call_method(h, "encode", vec![Value::str("idna")])?;
                let bytes = it.bytes_of(&encoded)?;
                Some(String::from_utf8_lossy(&bytes).into_owned())
            }
            Some(h) => {
                let t = it.type_name_of(h);
                return Err(it.type_error(&format!("server_hostname must be str or None, not {t}")));
            }
        };
        let mut s = {
            let c = ctx.borrow(it)?;
            if c.protocol == PROTOCOL_TLS_CLIENT && server_side {
                return Err(it.value_error("Cannot create a server socket with a PROTOCOL_TLS_CLIENT context"));
            }
            if c.protocol == PROTOCOL_TLS_SERVER && !server_side {
                return Err(it.value_error("Cannot create a client socket with a PROTOCOL_TLS_SERVER context"));
            }
            let mut s = engine::Session::new_inherit(&c.engine, server_side).map_err(|e| engine_error(it, &e))?;
            if let Some(host) = &hostname {
                configure_hostname(it, &mut s, host, c.check_hostname, c.hostflags)?;
            }
            if !c.alpn.is_empty() && !s.set_alpn_protocols(&c.alpn) {
                return Err(ssl_error(it, "failed to set ALPN protocols"));
            }
            if server_side {
                s.set_alpn_lenient();
                s.enable_hello_callback();
            } else if c.post_handshake_auth {
                s.enable_post_handshake_auth();
            }
            s.enable_keylog();
            s
        };
        if let Some(value) = session {
            if server_side {
                return Err(it.value_error("Cannot set session for server-side SSLSocket."));
            }
            let Some(saved) = Py::<SslSession>::from_value(it, value) else {
                return Err(it.type_error("Value is not a SSLSession."));
            };
            let (der, saved_ctx) = {
                let b = saved.borrow(it)?;
                (b.der.clone(), b.ctx.clone())
            };
            if !same(saved_ctx.value(), ctx.value()) {
                return Err(it.value_error("Session refers to a different SSLContext."));
            }
            if !s.set_session(&der) {
                return Err(ssl_error(it, "failed to set session"));
            }
        }
        let owner = owner.map(|o| Ref::new(it, o));
        let sock = SslSocket {
            session: s,
            ctx: ctx.clone(),
            transport,
            server_side,
            server_hostname: hostname,
            owner,
            handshake_done: false,
            outbuf: Vec::new(),
            eof_fed: false,
            messages: false,
        };
        Ok(Py::new(it, sock).into_value())
    }

    enum Step<T> {
        Done(T),
        Fail(i32, i64),
    }

    struct Turn<T> {
        step: Step<T>,
        out: Vec<u8>,
        raw: u64,
        verify: i64,
        host: Option<String>,
        events: Vec<Event>,
        messages: Vec<engine::Message>,
        transport: Transport,
        ctx: Py<Context>,
    }

    impl SslSocket {
        /// Memory-BIO transport: hands everything the incoming BIO holds to the engine.
        fn feed_bio(&mut self, it: &mut Interp) -> R<()> {
            let Transport::Bio(incoming, _) = &self.transport else { return Ok(()) };
            let (bytes, eof) = incoming.with(it, |b| (b.take_all(), b.eof))?;
            if !bytes.is_empty() {
                self.session.feed(&bytes);
            }
            if eof && !self.eof_fed {
                self.session.feed_eof();
                self.eof_fed = true;
            }
            Ok(())
        }

        fn collect<T>(&mut self, step: Step<T>) -> Turn<T> {
            let failed = matches!(step, Step::Fail(..));
            let (raw, verify) = if failed { (self.session.peek_error(), self.session.verify_result()) } else { (0, 0) };
            if failed {
                self.session.clear_errors();
            }
            let mut out = std::mem::take(&mut self.outbuf);
            out.extend(self.session.take_output());
            Turn {
                step,
                out,
                raw,
                verify,
                host: self.server_hostname.clone(),
                events: self.session.take_events(),
                messages: self.session.take_messages(),
                transport: self.transport.clone(),
                ctx: self.ctx.clone(),
            }
        }
    }

    /// The descriptor and timeout of the socket a connection runs over.
    fn sock_info(it: &mut Interp, sock: &Ref) -> R<(i32, Option<f64>)> {
        let closed = |it: &mut Interp| ssl_error(it, "Underlying socket has been closed.");
        let Some(v) = sock.get(it)? else { return Err(closed(it)) };
        match fd_and_timeout(it, &v) {
            Some((fd, timeout)) if fd >= 0 => Ok((fd, timeout)),
            _ => Err(closed(it)),
        }
    }

    /// Sends `data`, waiting for the socket as its timeout says; what a non-blocking socket did
    /// not take is returned.
    fn send_all(it: &mut Interp, fd: i32, timeout: Option<f64>, deadline: Option<f64>, data: &[u8], msg: &str) -> R<usize> {
        let mut sent = 0;
        while sent < data.len() {
            if timeout != Some(0.0) && !wait_ready(it, fd, true, deadline)? {
                return Err(timeout_error(it, msg));
            }
            match net::send(fd, &data[sent..], nosignal()) {
                Ok(n) => sent += n,
                Err(e) if e.errno() == EINTR => crate::builtins::signalm::check(it)?,
                Err(e) if net::would_block(e.errno()) => {
                    if timeout == Some(0.0) {
                        break;
                    }
                }
                Err(e) => return Err(it.os_error_errno(e.errno(), None, None)),
            }
        }
        Ok(sent)
    }

    enum Fill {
        Data,
        Eof,
        WouldBlock,
    }

    /// Reads from the socket into the engine (or signals EOF).
    fn fill_from_socket(
        it: &mut Interp,
        slf: &Py<SslSocket>,
        fd: i32,
        timeout: Option<f64>,
        deadline: Option<f64>,
        msg: &str,
    ) -> R<Fill> {
        let mut buf = vec![0u8; READ_CHUNK];
        loop {
            if timeout != Some(0.0) && !wait_ready(it, fd, false, deadline)? {
                return Err(timeout_error(it, msg));
            }
            match net::recv(fd, &mut buf, 0) {
                Ok(0) => {
                    slf.with(it, |s| s.session.feed_eof())?;
                    return Ok(Fill::Eof);
                }
                Ok(n) => {
                    slf.with(it, |s| s.session.feed(&buf[..n]))?;
                    return Ok(Fill::Data);
                }
                Err(e) if e.errno() == EINTR => crate::builtins::signalm::check(it)?,
                Err(e) if net::would_block(e.errno()) => {
                    if timeout == Some(0.0) {
                        return Ok(Fill::WouldBlock);
                    }
                }
                Err(e) => return Err(it.os_error_errno(e.errno(), None, None)),
            }
        }
    }

    /// The object message and SNI callbacks see as the connection: the owner, else the socket.
    fn conn_object(it: &mut Interp, slf: &Py<SslSocket>) -> R<Value> {
        let owner = slf.borrow(it)?.owner.clone();
        if let Some(owner) = owner {
            if let Some(v) = owner.get(it)? {
                return Ok(v);
            }
        }
        Ok(slf.value().clone())
    }

    /// Key log lines and protocol messages the engine reported during a turn.
    fn after_turn<T>(it: &mut Interp, slf: &Py<SslSocket>, turn: &mut Turn<T>) -> R<()> {
        for event in std::mem::take(&mut turn.events) {
            if let Event::Keylog(line) = event {
                let path = turn.ctx.borrow(it)?.keylog.clone();
                if let Some(path) = path {
                    use std::io::Write;
                    let opened = std::fs::OpenOptions::new().create(true).append(true).open(&path);
                    if let Ok(mut file) = opened {
                        let _ = file.write_all(&line);
                    }
                }
            }
        }
        if turn.messages.is_empty() {
            return Ok(());
        }
        let messages = std::mem::take(&mut turn.messages);
        let callback = turn.ctx.borrow(it)?.msg.clone();
        let Some(callback) = callback else { return Ok(()) };
        let conn = conn_object(it, slf)?;
        for m in messages {
            let byte = |i: usize| m.data.get(i).copied().unwrap_or(0) as i64;
            let (version, msg_type) = match m.content_type {
                20 => (m.version as i64, 0x101),
                21 => (m.version as i64, byte(1)),
                22 | 257 => (m.version as i64, byte(0)),
                256 => ((byte(1) << 8) | byte(2), byte(0)),
                _ => (m.version as i64, -1),
            };
            let args = vec![
                conn.clone(),
                Value::str(if m.write { "write" } else { "read" }),
                Value::Int(version),
                Value::Int(m.content_type as i64),
                Value::Int(msg_type),
                Value::bytes(m.data),
            ];
            if let Err(e) = it.call(&callback, args, Vec::new()) {
                write_unraisable(it, &callback, &e);
            }
        }
        Ok(())
    }

    /// Runs the server-name callback for a paused client hello and resumes (or aborts) it.
    fn run_sni(it: &mut Interp, slf: &Py<SslSocket>) -> R<()> {
        let (hello, ctx) = {
            let mut s = slf.borrow_mut(it)?;
            (s.session.take_hello(), s.ctx.clone())
        };
        let callback = ctx.borrow(it)?.sni.clone();
        let (Some(hello), Some(callback)) = (hello, callback) else {
            return slf.with(it, |s| s.session.hello_done());
        };
        if !hello.servername.is_ascii() {
            let e = it.new_exc_str("UnicodeDecodeError", "servername is not ASCII");
            write_unraisable(it, &callback, &e);
            return slf.with(it, |s| s.session.set_hello_alert(ALERT_INTERNAL_ERROR));
        }
        let conn = conn_object(it, slf)?;
        let name = if hello.servername.is_empty() { Value::None } else { Value::string(hello.servername) };
        let outcome = it.call(&callback, vec![conn, name, ctx.value().clone()], Vec::new());
        let alert = match outcome {
            Err(e) => {
                write_unraisable(it, &callback, &e);
                Some(ALERT_HANDSHAKE_FAILURE)
            }
            Ok(Value::None) => None,
            Ok(v) => match v.is_int_like().then(|| it.index_of(&v)) {
                Some(Ok(n)) => Some(n as i32),
                _ => {
                    let e = it.type_error("sni_callback must return None or an integer");
                    write_unraisable(it, &callback, &e);
                    Some(ALERT_INTERNAL_ERROR)
                }
            },
        };
        slf.with(it, |s| match alert {
            Some(a) => s.session.set_hello_alert(a),
            None => s.session.hello_done(),
        })
    }

    /// Runs `step` on the connection until it completes: after every try the bytes the engine
    /// produced go to the transport, and what it asks for (input, a client-hello decision) is
    /// fetched, waiting as the socket's timeout says. A non-blocking socket or an empty memory
    /// BIO raises `SSLWantReadError`.
    fn drive<T>(it: &mut Interp, slf: &Py<SslSocket>, timeout_msg: &str, mut step: impl FnMut(&mut SslSocket) -> Step<T>) -> R<T> {
        let mut deadline: Option<Option<f64>> = None;
        loop {
            let mut turn = {
                let mut s = slf.borrow_mut(it)?;
                if !s.messages && s.ctx.borrow(it)?.msg.is_some() {
                    s.session.enable_messages();
                    s.messages = true;
                }
                s.feed_bio(it)?;
                let outcome = step(&mut s);
                s.collect(outcome)
            };
            after_turn(it, slf, &mut turn)?;
            let Turn { step: outcome, out, raw, verify, host, transport, .. } = turn;
            let fail = |code: i32, ret: i64| Fail { code, ret, raw, verify, host: host.clone() };
            let mut unsent = Vec::new();
            let mut sock_state: Option<(i32, Option<f64>)> = None;
            if let Transport::Socket(sock) = &transport {
                let wants_socket = !out.is_empty() || matches!(outcome, Step::Fail(engine::SSL_ERROR_WANT_READ | engine::SSL_ERROR_WANT_WRITE, _));
                if wants_socket {
                    let (fd, timeout) = sock_info(it, sock)?;
                    sock_state = Some((fd, timeout));
                    let until = *deadline.get_or_insert_with(|| timeout.filter(|t| *t > 0.0).map(|t| now(it) + t));
                    if !out.is_empty() {
                        let sent = send_all(it, fd, timeout, until, &out, timeout_msg)?;
                        unsent = out[sent..].to_vec();
                    }
                }
            } else if let Transport::Bio(_, outgoing) = &transport {
                if !out.is_empty() {
                    outgoing.with(it, |b| b.push(&out))?;
                }
            }
            if !unsent.is_empty() {
                slf.with(it, |s| s.outbuf = unsent.clone())?;
            }
            let (code, ret) = match outcome {
                Step::Done(v) => return Ok(v),
                Step::Fail(code, ret) => (code, ret),
            };
            match code {
                engine::SSL_ERROR_WANT_READ => match &transport {
                    Transport::Bio(incoming, _) => {
                        let ready = incoming.with(it, |b| b.pending_len() > 0 || b.eof)?;
                        if !ready {
                            return Err(session_error(it, &fail(code, ret)));
                        }
                    }
                    Transport::Socket(_) => {
                        let (fd, timeout) = sock_state.unwrap_or((-1, Some(0.0)));
                        let until = deadline.flatten();
                        if let Fill::WouldBlock = fill_from_socket(it, slf, fd, timeout, until, timeout_msg)? {
                            return Err(session_error(it, &fail(code, ret)));
                        }
                    }
                },
                engine::SSL_ERROR_WANT_WRITE => match sock_state {
                    Some((_, Some(t))) if t == 0.0 => return Err(session_error(it, &fail(code, ret))),
                    Some(_) => {}
                    None => return Err(session_error(it, &fail(code, ret))),
                },
                engine::SSL_ERROR_WANT_CLIENT_HELLO_CB => run_sni(it, slf)?,
                _ => return Err(session_error(it, &fail(code, ret))),
            }
        }
    }

    #[methods]
    impl SslSocket {
        /// Does the handshake.
        fn do_handshake(slf: This<Py<Self>>, it: &mut Interp) -> R<()> {
            drive(it, &slf.0, "The handshake operation timed out", |s| match s.session.do_handshake() {
                0 => {
                    s.handshake_done = true;
                    Step::Done(())
                }
                code => Step::Fail(code, 0),
            })
        }

        /// Writes the bytes-like object b into the SSL object.
        ///
        /// Returns the number of bytes written.
        fn write(slf: This<Py<Self>>, it: &mut Interp, b: &[u8]) -> R<usize> {
            if b.len() > i32::MAX as usize {
                return Err(it.new_exc_str("OverflowError", &format!("string longer than {} bytes", i32::MAX)));
            }
            let blocked = {
                let s = slf.0.borrow(it)?;
                !s.outbuf.is_empty()
            };
            if blocked {
                drive(it, &slf.0, "The write operation timed out", |s| {
                    if s.outbuf.is_empty() {
                        Step::Done(())
                    } else {
                        Step::Fail(engine::SSL_ERROR_WANT_WRITE, -1)
                    }
                })?;
            }
            drive(it, &slf.0, "The write operation timed out", |s| {
                let n = s.session.write(b);
                if n >= 0 {
                    Step::Done(n as usize)
                } else {
                    Step::Fail((-n) as i32, -1)
                }
            })
        }

        /// Read up to size bytes from the SSL socket.
        fn read(slf: This<Py<Self>>, it: &mut Interp, size: i64, buffer: Option<&mut [u8]>) -> R<Value> {
            let want = match &buffer {
                None => {
                    if size < 0 {
                        return Err(it.value_error("size should not be negative"));
                    }
                    size as usize
                }
                Some(buf) => {
                    if size <= 0 || size as usize > buf.len() {
                        buf.len()
                    } else {
                        size as usize
                    }
                }
            };
            if want == 0 {
                return Ok(match buffer {
                    Some(_) => Value::Int(0),
                    None => Value::bytes(Vec::new()),
                });
            }
            let data = drive(it, &slf.0, "The read operation timed out", |s| match s.session.read(want) {
                Io::Data(d) => Step::Done(d),
                Io::Code(code) => {
                    if code == engine::SSL_ERROR_ZERO_RETURN && s.session.received_shutdown() {
                        Step::Done(Vec::new())
                    } else {
                        Step::Fail(code, 0)
                    }
                }
            })?;
            Ok(match buffer {
                Some(buf) => {
                    buf[..data.len()].copy_from_slice(&data);
                    Value::Int(data.len() as i64)
                }
                None => Value::bytes(data),
            })
        }

        /// Returns the number of already decrypted bytes available for read, pending on the connection.
        fn pending(&self) -> i64 {
            self.session.pending() as i64
        }

        /// Does the SSL shutdown handshake with the remote end.
        fn shutdown(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
            let mut zeros = 0;
            loop {
                let closed = drive(it, &slf.0, "The read operation timed out", |s| {
                    let (ret, err) = s.session.shutdown_step();
                    if ret > 0 {
                        Step::Done(true)
                    } else if ret == 0 {
                        Step::Done(false)
                    } else {
                        Step::Fail(err, ret as i64)
                    }
                })?;
                if closed {
                    break;
                }
                zeros += 1;
                if zeros > 1 {
                    break;
                }
            }
            let transport = slf.0.borrow(it)?.transport.clone();
            match transport {
                Transport::Socket(sock) => Ok(sock.get(it)?.unwrap_or(Value::None)),
                Transport::Bio(..) => Ok(Value::None),
            }
        }

        /// Returns the certificate for the peer.
        ///
        /// If no certificate was provided, returns None.  If a certificate was
        /// provided, but not validated, returns an empty dictionary.  Otherwise
        /// returns a dict containing information about the peer certificate.
        ///
        /// If the optional argument is True, returns a DER-encoded copy of the
        /// peer certificate, or None if no certificate was provided.  This will
        /// return the certificate even if it wasn't validated.
        fn getpeercert(&self, it: &mut Interp, #[default(false)] binary_form: bool) -> R<Value> {
            if !self.handshake_done {
                return Err(it.value_error("handshake not done yet"));
            }
            let chain = self.session.peer_certificates();
            let Some(leaf) = chain.into_iter().next() else { return Ok(Value::None) };
            if binary_form {
                return Ok(Value::bytes(leaf));
            }
            if self.session.verify_mode() & SSL_VERIFY_PEER == 0 {
                return Ok(Value::Obj(it.new_dict()));
            }
            cert_dict(it, &leaf)
        }

        /// Returns the protocol that was selected during the TLS handshake, or None.
        fn selected_alpn_protocol(&self) -> Option<String> {
            self.session.alpn_selected().map(|p| String::from_utf8_lossy(&p).into_owned())
        }

        fn cipher(&self, _it: &mut Interp) -> Value {
            match self.session.cipher() {
                Some((name, _, version)) => {
                    Value::tuple(vec![Value::string(name), Value::string(version), Value::Int(self.session.cipher_bits() as i64)])
                }
                None => Value::None,
            }
        }

        fn shared_ciphers(&self) -> Value {
            match self.session.shared_ciphers() {
                None => Value::None,
                Some(list) => Value::list(
                    list.into_iter()
                        .map(|(name, version, bits)| Value::tuple(vec![Value::string(name), Value::string(version), Value::Int(bits as i64)]))
                        .collect(),
                ),
            }
        }

        fn compression(&self) -> Option<String> {
            self.session.compression()
        }

        /// Return the TLS protocol version negotiated, or None before the handshake is done.
        fn version(&self) -> Option<String> {
            if self.session.handshake_finished() {
                self.session.protocol()
            } else {
                None
            }
        }

        fn verify_client_post_handshake(slf: This<Py<Self>>, it: &mut Interp) -> R<()> {
            let result = slf.0.with(it, |s| s.session.verify_client_post_handshake())?;
            if let Err(e) = result {
                return Err(engine_error(it, &e));
            }
            drive(it, &slf.0, "The write operation timed out", |_| Step::Done(()))
        }

        /// Get channel binding data for current connection.
        ///
        /// Raise ValueError if the requested `cb_type` is not supported.  Return bytes
        /// of the data or None if the data is not available (e.g. before the handshake).
        /// Only 'tls-unique' channel binding data from RFC 5929 is supported.
        fn get_channel_binding(&self, it: &mut Interp, #[kw] cb_type: Option<&str>) -> R<Value> {
            let cb_type = cb_type.unwrap_or("tls-unique");
            if cb_type != "tls-unique" {
                return Err(it.value_error(&format!("'{cb_type}' channel binding type not implemented")));
            }
            let own = self.session.session_reused() ^ !self.server_side;
            Ok(self.session.finished(!own).map_or(Value::None, Value::bytes))
        }

        #[getter]
        fn session(&self, it: &mut Interp) -> R<Value> {
            match self.session.session_bytes() {
                Some(der) => {
                    let info = engine::session_info(&der).unwrap_or_default();
                    Ok(Py::new(it, SslSession { der, info, ctx: self.ctx.clone() }).into_value())
                }
                None => Ok(Value::None),
            }
        }

        #[setter]
        fn set_session(&mut self, it: &mut Interp, value: &Value) -> R<()> {
            if self.server_side {
                return Err(it.value_error("Cannot set session for server-side SSLSocket."));
            }
            if self.handshake_done {
                return Err(it.value_error("Cannot set session after handshake."));
            }
            let Some(saved) = Py::<SslSession>::from_value(it, value) else {
                return Err(it.type_error("Value is not a SSLSession."));
            };
            let (der, saved_ctx) = {
                let b = saved.borrow(it)?;
                (b.der.clone(), b.ctx.clone())
            };
            if !same(saved_ctx.value(), self.ctx.value()) {
                return Err(it.value_error("Session refers to a different SSLContext."));
            }
            if !self.session.set_session(&der) {
                return Err(ssl_error(it, "failed to set session"));
            }
            Ok(())
        }

        /// Was the client session reused during handshake?
        #[getter]
        fn session_reused(&self) -> bool {
            self.session.session_reused()
        }

        /// The Python-level owner of this object.
        #[getter]
        fn owner(&self, it: &mut Interp) -> R<Value> {
            match &self.owner {
                Some(owner) => Ok(owner.get(it)?.unwrap_or(Value::None)),
                None => Ok(Value::None),
            }
        }

        #[setter]
        fn set_owner(&mut self, it: &mut Interp, value: &Value) {
            self.owner = if value.is_none() { None } else { Some(Ref::new(it, value)) };
        }

        /// The SSLContext that is currently in use.
        #[getter]
        fn context(&self) -> Value {
            self.ctx.value().clone()
        }

        #[setter]
        fn set_context(&mut self, it: &mut Interp, value: &Value) -> R<()> {
            let Some(ctx) = Py::<Context>::from_value(it, value) else {
                return Err(it.type_error("The value must be a SSLContext"));
            };
            {
                let c = ctx.borrow(it)?;
                self.session.switch_context(&c.engine).map_err(|e| engine_error(it, &e))?;
            }
            self.ctx = ctx;
            Ok(())
        }

        /// Whether this is a server-side socket.
        #[getter]
        fn server_side(&self) -> bool {
            self.server_side
        }

        /// The currently set server hostname (for SNI).
        #[getter]
        fn server_hostname(&self) -> Option<String> {
            self.server_hostname.clone()
        }
    }

    // ---- module functions ---------------------------------------------------------------------

    /// Mix string into the OpenSSL PRNG state.
    ///
    /// entropy (a float) is a lower bound on the entropy contained in
    /// string.  See RFC 4086.
    #[op]
    #[allow(non_snake_case)]
    fn RAND_add(it: &mut Interp, string: &Value, entropy: f64) -> R<()> {
        let _ = entropy;
        if string.as_str().is_none() && it.buffer_bytes(string).is_err() {
            let t = it.type_name_of(string);
            return Err(it.type_error(&format!("RAND_add() argument 1 must be str or bytes-like object, not {t}")));
        }
        Ok(())
    }

    /// Generate n cryptographically strong pseudo-random bytes.
    #[op]
    #[allow(non_snake_case)]
    fn RAND_bytes(it: &mut Interp, n: i64) -> R<Vec<u8>> {
        if n < 0 {
            return Err(it.value_error("num must be positive"));
        }
        let mut buf = vec![0u8; n as usize];
        lumen_os::proc::entropy(&mut buf).map_err(|_| ssl_error(it, "RAND_bytes failed"))?;
        Ok(buf)
    }

    /// Returns True if the OpenSSL PRNG has been seeded with enough data and False if not.
    ///
    /// It is necessary to seed the PRNG with RAND_add() on some platforms before
    /// using the ssl() function.
    #[op]
    #[allow(non_snake_case)]
    fn RAND_status() -> bool {
        true
    }

    fn object_value(info: engine::ObjectInfo) -> Value {
        Value::tuple(vec![
            Value::Int(info.nid as i64),
            Value::string(info.short_name),
            Value::string(info.long_name),
            Value::string(info.oid),
        ])
    }

    /// Lookup NID, short name, long name and OID of an ASN1_OBJECT.
    ///
    /// By default objects are looked up by OID. With name=True short and
    /// long name are also matched.
    #[op]
    fn txt2obj(it: &mut Interp, txt: &str, #[kw] #[default(false)] name: bool) -> R<Value> {
        match engine::object_from_text(txt, name) {
            Some(info) => Ok(object_value(info)),
            None => {
                let shown: String = txt.chars().take(100).collect();
                Err(it.value_error(&format!("unknown object '{shown}'")))
            }
        }
    }

    /// Lookup NID, short name, long name and OID of an ASN1_OBJECT by NID.
    #[op]
    fn nid2obj(it: &mut Interp, nid: i64) -> R<Value> {
        if nid < 0 {
            return Err(it.value_error("NID must be positive."));
        }
        match i32::try_from(nid).ok().and_then(engine::object_from_nid) {
            Some(info) => Ok(object_value(info)),
            None => Err(it.value_error(&format!("unknown NID {nid}"))),
        }
    }

    /// Returns (cafile env var, cafile, capath env var, capath) of the default verify paths.
    #[op]
    fn get_default_verify_paths() -> Value {
        match engine::default_verify_paths() {
            Some((cafile_env, cafile, capath_env, capath)) => Value::tuple(vec![
                Value::string(cafile_env),
                Value::string(cafile),
                Value::string(capath_env),
                Value::string(capath),
            ]),
            None => Value::tuple(vec![Value::str("SSL_CERT_FILE"), Value::str(""), Value::str("SSL_CERT_DIR"), Value::str("")]),
        }
    }

    #[op]
    fn _test_decode_cert(it: &mut Interp, path: &Value) -> R<Value> {
        let pem = read_file(it, path)?;
        match x509::pem_find(&pem, x509::CERTIFICATE_LABELS) {
            Ok(der) => cert_dict(it, &der),
            Err(_) => Err(ssl_error(it, "Error decoding PEM-encoded file")),
        }
    }

    // ---- module setup -------------------------------------------------------------------------

    fn exception_class(it: &mut Interp, name: &str, base: &Obj, doc: &str) -> Obj {
        let ty = crate::builtins::native::new_type(it, "ssl", name, Some(base), Layout::Exception);
        if let Some(d) = ty.dict.borrow().as_ref() {
            dict_set_str(d, "__doc__", Value::str(doc));
        }
        ty
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        let os_error = it.exc_type("OSError");
        let value_error = it.exc_type("ValueError");

        let ssl = exception_class(it, "SSLError", &os_error, "An error occurred in the SSL implementation.");
        crate::bind::extend_type::<SslErrorExt>(it, &ssl);
        let zero_return = exception_class(it, "SSLZeroReturnError", &ssl, "SSL/TLS connection was closed cleanly.");
        let want_read = exception_class(it, "SSLWantReadError", &ssl, "Non-blocking SSL socket needs to read more data before the requested operation can be completed.");
        let want_write = exception_class(it, "SSLWantWriteError", &ssl, "Non-blocking SSL socket needs to write more data before the requested operation can be completed.");
        let syscall = exception_class(it, "SSLSyscallError", &ssl, "System error when attempting SSL operation.");
        let eof = exception_class(it, "SSLEOFError", &ssl, "SSL/TLS connection terminated abruptly.");
        let cert_verification = exception_class(it, "SSLCertVerificationError", &ssl, "A certificate could not be verified.");
        it.set_bases(&cert_verification, vec![ssl.clone(), value_error]);
        for (name, cls) in [
            ("SSLError", &ssl),
            ("SSLZeroReturnError", &zero_return),
            ("SSLWantReadError", &want_read),
            ("SSLWantWriteError", &want_write),
            ("SSLSyscallError", &syscall),
            ("SSLEOFError", &eof),
            ("SSLCertVerificationError", &cert_verification),
        ] {
            dict_set_str(&d, name, Value::Obj(cls.clone()));
        }
        {
            let st = it.native_state::<State>();
            st.ssl = Some(ssl);
            st.zero_return = Some(zero_return);
            st.want_read = Some(want_read);
            st.want_write = Some(want_write);
            st.syscall = Some(syscall);
            st.eof = Some(eof);
            st.cert_verification = Some(cert_verification);
        }

        let ints: &[(&str, i64)] = &[
            ("SSL_ERROR_SSL", 1),
            ("SSL_ERROR_WANT_READ", 2),
            ("SSL_ERROR_WANT_WRITE", 3),
            ("SSL_ERROR_WANT_X509_LOOKUP", 4),
            ("SSL_ERROR_SYSCALL", 5),
            ("SSL_ERROR_ZERO_RETURN", 6),
            ("SSL_ERROR_WANT_CONNECT", 7),
            ("SSL_ERROR_EOF", 8),
            ("SSL_ERROR_INVALID_ERROR_CODE", 10),
            ("CERT_NONE", CERT_NONE),
            ("CERT_OPTIONAL", CERT_OPTIONAL),
            ("CERT_REQUIRED", CERT_REQUIRED),
            ("VERIFY_DEFAULT", 0),
            ("VERIFY_CRL_CHECK_LEAF", 0x4),
            ("VERIFY_CRL_CHECK_CHAIN", 0xc),
            ("VERIFY_X509_STRICT", 0x20),
            ("VERIFY_ALLOW_PROXY_CERTS", 0x40),
            ("VERIFY_X509_TRUSTED_FIRST", 0x8000),
            ("VERIFY_X509_PARTIAL_CHAIN", 0x80000),
            ("PROTOCOL_SSLv23", PROTOCOL_TLS as i64),
            ("PROTOCOL_TLS", PROTOCOL_TLS as i64),
            ("PROTOCOL_TLS_CLIENT", PROTOCOL_TLS_CLIENT as i64),
            ("PROTOCOL_TLS_SERVER", PROTOCOL_TLS_SERVER as i64),
            ("PROTOCOL_TLSv1", PROTOCOL_TLSV1 as i64),
            ("PROTOCOL_TLSv1_1", PROTOCOL_TLSV1_1 as i64),
            ("PROTOCOL_TLSv1_2", PROTOCOL_TLSV1_2 as i64),
            ("OP_ALL", OP_ALL as i64),
            ("OP_NO_SSLv2", 0),
            ("OP_NO_SSLv3", OP_NO_SSLV3 as i64),
            ("OP_NO_TLSv1", 0x0400_0000),
            ("OP_NO_TLSv1_1", 0x1000_0000),
            ("OP_NO_TLSv1_2", 0x0800_0000),
            ("OP_NO_TLSv1_3", 0x2000_0000),
            ("OP_CIPHER_SERVER_PREFERENCE", OP_CIPHER_SERVER_PREFERENCE as i64),
            ("OP_SINGLE_DH_USE", 0),
            ("OP_SINGLE_ECDH_USE", 0),
            ("OP_NO_TICKET", 0x4000),
            ("OP_NO_COMPRESSION", OP_NO_COMPRESSION as i64),
            ("OP_ENABLE_MIDDLEBOX_COMPAT", 0x10_0000),
            ("OP_NO_RENEGOTIATION", 0x4000_0000),
            ("OP_IGNORE_UNEXPECTED_EOF", 0x80),
            ("OP_ENABLE_KTLS", 0x8),
            ("OP_LEGACY_SERVER_CONNECT", 0x4),
            ("OP_ALLOW_NO_DHE_KEX", 0x400),
            ("PROTO_MINIMUM_SUPPORTED", -2),
            ("PROTO_MAXIMUM_SUPPORTED", -1),
            ("PROTO_SSLv3", 0x300),
            ("PROTO_TLSv1", 0x301),
            ("PROTO_TLSv1_1", 0x302),
            ("PROTO_TLSv1_2", 0x303),
            ("PROTO_TLSv1_3", 0x304),
            ("ALERT_DESCRIPTION_CLOSE_NOTIFY", 0),
            ("ALERT_DESCRIPTION_UNEXPECTED_MESSAGE", 10),
            ("ALERT_DESCRIPTION_BAD_RECORD_MAC", 20),
            ("ALERT_DESCRIPTION_RECORD_OVERFLOW", 22),
            ("ALERT_DESCRIPTION_DECOMPRESSION_FAILURE", 30),
            ("ALERT_DESCRIPTION_HANDSHAKE_FAILURE", 40),
            ("ALERT_DESCRIPTION_BAD_CERTIFICATE", 42),
            ("ALERT_DESCRIPTION_UNSUPPORTED_CERTIFICATE", 43),
            ("ALERT_DESCRIPTION_CERTIFICATE_REVOKED", 44),
            ("ALERT_DESCRIPTION_CERTIFICATE_EXPIRED", 45),
            ("ALERT_DESCRIPTION_CERTIFICATE_UNKNOWN", 46),
            ("ALERT_DESCRIPTION_ILLEGAL_PARAMETER", 47),
            ("ALERT_DESCRIPTION_UNKNOWN_CA", 48),
            ("ALERT_DESCRIPTION_ACCESS_DENIED", 49),
            ("ALERT_DESCRIPTION_DECODE_ERROR", 50),
            ("ALERT_DESCRIPTION_DECRYPT_ERROR", 51),
            ("ALERT_DESCRIPTION_PROTOCOL_VERSION", 70),
            ("ALERT_DESCRIPTION_INSUFFICIENT_SECURITY", 71),
            ("ALERT_DESCRIPTION_INTERNAL_ERROR", 80),
            ("ALERT_DESCRIPTION_USER_CANCELLED", 90),
            ("ALERT_DESCRIPTION_NO_RENEGOTIATION", 100),
            ("ALERT_DESCRIPTION_UNSUPPORTED_EXTENSION", 110),
            ("ALERT_DESCRIPTION_CERTIFICATE_UNOBTAINABLE", 111),
            ("ALERT_DESCRIPTION_UNRECOGNIZED_NAME", 112),
            ("ALERT_DESCRIPTION_BAD_CERTIFICATE_STATUS_RESPONSE", 113),
            ("ALERT_DESCRIPTION_BAD_CERTIFICATE_HASH_VALUE", 114),
            ("ALERT_DESCRIPTION_UNKNOWN_PSK_IDENTITY", 115),
        ];
        for &(name, value) in ints {
            dict_set_str(&d, name, Value::Int(value));
        }
        let flags: &[(&str, bool)] = &[
            ("HAS_SNI", true),
            ("HAS_TLS_UNIQUE", true),
            ("HAS_ECDH", true),
            ("HAS_NPN", false),
            ("HAS_ALPN", true),
            ("HAS_SSLv2", false),
            ("HAS_SSLv3", false),
            ("HAS_TLSv1", true),
            ("HAS_TLSv1_1", true),
            ("HAS_TLSv1_2", true),
            ("HAS_TLSv1_3", true),
            ("HAS_NEVER_CHECK_COMMON_NAME", true),
        ];
        for &(name, value) in flags {
            dict_set_str(&d, name, Value::Bool(value));
        }
        let (number, text) = engine::openssl_version().unwrap_or((0x3000_0000, "OpenSSL 3.0.0".to_string()));
        let info = vec![
            Value::Int(((number >> 28) & 0xf) as i64),
            Value::Int(((number >> 20) & 0xff) as i64),
            Value::Int(((number >> 12) & 0xff) as i64),
            Value::Int(((number >> 4) & 0xff) as i64),
            Value::Int((number & 0xf) as i64),
        ];
        dict_set_str(&d, "OPENSSL_VERSION_NUMBER", Value::Int(number as i64));
        dict_set_str(&d, "OPENSSL_VERSION_INFO", Value::tuple(info.clone()));
        dict_set_str(&d, "OPENSSL_VERSION", Value::string(text));
        dict_set_str(&d, "_OPENSSL_API_VERSION", Value::tuple(info));
        dict_set_str(&d, "_DEFAULT_CIPHERS", Value::str(DEFAULT_CIPHERS));
    }
}
