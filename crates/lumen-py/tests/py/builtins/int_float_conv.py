print(int("101", 2), int("ff", 16), int("FF", 16), int("777", 8), int("zz", 36), int("Zz", 36))
print(int("0b101", 2), int("0o17", 8), int("0xff", 16), int("0xFF", 0), int("0b11", 0), int("0o7", 0))
print(int("-0x10", 16), int("+7"), int("-7"), int("  12 "), int("\t\n12\n"), int("0"), int("-0"), int("007"))
print(int("1_000"), int("1_0_0"), int("0x_f", 16), int("0b_1", 0), int("1_000", 10))
print(int("10", 3), int("12", 5), int("z", 36), int("11", 2), int("10", 36))
print(int(3.9), int(-3.9), int(0.0), int(-0.5), int(1e15), int(1e20), int(True), int(False))
print(int(), int(5), int(2**70), int("123456789012345678901234567890"))
print(int(b"42"), int(bytearray(b"7")))
for bad in ["", " ", "1.5", "abc", "1__0", "_1", "1_", "0x10", "1e3", "--1", "0b2", "٣x"]:
    try:
        int(bad)
        print("ok", repr(bad))
    except ValueError as e:
        print(type(e).__name__, e)
try:
    int("12", 1)
except ValueError as e:
    print(type(e).__name__, e)
try:
    int("12", 37)
except ValueError as e:
    print(type(e).__name__)
try:
    int(12, 10)
except TypeError as e:
    print(type(e).__name__)
try:
    int("9", 8)
except ValueError as e:
    print(type(e).__name__, e)
try:
    int(None)
except TypeError as e:
    print(type(e).__name__)
try:
    int(float("inf"))
except OverflowError as e:
    print(type(e).__name__)
try:
    int(float("nan"))
except ValueError as e:
    print(type(e).__name__)

print(float("1.5"), float("  2.5  "), float("1e3"), float("-1E-2"), float(".5"), float("5."), float("1_0.5"))
print(float("inf"), float("-inf"), float("+Infinity"), float("nan"), float("-nan"), float("INF"), float("NaN"))
print(float(3), float(True), float(), float("0"), float("-0"), float(2**70), float("1e400"))
for bad in ["", "abc", "1.2.3", "1e", "0x10", "1__0", "--1"]:
    try:
        float(bad)
        print("ok", repr(bad))
    except ValueError as e:
        print(type(e).__name__, e)
try:
    float(None)
except TypeError as e:
    print(type(e).__name__)
try:
    float(10**400)
except OverflowError as e:
    print(type(e).__name__)

print(1e16, 1e15, 1e-4, 1e-5, 123456789.123456789, 1.0, 100.0, 1.5e300, 1e22, 1e21)
print(0.1 + 0.2, 0.1 * 3, 1 / 3, 2 / 3, 5e-324, 1.7976931348623157e308, 2.2250738585072014e-308)
print(float(2**53), float(2**53 + 1), 9007199254740993.0, 1e100, 12345678901234567890.0)
print(-0.0, 0.0 == -0.0, 3.0, 3.14, -1e-10, 1234567.0, 0.000123)
print(repr(0.1), str(0.1), repr(1e16), repr(1e-7), repr(float("inf")), repr(-float("inf")))
print(float("nan") == float("nan"), float("nan") != float("nan"), float("inf") > 10**308)
print((1.0).is_integer(), (1.5).is_integer(), (-2.0).is_integer(), float("inf").is_integer())
print((0.5).as_integer_ratio(), (3.0).as_integer_ratio(), (0.1).hex(), float.fromhex("0x1.8p1"))
print(int.__format__(255, "x"), int.__format__(255, "#X"), int.__format__(5, "08b"), format(255, "o"))
print(format(-255, "#x"), format(1234567, ","), format(255, "#010b"), format(3, "+d"))
print(hex(255), hex(-255), oct(8), bin(5), bin(-5), hex(0))
print(bool(0), bool(1), bool(-1), bool(0.0), bool(-0.0), bool(float("nan")), bool(""), bool(" "))
print(bool([]), bool([0]), bool(()), bool({}), bool(None), bool(0j if False else 0))
print(bool("False"), bool(b""), bool(range(0)), bool(object()))
print(int(" 12 ") + int("-3"), float(" 1 ") + 1, int("0_0"), int("00"))
print(1_000_000, 0x_ff, 0b_11, 0o_7, 1_0.0_1, 1e1_0)
