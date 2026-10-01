A = 10 ** 60 + 12345678901234567890123
B = 987654321987654321987654321
C = 7 ** 80

print("--- division and modulo signs")
for x in (A, -A):
    for y in (B, -B, 97, -97, 10 ** 70, -(10 ** 70)):
        q, r = divmod(x, y)
        assert q * y + r == x
        assert r == 0 or (r < 0) == (y < 0)
        print(len(str(x)) * (1 if x > 0 else -1), y if abs(y) < 1000 else len(str(y)), q, r)
print(A // B, A % B, -A // B, -A % B)
print(C // A, C % A, C // -A, C % -A)
print(divmod(-(2 ** 100), 2 ** 50), divmod(2 ** 100, -(2 ** 50) - 1))
print((2 ** 100) % (2 ** 64), (-(2 ** 100)) % (2 ** 64), (2 ** 100) // (2 ** 64))
try:
    A // 0
except ZeroDivisionError:
    print("ZeroDivisionError //")
try:
    A % 0
except ZeroDivisionError:
    print("ZeroDivisionError %")
try:
    divmod(A, 0)
except ZeroDivisionError:
    print("ZeroDivisionError divmod")

print("--- true division")
print(10 ** 30 / 10 ** 10, 2 ** 100 / 2 ** 98, (10 ** 400) / (10 ** 399), 1 / 3 ** 40)
try:
    10 ** 400 / 1
except OverflowError:
    print("OverflowError true div")

print("--- str/int of 400 digits")
n400 = int("".join(str((i * 7 + 3) % 10) for i in range(400)))
s = str(n400)
print(len(s), s[:30], s[-30:])
print(int(s) == n400, int(s[::-1]) != n400, n400 % 1000003, n400 // 10 ** 380)
neg = -n400
print(str(neg)[:5], len(str(neg)), int(str(neg)) == neg)
print(int("0" * 100 + "123"), int("-0"), int("1" + "0" * 399) == 10 ** 399)
print(n400 * n400 == int(str(n400 * n400)), len(str(n400 ** 2)), len(str(n400 ** 3)))
print(repr(n400)[:10], "%d" % (10 ** 25), "{:d}".format(-(10 ** 25)), f"{10 ** 22:,}")
print(f"{2 ** 70:>30}|{2 ** 70:<30}|{-(2 ** 70):^30}|{2 ** 70:030d}")

print("--- hex/bin/oct")
v = 2 ** 130 + 0xDEADBEEF
print(hex(v), oct(v), bin(v))
print(hex(-v), bin(-(2 ** 70)), oct(-(2 ** 66)))
print(int(hex(v), 16) == v, int(bin(v), 2) == v, int(oct(v), 8) == v)
print(int("ffffffffffffffffffffffffffffffff", 16), int("-zz" * 1 + "", 36) if False else int("zzzzzzzzzzzzzzzzz", 36))
print(f"{v:x}", f"{v:X}", f"{v:#x}", f"{v:o}", f"{2 ** 65:b}")
print(format(255 ** 20, "x"), format(-(255 ** 20), "x"))

print("--- bytes")
b = (2 ** 200 + 12345).to_bytes(26, "big")
print(b.hex(), len(b))
print(int.from_bytes(b, "big") == 2 ** 200 + 12345, int.from_bytes(b, "little") != 2 ** 200 + 12345)
print(int.from_bytes(b"\xff" * 20, "big"), int.from_bytes(b"\xff" * 20, "big", signed=True))
print((-(2 ** 100)).to_bytes(14, "big", signed=True).hex())
print(int.from_bytes((-(2 ** 100)).to_bytes(14, "big", signed=True), "big", signed=True) == -(2 ** 100))
print((2 ** 64).to_bytes(9, "little").hex(), (0).to_bytes(3, "big"))
try:
    (2 ** 100).to_bytes(4, "big")
except OverflowError:
    print("OverflowError to_bytes")
try:
    (-1).to_bytes(4, "big")
except OverflowError:
    print("OverflowError negative unsigned")

print("--- pow with modulus")
m = 10 ** 40 + 121
print(pow(A, B, m), pow(-A, B, m), pow(A, 0, m), pow(2, 10 ** 30, 10 ** 9 + 7))
print(pow(3, -1, 10 ** 30 + 57) * 3 % (10 ** 30 + 57))
print(pow(A, 3, 1), pow(5, 3, -7), pow(-5, 3, 7))
p = 2 ** 127 - 1
print(pow(65537, p - 1, p), pow(12345, (p - 1) // 2, p) in (1, p - 1))
print(pow(2, 100), pow(2, 100) == 2 ** 100, pow(-2, 101), pow(10, 50) // pow(10, 48))
try:
    pow(2, 10, 0)
except ValueError:
    print("ValueError pow mod 0")

print("--- comparisons with floats")
big = 2 ** 80
print(big == float(big), big < float(big) * 2, big + 1 > float(big), big + 1 == float(big))
print(10 ** 400 > 1e308, -(10 ** 400) < -1e308, 10 ** 400 < float("inf"), 2 ** 53 + 1 == float(2 ** 53 + 1))
print(float(2 ** 53 + 1), float(2 ** 53 + 3), 2 ** 53 + 1 > float(2 ** 53))
print(10 ** 22 == 1e22, 10 ** 23 == 1e23, 10 ** 15 == 1e15, 3 ** 40 < 1.2157665459056929e19, 3 ** 40 > 1.2157665459056928e19)
print(sorted([10 ** 30, 1e29, 2 ** 100, 1.5e30, -1e40, -(10 ** 41)]))
print(max(2 ** 70, 1e21), min(2 ** 70, 1e21))

print("--- float(big) rounding")
print(float(10 ** 22), float(10 ** 23), float(2 ** 100), float(-(3 ** 70)))
print(float((1 << 64) + (1 << 11)), float((1 << 64) + (1 << 11) + 1), float((1 << 64) + (1 << 12)))
print(float((1 << 100) - 1) == float(1 << 100), float(12345678901234567890123))
print(float(10 ** 308), float(int("9" * 308)))
try:
    float(10 ** 309)
except OverflowError:
    print("OverflowError float()")
print((10 ** 20) * 1.5, (2 ** 70) + 0.5, 10 ** 18 * 0.1, 2 ** 100 * 1.0, -(2 ** 80) / 1.0)

print("--- int(float)")
print(int(1e22), int(1e23), int(-1e22), int(1.5e300) == int("15" + "0" * 299) or "inexact")
print(int(2.0 ** 100), int(2.0 ** 70 + 2.0 ** 20), int(123456789012345678.0), int(-0.5), int(0.9999))
print(int(1e22) == 10 ** 22, int(1e23) == 10 ** 23, int(float(2 ** 80)) == 2 ** 80)
for bad in (float("inf"), float("nan")):
    try:
        int(bad)
    except (OverflowError, ValueError) as e:
        print(type(e).__name__)
print(round(1e22), round(2.5e21), round(10 ** 30, -29), round(15 * 10 ** 20, -21), round(-(10 ** 25) - 5, -1))
print(int(float(10 ** 16 + 1)), 7 ** 30 // 1.0 == float(7 ** 30), (10 ** 25) // 3.0)
print(divmod(10 ** 25, 7.0), 10 ** 25 % 7.0, (2 ** 70) ** 0.5)

print("--- bit_length, shifts, bit ops")
for x in (0, 1, 255, 256, -256, 2 ** 64 - 1, 2 ** 64, -(2 ** 64), 2 ** 1000, 10 ** 100):
    print(x if abs(x) < 10 ** 20 else "big", x.bit_length(), (-x).bit_length())
print(1 << 200, (1 << 200) >> 199, (1 << 200) >> 201, -(1 << 200) >> 199, -(1 << 200) >> 400, -5 >> 100)
print((3 << 100) >> 98, (-3 << 100) >> 98, (10 ** 30) >> 10, (10 ** 30) << 10, -(10 ** 30) >> 10)
print((2 ** 100 - 1) & (2 ** 70), -(2 ** 100) & (2 ** 101 - 1), (2 ** 100) | -1, -(2 ** 100) ^ (2 ** 100))
print(~0, ~(2 ** 100), ~(-(2 ** 100)), (2 ** 100 + 5) & 0xFF, (-(2 ** 100) - 5) & 0xFF)
print((1 << 64).bit_count(), (2 ** 100 - 1).bit_count(), (-(2 ** 100 - 1)).bit_count())
try:
    1 << -1
except ValueError:
    print("ValueError negative shift")
print(0 << 10 ** 6, 5 >> 10 ** 6, hash(2 ** 61 - 1) == hash(0) if False else "skip")
