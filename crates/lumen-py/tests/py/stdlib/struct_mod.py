import struct


def t(f, *a, **k):
    try:
        print(repr(f(*a, **k)))
    except BaseException as e:
        print(type(e).__name__, e)


print(struct.error.__name__, struct.error.__module__, struct.error.__mro__[1].__name__, issubclass(struct.error, Exception))
print(struct.Struct.__name__, struct.__all__)
print(struct._clearcache())

t(struct.calcsize, 1)
t(struct.Struct, 1)
t(struct.Struct)
t(struct.Struct, "<i", 1)
t(struct.calcsize, "z")
t(struct.calcsize, "4")
t(struct.calcsize, "<n")
t(struct.calcsize, ">>")
t(struct.calcsize, "2 h")
t(struct.calcsize, "<P")
t(struct.calcsize, "99999999999999999999i")
t(struct.calcsize, "9223372036854775808c")
t(struct.calcsize, "4611686018427387904ii")
t(struct.calcsize, "i\0")
t(struct.calcsize, b"\0")
t(struct.calcsize, "\xff")

for fmt, v in [("h", 40000), ("H", -1), ("b", 200), ("B", -1), ("i", 2**31), ("I", -1), ("q", 2**63), ("Q", -1),
               ("<q", 2**63), ("<Q", 2**64), ("<i", 2**100), ("<Q", 2**100), ("<q", -2**100), ("<I", -2**100),
               ("n", 2**63), ("N", -1), ("@l", 2**63), ("<l", 2**31), ("<L", 2**32), (">h", -32769)]:
    print(fmt, end=" ")
    t(struct.pack, fmt, v)
t(struct.pack, "i", 1.0)
t(struct.pack, "i", "a")
t(struct.pack, "i", None)
t(struct.pack, "<h", 1.5)
t(struct.pack, "f", "a")
t(struct.pack, "d", None)
t(struct.pack, "e", "a")
t(struct.pack, "<d", 2**1100)
t(struct.pack, "e", 1e10)
t(struct.pack, "<e", float("inf"))
t(struct.pack, "<e", 65520.0)
t(struct.pack, "c", b"ab")
t(struct.pack, "c", 1)
t(struct.pack, "c", b"")
t(struct.pack, "s", 1)
t(struct.pack, "5s", "ab")
t(struct.pack, "p", "a")
t(struct.pack, "hh", 1)
t(struct.pack, "h", 1, 2)
t(struct.pack, "<3h", 1, 2)
t(struct.pack, "x", 1)
t(struct.pack)


class Div0:
    def __index__(self):
        1 / 0


class BadIdx:
    def __index__(self):
        return "x"


t(struct.pack, "<i", Div0())
t(struct.pack, "<i", BadIdx())
t(struct.pack, "<I", Div0())

b = bytearray(10)
print(struct.pack_into("<hh", b, 1, 1, 2), b)
t(struct.pack_into, "<i", b, 7, 1)
t(struct.pack_into, "<i", b, 6, 3)
print(b)
t(struct.pack_into, "<i", b, -4, 5)
print(b)
t(struct.pack_into, "<i", b, -3, 5)
t(struct.pack_into, "<i", b, -11, 5)
t(struct.pack_into, "<i", b, 2**70, 5)
t(struct.pack_into, "<i", b, "a", 5)
t(struct.pack_into, "<i", b, 1.5, 5)
t(struct.pack_into, "<i", b"abcdefgh", 0, 5)
t(struct.pack_into, "<i", [], 0, 5)
t(struct.pack_into, "<i", b)
t(struct.pack_into, "<i")
t(struct.pack_into, "<i", b, 0)
t(struct.pack_into, "<i", b, 0, 1, 2)
p = bytearray(b"\xff" * 8)
struct.pack_into("<bxi", p, 1, 1, 2)
print(p)
p = bytearray(b"\xff" * 8)
struct.pack_into("xi", p, 0, 1)
print(p)

t(struct.unpack, "i", b"abc")
t(struct.unpack, "i", b"abcde")
t(struct.unpack, "i", "abcd")
t(struct.unpack, "i", [1, 2, 3, 4])
t(struct.unpack, "i")
print(struct.unpack("<i", bytearray(b"\x01\x00\x00\x00")))
data = bytes(range(16))
print(struct.unpack_from("<i", data), struct.unpack_from("<i", data, 4), struct.unpack_from("<i", data, offset=12), struct.unpack_from("<i", buffer=data))
print(struct.unpack_from("<i", data, -4), struct.unpack_from("<i", data, -16), struct.unpack_from("<2s", data, -2))
t(struct.unpack_from, "<i", data, 13)
t(struct.unpack_from, "<i", data, 16)
t(struct.unpack_from, "<i", data, -3)
t(struct.unpack_from, "<i", data, -17)
t(struct.unpack_from, "<i", data, -2**70)
t(struct.unpack_from, "<i", data, 2**70)
t(struct.unpack_from, "<i", data, 1.0)
t(struct.unpack_from, "<i", data, "a")
t(struct.unpack_from, "<i")
t(struct.unpack_from, "<i", data, 0, 0)
t(struct.unpack_from, "<i", b"abc")
print(struct.unpack_from("", data), struct.unpack_from("", b"", 0))
t(struct.unpack_from, "", b"", 1)

t(struct.iter_unpack, "<i", b"abcde")
t(struct.iter_unpack, "<0i", b"abcde")
t(struct.iter_unpack, "", b"")
t(struct.iter_unpack, "<i", 5)
print(list(struct.iter_unpack("<ib", b"\x01\x00\x00\x00\x02" * 3)), list(struct.iter_unpack("<i", b"")))
it = struct.iter_unpack("<h", b"\x01\x00\x02\x00\x03\x00")
print(type(it).__name__, it.__length_hint__(), next(it), it.__length_hint__(), iter(it) is it, list(it), it.__length_hint__())
t(next, it)
t(type(it))
print(list(struct.iter_unpack(">IB", bytes(range(1, 11)))))

s = struct.Struct("<2i")
print(s.size, s.format, s.pack(1, 2), s.unpack(b"\x01\x00\x00\x00\x02\x00\x00\x00"), s.__sizeof__() > 0)
print(struct.Struct(b"<2i").format, type(struct.Struct(b"<2i").format).__name__)
t(s.unpack, b"a" * 8, 1)
t(s.unpack)
t(s.unpack, b"a")
t(s.unpack, buffer=b"a" * 8)
bb = bytearray(8)
print(s.pack_into(bb, 0, 5, -5), bb)
t(s.pack_into, bb, 1, 5, 6)
t(s.pack, 1)
print(s.unpack_from(bb), s.unpack_from(b"\x00" + bytes(bb), 1), s.unpack_from(buffer=bb), s.unpack_from(bb, offset=0))
t(s.unpack_from, bb, 1)
t(s.unpack_from)
print(list(s.iter_unpack(bytes(bb) * 2)))
t(s.iter_unpack, b"a" * 9)
t(setattr, s, "size", 3)
t(setattr, s, "format", "x")
t(struct.Struct("").iter_unpack, b"")
print(struct.Struct("").size, struct.Struct("").pack(), struct.Struct("").unpack(b""))

s0 = struct.Struct("i")
s0.__init__("<2h")
print(s0.format, s0.size, s0.pack(1, 2))
t(s0.__init__, 1)
t(s0.__init__)
print(s0.format)


class Sub(struct.Struct):
    def __init__(self, fmt):
        super().__init__(fmt)
        self.tag = "sub"


class Sub2(struct.Struct):
    pass


class Sub3(struct.Struct):
    def __init__(self):
        super().__init__(">h")


sub = Sub("<h")
print(sub.pack(5), sub.tag, sub.size, isinstance(sub, struct.Struct))
print(Sub2("<H").pack(65535), Sub3().pack(12345), Sub3().unpack(b"\x30\x39"))
fresh = struct.Struct.__new__(struct.Struct)
t(fresh.unpack, b"")
t(fresh.pack)

for fmt in ("<e", "<f", "<d", "<h", "<I", "<q", "<?", "<c", "<5s", "<4p", "<3x"):
    sz = struct.calcsize(fmt)
    print(fmt, sz, struct.Struct(fmt).size)

for i in range(150):
    struct.pack("<%di" % i, *range(i))
print(struct.calcsize("<150i"), struct.pack("<2i", 1, 2) == struct.Struct("<2i").pack(1, 2))
print(struct.unpack("<?", memoryview(b"\x01")), struct.unpack_from("<h", memoryview(b"\x00\x01\x00"), 1))
