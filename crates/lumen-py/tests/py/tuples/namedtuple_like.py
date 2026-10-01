class Point(tuple):
    def __new__(cls, x, y):
        return super().__new__(cls, (x, y))

    @property
    def x(self):
        return self[0]

    @property
    def y(self):
        return self[1]

    def __repr__(self):
        return f"Point(x={self.x}, y={self.y})"

    def dist2(self):
        return self.x * self.x + self.y * self.y

    def _replace(self, **kw):
        return Point(kw.get("x", self.x), kw.get("y", self.y))


p = Point(3, 4)
print(p)
print(repr(p), str(p))
print(p.x, p.y, p[0], p[1], len(p))
print(p.dist2())
print(isinstance(p, tuple), isinstance(p, Point), type(p).__name__)
x, y = p
print(x, y)
print(p == (3, 4), p == Point(3, 4), p != Point(4, 3))
print(hash(p) == hash((3, 4)))
print({p: 1}[(3, 4)])
print(p + (5,), type(p + (5,)).__name__)
print(p < Point(3, 5), max(Point(1, 2), Point(1, 3)))
print(p._replace(y=10))
print(list(p), tuple(p), sorted(p))
print(p.count(3), p.index(4))
try:
    p.x = 5
except AttributeError as e:
    print(type(e).__name__)
try:
    Point(1)
except TypeError as e:
    print(type(e).__name__)


class Pair(tuple):
    __slots__ = ()

    def __new__(cls, a, b):
        return tuple.__new__(cls, (a, b))

    first = property(lambda self: self[0])
    second = property(lambda self: self[1])

    def swap(self):
        return Pair(self.second, self.first)

    def __repr__(self):
        return "Pair(%r, %r)" % (self.first, self.second)


q = Pair("a", 1)
print(q, q.swap(), q.swap().swap() == q)
print([Pair(i, i * i) for i in range(3)])
print(sorted([Pair(2, 1), Pair(1, 5), Pair(1, 2)]))
print(len(q), q[-1], q[::-1])


class Empty(tuple):
    pass


e = Empty()
print(len(e), e == (), type(e).__name__, bool(e))
e2 = Empty([1, 2, 3])
print(e2, len(e2), type(e2).__name__)
