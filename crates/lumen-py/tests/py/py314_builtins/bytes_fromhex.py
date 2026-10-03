def t(f):
    try:
        print(repr(f()))
    except BaseException as e:
        print(type(e).__name__, e)


t(lambda: bytes.fromhex(b'ab cd'))
t(lambda: bytearray.fromhex(memoryview(b'abcd')))
t(lambda: bytes.fromhex(bytearray(b'0a\x0b0c')))
t(lambda: bytes.fromhex(b'abc'))
t(lambda: bytes.fromhex('abc'))
t(lambda: bytes.fromhex('a'))
t(lambda: bytes.fromhex(''))
t(lambda: bytes.fromhex(b''))
t(lambda: bytes.fromhex(1))
t(lambda: bytes.fromhex(None))
t(lambda: bytes.fromhex(bytearray(b'0g')))
t(lambda: bytes.fromhex(b'\xff0'))
t(lambda: bytes.fromhex(b'0\xff'))
for a in ['a b zz', 'ab cd', 'ab\x0bcd', 'é0', '0é', 'a \xe9', 'a b c', 'zz', ' 0z', '١٢', 'ab cd']:
    t(lambda: bytes.fromhex(a))
    t(lambda: bytes.fromhex(a.encode('latin1', 'replace')))


class B(bytes):
    pass


t(lambda: type(B.fromhex(b'00')))
t(lambda: type(bytearray.fromhex(b'00')))
