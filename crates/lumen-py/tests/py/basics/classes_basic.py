class P:
    count = 0
    def __init__(self, x, y=0):
        self.x, self.y = x, y
        P.count += 1
    def __repr__(self):
        return "P(%r, %r)" % (self.x, self.y)
    def __eq__(self, o):
        return isinstance(o, P) and (self.x, self.y) == (o.x, o.y)
    def __hash__(self):
        return hash((self.x, self.y))
    def __add__(self, o):
        return P(self.x + o.x, self.y + o.y)
    def __lt__(self, o):
        return (self.x, self.y) < (o.x, o.y)
    def __bool__(self):
        return bool(self.x or self.y)
    def __len__(self):
        return 2
    def __getitem__(self, i):
        return (self.x, self.y)[i]
    @property
    def norm1(self):
        return abs(self.x) + abs(self.y)
    @staticmethod
    def origin():
        return P(0)
    @classmethod
    def make(cls, v):
        return cls(v, v)
p, q = P(1, 2), P(3)
print(p, q, p + q, p == P(1, 2), p != q, p < q)
print(sorted([q, p, P(0, 9)]))
print(bool(P(0)), len(p), p[1], list(p))
print(p.norm1, P.origin(), P.make(4), P.count)
print(len({P(1, 1), P(1, 1), P(2, 2)}))
class Q(P):
    def __repr__(self):
        return "Q" + super().__repr__()
print(Q.make(2), isinstance(Q(1), P), issubclass(Q, P), Q.__mro__[1].__name__)
print(type(p).__name__, p.__class__ is P, hasattr(p, "x"), getattr(p, "zz", "dflt"))
setattr(p, "zz", 5)
print(p.zz, vars(p) == {"x": 1, "y": 2, "zz": 5})
del p.zz
print(hasattr(p, "zz"))
class S:
    __slots__ = ("a",)
s = S()
s.a = 1
try:
    s.b = 2
except AttributeError:
    print("slots")
class D:
    def __getattr__(self, n):
        return "dyn_" + n
print(D().foo)
class Cnt:
    def __call__(self, n):
        return n * 2
    def __contains__(self, v):
        return v == 3
    def __iter__(self):
        return iter([1, 2])
print(Cnt()(4), 3 in Cnt(), list(Cnt()))
