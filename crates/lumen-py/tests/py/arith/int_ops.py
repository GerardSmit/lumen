for a in (7, -7):
    for b in (2, -2, 3, -3):
        print(a, b, a // b, a % b, divmod(a, b), a / b)
print(2 ** 10, 2 ** -2, (-2) ** 3, (-2) ** 2, 0 ** 0, 10 ** 0)
print(-2 ** 2, (-2) ** 2)
print(pow(3, 4, 5), pow(2, 100, 1000), pow(-3, 3, 7), pow(2, -1, 7))
print(abs(-5), abs(5), -(-5), +5, ~5, ~-1, ~0)
print(5 & 3, 5 | 3, 5 ^ 3, 1 << 4, 256 >> 3, -16 >> 2, -1 >> 10)
print(-5 & 0xFF, -5 | 0xFF, -5 ^ 0xFF)
print(int(3.9), int(-3.9), int("  42  "), int("-0"), int("1_000"), int("ff", 16), int("0x1f", 16), int("0b101", 0), int("z", 36))
print(round(2.5), round(3.5), round(-2.5), round(2.675, 2), round(1234, -2), round(1250, -2), round(1350, -2))
print(True + True, True * 5, False - 1, True / 2)
print(type(True + 1).__name__, type(7 // 2).__name__, type(7 / 7).__name__)
print(1_000_000, 0xFF, 0o17, 0b1010, 0XaB)
print(bin(10), oct(64), hex(255), bin(-5), hex(-255), bin(0))
print(int.bit_length(255), (0).bit_length(), (-8).bit_length(), (1024).bit_length())
print((10).to_bytes(2, "big"), (1000).to_bytes(2, "little"), int.from_bytes(b"\x01\x00", "big"))
print(max(1, 5, 3), min([4, 2, 8]), sum([1, 2, 3]), sum([0.5, 0.25]), sum(range(101)))
print(max("a", "b"), min(3, 1.5), max([], default=0), max(1, 2, key=lambda v: -v))
print(10 == 10.0, 10 is 10 or True, hash(10) == hash(10.0), 1 == True)
for bad in ("1 / 0", "1 // 0", "1 % 0", "divmod(1, 0)", "0 ** -1", "int('1.5')", "int('')", "1 << -1"):
    try:
        eval(bad)
    except (ZeroDivisionError, ValueError) as e:
        print(bad, type(e).__name__)
print(17 % 5, -17 % 5, 17 % -5, -17 % -5, 5 % 1)
x = 10
x += 5; x -= 3; x *= 2; x //= 5; x **= 3; x %= 50; x <<= 2; x >>= 1; x |= 1; x &= 7; x ^= 2
print(x)
print(isinstance(True, int), int(True), float(True), complex(1, 2) == 1 + 2j)
