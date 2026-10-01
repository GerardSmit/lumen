print(round(0.5), round(1.5), round(2.5), round(3.5), round(-0.5), round(-1.5), round(-2.5))
print(round(2.675, 2), round(1.005, 2), round(0.125, 2), round(0.375, 2))
print(round(1234, -2), round(1250, -2), round(1350, -2), round(-1250, -2), round(15, -1), round(25, -1))
print(round(3.14159, 3), round(3.14159, 0), round(1e20, 2), round(5, 2))
print(type(round(2.5)).__name__, type(round(2.5, 0)).__name__, type(round(2, 1)).__name__)
print(divmod(7, 2), divmod(-7, 2), divmod(7, -2), divmod(-7, -2))
print(7 // 2, -7 // 2, 7 // -2, -7 // -2, 7 % 3, -7 % 3, 7 % -3, -7 % -3)
print(divmod(7.5, 2), divmod(-7.5, 2), -7.5 // 2, -7.5 % 2)
print(5.0 // 2, 5 % 2.5, 2 ** 10, 2 ** -1, (-2) ** 3, 2 ** 100)
print(abs(-5), abs(-5.5), abs(3), abs(-0.0), abs(True))
print(pow(2, 10), pow(2, 10, 1000), pow(3, -1, 7), pow(2.0, 3), pow(-8, 1 / 3) != 0)
print(sum([1, 2, 3]), sum([1.5, 2.5]), sum([], 5), sum(range(101)))
print(int(3.99), int(-3.99), int("42"), int(" 7 "), int(True), int(2 ** 70))
print(int("ff", 16), int("0xff", 16), int("101", 2), int("0b101", 2), int("z", 36), int("0o17", 8), int("0x1f", 0), int("1_000"))
print(int("-12"), int("+5"), int("0"), int("00"), int(b"12"))
for bad in ["", "1.5", "abc", "1 2", "0x1f"]:
    try:
        int(bad)
    except ValueError as e:
        print("ValueError", e)
try:
    int("12", 1)
except ValueError as e:
    print("ValueError")
try:
    int(None)
except TypeError:
    print("TypeError")
print(float("1.5"), float("-inf"), float(" 2 "), float("1e3"), float(3))
print(bin(10), bin(-5), bin(0), hex(255), hex(-255), oct(8), oct(-8))
print(ord("a"), ord("€"), chr(65), chr(8364), chr(0x1F600) == "\U0001F600")
print(bool(0), bool(""), bool([]), bool([0]), bool(None), bool(0.0), bool(-1))
print(10 ** 20, 10 ** 20 // 3, (10 ** 20) % 7, -(10 ** 20) // 3)
print(1 / 3, 2 / 2, 7 / 2, 1e3 / 3)
print((1).bit_length(), (255).bit_length(), (-8).bit_length(), (10).to_bytes(2, "big"), int.from_bytes(b"\x01\x00", "big"))
print(5 & 3, 5 | 3, 5 ^ 3, ~5, 1 << 10, -16 >> 2, 2 ** 64 >> 60)
print(True + True, 3 * True, 7 // True, isinstance(True, int))
