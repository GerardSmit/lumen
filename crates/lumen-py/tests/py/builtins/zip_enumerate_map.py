print(list(zip([1, 2, 3], "ab")))
print(list(zip("ab", [1, 2, 3])))
print(list(zip()), list(zip([1])), list(zip([], [1])))
print(list(zip(range(3), range(10, 13), "xyz")))
print(list(zip(*[(1, "a"), (2, "b")])))
rows = [(1, 2, 3), (4, 5, 6)]
print(list(zip(*rows)))
print([list(c) for c in zip(*rows)])
print(dict(zip("abc", range(3))))
print(list(zip(*zip([1, 2], [3, 4]))))
print(list(zip([1, 2], [3, 4], strict=True)))
try:
    list(zip([1, 2], [3], strict=True))
except ValueError as e:
    print(type(e).__name__)
print(list(enumerate("ab")), list(enumerate("ab", 5)), list(enumerate([], 3)))
print(list(enumerate("ab", start=-1)))
print(dict(enumerate("xy")))
for i, v in enumerate(range(5, 8)):
    print(i, v, end="; ")
print()
print(list(map(str, [1, 2])), list(map(lambda x: x * 2, range(3))))
print(list(map(lambda a, b: a + b, [1, 2, 3], [10, 20])))
print(list(map(lambda a, b, c: (a, b, c), "ab", "cd", "ef")))
print(list(map(None.__class__, [])), list(map(abs, [-1, 2, -3])))
print(list(map(int, "123")), sum(map(int, "999")))
print(list(map(len, ["a", "bb", ""])), list(map(max, [1, 5], [3, 2])))
print(list(filter(None, [0, 1, "", "a", [], [0], None, False, True])))
print(list(filter(lambda x: x > 1, [1, 2, 3])), list(filter(str.isdigit, "a1b2")))
print(list(filter(None, range(4))), list(filter(bool, {0: 1, 1: 0})))
print(list(reversed([1, 2, 3])), list(reversed("abc")), list(reversed(range(3))))
print(list(reversed((1, 2))), list(reversed([])))
try:
    reversed({1, 2})
except TypeError as e:
    print(type(e).__name__)
try:
    reversed(5)
except TypeError as e:
    print(type(e).__name__)

log = []


def noisy(x):
    log.append(x)
    return x * 2


m = map(noisy, [1, 2, 3])
print(log)
print(next(m), log)
print(next(m), log)
print(list(m), log)
print(list(m), next(m, "end"))
z = zip([1, 2, 3], "abc")
print(next(z), next(z))
print(list(z), list(z))
e = enumerate("abc")
print(next(e), next(e), list(e))
f = filter(None, [0, 1, 2])
print(next(f), next(f), next(f, "x"))
src = iter([1, 2, 3, 4])
print(list(zip(src, "ab")), list(src))
print(iter(m) is m, iter(z) is z, iter(e) is e)
it = iter([1, 2, 3])
print(list(zip(it, it)))
print(type(map(abs, [])).__name__, type(zip()).__name__, type(enumerate([])).__name__)
print(type(filter(None, [])).__name__, type(reversed([])).__name__)
lazy = (x * x for x in range(3))
print(list(lazy), list(lazy))
print(sorted(map(lambda p: p[0] + p[1], zip([3, 1], [4, 1]))))
