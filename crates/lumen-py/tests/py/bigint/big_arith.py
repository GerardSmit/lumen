a = 2 ** 100
b = 3 ** 50
print(a, b)
print(a + b, a - b, b - a, a * b)
print(a // b, a % b, divmod(a, b), divmod(-a, b), divmod(a, -b), divmod(-a, -b))
print(a ** 2, (-a) ** 3, a ** 0, 10 ** 30, -10 ** 30)
f = 1
for i in range(1, 31):
    f *= i
print(f, f // 10 ** 10, f % 10 ** 7)
print(2 ** 64 - 1, 2 ** 63, -2 ** 63, 2 ** 63 - 1 + 1, 2 ** 62 * 2, 9223372036854775807 + 1, -9223372036854775808 - 1)
print(9223372036854775807 * 9223372036854775807, 4611686018427387904 * 2, 4611686018427387904 + 4611686018427387904)
print(a == 2 ** 100, a > b, a < b, a != a + 1, a >= a, max(a, b), min(a, b))
print(abs(-a), -(-a), +a, bool(a), bool(a - a), a - a)
print(a / b, a / 3, (10 ** 400) // (10 ** 399), float(a), float(10 ** 30), int(1e30))
print(pow(a, 3, 10 ** 9 + 7), pow(3, 10 ** 20, 10 ** 9 + 7), pow(2, 1000, 10 ** 10))
print(a.bit_length(), (a - 1).bit_length(), (-a).bit_length(), (10 ** 50).bit_length())
print(hash(2 ** 61 - 1), hash(2 ** 61), hash(-(2 ** 61 - 1)), hash(2 ** 100) == hash(float(2 ** 100)))
fib = [0, 1]
for _ in range(198):
    fib.append(fib[-1] + fib[-2])
print(fib[100], fib[199])
print(sum(range(10 ** 6)), sum([10 ** 20] * 5), 7 ** 77 % 1000)
print(1 // 10 ** 30, -1 // 10 ** 30, 1 % 10 ** 30, -1 % 10 ** 30, 10 ** 30 // -7, 10 ** 30 % -7)
print(len(str(3 ** 1000)), str(3 ** 1000)[:20], str(3 ** 1000)[-20:])
print(isinstance(a, int), type(a).__name__, a.__class__ is int)
print(int(2 ** 70 + 0.0) == 2 ** 70, 2 ** 70 == float(2 ** 70), 2 ** 53 + 1 == float(2 ** 53 + 1), 2 ** 1024 > 1e308)
try:
    print(float(10 ** 400))
except OverflowError:
    print("OverflowError")
try:
    print(10 ** 400 / 3)
except OverflowError:
    print("OverflowError2")
print((10 ** 400) / (10 ** 399), (10 ** 400) // 7 % 1000)
