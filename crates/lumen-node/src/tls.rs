//! Native half of `node:tls`: secure contexts and TLS sessions over `lumen_tls::engine`. A session
//! owns no socket; `tls.js` moves encrypted bytes between it and the underlying stream.

#[cfg(all(unix, not(target_os = "android")))]
pub use imp::TlsRegistry;
#[cfg(all(unix, not(target_os = "android")))]
pub(crate) use imp::bindings::Module;

#[cfg(not(all(unix, not(target_os = "android"))))]
pub(crate) use unsupported::Module;

#[cfg(not(all(unix, not(target_os = "android"))))]
#[derive(Default)]
pub struct TlsRegistry;

#[cfg(not(all(unix, not(target_os = "android"))))]
mod unsupported {
    use lumen::embed::Value;

    pub(crate) use bindings::Module;

    #[lumen_bind::module(name = "__tls")]
    pub(crate) mod bindings {
        use super::*;
        use lumen::embed::{NativeError, NativeResult};

        fn unavailable() -> NativeResult<()> {
            Err(NativeError::runtime("node:tls is not available on this platform"))
        }

        #[op]
        pub fn available() -> bool {
            false
        }

        #[op(name = "rootCertificates")]
        pub fn root_certificates() -> NativeResult<()> {
            unavailable()
        }

        #[op]
        pub fn ciphers() -> NativeResult<()> {
            unavailable()
        }

        #[op(name = "ctxNew")]
        pub fn ctx_new(_method: &Value, _min: &Value, _max: &Value) -> NativeResult<()> {
            unavailable()
        }

        #[op(name = "ctxOp")]
        pub fn ctx_op(_id: &Value, _name: &Value, _a: &Value, _b: &Value) -> NativeResult<()> {
            unavailable()
        }

        #[op(name = "sessNew")]
        pub fn sess_new(_context_id: &Value, _is_server: &Value) -> NativeResult<()> {
            unavailable()
        }

        #[op(name = "sessFree")]
        pub fn sess_free(_id: &Value) -> NativeResult<()> {
            unavailable()
        }

        #[op(name = "sessOp")]
        pub fn sess_op(_id: &Value, _name: &Value, _a: &Value, _b: &Value) -> NativeResult<()> {
            unavailable()
        }

        #[op]
        pub fn feed(_id: &Value, _data: &Value) -> NativeResult<()> {
            unavailable()
        }

        #[op]
        pub fn output(_id: &Value) -> NativeResult<()> {
            unavailable()
        }

        #[op]
        pub fn read(_id: &Value, _max: &Value) -> NativeResult<()> {
            unavailable()
        }

        #[op]
        pub fn write(_id: &Value, _data: &Value) -> NativeResult<()> {
            unavailable()
        }

        #[op]
        pub fn events(_id: &Value) -> NativeResult<()> {
            unavailable()
        }

        #[op(name = "lastError")]
        pub fn last_error(_id: &Value) -> NativeResult<()> {
            unavailable()
        }
    }
}

#[cfg(all(unix, not(target_os = "android")))]
mod imp {
    use std::collections::HashMap;

    use lumen::embed::{Ctx, OpError, State, Value};
    use lumen_bind::{Data, Flag, Lenient, NativeError};
    use lumen_tls::engine::{ClientHello, Context, EngineError, Event, Io, Session};

    #[derive(Default)]
    pub struct TlsRegistry {
        next: u64,
        contexts: HashMap<u64, Context>,
        sessions: HashMap<u64, Session>,
    }

    type TlsResult = Result<Data, NativeError>;

    fn done(result: Result<(), EngineError>) -> TlsResult {
        result?;
        Ok(Data::None)
    }

    /// A polymorphic argument: the operation name decides which reading applies.
    struct Poly {
        bytes: Option<Vec<u8>>,
        text: Option<String>,
        num: f64,
        flag: bool,
    }

    fn poly(ctx: &mut Ctx, v: &Value) -> Result<Poly, NativeError> {
        let bytes = ctx.typed_array_bytes(v);
        let text = match v {
            _ if bytes.is_some() => None,
            Value::Undefined | Value::Null => None,
            Value::Str(s) => Some(s.as_str().to_string()),
            _ => Some(
                ctx.coerce_string(v)
                    .map_err(|_| NativeError::type_error("cannot convert the argument to a string"))?
                    .to_string(),
            ),
        };
        Ok(Poly { bytes, text, num: v.as_num_opt().unwrap_or(0.0), flag: matches!(v, Value::Bool(true)) })
    }

    fn hello_out(hello: ClientHello) -> Data {
        Data::List(vec![
            Data::Bytes(hello.session_id),
            Data::Str(hello.servername),
            Data::Bool(hello.has_ticket),
            Data::Bool(hello.ocsp_request),
            Data::Bytes(hello.alpn),
        ])
    }

    fn certificates_out(chain: Vec<Vec<u8>>) -> Data {
        Data::List(chain.into_iter().map(Data::Bytes).collect())
    }

    #[lumen_bind::module(name = "__tls")]
    pub(crate) mod bindings {
        use super::*;

        #[op]
        pub fn available() -> bool {
            lumen_tls::engine::openssl_available()
        }

        #[op(name = "rootCertificates")]
        pub fn root_certificates() -> Vec<String> {
            lumen_tls::engine::root_certificate_pems()
        }

        #[op]
        pub fn ciphers() -> Result<Vec<String>, NativeError> {
            Ok(lumen_tls::engine::cipher_names()?)
        }

        #[op(name = "ctxNew")]
        pub fn ctx_new(state: &mut State<TlsRegistry>, method: Lenient<Option<String>>, min: Lenient<f64>, max: Lenient<f64>) -> Result<f64, NativeError> {
            let context = Context::new(method.0.as_deref(), min.0 as i32, max.0 as i32)?;
            state.next += 1;
            let id = state.next;
            state.contexts.insert(id, context);
            Ok(id as f64)
        }

        #[op(name = "ctxOp")]
        pub fn ctx_op(ctx: &mut Ctx, id: Lenient<u64>, name: Lenient<Option<String>>, a: &Value, passphrase: &Value) -> TlsResult {
            let id = id.0;
            let name = name.0.unwrap_or_default();
            let a = poly(ctx, a)?;
            let passphrase = poly(ctx, passphrase)?.bytes;
            let Some(state) = ctx.op_state().get_mut::<TlsRegistry>() else {
                return Err(NativeError::runtime("the TLS registry is not installed"));
            };
            let text = match name.as_str() {
                "setCiphers" | "setCipherSuites" | "setSigalgs" | "setECDHCurve" => a.text,
                _ => None,
            };
            let number = a.num;
            let bytes = a.bytes;
            let out: TlsResult = match state.contexts.get_mut(&id) {
                None => Err(EngineError::plain("SecureContext is closed").into()),
                Some(context) => (|| -> TlsResult {
                    Ok(match name.as_str() {
                        "setKey" => match bytes {
                            Some(pem) => done(context.set_key(&pem, passphrase.as_deref()))?,
                            None => Data::None,
                        },
                        "setCert" => match bytes {
                            Some(pem) => done(context.set_cert(&pem))?,
                            None => Data::None,
                        },
                        "addCACert" => match bytes {
                            Some(pem) => done(context.add_ca_cert(&pem))?,
                            None => Data::None,
                        },
                        "addCRL" => match bytes {
                            Some(pem) => done(context.add_crl(&pem))?,
                            None => Data::None,
                        },
                        "addRootCerts" => done(context.add_root_certs())?,
                        "setCiphers" => done(context.set_ciphers(&text.unwrap_or_default()))?,
                        "setCipherSuites" => done(context.set_cipher_suites(&text.unwrap_or_default()))?,
                        "setSigalgs" => done(context.set_sigalgs(&text.unwrap_or_default()))?,
                        "setECDHCurve" => done(context.set_ecdh_curve(&text.unwrap_or_default()))?,
                        "setDHParam" => match context.set_dh_param(bytes.as_deref())? {
                            Some(warning) => Data::Str(warning.to_string()),
                            None => Data::None,
                        },
                        "setMinProto" => done(context.set_min_proto(number as i32))?,
                        "setMaxProto" => done(context.set_max_proto(number as i32))?,
                        "getMinProto" => Data::Float(context.min_proto() as f64),
                        "getMaxProto" => Data::Float(context.max_proto() as f64),
                        "setOptions" => done(context.set_options(number as u64))?,
                        "setSessionIdContext" => match bytes {
                            Some(context_id) => done(context.set_session_id_context(&context_id))?,
                            None => Data::None,
                        },
                        "setSessionTimeout" => done(context.set_session_timeout(number as i32))?,
                        "setTicketKeys" => match bytes {
                            Some(keys) => done(context.set_ticket_keys(&keys))?,
                            None => Data::None,
                        },
                        "getTicketKeys" => Data::Bytes(context.ticket_keys()),
                        "loadPKCS12" => match bytes {
                            Some(data) => done(context.load_pkcs12(&data, passphrase.as_deref()))?,
                            None => Data::None,
                        },
                        "getCertificate" => Data::Bytes(context.certificate().to_vec()),
                        "getIssuer" => Data::Bytes(context.issuer().to_vec()),
                        "close" => {
                            context.close();
                            Data::None
                        }
                        other => return Err(EngineError::plain(format!("unknown context operation {other}")).into()),
                    })
                })(),
            };
            if name == "close" {
                state.contexts.remove(&id);
            }
            out
        }

        #[op(name = "sessNew")]
        pub fn sess_new(state: &mut State<TlsRegistry>, context_id: Lenient<u64>, is_server: Flag) -> Result<f64, NativeError> {
            let registry = &mut **state;
            let Some(context) = registry.contexts.get(&context_id.0) else {
                return Err(EngineError::plain("SecureContext is closed").into());
            };
            let session = Session::new(context, is_server.0)?;
            registry.next += 1;
            let id = registry.next;
            registry.sessions.insert(id, session);
            Ok(id as f64)
        }

        #[op(name = "sessFree")]
        pub fn sess_free(state: &mut State<TlsRegistry>, id: Lenient<u64>) {
            state.sessions.remove(&id.0);
        }

        #[op(name = "sessOp")]
        pub fn sess_op(
            ctx: &mut Ctx,
            id: Lenient<u64>,
            name: Lenient<Option<String>>,
            a: &Value,
            b: &Value,
            context: Lenient<Option<Vec<u8>>>,
        ) -> TlsResult {
            let id = id.0;
            let name = name.0.unwrap_or_default();
            let a = poly(ctx, a)?;
            let b = poly(ctx, b)?;
            let Some(state) = ctx.op_state().get_mut::<TlsRegistry>() else {
                return Err(NativeError::runtime("the TLS registry is not installed"));
            };
            let number = a.num;
            let flag_a = a.flag;
            let flag_b = b.flag;
            let sni_context = if name == "setSniContext" { Some(a.num as u64) } else { None };
            let bytes = a.bytes;
            let text = a.text;
            let label = b.text;
            let context_bytes = context.0;
            let registry = state;
            let sni = sni_context.and_then(|id| registry.contexts.get(&id).map(|context| context as *const Context));
            let Some(session) = registry.sessions.get_mut(&id) else {
                return Err(EngineError::plain("TLS session is closed").into());
            };
            Ok(match name.as_str() {
                "setVerifyMode" => {
                    session.set_verify_mode(flag_a, flag_b);
                    Data::None
                }
                "verifyError" => match session.verify_error() {
                    Some((code, reason)) => Data::List(vec![Data::Float(code as f64), Data::Str(reason)]),
                    None => Data::None,
                },
                "protocol" => session.protocol().map_or(Data::None, Data::Str),
                "cipher" => match session.cipher() {
                    Some((name, standard, version)) => {
                        Data::List(vec![Data::Str(name), Data::Str(standard), Data::Str(version)])
                    }
                    None => Data::None,
                },
                "alpnSelected" => session.alpn_selected().map_or(Data::Bool(false), Data::Bytes),
                "setAlpn" => Data::Bool(bytes.is_some_and(|protocols| session.set_alpn_protocols(&protocols))),
                "servername" => session.servername().map_or(Data::Bool(false), Data::Str),
                "setServername" => Data::Bool(text.is_some_and(|name| session.set_servername(&name))),
                "getSession" => session.session_bytes().map_or(Data::None, Data::Bytes),
                "setSession" => Data::Bool(bytes.is_some_and(|data| session.set_session(&data))),
                "loadSession" => {
                    session.load_session(bytes.as_deref());
                    Data::None
                }
                "isSessionReused" => Data::Bool(session.session_reused()),
                "finished" => session.finished(flag_a).map_or(Data::None, Data::Bytes),
                "exportKeyingMaterial" => Data::Bytes(session.export_keying_material(
                    number as usize,
                    &label.unwrap_or_default(),
                    context_bytes.as_deref(),
                )?),
                "peerCertificates" => certificates_out(session.peer_certificates()),
                "ownCertificate" => session.own_certificate().map_or(Data::None, Data::Bytes),
                "setSniContext" => match sni {
                    // SAFETY: contexts are only removed by `ctxOp close`, which cannot run during this call.
                    Some(context) => done(session.set_sni_context(unsafe { &*context }))?,
                    None => Data::None,
                },
                "setMaxSendFragment" => Data::Bool(session.set_max_send_fragment(number as i64)),
                "requestOCSP" => {
                    session.request_ocsp();
                    Data::None
                }
                "setOCSPResponse" => {
                    if let Some(response) = bytes {
                        session.set_ocsp_response(response);
                    }
                    Data::None
                }
                "renegotiate" => done(session.renegotiate())?,
                "enableSessionCallbacks" => {
                    session.enable_session_callbacks();
                    Data::None
                }
                "enableKeylog" => {
                    session.enable_keylog();
                    Data::None
                }
                "enableCertCb" => {
                    session.enable_cert_cb();
                    Data::None
                }
                "enableAlpnCb" => {
                    session.enable_alpn_callback();
                    Data::None
                }
                "enableHelloCb" => {
                    session.enable_hello_callback();
                    Data::None
                }
                "helloRequest" => session.take_hello().map_or(Data::None, hello_out),
                "helloDone" => {
                    session.hello_done();
                    Data::None
                }
                "setAlpnChoice" => {
                    session.set_alpn_choice(if number < 0.0 { None } else { Some(number as usize) });
                    Data::None
                }
                "certRequest" => match session.take_cert_request() {
                    Some((servername, ocsp)) => Data::List(vec![Data::Str(servername), Data::Bool(ocsp)]),
                    None => Data::None,
                },
                "certDone" => {
                    session.cert_done();
                    Data::None
                }
                "ephemeralKey" => match session.ephemeral_key() {
                    Some((kind, bits)) => Data::List(vec![Data::Float(kind as f64), Data::Float(bits as f64)]),
                    None => Data::None,
                },
                "sharedSigalgs" => Data::List(session.shared_sigalgs().into_iter().map(Data::Str).collect()),
                "shutdown" => {
                    session.shutdown();
                    Data::None
                }
                "shutdownReceived" => Data::Bool(session.handshake_pending_close()),
                "clearErrors" => {
                    session.clear_errors();
                    Data::None
                }
                "handshakeFinished" => Data::Bool(session.handshake_finished()),
                other => return Err(EngineError::plain(format!("unknown session operation {other}")).into()),
            })
        }

        #[op]
        pub fn feed(state: &mut State<TlsRegistry>, id: Lenient<u64>, data: Lenient<Option<Vec<u8>>>) -> bool {
            let data = data.0.unwrap_or_default();
            state.sessions.get_mut(&id.0).is_some_and(|session| session.feed(&data))
        }

        #[op]
        pub fn output(state: &mut State<TlsRegistry>, id: Lenient<u64>) -> Data {
            match state.sessions.get_mut(&id.0) {
                Some(session) if session.pending_output() > 0 => Data::Bytes(session.take_output()),
                _ => Data::None,
            }
        }

        #[op]
        pub fn read(state: &mut State<TlsRegistry>, id: Lenient<u64>, max: Lenient<Option<f64>>) -> Data {
            let max = max.0.unwrap_or(65536.0) as usize;
            match state.sessions.get_mut(&id.0) {
                None => Data::Float(-1.0),
                Some(session) => match session.read(max) {
                    Io::Data(data) => Data::Bytes(data),
                    Io::Code(code) => Data::Float(code as f64),
                },
            }
        }

        #[op]
        pub fn write(state: &mut State<TlsRegistry>, id: Lenient<u64>, data: Lenient<Option<Vec<u8>>>) -> f64 {
            let data = data.0.unwrap_or_default();
            state.sessions.get_mut(&id.0).map_or(-1, |session| session.write(&data)) as f64
        }

        #[op]
        pub fn events(state: &mut State<TlsRegistry>, id: Lenient<u64>) -> Data {
            let events = match state.sessions.get_mut(&id.0) {
                Some(session) => session.take_events(),
                None => Vec::new(),
            };
            if events.is_empty() {
                return Data::None;
            }
            Data::List(
                events
                    .into_iter()
                    .map(|event| match event {
                        Event::HandshakeStart => Data::List(vec![Data::Str("hs-start".into())]),
                        Event::HandshakeDone => Data::List(vec![Data::Str("hs-done".into())]),
                        Event::NewSession { id, session } => {
                            Data::List(vec![Data::Str("session".into()), Data::Bytes(id), Data::Bytes(session)])
                        }
                        Event::Keylog(line) => Data::List(vec![Data::Str("keylog".into()), Data::Bytes(line)]),
                        Event::OcspResponse(response) => Data::List(vec![
                            Data::Str("ocsp".into()),
                            response.map_or(Data::None, Data::Bytes),
                        ]),
                    })
                    .collect(),
            )
        }

        /// The session's last error, returned (not thrown) as an error value.
        #[op(name = "lastError")]
        pub fn last_error(ctx: &mut Ctx, id: Lenient<u64>) -> Value {
            let error = match ctx.op_state().get::<TlsRegistry>().and_then(|state| state.sessions.get(&id.0)) {
                Some(session) => session.last_error(),
                None => EngineError::plain("TLS session is closed"),
            };
            OpError::from(NativeError::from(error)).to_value(ctx)
        }
    }
}
