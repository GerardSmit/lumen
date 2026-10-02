import _ssl

print(_ssl.txt2obj("1.3.6.1.5.5.7.3.1"))
print(_ssl.txt2obj("serverAuth", name=True))
print(_ssl.nid2obj(129))
try:
    _ssl.txt2obj("serverAuth")
except ValueError as e:
    print(e)
try:
    _ssl.nid2obj(-1)
except ValueError as e:
    print(e)

ctx = _ssl._SSLContext(_ssl.PROTOCOL_TLS_CLIENT)
print(ctx.protocol, ctx.verify_mode, ctx.check_hostname)
print(ctx.minimum_version, ctx.maximum_version)
print(ctx.post_handshake_auth)
ctx.check_hostname = False
ctx.verify_mode = _ssl.CERT_NONE
print(ctx.verify_mode, ctx.check_hostname)
try:
    ctx.check_hostname = True
    ctx.verify_mode = _ssl.CERT_NONE
except ValueError as e:
    print(e)
print(ctx.verify_mode)

srv = _ssl._SSLContext(_ssl.PROTOCOL_TLS_SERVER)
print(srv.protocol, srv.verify_mode, srv.check_hostname)
print(srv.cert_store_stats()["x509"])
print(srv.get_ca_certs())
print(len(srv.get_ciphers()) > 0)
try:
    srv.set_ciphers("")
except _ssl.SSLError as e:
    print(e)
try:
    srv.set_ciphers("NOT-A-CIPHER")
except _ssl.SSLError as e:
    print(e)
try:
    ctx.load_verify_locations()
except TypeError as e:
    print(e)
try:
    _ssl._SSLContext(99)
except ValueError as e:
    print(e)
try:
    ctx.set_ecdh_curve("nonexistent-curve")
except ValueError as e:
    print(e)
try:
    ctx.sni_callback = print
except ValueError as e:
    print(e)
legacy = _ssl._SSLContext(_ssl.PROTOCOL_TLSv1_2)
try:
    legacy.minimum_version = _ssl.PROTO_TLSv1_2
except ValueError as e:
    print(e)

c = _ssl._SSLContext(_ssl.PROTOCOL_TLS_SERVER)
try:
    c.num_tickets = -1
except ValueError as e:
    print(e)
c.num_tickets = 3
print(c.num_tickets)
