
// ---- internalBinding ----------------------------------------------------------------------------

const bindings = {
  __proto__: null,
  tls_wrap: { TLSWrap, wrap: wrapTls, HAVE_SSL_TRACE: 0 },
  crypto: { SecureContext: NativeSecureContext, getRootCertificates, getSSLCiphers },
  js_stream: { JSStream },
  symbols: { onpskexchange: Symbol("onpskexchange") },
  constants: {
    crypto: {
      SSL_OP_CIPHER_SERVER_PREFERENCE: 4194304,
      TLS1_VERSION: 769,
      TLS1_1_VERSION: 770,
      TLS1_2_VERSION: 771,
      TLS1_3_VERSION: 772,
    },
  },
  cares_wrap: netBinding("cares_wrap"),
  tcp_wrap: netBinding("tcp_wrap"),
  pipe_wrap: netBinding("pipe_wrap"),
  uv: uvBinding,
};
function internalBinding(name) {
  const binding = bindings[name];
  if (binding === undefined) throw new Error(`lumen tls: internalBinding('${name}') is not available`);
  return binding;
}

