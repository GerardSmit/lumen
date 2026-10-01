class A:
    def __init__(self, v):
        self.v = v

    def __repr__(self):
        return "%s(%r)" % (type(self).__name__, self.v)

    def __add__(self, o):
        print("A.__add__", type(o).__name__)
        return A(self.v + (o.v if hasattr(o, "v") else o))

    def __radd__(self, o):
        print("A.__radd__", type(o).__name__)
        return A((o.v if hasattr(o, "v") else o) + self.v)

    def __rsub__(self, o):
        print("A.__rsub__", type(o).__name__)
        return A(o - self.v)

    def __sub__(self, o):
        print("A.__sub__")
        return NotImplemented


class B(A):
    def __radd__(self, o):
        print("B.__radd__", type(o).__name__)
        return B(99)

    def __rsub__(self, o):
        print("B.__rsub__", type(o).__name__)
        return B(-1)


print(A(1) + A(2))
print(A(1) + 5)
print(5 + A(1))
print(10 - A(3))
print(A(1) + B(2))
print(B(1) + A(2))
print(A(1) - B(2))
print(A(1) - 5 if False else "skip")
print(sum([A(1), A(2)], A(0)))
print(sum([A(1), A(2)]))
try:
    A(1) - A(2)
except TypeError as e:
    print(type(e).__name__)
try:
    A(1) * 2
except TypeError as e:
    print(type(e).__name__)
try:
    2 * A(1)
except TypeError as e:
    print(type(e).__name__)
try:
    [] + A(1)
except TypeError as e:
    print(type(e).__name__)


class Acc:
    def __init__(self):
        self.items = []

    def __iadd__(self, o):
        print("Acc.__iadd__")
        self.items.append(o)
        return self

    def __add__(self, o):
        print("Acc.__add__")
        r = Acc()
        r.items = self.items + [o]
        return r


x = Acc()
y = x
x += 1
print(x is y, x.items)
x = x + 2
print(x is y, x.items, y.items)


class N:
    def __init__(self, v):
        self.v = v

    def __repr__(self):
        return "N(%r)" % (self.v,)

    def __neg__(self):
        return N(-self.v)

    def __pos__(self):
        return N(+self.v + 1000)

    def __invert__(self):
        return N(~self.v)

    def __abs__(self):
        return N(abs(self.v))

    def __bool__(self):
        print("bool")
        return self.v != 0

    def __len__(self):
        print("len")
        return 99

    def __contains__(self, item):
        return item == self.v

    def __index__(self):
        return self.v

    def __matmul__(self, o):
        return ("matmul", self.v, o)

    def __rmatmul__(self, o):
        return ("rmatmul", o, self.v)

    def __divmod__(self, o):
        return ("divmod", self.v, o)

    def __rdivmod__(self, o):
        return ("rdivmod", o, self.v)

    def __pow__(self, o, mod=None):
        return ("pow", self.v, o, mod)

    def __rpow__(self, o):
        return ("rpow", o, self.v)

    def __floordiv__(self, o):
        return ("fd", self.v, o)

    def __mod__(self, o):
        return ("mod", self.v, o)

    def __lshift__(self, o):
        return ("lshift", self.v, o)

    def __and__(self, o):
        return ("and", self.v, o)

    def __xor__(self, o):
        return ("xor", self.v, o)

    def __or__(self, o):
        return ("or", self.v, o)


n = N(3)
print(-n, +n, ~n, abs(N(-4)))
print(bool(N(0)), bool(N(2)), not N(0))
print(5 in n, 3 in n, 3 not in n)
print([10, 20, 30, 40][n], "abcdef"[N(2)], list(range(10))[N(1):N(4)])
print(hex(N(255)), bin(N(5)), oct(N(8)), range(N(3)))
print(n @ 2, 2 @ n)
print(divmod(n, 2), divmod(2, n))
print(pow(n, 2), pow(n, 2, 5), n ** 2, 2 ** n)
print(n // 2, n % 2, n << 1, n & 1, n ^ 1, n | 1)
print(len(N(1)))
print("x" if N(0) else "y", "x" if N(1) else "y")
