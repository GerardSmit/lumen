class V:
    def __init__(self, x):
        self.x = x
    def __repr__(self):
        return "V(%r)" % (self.x,)
    def __add__(self, o):
        if isinstance(o, V):
            return V(self.x + o.x)
        if isinstance(o, int):
            return V(self.x + o)
        return NotImplemented
    def __radd__(self, o):
        print("radd", o)
        return V(o + self.x)
    def __sub__(self, o):
        return V(self.x - (o.x if isinstance(o, V) else o))
    def __rsub__(self, o):
        return V(o - self.x)
    def __mul__(self, o):
        return V(self.x * o)
    __rmul__ = __mul__
    def __neg__(self): return V(-self.x)
    def __pos__(self): return V(+self.x)
    def __abs__(self): return V(abs(self.x))
    def __invert__(self): return V(~self.x)
    def __truediv__(self, o): return V(self.x / o)
    def __floordiv__(self, o): return V(self.x // o)
    def __mod__(self, o): return V(self.x % o)
    def __pow__(self, o): return V(self.x ** o)
    def __rpow__(self, o): return V(o ** self.x)
    def __divmod__(self, o): return (V(self.x // o), V(self.x % o))
    def __lshift__(self, o): return V(self.x << o)
    def __rshift__(self, o): return V(self.x >> o)
    def __and__(self, o): return V(self.x & o)
    def __or__(self, o): return V(self.x | o)
    def __xor__(self, o): return V(self.x ^ o)
    def __index__(self): return self.x
    def __int__(self): return self.x
    def __float__(self): return float(self.x)

a, b = V(7), V(3)
print(a + b, a + 1, 1 + a, sum([V(1), V(2)], V(0)))
print(a - b, 10 - a, a - 1)
print(a * 2, 2 * a, -a, +a, abs(V(-4)), ~a)
print(a / 2, a // 2, a % 4, a ** 2, 2 ** a)
print(divmod(a, 2), a << 1, a >> 1, a & 3, a | 8, a ^ 1)
print([10, 20, 30, 40, 50, 60, 70, 80][a], "abcdefgh"[a])
print(hex(a), bin(b), oct(a), range(a)[-1], int(a), float(a))
print(round(1.5), round(2.5), round(-0.5), round(1.2345, 2))
try:
    a + "s"
except TypeError:
    print("TypeError add")
try:
    "s" + a
except TypeError:
    print("TypeError radd")
try:
    a @ b
except TypeError:
    print("TypeError matmul")
class Sub(V):
    def __radd__(self, o):
        return "Sub.radd"
print(V(1) + Sub(2))
print(divmod(7, 2), divmod(-7, 2), divmod(7, -2), divmod(7.5, 2))
print(-7 // 2, -7 % 3, 7 % -3, 2 ** -1, 2 ** 100)
