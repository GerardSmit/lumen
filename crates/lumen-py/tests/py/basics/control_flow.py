for i in range(10):
    if i % 2:
        continue
    if i > 6:
        break
    print("i", i)
else:
    print("no else expected")
for i in range(3):
    pass
else:
    print("for-else ran", i)
n = 0
while n < 5:
    n += 1
    if n == 3:
        continue
    print("n", n)
else:
    print("while-else", n)
while True:
    n -= 1
    if n < 0:
        break
else:
    print("never")
print(n)
x = 5
r = "big" if x > 3 else "small"
print(r)
print(1 if 0 else 2 if 0 else 3)
print(0 or "a", "" or 0, 1 and 2, 0 and 1, None or [] or "z")
print(not 0, not "x", not [], not None)
print(1 < 2 < 3, 1 < 3 < 2, 3 > 2 > 1 == 1)
def f(v):
    if v < 0:
        return "neg"
    elif v == 0:
        return "zero"
    else:
        return "pos"
print([f(v) for v in (-1, 0, 1)])
for a, b in [(1, 2), (3, 4)]:
    print(a + b)
for i, c in enumerate("abc", 1):
    print(i, c)
print(list(zip(range(3), "xyz")))
y = (z := 10) + 1
print(y, z)
if (m := len("hello")) > 3:
    print("m", m)
for i in range(3):
    try:
        if i == 1:
            continue
        print("body", i)
    finally:
        print("finally", i)
def g():
    for i in range(5):
        try:
            return i
        finally:
            print("cleanup")
print(g())
