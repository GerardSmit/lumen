print(1 << 100, 1 << 64, (1 << 100) >> 98, -1 << 70, -(1 << 70) >> 69, 1 >> 1000, -1 >> 1000, 5 << 0)
print(2**64 - 1, -2**63, 2**63, 2**1000 > 10**300, (2**200) // (2**199), 2**200 % 1000)
print(10**30 + 1 - 10**30, (10**20) * (10**20), -(10**25) // 7, -(10**25) % 7, (10**25) // -7)
print(-7 // 2, -7 % 3, 7 // -2, 7 % -3, -7 // -2, -7 % -3, 7 // 2, 7 % 2, 0 // 5, 0 % -5)
print(-1 // 5, -1 % 5, 1 // -5, 1 % -5, -5 // 5, -5 % 5, 5 // 5, 2**63 // -1)
print(2 ** -1, 2 ** -2, (-2) ** -1, 10 ** -3, 0.5 ** -2, 2 ** 0.5 > 1.41)
print(0 ** 0, 0 ** 1, 1 ** 0, (-1) ** 0, 5 ** 0, 0.0 ** 0, 0 ** 0.0, 7 ** 1)
print((-1) ** 100, (-1) ** 101, (-2) ** 3, -2 ** 2, 2 ** 3 ** 2, (2 ** 3) ** 2, 3 ** 40)
try:
    0 ** -1
except ZeroDivisionError as e:
    print(type(e).__name__)
try:
    1 << -1
except ValueError as e:
    print(type(e).__name__, e)
try:
    1 >> -1
except ValueError as e:
    print(type(e).__name__)
print(True + True, True + 1, True * 5, False - True, True / True, True // True, True ** 3, -True)
print(True & True, True | False, True ^ True, False ^ True, True & 3, True << 3, 5 & True)
print(type(True & True).__name__, type(True & 3).__name__, type(True | 2).__name__)
print(True and 5, False or 5, not 5, not 0, 3 if 0 else 4)
print((0).bit_length(), (1).bit_length(), (255).bit_length(), (256).bit_length(), (-1).bit_length(), (-256).bit_length(), (2**100).bit_length())
print((255).bit_count(), (-255).bit_count(), (0).bit_count(), (2**100 - 1).bit_count())
print((1024).to_bytes(2, "big"), (1024).to_bytes(2, "little"), (0).to_bytes(1, "big"), (255).to_bytes(1, "little"))
print((256).to_bytes(4, "big"), (-1).to_bytes(2, "big", signed=True), (-2).to_bytes(2, "little", signed=True), (2**64).to_bytes(9, "big"))
print(int.from_bytes(b"\x04\x00", "big"), int.from_bytes(b"\x04\x00", "little"), int.from_bytes(b"\xff\xff", "big", signed=True))
print(int.from_bytes(b"", "big"), int.from_bytes(b"\xff", "big"), int.from_bytes(b"\x80", "big", signed=True), int.from_bytes([1, 2, 3], "big"))
try:
    (256).to_bytes(1, "big")
except OverflowError as e:
    print(type(e).__name__)
try:
    (-1).to_bytes(2, "big")
except OverflowError as e:
    print(type(e).__name__)
print(1 == 1.0, 2**53 == float(2**53), 2**53 + 1 == float(2**53 + 1), 2**53 + 1 > float(2**53), 2**53 + 1 == 2**53 + 1.0)
print(10**20 == 1e20, 10**23 == 1e23, 10**22 == 1e22, 2**70 == float(2**70), 2**70 + 1 > float(2**70))
print(0.1 + 0.2 == 0.3, 0.5 + 0.25 == 0.75, 3 < 3.5, 3 > 2.9999999999999999, 10**400 > 1e308, -10**400 < -1e308)
print(1 < float("inf"), 10**400 < float("inf"), 5 > float("-inf"), 1 == float("nan"), 1 != float("nan"))
print(int(1e15) + 1, int(2.0**63), int(-2.0**63), int(1e22), int(123456789.987654321))
print(abs(-2**63), abs(-(2**100)), -(-2**63), 2**63 - 1 + 1, -2**63 - 1, (2**63) * 2, (2**32) * (2**32))
print(sum([2**62, 2**62, 2**62]), 2**62 + 2**62, 9223372036854775807 + 1, -9223372036854775808 - 1)
print(9223372036854775807 * 9223372036854775807, -9223372036854775807 * 9223372036854775807)
print(12345678901234567890 % 97, 12345678901234567890 // 97, divmod(12345678901234567890, -97))
print(int("9" * 30) + 1, str(2**100), len(str(3**200)), hash(2**100) == hash(2**100))
print(6 & 3, 6 | 3, 6 ^ 3, ~6, -6 & 3, -6 | 3, -6 ^ 3, ~-6, (2**70) & (2**70 - 1), -(2**70) | 1)
print(1_000_000 * 1_000_000, 0xFF, 0o17, 0b101, 1e3, 5 // 2 * 2 + 5 % 2)
print(round(3.5), round(4.5), 7 / 2, 8 / 2, -7 / 2, 2**53 / 3, 10**20 / 3, 1 / 10**400 == 0.0)
print(f"{255:x} {255:o} {255:b} {-255:x} {2**70:x}")
