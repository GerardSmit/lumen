for i, c in enumerate("abc"):
    print(i, c)
for i, c in enumerate("abc", 10):
    print(i, c)
for a, b in [(1, 2), (3, 4)]:
    print(a + b)
for (a, b), c in [((1, 2), 3), ((4, 5), 6)]:
    print(a, b, c)
for i, (a, b) in enumerate([(1, 2), (3, 4)]):
    print(i, a, b)
for i, (a, (b, c)) in enumerate([(1, (2, 3))]):
    print(i, a, b, c)
d = {"a": 1, "b": 2, "c": 3}
for k, v in d.items():
    print(k, v)
for k, v in sorted(d.items(), key=lambda kv: -kv[1]):
    print(k, v)
for [a, b] in [[1, 2], "xy"]:
    print(a, b)
for a, *rest in [(1, 2, 3), (4,)]:
    print(a, rest)
for x, y in zip(range(3), "abc"):
    print(x, y)
for a, b, c in zip(*[[1, 2], [3, 4], [5, 6]]):
    print(a, b, c)
for p, q in zip(*zip(*[(1, 2), (3, 4)])):
    print(p, q)
print([a * b for a, b in [(1, 2), (3, 4)]])
print([(i, c) for i, c in enumerate("xy")])
print({k: v for k, v in zip("ab", [1, 2])})
print({v: k for k, v in d.items()})
print(sum(a * b for a, b in zip([1, 2, 3], [4, 5, 6])))
print([a for (a, _) in [(1, 2), (3, 4)]])
print([x + y + z for x, (y, z) in [(1, (2, 3)), (4, (5, 6))]])
print([k for k, in [(1,), (2,)]])
print({a for a, b in [(1, 2), (1, 3)]} == {1})
print(list(i for i, _ in enumerate("abc") if i % 2 == 0))
print([(a, b) for a in range(2) for b in range(a, 2)])
print([x for row in [[1, 2], [3]] for x in row])
for self_i, self_c in enumerate("ab"):
    pass
print(self_i, self_c)
i = "outer"
print([i for i in range(3)], i)
for i in range(2):
    pass
print(i)
for i in []:
    pass
print(i)
for _ in range(2):
    print("tick")
for ch in "hé":
    print(ch, ord(ch))
try:
    for a, b in [(1, 2, 3)]:
        pass
except ValueError as e:
    print(type(e).__name__, e)
try:
    for a, b in [1]:
        pass
except TypeError as e:
    print(type(e).__name__)
x = {}
for x["k"], x["j"] in [(1, 2)]:
    pass
print(sorted(x.items()))
r = []
for r_i, r_x in enumerate(reversed([5, 6, 7])):
    r.append(r_i * r_x)
print(r)
