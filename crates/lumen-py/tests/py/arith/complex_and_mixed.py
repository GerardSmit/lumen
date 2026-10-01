a = 3 + 4j
b = 1 - 2j
print(a, b, a + b, a - b, a * b, a / b)
print(a.real, a.imag, abs(a), a.conjugate(), -a, +a)
print(1j * 1j, 1j ** 2, (1 + 1j) ** 2, 2 ** 0.5j == 2 ** 0.5j)
print(complex(1, 2), complex("1+2j"), complex(3), complex(), complex(1.5, -0.5))
print(repr(1j), repr(0j), repr(-1j), repr(1 + 0j), repr(1.5 + 2.5j), repr(complex(0, -0.0)))
print(a == 3 + 4j, a != b, 3 + 0j == 3, hash(3 + 0j) == hash(3))
print(type(1j).__name__, type(a * 1.0).__name__, type(1 + 1.0).__name__, type(True + 1.0).__name__)
print(1 + 2.0, 1 + 2j, 1.5 + 2j, 2 * 3.0, 3 * 2j, 6 / 3, 2 ** 3, 2 ** 3.0, 2.0 ** 3)
try:
    print(a < b)
except TypeError as e:
    print("TypeError")
try:
    print(a // b)
except TypeError:
    print("TypeError floor")
try:
    print(a % 2)
except TypeError:
    print("TypeError mod")
try:
    print(1j / 0)
except ZeroDivisionError:
    print("ZeroDivisionError")
print(sum([1, 2.5, 3j]), sum([1j, 2j]), abs(-0.0), abs(3 + 4j) == 5.0)
print(divmod(7, 2.0), divmod(-7, 2.0), 7 // 2.0, 7 % 2.5)
print(bool(0j), bool(1j), bool(0.0), bool(-0.0), bool(float("nan")))
print(str(3 + 0j), str(-3 - 3j), str(1e20 + 1e-5j), str(1.0 + 1.0j))
print(int(True) + int(3.99) + int("7"), float("1") + int(2), 0.5 + 1)
print(5 // 2 * 2 + 5 % 2, -5 // 2 * 2 + -5 % 2, 2 ** 3 ** 2, (2 ** 3) ** 2, -3 ** 2, 2 * 3 + 4 * 5 - 6 / 3)
print(1 + 2 * 3 ** 2 // 4 % 5, 10 - 2 - 3, 2 ** -1 ** 2, 100 / 10 / 5, 7 & 3 | 8 ^ 1, 1 << 2 + 1, not 1 + 1 == 2)
print(5 > 3 == True, 1 < 2 > 1, 0 == 0.0 == False, "a" < "b" < "c", [1] < [2] < [3])
