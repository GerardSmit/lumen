a, b = 1, 2
a, b = b, a
print(a, b)
a, b, c = "xyz"
print(a, b, c)
a, b = [10, 20]
a, b = {1: "k", 2: "v"}
print(a, b)
a, b = range(2)
(a, b), c = (1, 2), 3
print(a, b, c)
[a, [b, c]] = [1, [2, 3]]
print(a, b, c)
a, *rest = [1, 2, 3, 4]
print(a, rest)
*init, last = [1, 2, 3, 4]
print(init, last)
first, *mid, last = [1, 2, 3, 4, 5]
print(first, mid, last)
first, *mid, last = [1, 2]
print(first, mid, last)
a, *b, c, d = "abcdef"
print(a, b, c, d)
*a, = (1, 2, 3)
print(a, type(a).__name__)
a, *b = "x"
print(a, b)
a, *b = (1,)
print(a, b)
x, (y, *z), w = 1, (2, 3, 4), 5
print(x, y, z, w)
(a, *b), *c = [1, 2, 3], 4, 5
print(a, b, c)
for first, *others in [(1, 2, 3), (4,), (5, 6)]:
    print(first, others)
for i, (a, *b) in enumerate(["abc", "d"]):
    print(i, a, b)
a, b, *c = range(5)
print(a, b, c)
a = b = c = []
a.append(1)
print(a, b is c)
i = 0
lst = [0, 0, 0]
i, lst[i] = 1, 5
print(i, lst)
d = {}
d["a"], d["b"] = 1, 2
print(d)
class O:
    pass
o = O()
o.x, o.y = 3, 4
print(o.x, o.y)
a, b = b, a = 1, 2
print(a, b)
for bad, n in ((lambda: [1, 2, 3], 2), (lambda: [1], 2), (lambda: 5, 1), (lambda: [], 1), (lambda: "ab", 3)):
    try:
        if n == 1:
            q, = bad()
        elif n == 2:
            q, r = bad()
        else:
            q, r, s = bad()
    except (ValueError, TypeError) as e:
        print(type(e).__name__)
try:
    a, *b, c = [1]
except ValueError as e:
    print("ValueError", e)
try:
    a, b = 1, 2, 3
except ValueError as e:
    print("ValueError", e)
try:
    a, b, c = 1, 2
except ValueError as e:
    print("ValueError", e)
def gen():
    yield 1
    yield 2
    yield 3
a, *b = gen()
print(a, b)
a, b, c = gen()
print(a, b, c)
a, *_ = gen()
*_, z = gen()
print(a, z)
print(*[1, 2], *"ab", *(3,), sep=",")
print([*range(3), *[7], 8], (*[1], *(2,)), {*[1, 2], 3} == {1, 2, 3}, {**{"a": 1}, **{"b": 2}}, [*"ab"], [*{1: 2}])
def f(a, b, *c, d=4, **e):
    return a, b, c, d, e
print(f(*[1, 2, 3], **{"d": 5, "z": 6}), f(*"ab", *"cd"), f(1, *[2], d=0, **{"k": 1}))
