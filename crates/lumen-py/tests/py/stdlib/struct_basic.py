import struct


def show(label, value):
    print(label, value)


def hexs(b):
    return b.hex()


for prefix in ("<", ">", "=", "!", "@", ""):
    print("prefix", repr(prefix), struct.calcsize(prefix + "bBhHiIlLqQ"), struct.calcsize(prefix + "?cs"))

print(struct.calcsize("@bi"), struct.calcsize("bi"), struct.calcsize("=bi"), struct.calcsize("@bq"))
print(struct.calcsize("@bP"), struct.calcsize("@be"), struct.calcsize("@b0i"), struct.calcsize("i0q"))
print(struct.calcsize("@n"), struct.calcsize("@N"), struct.calcsize("@P"), struct.calcsize("@l"), struct.calcsize("<l"))
print(struct.calcsize("  2h  3s"), struct.calcsize("<  i"), struct.calcsize(""), struct.calcsize(b"i"))
print(struct.calcsize("3xi"), struct.calcsize("@3xi"), struct.calcsize("<3xi"), struct.calcsize("0q"))

int_cases = [
    ("b", -128), ("b", 127), ("B", 0), ("B", 255), ("h", -32768), ("h", 32767),
    ("H", 65535), ("i", -2**31), ("i", 2**31 - 1), ("I", 2**32 - 1),
    ("l", -2**63), ("L", 2**64 - 1), ("q", -2**63), ("q", 2**63 - 1),
    ("Q", 2**64 - 1), ("Q", 0), ("n", -2**63), ("N", 2**64 - 1), ("P", 2**64 - 1),
]
for prefix in ("@", "<", ">", "=", "!"):
    for ch, v in int_cases:
        fmt = prefix + ch
        try:
            packed = struct.pack(fmt, v)
        except struct.error as e:
            print(fmt, v, "error", e)
            continue
        print(fmt, v, hexs(packed), struct.unpack(fmt, packed)[0] == v)

print(hexs(struct.pack("<h", 1)), hexs(struct.pack(">h", 1)), hexs(struct.pack("<i", -1)), hexs(struct.pack(">q", -2)))
print(struct.unpack("<q", b"\xff" * 8), struct.unpack(">Q", b"\xff" * 8), struct.unpack("<L", b"\xff" * 4))
print(struct.unpack("@l", b"\xff" * 8), struct.unpack("@L", b"\xff" * 8), struct.unpack("@P", b"\xff" * 8))
print(struct.unpack("<bBhH", bytes([0xff, 0xff, 0xff, 0xff, 0xff, 0xff])))

print(struct.pack("<?", True), struct.pack("<?", False), struct.pack("<?", 5), struct.pack("<?", []), struct.pack("<?", "x"))
print(struct.unpack("<?", b"\x02"), struct.unpack("<?", b"\x00"))
print(struct.pack("c", b"a"), struct.unpack("c", b"z"))
print(struct.pack("<h", True), struct.pack("<i", False))


class Idx:
    def __index__(self):
        return 7


class Flt:
    def __float__(self):
        return 2.5


print(struct.pack("<i", Idx()), struct.pack("<d", Flt()), struct.pack("<d", Idx()), struct.pack("<d", 3))

for fmt in ("<e", ">e", "<f", ">f", "<d", ">d", "@f", "@d", "@e"):
    for v in (0.0, -0.0, 1.0, -1.5, 0.1, 65504.0, 5.960464477539063e-08, 1e-8, float("inf"), float("-inf")):
        try:
            p = struct.pack(fmt, v)
        except Exception as e:
            print(fmt, v, type(e).__name__, e)
            continue
        print(fmt, v, hexs(p), struct.unpack(fmt, p)[0])

for v in (65504.0, 65519.9, 65520.0, 65536.0, 1e300, -65520.0, -1e300, 2.0**-24, 2.0**-25, 3 * 2.0**-25, 2.0**-26, 1.0009765625, 1.00048828125, 1.0014648437500002):
    try:
        print("e", v, hexs(struct.pack("<e", v)))
    except OverflowError as e:
        print("e", v, "OverflowError", e)

print(hexs(struct.pack("<e", float("nan"))), hexs(struct.pack("<e", float("-nan"))))
print(struct.unpack("<e", b"\x01\x00"), struct.unpack("<e", b"\xff\x7f"), struct.unpack("<e", b"\x00\x7e"), struct.unpack("<e", b"\x00\xfc"))
nan = struct.unpack("<e", b"\x00\x7e")[0]
print(nan != nan)
print(struct.unpack("<f", struct.pack("<f", 1.1)), struct.unpack("<d", struct.pack("<d", 1.1)))
d = struct.unpack("<d", struct.pack("<d", float("nan")))[0]
print(d != d)
try:
    struct.pack("<f", 1e300)
except OverflowError as e:
    print("OverflowError", e)
print(hexs(struct.pack("@f", 1e300)), hexs(struct.pack("@f", -1e300)))
print(hexs(struct.pack("<f", 3.4028234663852886e38)))
try:
    struct.pack("<f", 3.5e38)
except OverflowError as e:
    print("OverflowError", e)

print(struct.pack("5s", b"ab"), struct.pack("2s", b"abcdef"), struct.pack("0s", b"abc"), struct.pack("s", b"x"), struct.pack("3s", bytearray(b"hello")))
print(struct.pack("3p", b"abcdef"), struct.pack("10p", b"abc"), struct.pack("0p", b"abc"), struct.pack("1p", b"abc"), len(struct.pack("300p", b"a" * 300)), struct.pack("300p", b"a" * 300)[0])
print(struct.unpack("4p", b"\x09abc"), struct.unpack("5p", b"\x02abxx"), struct.unpack("0p", b""), struct.unpack("1p", b"\x05"))
print(struct.unpack("5s", b"ab\x00cd"), struct.unpack("0s", b""), struct.unpack("2s3s", b"abcde"))
print(struct.pack("xx"), struct.pack("3x"), struct.unpack("3x", b"abc"), struct.unpack("", b""), struct.pack(""))
print(struct.pack("<bxi", 1, 2), struct.pack("@bxi", 1, 2), struct.pack("<2h", 1, 2), struct.pack("<3?", 1, 0, 1))
print(struct.pack("@hi", 1, 2), struct.pack("@bq", 1, 2), struct.pack("@bhbi", 1, 2, 3, 4), struct.pack("@ci", b"a", 1))
print(struct.unpack("@bhbi", struct.pack("@bhbi", 1, 2, 3, 4)), struct.unpack("<2h3B", bytes(range(7))))
print(struct.unpack(">2s2h", b"ab\x00\x01\x00\x02"), struct.unpack("!hh", b"\x00\x01\xff\xff"))
