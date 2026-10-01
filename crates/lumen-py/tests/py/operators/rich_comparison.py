class V:
    def __init__(self, n):
        self.n = n

    def __repr__(self):
        return "V(%d)" % self.n

    def __eq__(self, other):
        if not isinstance(other, V):
            return NotImplemented
        return self.n == other.n

    def __lt__(self, other):
        if not isinstance(other, V):
            return NotImplemented
        return self.n < other.n

    def __le__(self, other):
        if not isinstance(other, V):
            return NotImplemented
        return self.n <= other.n

    def __hash__(self):
        return hash(self.n)


a, b, c = V(1), V(2), V(1)
print(a == c, a != b, a == b, a != c)
print(a < b, a <= c, b < a, a <= b, b <= a)
print(a > b, b > a, a >= c, b >= a, a >= b)
print(sorted([V(3), V(1), V(2)]), max(V(3), V(9), V(2)), min([V(4), V(0)]))
print(a == 1, a != 1, a == None)
try:
    a < 1
except TypeError as e:
    print(type(e).__name__, e)
try:
    1 > a
except TypeError as e:
    print(type(e).__name__)


class Nope:
    def __lt__(self, other):
        return NotImplemented

    def __gt__(self, other):
        return NotImplemented


try:
    Nope() < Nope()
except TypeError as e:
    print(type(e).__name__)
try:
    Nope() >= Nope()
except TypeError as e:
    print(type(e).__name__)
print(Nope() == Nope() or True, Nope() != 3)


class Log:
    def __init__(self, name):
        self.name = name

    def __lt__(self, other):
        print(self.name, "lt", other.name)
        return True

    def __gt__(self, other):
        print(self.name, "gt", other.name)
        return False

    def __eq__(self, other):
        print(self.name, "eq", other.name)
        return True

    __hash__ = None


x, y = Log("x"), Log("y")
print(x < y)
print(x > y)
print(x == y)
print(x != y)


class Sub(Log):
    def __lt__(self, other):
        print("Sub", self.name, "lt", other.name)
        return True

    def __gt__(self, other):
        print("Sub", self.name, "gt", other.name)
        return True


s = Sub("s")
print(x < s)
print(s < x)
print(x > s)


class Total:
    def __init__(self, v):
        self.v = v

    def __eq__(self, o):
        return self.v == o.v

    def __lt__(self, o):
        return self.v < o.v

    def __le__(self, o):
        return self < o or self == o

    def __gt__(self, o):
        return o < self

    def __ge__(self, o):
        return o <= self

    def __ne__(self, o):
        return not self == o


p, q = Total(1), Total(2)
print(p < q, p <= q, p > q, p >= q, p == q, p != q, p == Total(1), p != Total(1))
print((p < q) == (q > p), [t.v for t in sorted([Total(3), Total(1), Total(2)], reverse=True)])
print(1 < 2.5, 2.5 > 2, "a" < "b", [1, 2] < [1, 3], (1, 2) <= (1, 2), {1} < {1, 2}, None is None)
print(max([Total(1), Total(5)]).v, 1 < True + 1)
print(bool(a), a.__eq__(c), a.__eq__(1), a.__lt__(1) is NotImplemented, a.__ne__(b))
