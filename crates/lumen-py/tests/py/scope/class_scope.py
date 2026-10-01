x = "global"

class A:
    x = "class"
    y = x + "-y"
    def m(self):
        return x
    def n(self):
        return self.x, A.x

a = A()
print(A.y, a.m(), a.n())

class B:
    vals = [1, 2, 3]
    total = sum(vals)
    first = vals[0]
    f = lambda self: B.total
print(B.total, B.first, B().f())

def factory():
    z = "enclosing"
    class C:
        z = "classz"
        def get(self):
            return z
        w = z
    return C
C = factory()
print(C().get(), C.w)

class D:
    x = 1
    def f(self):
        return x
    x = 2
print(D.x, D().f())

class E:
    qq = 1
    del qq
    try:
        print(qq)
    except NameError:
        print("qq deleted")

class F:
    n = 0
    def __init__(self):
        F.n += 1
        self.id = F.n
f1, f2 = F(), F()
print(f1.id, f2.id, F.n, f1.n)
f1.n = 100
print(f1.n, f2.n, F.n)

class G:
    items = []
    def add(self, v):
        self.items.append(v)
g1, g2 = G(), G()
g1.add(1); g2.add(2)
print(G.items, g1.items is g2.items)

class H:
    print("body runs once", __name__)
    name = "H"
    print(name)
    def meth(self): pass
print(sorted(k for k in H.__dict__ if not k.startswith("__")))
print(H.__name__, H.__qualname__, H().meth.__name__)
class I:
    class Inner:
        v = 5
    w = Inner.v + 1
print(I.w, I.Inner.v, I.Inner.__qualname__)
