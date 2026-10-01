fib = [0, 1]
a, b = 0, 1
for _ in range(10):
    a, b = b, a + b
print(a, b)
l = [3, 1, 2]
l[0], l[1], l[2] = l[2], l[0], l[1]
print(l)
l[0], l[-1] = l[-1], l[0]
print(l)
x, y, z = 1, 2, 3
x, y, z = z, x, y
print(x, y, z)
x, y = y, x + y
print(x, y)
pairs = {"a": 1, "b": 2}
for k, v in pairs.items():
    print(k, v)
for k, (v, w) in {"p": (1, 2)}.items():
    print(k, v, w)
for (i, j), v in {(0, 1): "x", (1, 0): "y"}.items():
    print(i, j, v)
for a, b in zip("ab", [1, 2]):
    print(a, b)
for i, (a, b) in enumerate(zip("ab", "cd")):
    print(i, a, b)
print([a + b for a, b in [(1, 2), (3, 4)]], {k: v for k, v in zip("ab", "xy")}, [(a, b) for a, *b in ["abc", "de"]], [c for _, c in [(1, 2), (3, 4)]])
print(sum(a * b for a, b in zip([1, 2, 3], [4, 5, 6])), list(map(lambda p: p[0] - p[1], [(5, 1), (3, 3)])), [m for (m, _) in [(1, 2)]])
print(dict((k, v) for k, v in [("a", 1)]), list(zip(*[(1, 2), (3, 4), (5, 6)])), [list(t) for t in zip(*[[1, 2], [3, 4]])])
def swap(p):
    a, b = p
    return b, a
print(swap((1, 2)), swap([3, 4]), swap("xy"), swap(swap((1, 2))))
def stats(v):
    return min(v), max(v), sum(v) / len(v)
lo, hi, avg = stats([1, 2, 3, 4])
print(lo, hi, avg)
q, r = divmod(17, 5)
print(q, r)
head, *tail = "hello"
print(head, tail, "".join(tail))
def lst(*a):
    return a
m, n = lst(1, 2)
(m, n), = [(7, 8)]
print(m, n)
[p, q] = "ab"
print(p, q)
(p), q = 1, 2
print(p, q)
p, = [9]
print(p)
for a, in [(1,), (2,)]:
    print(a)
with_idx = [(i, c) for i, c in enumerate("ab")]
print(with_idx)
nested = [((1, 2), [3, 4]), ((5, 6), [7, 8])]
for (a, b), [c, d] in nested:
    print(a, b, c, d)
for [a, b] in [[1, 2], (3, 4), "56"]:
    print(a, b)
s = 0
for i, *r in [(1, 2, 3), (4, 5)]:
    s += i + sum(r)
print(s)
a = [1, 2, 3]
a[0], a[1:] = 10, [20, 30, 40]
print(a)
a[1:3], b = [0], 5
print(a, b)
i = 0
i, a[i] = 2, "set"
print(i, a)
x = [1, 2, 3]
x[0], x[x[0]] = x[x[0]], x[0]
print(x)
t = (1, 2)
t = t[1], t[0]
print(t)
a = b = 5
a += 1
print(a, b)
a, b = (b, a) if a > b else (a, b)
print(a, b)
print([(i, j) for i, j in [(1, 2)] for k in range(2)], [x for x, in [(1,), (2,)]])
