b = b"hello"
print(b, len(b), b[0], b[-1], b[1:3], b[::-1], type(b[0]).__name__, type(b[1:2]).__name__)
print(list(b), bytes([104, 105]), bytes(range(3)), bytes(3), bytes("é", "utf-8"))
print(b + b" world", b * 2, b"l" in b, b"ll" in b, 108 in b, b"z" in b)
print(b.upper(), b.title(), b.replace(b"l", b"L"), b.split(b"l"), b" a b ".strip(), b"a,b".split(b","))
print(b.find(b"l"), b.count(b"l"), b.startswith(b"he"), b.endswith(b"lo"), b.index(b"e"))
print(b"-".join([b"a", b"b"]), b.hex(), bytes.fromhex("48 69"), b"abc".isalpha(), b"123".isdigit())
print(b == b"hello", b < b"help", b"a" != b"b", b"" == b"", bool(b""), bool(b"\0"))
print(b.decode(), b.decode("ascii"), bytes(b) == b, b.partition(b"l"), b.rjust(7, b"*"))
ba = bytearray(b"abc")
ba[0] = 65
ba.append(100)
ba.extend(b"ef")
ba += b"g"
print(ba, len(ba), ba.pop(), ba, ba.pop(0), ba)
ba[1:3] = b"XYZ"
print(ba, bytes(ba), ba.decode())
ba.insert(0, 33)
ba.remove(88)
print(ba, ba.index(89), ba.count(33))
ba.reverse()
print(ba, ba == bytearray(b"gfedZY!"), bytearray(2), bytearray([1, 2]))
del ba[0]
print(ba, ba[1:], type(ba[1:]).__name__)
ba.clear()
print(ba, len(ba), bool(ba))
print(b"a" < b"b", sorted([b"b", b"a", b"ab"]), max(b"abc"), sum(b"\x01\x02"))
print(bytes(bytearray(b"xy")), bytearray(b"xy") == b"xy", b"xy" == bytearray(b"xy"))
print(b"%s-%d" % (b"a", 5), b"abc".translate(None, b"b"))
print(b"A".lower(), b"a b c".split(), b"line1\nline2".splitlines(), b"x".center(5, b"-"))
print(int.from_bytes(b"\x01\x00", "big"), int.from_bytes(b"\x01\x00", "little"), (258).to_bytes(2, "big"), (255).to_bytes(1, "little"))
for f in (lambda: b"a" + "b", lambda: bytes(-1), lambda: b"abc"[5], lambda: bytes([256]), lambda: b"\xff".decode("ascii")):
    try:
        f()
    except Exception as e:
        print(type(e).__name__)
try:
    b"abc"[0] = 1
except TypeError as e:
    print(type(e).__name__)
print(bytes.fromhex("ff00").hex(), b"\xff\x00".hex(), repr(b"\xff\x00"))
for i in b"ab":
    print(i, end=" ")
print()
print(b"abc".ljust(5, b"."), b"abc".zfill(5), b"a\tb".expandtabs(4), b"abc".rstrip(b"c"))
