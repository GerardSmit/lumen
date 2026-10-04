//! Native half of `node:tls`: secure contexts and TLS sessions over `lumen_tls::engine`. A session
//! owns no socket; `tls.js` moves encrypted bytes between it and the underlying stream.

use lumen_host::{ops, OpDecl};

pub const TLS_OPS: &[OpDecl] = ops![
    "available" (0) => op_available,
    "rootCertificates" (0) => op_root_certificates,
    "ciphers" (0) => op_ciphers,
    "ctxNew" (3) => op_ctx_new,
    "ctxOp" (4) => op_ctx_op,
    "sessNew" (2) => op_sess_new,
    "sessFree" (1) => op_sess_free,
    "sessOp" (4) => op_sess_op,
    "feed" (2) => op_feed,
    "output" (1) => op_output,
    "read" (2) => op_read,
    "write" (2) => op_write,
    "events" (1) => op_events,
    "lastError" (1) => op_last_error,
];

#[cfg(all(unix, not(target_os = "android")))]
pub use imp::TlsRegistry;

#[cfg(not(all(unix, not(target_os = "android"))))]
#[derive(Default)]
pub struct TlsRegistry;

#[cfg(not(all(unix, not(target_os = "android"))))]
mod unsupported {
    use lumen_host::{Ctx, Value};

    fn unavailable(ctx: &mut Ctx) -> Result<Value, Value> {
        Err(ctx.make_error("Error", "node:tls is not available on this platform"))
    }

    pub fn available(_ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
        Ok(Value::Bool(false))
    }

    pub fn any(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
        unavailable(ctx)
    }
}

#[cfg(not(all(unix, not(target_os = "android"))))]
use unsupported::{
    any as op_ciphers, any as op_ctx_new, any as op_ctx_op, any as op_events, any as op_feed,
    any as op_last_error, any as op_output, any as op_read, any as op_root_certificates,
    any as op_sess_free, any as op_sess_new, any as op_sess_op, any as op_write,
    available as op_available,
};

#[cfg(all(unix, not(target_os = "android")))]
use imp::{
    op_available, op_ciphers, op_ctx_new, op_ctx_op, op_events, op_feed, op_last_error, op_output,
    op_read, op_root_certificates, op_sess_free, op_sess_new, op_sess_op, op_write,
};

#[cfg(all(unix, not(target_os = "android")))]
mod imp {
    use std::collections::HashMap;

    use lumen_host::{Ctx, Value};
    use lumen_tls::engine::{ClientHello, Context, EngineError, Event, Io, Session};

    #[derive(Default)]
    pub struct TlsRegistry {
        next: u64,
        contexts: HashMap<u64, Context>,
        sessions: HashMap<u64, Session>,
    }

    enum Out {
        Undefined,
        Null,
        Bool(bool),
        Num(f64),
        Str(String),
        Bytes(Vec<u8>),
        Array(Vec<Out>),
        Error(EngineError),
    }

    impl From<Result<(), EngineError>> for Out {
        fn from(result: Result<(), EngineError>) -> Out {
            match result {
                Ok(()) => Out::Undefined,
                Err(error) => Out::Error(error),
            }
        }
    }

    fn error_value(ctx: &mut Ctx, error: &EngineError) -> Value {
        let value = ctx.make_error(error.kind.unwrap_or("Error"), error.message.clone());
        let mut set = |key: &str, text: &Option<String>| {
            if let Some(text) = text {
                let _ = ctx.set_member(&value, key, Value::from_string(text.clone()));
            }
        };
        set("library", &error.library);
        set("function", &error.function);
        set("reason", &error.reason);
        set("code", &error.code);
        value
    }

    fn to_value(ctx: &mut Ctx, out: Out) -> Result<Value, Value> {
        Ok(match out {
            Out::Undefined => Value::Undefined,
            Out::Null => Value::Null,
            Out::Bool(flag) => Value::Bool(flag),
            Out::Num(number) => Value::Num(number),
            Out::Str(text) => Value::from_string(text),
            Out::Bytes(bytes) => ctx.make_uint8array(&bytes)?,
            Out::Array(items) => {
                let mut values = Vec::with_capacity(items.len());
                for item in items {
                    values.push(to_value(ctx, item)?);
                }
                ctx.make_array(values)
            }
            Out::Error(error) => return Err(error_value(ctx, &error)),
        })
    }

    fn registry(ctx: &mut Ctx) -> &mut TlsRegistry {
        ctx.host_mut::<TlsRegistry>()
            .expect("runtime installs the TLS registry")
    }

    fn id_arg(args: &[Value]) -> u64 {
        args.first().and_then(Value::as_num_opt).unwrap_or(0.0) as u64
    }

    fn num_arg(args: &[Value], index: usize) -> f64 {
        args.get(index).and_then(Value::as_num_opt).unwrap_or(0.0)
    }

    fn bytes_arg(ctx: &mut Ctx, args: &[Value], index: usize) -> Option<Vec<u8>> {
        args.get(index)
            .and_then(|value| ctx.typed_array_bytes(value))
    }

    fn string_arg(ctx: &mut Ctx, args: &[Value], index: usize) -> Result<Option<String>, Value> {
        match args.get(index) {
            None | Some(Value::Undefined) | Some(Value::Null) => Ok(None),
            Some(value) => Ok(Some(ctx.coerce_string(value)?.to_string())),
        }
    }

    pub fn op_available(_ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
        Ok(Value::Bool(lumen_tls::engine::openssl_available()))
    }

    pub fn op_root_certificates(
        ctx: &mut Ctx,
        _this: Value,
        _args: &[Value],
    ) -> Result<Value, Value> {
        let out = Out::Array(
            lumen_tls::engine::root_certificate_pems()
                .into_iter()
                .map(Out::Str)
                .collect(),
        );
        to_value(ctx, out)
    }

    pub fn op_ciphers(ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
        let out = match lumen_tls::engine::cipher_names() {
            Ok(names) => Out::Array(names.into_iter().map(Out::Str).collect()),
            Err(error) => Out::Error(error),
        };
        to_value(ctx, out)
    }

    pub fn op_ctx_new(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
        let method = string_arg(ctx, args, 0)?;
        let min = num_arg(args, 1) as i32;
        let max = num_arg(args, 2) as i32;
        let out = match Context::new(method.as_deref(), min, max) {
            Ok(context) => {
                let registry = registry(ctx);
                registry.next += 1;
                let id = registry.next;
                registry.contexts.insert(id, context);
                Out::Num(id as f64)
            }
            Err(error) => Out::Error(error),
        };
        to_value(ctx, out)
    }

    pub fn op_ctx_op(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
        let id = id_arg(args);
        let name = string_arg(ctx, args, 1)?.unwrap_or_default();
        let bytes = bytes_arg(ctx, args, 2);
        let text = match name.as_str() {
            "setCiphers" | "setCipherSuites" | "setSigalgs" | "setECDHCurve" => {
                string_arg(ctx, args, 2)?
            }
            _ => None,
        };
        let number = num_arg(args, 2);
        let passphrase = bytes_arg(ctx, args, 3);
        let out = match registry(ctx).contexts.get_mut(&id) {
            None => Out::Error(EngineError::plain("SecureContext is closed")),
            Some(context) => match name.as_str() {
                "setKey" => bytes.map_or(Out::Undefined, |pem| {
                    context.set_key(&pem, passphrase.as_deref()).into()
                }),
                "setCert" => bytes.map_or(Out::Undefined, |pem| context.set_cert(&pem).into()),
                "addCACert" => bytes.map_or(Out::Undefined, |pem| context.add_ca_cert(&pem).into()),
                "addCRL" => bytes.map_or(Out::Undefined, |pem| context.add_crl(&pem).into()),
                "addRootCerts" => context.add_root_certs().into(),
                "setCiphers" => context.set_ciphers(&text.unwrap_or_default()).into(),
                "setCipherSuites" => context.set_cipher_suites(&text.unwrap_or_default()).into(),
                "setSigalgs" => context.set_sigalgs(&text.unwrap_or_default()).into(),
                "setECDHCurve" => context.set_ecdh_curve(&text.unwrap_or_default()).into(),
                "setDHParam" => match context.set_dh_param(bytes.as_deref()) {
                    Ok(Some(warning)) => Out::Str(warning.to_string()),
                    Ok(None) => Out::Undefined,
                    Err(error) => Out::Error(error),
                },
                "setMinProto" => context.set_min_proto(number as i32).into(),
                "setMaxProto" => context.set_max_proto(number as i32).into(),
                "getMinProto" => Out::Num(context.min_proto() as f64),
                "getMaxProto" => Out::Num(context.max_proto() as f64),
                "setOptions" => context.set_options(number as u64).into(),
                "setSessionIdContext" => bytes.map_or(Out::Undefined, |context_id| {
                    context.set_session_id_context(&context_id).into()
                }),
                "setSessionTimeout" => context.set_session_timeout(number as i32).into(),
                "setTicketKeys" => {
                    bytes.map_or(Out::Undefined, |keys| context.set_ticket_keys(&keys).into())
                }
                "getTicketKeys" => Out::Bytes(context.ticket_keys()),
                "loadPKCS12" => bytes.map_or(Out::Undefined, |data| {
                    context.load_pkcs12(&data, passphrase.as_deref()).into()
                }),
                "getCertificate" => Out::Bytes(context.certificate().to_vec()),
                "getIssuer" => Out::Bytes(context.issuer().to_vec()),
                "close" => {
                    context.close();
                    Out::Undefined
                }
                other => Out::Error(EngineError::plain(format!(
                    "unknown context operation {other}"
                ))),
            },
        };
        if name == "close" {
            registry(ctx).contexts.remove(&id);
        }
        to_value(ctx, out)
    }

    pub fn op_sess_new(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
        let context_id = id_arg(args);
        let is_server = matches!(args.get(1), Some(Value::Bool(true)));
        let registry = registry(ctx);
        let out = match registry.contexts.get(&context_id) {
            None => Out::Error(EngineError::plain("SecureContext is closed")),
            Some(context) => match Session::new(context, is_server) {
                Ok(session) => {
                    registry.next += 1;
                    let id = registry.next;
                    registry.sessions.insert(id, session);
                    Out::Num(id as f64)
                }
                Err(error) => Out::Error(error),
            },
        };
        to_value(ctx, out)
    }

    pub fn op_sess_free(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
        let id = id_arg(args);
        registry(ctx).sessions.remove(&id);
        Ok(Value::Undefined)
    }

    fn hello_out(hello: ClientHello) -> Out {
        Out::Array(vec![
            Out::Bytes(hello.session_id),
            Out::Str(hello.servername),
            Out::Bool(hello.has_ticket),
            Out::Bool(hello.ocsp_request),
            Out::Bytes(hello.alpn),
        ])
    }

    fn certificates_out(chain: Vec<Vec<u8>>) -> Out {
        Out::Array(chain.into_iter().map(Out::Bytes).collect())
    }

    pub fn op_sess_op(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
        let id = id_arg(args);
        let name = string_arg(ctx, args, 1)?.unwrap_or_default();
        let bytes = bytes_arg(ctx, args, 2);
        let text = match name.as_str() {
            "setServername" => string_arg(ctx, args, 2)?,
            _ => None,
        };
        let label = if name == "exportKeyingMaterial" {
            string_arg(ctx, args, 3)?
        } else {
            None
        };
        let context_bytes = if name == "exportKeyingMaterial" {
            bytes_arg(ctx, args, 4)
        } else {
            None
        };
        let flag_a = matches!(args.get(2), Some(Value::Bool(true)));
        let flag_b = matches!(args.get(3), Some(Value::Bool(true)));
        let number = num_arg(args, 2);
        let sni_context = if name == "setSniContext" {
            Some(num_arg(args, 2) as u64)
        } else {
            None
        };
        let registry = registry(ctx);
        let sni = sni_context.and_then(|id| {
            registry
                .contexts
                .get(&id)
                .map(|context| context as *const Context)
        });
        let Some(session) = registry.sessions.get_mut(&id) else {
            return to_value(ctx, Out::Error(EngineError::plain("TLS session is closed")));
        };
        let out = match name.as_str() {
            "setVerifyMode" => {
                session.set_verify_mode(flag_a, flag_b);
                Out::Undefined
            }
            "verifyError" => match session.verify_error() {
                Some((code, reason)) => Out::Array(vec![Out::Num(code as f64), Out::Str(reason)]),
                None => Out::Undefined,
            },
            "protocol" => session.protocol().map_or(Out::Null, Out::Str),
            "cipher" => match session.cipher() {
                Some((name, standard, version)) => {
                    Out::Array(vec![Out::Str(name), Out::Str(standard), Out::Str(version)])
                }
                None => Out::Null,
            },
            "alpnSelected" => session.alpn_selected().map_or(Out::Bool(false), Out::Bytes),
            "setAlpn" => {
                Out::Bool(bytes.is_some_and(|protocols| session.set_alpn_protocols(&protocols)))
            }
            "servername" => session.servername().map_or(Out::Bool(false), Out::Str),
            "setServername" => Out::Bool(text.is_some_and(|name| session.set_servername(&name))),
            "getSession" => session.session_bytes().map_or(Out::Undefined, Out::Bytes),
            "setSession" => Out::Bool(bytes.is_some_and(|data| session.set_session(&data))),
            "loadSession" => {
                session.load_session(bytes.as_deref());
                Out::Undefined
            }
            "isSessionReused" => Out::Bool(session.session_reused()),
            "finished" => session.finished(flag_a).map_or(Out::Undefined, Out::Bytes),
            "exportKeyingMaterial" => match session.export_keying_material(
                number as usize,
                &label.unwrap_or_default(),
                context_bytes.as_deref(),
            ) {
                Ok(out) => Out::Bytes(out),
                Err(error) => Out::Error(error),
            },
            "peerCertificates" => certificates_out(session.peer_certificates()),
            "ownCertificate" => session.own_certificate().map_or(Out::Undefined, Out::Bytes),
            "setSniContext" => match sni {
                // SAFETY: contexts are only removed by `ctxOp close`, which cannot run during this call.
                Some(context) => session.set_sni_context(unsafe { &*context }).into(),
                None => Out::Undefined,
            },
            "setMaxSendFragment" => Out::Bool(session.set_max_send_fragment(number as i64)),
            "requestOCSP" => {
                session.request_ocsp();
                Out::Undefined
            }
            "setOCSPResponse" => {
                if let Some(response) = bytes {
                    session.set_ocsp_response(response);
                }
                Out::Undefined
            }
            "renegotiate" => session.renegotiate().into(),
            "enableSessionCallbacks" => {
                session.enable_session_callbacks();
                Out::Undefined
            }
            "enableKeylog" => {
                session.enable_keylog();
                Out::Undefined
            }
            "enableCertCb" => {
                session.enable_cert_cb();
                Out::Undefined
            }
            "enableAlpnCb" => {
                session.enable_alpn_callback();
                Out::Undefined
            }
            "enableHelloCb" => {
                session.enable_hello_callback();
                Out::Undefined
            }
            "helloRequest" => session.take_hello().map_or(Out::Undefined, hello_out),
            "helloDone" => {
                session.hello_done();
                Out::Undefined
            }
            "setAlpnChoice" => {
                session.set_alpn_choice(if number < 0.0 {
                    None
                } else {
                    Some(number as usize)
                });
                Out::Undefined
            }
            "certRequest" => match session.take_cert_request() {
                Some((servername, ocsp)) => Out::Array(vec![Out::Str(servername), Out::Bool(ocsp)]),
                None => Out::Undefined,
            },
            "certDone" => {
                session.cert_done();
                Out::Undefined
            }
            "ephemeralKey" => match session.ephemeral_key() {
                Some((kind, bits)) => {
                    Out::Array(vec![Out::Num(kind as f64), Out::Num(bits as f64)])
                }
                None => Out::Undefined,
            },
            "sharedSigalgs" => {
                Out::Array(session.shared_sigalgs().into_iter().map(Out::Str).collect())
            }
            "shutdown" => {
                session.shutdown();
                Out::Undefined
            }
            "shutdownReceived" => Out::Bool(session.handshake_pending_close()),
            "clearErrors" => {
                session.clear_errors();
                Out::Undefined
            }
            "handshakeFinished" => Out::Bool(session.handshake_finished()),
            other => Out::Error(EngineError::plain(format!(
                "unknown session operation {other}"
            ))),
        };
        to_value(ctx, out)
    }

    pub fn op_feed(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
        let id = id_arg(args);
        let bytes = bytes_arg(ctx, args, 1).unwrap_or_default();
        let accepted = registry(ctx)
            .sessions
            .get_mut(&id)
            .is_some_and(|session| session.feed(&bytes));
        Ok(Value::Bool(accepted))
    }

    pub fn op_output(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
        let id = id_arg(args);
        let out = match registry(ctx).sessions.get_mut(&id) {
            Some(session) if session.pending_output() > 0 => Out::Bytes(session.take_output()),
            _ => Out::Undefined,
        };
        to_value(ctx, out)
    }

    pub fn op_read(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
        let id = id_arg(args);
        let max = args.get(1).and_then(Value::as_num_opt).unwrap_or(65536.0) as usize;
        let out = match registry(ctx).sessions.get_mut(&id) {
            None => Out::Num(-1.0),
            Some(session) => match session.read(max) {
                Io::Data(data) => Out::Bytes(data),
                Io::Code(code) => Out::Num(code as f64),
            },
        };
        to_value(ctx, out)
    }

    pub fn op_write(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
        let id = id_arg(args);
        let bytes = bytes_arg(ctx, args, 1).unwrap_or_default();
        let result = registry(ctx)
            .sessions
            .get_mut(&id)
            .map_or(-1, |session| session.write(&bytes));
        Ok(Value::Num(result as f64))
    }

    pub fn op_events(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
        let id = id_arg(args);
        let events = match registry(ctx).sessions.get_mut(&id) {
            Some(session) => session.take_events(),
            None => Vec::new(),
        };
        if events.is_empty() {
            return Ok(Value::Undefined);
        }
        let out = Out::Array(
            events
                .into_iter()
                .map(|event| match event {
                    Event::HandshakeStart => Out::Array(vec![Out::Str("hs-start".into())]),
                    Event::HandshakeDone => Out::Array(vec![Out::Str("hs-done".into())]),
                    Event::NewSession { id, session } => Out::Array(vec![
                        Out::Str("session".into()),
                        Out::Bytes(id),
                        Out::Bytes(session),
                    ]),
                    Event::Keylog(line) => {
                        Out::Array(vec![Out::Str("keylog".into()), Out::Bytes(line)])
                    }
                    Event::OcspResponse(response) => Out::Array(vec![
                        Out::Str("ocsp".into()),
                        response.map_or(Out::Undefined, Out::Bytes),
                    ]),
                })
                .collect(),
        );
        to_value(ctx, out)
    }

    pub fn op_last_error(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
        let id = id_arg(args);
        let error = match registry(ctx).sessions.get(&id) {
            Some(session) => session.last_error(),
            None => EngineError::plain("TLS session is closed"),
        };
        Ok(error_value(ctx, &error))
    }
}
