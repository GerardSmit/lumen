def minmax(seq):
    return min(seq), max(seq)
lo, hi = minmax([3, 1, 4, 1, 5])
print(lo, hi, minmax("hello"), minmax((2.5, 1)))
pts = [(1, 2), (3, 1), (2, 2), (1, 1)]
print(sorted(pts), sorted(pts, key=lambda p: (p[1], p[0])), max(pts), min(pts, key=lambda p: p[0] + p[1]), [x + y for x, y in pts], sum(x for x, _ in pts))
print(list(zip(*pts)), [p for p in pts if p[0] == 1], dict(pts) if len(set(p[0] for p in pts)) == len(pts) else "dup keys", dict(pts))
dist = {}
for x, y in pts:
    dist[(x, y)] = x * x + y * y
print(dist, dist[(3, 1)])
grid = {}
for r in range(2):
    for c in range(3):
        grid[r, c] = r * 3 + c
print(grid, grid[1, 2], (1, 2) in grid, (5, 5) in grid)
a, b = 1, 2
a, b = b, a
print(a, b)
a, b, c = 1, 2, 3
a, b, c = c, a, b
print(a, b, c)
x = y = (1, 2)
print(x is y, x == y)
nested = ((1, 2), (3, (4, 5)))
(p, q), (r, (s, t)) = nested
print(p, q, r, s, t)
for i, (a, b) in enumerate([(1, 2), (3, 4)]):
    print(i, a, b)
for (a, b), c in [((1, 2), 3), ((4, 5), 6)]:
    print(a + b + c)
print([(i, j) for i in range(2) for j in range(2)], [(i, c) for i, c in enumerate("ab")], list(map(lambda p: p[0] * p[1], pts)))
print(tuple(sorted(set((1, 2, 3, 2, 1)))), tuple(x for x in range(10) if x % 4 == 0), len(tuple(range(1000))), tuple("abc")[::-1])
def varargs(*args):
    return args, type(args).__name__
print(varargs(), varargs(1), varargs(1, 2), varargs(*(1, 2), 3), varargs(*"ab"))
def kw(**k):
    return tuple(sorted(k.items()))
print(kw(b=1, a=2), kw())
print((1, 2) + (3, 4) == (1, 2, 3, 4), (1, 2) * 0 == (), ("a",) * 3, ("ab" "c",), ("x", "y")[::-1], (1, 2, 3, 4)[1:-1], (1, 2, 3)[5:], (1, 2, 3)[-5:2])
t = (1, 2, 3)
print(t[0] + t[-1], t.__class__.__name__, t.__add__((4,)), t.__mul__(2), t.__getitem__(1), t.__eq__((1, 2, 3)), t.__lt__((1, 2, 4)), t.__hash__() == hash(t))
print(str((1, "a")), "%s" % ((1, 2),), "%s-%s" % (1, 2), "{} {}".format(*(1, 2)), "{0[0]} {0[1]}".format((5, 6)), f"{(1, 2)} {(1,)}")
print(isinstance((), tuple), issubclass(tuple, object), bool(()), bool((0,)), bool((None,)), len(()) == 0, () is () or True, (1,) is not None)
print(sorted([(2, "b"), (1, "z"), (2, "a")]), sorted([(1, 2), (1,), ()]), max([(1, 2), (1, 3)]), (1, 2, 3) > (1, 2), ("a", 1) < ("a", 2))
from_func = tuple(divmod(17, 5))
print(from_func, from_func == (3, 2), *from_func, sep="|")
