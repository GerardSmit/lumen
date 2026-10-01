def classify(n):
    if n < 0:
        return "neg"
    elif n == 0:
        return "zero"
    elif n < 10:
        return "small"
    else:
        return "big"

for v in (-5, 0, 3, 10, 99):
    print(v, classify(v))

for v in (0, 1, "", "a", [], [0], None, 0.0, 0.1, (), {}):
    print(repr(v), "T" if v else "F", "yes" if v else "no")

x = 5
print("a" if x > 3 else "b" if x > 1 else "c")
print("a" if x > 9 else "b" if x > 1 else "c")
print("a" if x > 9 else "b" if x > 7 else "c")
print((1 if x else 2) + (3 if not x else 4))
print([i if i % 2 else -i for i in range(6)])

if x:
    pass
else:
    print("never")
y = None
if y is None:
    print("none")
if not y:
    print("falsy")
if x == 5 and y is None or False:
    print("combo")
if (z := x * 2) > 8:
    print("walrus", z)
print(not 0, not 1, not "", not [1])
print(1 if None else 2)
