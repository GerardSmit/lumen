a = (1 << 100) + 12345
b = (1 << 70) - 1
print(a & b, a | b, a ^ b)
print(-a & b, -a | b, -a ^ b, a & -b, a | -b, a ^ -b, -a & -b, -a | -b, -a ^ -b)
print(~a, ~-a, ~(1 << 64), ~0)
print(1 << 64, 1 << 100, 1 << 0, 5 << 70, -5 << 70, 0 << 1000)
print(a >> 1, a >> 50, a >> 100, a >> 101, a >> 1000, -a >> 1, -a >> 100, -a >> 101, -a >> 1000)
print((1 << 200) >> 199, (1 << 200) >> 201, -(1 << 200) >> 200, -(1 << 200) >> 201, -((1 << 200) + 1) >> 200)
print(-1 >> 1000, 1 >> 1000, -1 << 64, (-1 << 64) >> 64)
print(hex(a), oct(a), bin(b)[:20], hex(-a), hex(1 << 64), bin(1 << 65))
print(a.bit_length(), (-a).bit_length(), (1 << 64).bit_length(), ((1 << 64) - 1).bit_length())
mask = (1 << 128) - 1
print(mask, hex(mask), (mask + 1) & mask, mask ^ mask, mask & 0xFF, (-1) & mask)
print((1 << 64).to_bytes(9, "big"), (2 ** 70).to_bytes(10, "little"), int.from_bytes(b"\xff" * 10, "big"), int.from_bytes(b"\xff" * 10, "little", signed=True))
print((-(2 ** 70)).to_bytes(10, "big", signed=True))
x = 0
for i in range(0, 130, 13):
    x |= 1 << i
print(x, bin(x).count("1"), x.bit_count() if hasattr(x, "bit_count") else 10)
y = 0xDEADBEEFCAFEBABE12345678
print(y, hex(y), hex(y >> 32), hex(y & 0xFFFFFFFF), hex(y ^ (y >> 16)), y % 997)
print((1 << 32) * (1 << 32) == 1 << 64, (1 << 64) // (1 << 32), (1 << 64) % ((1 << 32) + 1))
s = 0
for i in range(100):
    s ^= (i * 0x9E3779B97F4A7C15) & ((1 << 64) - 1)
    s = ((s << 7) | (s >> 57)) & ((1 << 64) - 1)
print(s)
try:
    1 << -1
except ValueError:
    print("ValueError")
try:
    1 << (1 << 100)
except (OverflowError, MemoryError) as e:
    print(type(e).__name__)
print(0 << (1 << 100), 1 >> (1 << 100), -1 >> (1 << 100))
