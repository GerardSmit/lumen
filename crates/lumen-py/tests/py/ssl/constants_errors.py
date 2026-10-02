import _ssl

print(_ssl.CERT_NONE, _ssl.CERT_OPTIONAL, _ssl.CERT_REQUIRED)
print(_ssl.PROTOCOL_TLS, _ssl.PROTOCOL_TLS_CLIENT, _ssl.PROTOCOL_TLS_SERVER, _ssl.PROTOCOL_TLSv1_2)
print(_ssl.PROTO_MINIMUM_SUPPORTED, _ssl.PROTO_MAXIMUM_SUPPORTED, _ssl.PROTO_TLSv1_3)
print(_ssl.SSL_ERROR_SSL, _ssl.SSL_ERROR_WANT_READ, _ssl.SSL_ERROR_WANT_WRITE, _ssl.SSL_ERROR_EOF)
print(_ssl.ALERT_DESCRIPTION_HANDSHAKE_FAILURE, _ssl.ALERT_DESCRIPTION_UNRECOGNIZED_NAME)
print(_ssl.HAS_SNI, _ssl.HAS_ALPN, _ssl.HAS_TLS_UNIQUE, _ssl.HAS_TLSv1_3, _ssl.HAS_SSLv2)
print(isinstance(_ssl.OPENSSL_VERSION, str), isinstance(_ssl.OPENSSL_VERSION_NUMBER, int))
print(len(_ssl.OPENSSL_VERSION_INFO))

print([c.__name__ for c in _ssl.SSLError.__mro__])
print([c.__name__ for c in _ssl.SSLCertVerificationError.__mro__])
print(issubclass(_ssl.SSLWantReadError, _ssl.SSLError))
print(issubclass(_ssl.SSLZeroReturnError, OSError))
print(issubclass(_ssl.SSLCertVerificationError, ValueError))

e = _ssl.SSLError("plain")
print(str(e), hasattr(e, "library"))
e = _ssl.SSLError(1, "message")
print(str(e), e.errno, e.strerror)

print(len(_ssl.RAND_bytes(16)), _ssl.RAND_bytes(0))
print(_ssl.RAND_status())
try:
    _ssl.RAND_bytes(-1)
except ValueError as e:
    print(e)
