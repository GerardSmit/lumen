b = b"hello"
print(b, len(b), b[0], b[-1], b[1:3], b[::-1], type(b[0]).__name__, type(b[:1]).__name__)
print(b + b" world", b * 2, b"l" in b, b"xyz" in b, 104 in b)
print(list(b), bytes([104, 105]), bytes(3), bytes("é", "utf-8"), "é".encode(), "é".encode("latin-1"))
print(b.decode(), b"\xc3\xa9".decode("utf-8"), b.upper(), b.find(b"l"), b.count(b"l"), b.replace(b"l", b"L"))
print(b.startswith(b"he"), b.endswith(b"lo"), b.split(b"l"), b",".join([b"a", b"b"]), b" x ".strip())
print(b.hex(), bytes.fromhex("6869"), b"abc" < b"abd", b"a" == b"a", b == "hello")
print(bytes(range(5)), bytes(b"abc"), b"\x00\x01", b"a\nb", b"\\")
print(hash(b"") == hash(b""), bool(b""), bool(b"a"), sorted(b"cab"), max(b"abc"))
ba = bytearray(b"abc")
ba[0] = 65
ba.append(100)
ba.extend(b"ef")
print(ba, len(ba), ba[1], ba.decode(), bytes(ba), type(ba).__name__)
ba += b"!"
print(ba, ba.pop(), ba, ba == bytearray(b"Abcdef"), ba == b"Abcdef")
ba[1:3] = b"XYZ"
print(ba)
del ba[0]
print(ba, ba.index(b"Z"), ba.reverse(), ba)
ba.insert(0, 33)
ba.remove(33)
print(ba, bytearray(2), bytearray("hi", "ascii"), bytearray([1, 2]))
try:
    bytes([256])
except ValueError as e:
    print("ValueError", e)
try:
    b[0] = 1
except TypeError:
    print("TypeError immutable")
try:
    b + "x"
except TypeError:
    print("TypeError concat")
try:
    b"\xff".decode("utf-8")
except UnicodeDecodeError:
    print("UnicodeDecodeError")
print("héllo".encode("utf-8"), len("héllo".encode()), "héllo".encode("ascii", "ignore"), "héllo".encode("ascii", "replace"))
print(int.from_bytes(b"\x00\x10", "big"), (258).to_bytes(2, "little"), (1).to_bytes(1, "big"), bytes.maketrans(b"a", b"b") is not None)
print(b"a b c".split(), b"abc".ljust(5, b"-"), b"%d-%s" % (5, b"x"), memoryview(b"abc")[1], bytes(memoryview(b"abc")[1:]))
