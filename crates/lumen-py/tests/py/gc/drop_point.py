import weakref
log = []
class A:
    def __init__(self, n): self.n = n
    def __del__(self): log.append(self.n)

x = A(1); del x; assert log == [1], log; log.clear()
x = A(2); x = None; assert log == [2], log; log.clear()
def f():
    a = A(3)
f(); assert log == [3], log; log.clear()
l = [A(5)]; l.pop(); assert log == [5], log; log.clear()
d = {1: A(7)}; del d[1]; assert log == [7], log; log.clear()
l = [A(10)]; l[0] = 0; assert log == [10], log; log.clear()
class H: pass
h = H(); h.a = A(11); del h.a; assert log == [11], log; log.clear()
h.a = A(12); h.a = 1; assert log == [12], log; log.clear()
o = A(17); w = weakref.ref(o, lambda r: log.append("cb")); del o; assert log == [17, "cb"], log; log.clear()
l = [A(19)]; l = None; assert log == [19], log; log.clear()
t = (A(21),); t = None; assert log == [21], log; log.clear()
def m():
    A(22)
    assert log == [22], log
m(); log.clear()
def n():
    a = A(25)
    del a
    assert log == [25], log
n(); log.clear()
class B:
    def __init__(self): self.a = A(23)
b = B(); del b; assert log == [23], log; log.clear()
s = {A(15)}; s.clear(); assert log == [15], log; log.clear()
print("done")
