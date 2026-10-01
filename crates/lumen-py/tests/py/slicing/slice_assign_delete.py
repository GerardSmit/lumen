l = list(range(10))
l[2:5] = ["a", "b"]
print(l)
l[1:1] = [9, 9, 9]
print(l)
l[:2] = []
print(l)
l[-2:] = "xyz"
print(l)
l[100:] = [7]
print(l)
l[:0] = [0]
print(l)
l[1:3] = (5, 6, 7, 8)
print(l)
l[::] = [1, 2, 3]
print(l)
l[len(l):] = [4, 5]
print(l)
l[:] = []
print(l)
l = list(range(10))
l[::2] = ["e"] * 5
print(l)
l[1::2] = "odd!!"
print(l)
l[::-1] = list(range(10))
print(l)
l[8:2:-2] = [-1, -2, -3]
print(l)
l[-1:-4:-1] = [100, 200, 300]
print(l)
for bad in (lambda: l.__setitem__(slice(None, None, 2), [1]), lambda: l.__setitem__(slice(None, None, -1), [1, 2]), lambda: l.__setitem__(slice(1, 3), 5), lambda: l.__setitem__(slice(None, None, 0), []), lambda: l.__setitem__("a", 1)):
    try:
        bad()
    except (ValueError, TypeError) as e:
        print(type(e).__name__)
print(l)
l = list(range(10))
del l[2:5]
print(l)
del l[::2]
print(l)
del l[-1:]
print(l)
del l[:1]
print(l)
del l[5:100]
print(l)
del l[1:1]
print(l)
del l[::-1]
print(l)
l = list(range(10))
del l[::3], l[0]
print(l)
l = list(range(10))
del l[8:2:-2]
print(l)
l = list(range(5))
del l[:]
print(l)
l = list(range(5))
del l[-3]
print(l)
a = [1, 2, 3]
a[1:2] = a
print(a)
b = [1, 2, 3]
b[:] = b[::-1]
print(b)
c = [0] * 5
c[1:4] = [c[0]] * 3
print(c)
n = [[0] * 2 for _ in range(3)]
n[1:2] = [[1, 1], [2, 2]]
print(n)
ba = bytearray(b"hello")
ba[1:3] = b"EY!"
print(ba)
ba[::2] = b"___"
print(ba)
del ba[1:3]
print(ba)
class S:
    def __init__(self):
        self.log = []
    def __setitem__(self, k, v):
        self.log.append(("set", k, v))
    def __delitem__(self, k):
        self.log.append(("del", k))
s = S()
s[1:2] = 5
s[::2] = 6
del s[1:]
del s[::-1]
s[1:2, 3] = 0
print([(op, repr(k) if not isinstance(k, slice) else (k.start, k.stop, k.step)) if False else (op, k.start if isinstance(k, slice) else k, k.stop if isinstance(k, slice) else None) for op, k, *rest in s.log[:4]])
try:
    "abc"[1:2] = "x"
except TypeError:
    print("str immutable")
try:
    (1, 2)[0:1] = (5,)
except TypeError:
    print("tuple immutable")
