a, *b, c = [1, 2, 3, 4, 5]
print(a, b, c)
a, *b, c = (1, 2)
print(a, b, c)
a, *b = "xyz"
print(a, b)
*a, b = range(4)
print(a, b)
*a, = (1, 2, 3)
print(a)
a, b, *c = [1, 2]
print(a, b, c)
first, *mid, last = "hello"
print(first, mid, last)
x, (y, *z), w = 1, (2, 3, 4, 5), 6
print(x, y, z, w)
(a, b), (c, *d) = (1, 2), (3, 4, 5)
print(a, b, c, d)
[a, [b, *c]] = [1, [2, 3, 4]]
print(a, b, c)
a, *_, b = range(100)
print(a, b)
*init, last = [[1], [2], [3]]
print(init, last)
*a, b, c = {1: "a", 2: "b", 3: "c"}
print(a, b, c)
a, *b = iter([7])
print(a, b)
a, *b, c = "ab"
print(a, b, c)
print(type(b).__name__)

a, b = 1, 2
a, b = b, a
print(a, b)
a, b, c = 1, 2, 3
a, b, c = c, a, b
print(a, b, c)
L = [10, 20, 30]
L[0], L[2] = L[2], L[0]
print(L)
i = 0
i, L[i] = 2, 99
print(i, L)
d = {}
d["a"], d["b"] = 1, 2
print(sorted(d.items()))


class O:
    pass


o = O()
o.x, o.y = 5, 6
print(o.x, o.y)
a = b = c = []
a.append(1)
print(a, b, c, a is c)
x = y = 5
print(x, y)
a, b = b, a = 1, 2
print(a, b)
a, b = [1, 2]
c, d = {10, 20} - {20}, 0
print(a, b, c, d)
for a, *b in [(1, 2, 3), (4,), (5, 6)]:
    print(a, b)
print([(h, t) for h, *t in ["abc", "d"]])
def f():
    return 1, 2, 3
p, *q = f()
print(p, q)
s, = [42]
print(s)
(s,) = "z"
print(s)
