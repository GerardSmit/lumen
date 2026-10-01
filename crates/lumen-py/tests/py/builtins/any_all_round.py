print(any([]), all([]), any(()), all(""), any({}), all(set()))
print(any([0, "", None]), any([0, 1]), all([1, 2, 3]), all([1, 0, 2]))
print(any(x > 2 for x in range(5)), all(x < 5 for x in range(5)), all(x < 3 for x in range(5)))
seen = []


def probe(v):
    seen.append(v)
    return v


print(any(probe(v) for v in [0, 1, 2, 3]), seen)
seen.clear()
print(all(probe(v) for v in [1, 1, 0, 1]), seen)
print(any([[]]), all([[]]), any([[0]]))

print(round(0.5), round(1.5), round(2.5), round(3.5), round(-0.5), round(-1.5), round(-2.5))
print(round(0.4), round(0.6), round(-0.6), round(7), round(True))
print(type(round(2.5)).__name__, type(round(2.5, 0)).__name__, type(round(2.567, 2)).__name__)
print(round(2.675, 2), round(1.005, 2), round(3.14159, 3), round(-3.14159, 1), round(0.125, 2))
print(round(2.5, 0), round(-0.5, 0), round(1234.5678, -2), round(1234.5678, 0))
print(round(15, -1), round(25, -1), round(35, -1), round(-25, -1), round(123, -2), round(150, -2))
print(round(5, 1), round(12345, -10), round(0, -1), round(99, -5))
print(round(1e22, 2) == 1e22, round(1.5, None), round(2.5, None))
print(round(float("inf"), 2))
try:
    round(float("nan"))
except ValueError as e:
    print(type(e).__name__)
try:
    round(float("inf"))
except OverflowError as e:
    print(type(e).__name__)
print(abs(-5), abs(5), abs(-2.5), abs(0), abs(-0.0), abs(True), abs(-10**20))
print(pow(2, 10), pow(2, -1), pow(2.0, 3), pow(2, 0.5) == 2 ** 0.5, pow(-8, 1 / 3) != 0)
print(pow(3, 4, 5), pow(2, 100, 1000), pow(5, 0, 7), pow(2, 10, 1), pow(7, -1, 10), pow(2, 3, -5))
try:
    pow(2, -1, 5)
    pow(2, 3, 0)
except ValueError as e:
    print(type(e).__name__)

print(divmod(7, 2), divmod(-7, 2), divmod(7, -2), divmod(-7, -2), divmod(0, 5))
print(divmod(7.5, 2), divmod(-7.5, 2), divmod(7.5, -2), divmod(-7.5, -2))
print(divmod(7, 2.0), divmod(10**20, 3), divmod(-10**20, 3))
print(7 // 2, -7 // 2, 7 // -2, -7 // -2, 7 % 3, -7 % 3, 7 % -3, -7 % -3)
print(7.0 // 2, -7.0 // 2, 7 // -2.0, 7.5 % 2, -7.5 % 2, 7.5 % -2, -7.5 % -2)
print(5 % 1, -5 % 1, 5.5 % 1, -5.5 % 1, 0 % 3, 0.0 % -3)
print(-0.0 // 1, 6 % 2.0, 1e300 // 1e-300 == float("inf") or True)
for f in (lambda: 1 // 0, lambda: 1 % 0, lambda: divmod(1, 0), lambda: 1.0 // 0, lambda: 1 / 0, lambda: 1.0 % 0.0, lambda: divmod(1.0, 0)):
    try:
        f()
    except ZeroDivisionError as e:
        print(type(e).__name__)
print((-7) // 2 * 2 + (-7) % 2, 7 // -2 * -2 + 7 % -2)
print(1 / 2, -1 / 2, 6 / 3, type(6 / 3).__name__, 10**20 / 10**19, 1 / 3)
print(int(-7 / 2), int(7 / 2), -7 // 2, (-7) // 2)
