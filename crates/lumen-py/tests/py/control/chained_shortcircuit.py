print(1 < 2 < 3, 1 < 3 < 2, 3 > 2 > 1, 1 == 1 != 2)
print(1 < 2 > 1, 5 >= 5 >= 5, 1 <= 0 <= 1)
x = 5
print(0 < x < 10, 0 < x < 5, 0 <= x <= 5)
print("a" < "b" < "c", [1] < [2] < [3])
log = []
def f(v):
    log.append(v)
    return v
print(f(1) < f(2) < f(3), log)
log.clear()
print(f(3) < f(2) < f(1), log)
log.clear()
print(f(1) < f(0) < f(9), log)

print(0 or "x", 1 or "x", "" or [] or None)
print(0 and "x", 1 and "x", 1 and 2 and 3, 1 and 0 and 3)
print(None or 0 or "")
print(not (1 and 0))
log.clear()
r = f(0) and f(1)
print(r, log)
log.clear()
r = f(0) or f(2) or f(3)
print(r, log)
d = {}
print(d.get("k") or "default")
print((1, 2) == (1, 2) == (1, 2))
a = b = 3
print(a is b, a is not None, 1 in [1] in [[1]])
print(1 < 2 == 2 < 3)
n = None
print(n is not None and n > 3)
print(True + True, True and 5, False or 7)
print(1 if 1 < 2 < 3 else 0)
print(3 in {3} and "in")
print(float("nan") == float("nan"), 0.0 == -0.0)
