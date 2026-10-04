import copy


class Point:
    def __init__(self, x, y):
        self.x = x
        self.y = y

    def __eq__(self, other):
        return (self.x, self.y) == (other.x, other.y)


class Custom:
    def __init__(self):
        self.items = [1, 2]

    def __copy__(self):
        c = Custom()
        c.items = self.items
        return c

    def __deepcopy__(self, memo):
        c = Custom()
        c.items = copy.deepcopy(self.items, memo)
        return c


a = [1, [2, 3], {"k": [4]}]
b = copy.copy(a)
c = copy.deepcopy(a)
print(b == a, b is a, b[1] is a[1], c == a, c[1] is a[1], c[2]["k"] is a[2]["k"])

p = Point(1, [2])
q = copy.copy(p)
r = copy.deepcopy(p)
print(q == p, q.y is p.y, r.y is p.y, r.y == p.y)

cu = Custom()
print(copy.copy(cu).items is cu.items, copy.deepcopy(cu).items is cu.items)

cyc = [1]
cyc.append(cyc)
d = copy.deepcopy(cyc)
print(d[1] is d, d is cyc)

t = (1, [2])
print(copy.copy(t) is t, copy.deepcopy(t)[1] is t[1])
s = {1, 2}
print(copy.copy(s) == s, copy.deepcopy(frozenset([1])) == frozenset([1]))
print(copy.copy("x"), copy.copy(3), copy.copy(None), copy.deepcopy(1.5))
print(copy.copy(len) is len)

try:
    import copy as c2
    c2.Error
    print("Error ok")
except AttributeError:
    print("no Error")
