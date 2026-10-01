s = slice(1, 5, 2)
print(s.start, s.stop, s.step)
print(slice(5).start, slice(5).stop, slice(5).step)
print(slice(1, 2).step, slice(None, None, None).start)
print(s == slice(1, 5, 2), s == slice(1, 5), s != slice(0, 5, 2))
L = list(range(10))
print(L[s], L[slice(None)], L[slice(None, None, -1)], L[slice(-3, None)])
print(slice(1, 10, 2).indices(5))
print(slice(None).indices(10))
print(slice(None, None, -1).indices(10))
print(slice(-3, None).indices(10))
print(slice(2, 100).indices(10))
print(slice(-100, 100).indices(10))
print(slice(5, 1, -1).indices(10))
print(slice(None, None, -2).indices(0))
print(slice(0, 3).indices(0))
print(list(range(*slice(1, 8, 3).indices(10))))
try:
    slice(None, None, 0).indices(5)
except ValueError as e:
    print(type(e).__name__)
try:
    slice(1, 2).indices(-1)
except ValueError as e:
    print(type(e).__name__)
print(repr(slice(1, 2, 3)), repr(slice(4)), repr(slice(None, 2)))


class Probe:
    def __getitem__(self, key):
        if isinstance(key, slice):
            return ("slice", key.start, key.stop, key.step)
        if isinstance(key, tuple):
            return ("tuple", tuple(type(k).__name__ for k in key))
        return ("item", key)


p = Probe()
print(p[3])
print(p[1:2])
print(p[:])
print(p[::2])
print(p[1:2:3])
print(p[-1:])
print(p[:, 1])
print(p[1:2, ::3])
print(p[1, 2])
print(p[...])
print(p[..., 1])
print(p[1:2, ...])
print(p["k"])
print(p[None])
print(p[slice(1, 2)])
print(Ellipsis, ... is Ellipsis, type(...).__name__, repr(...))
print(bool(...))


class Seq:
    def __init__(self, data):
        self.data = data

    def __getitem__(self, key):
        if isinstance(key, slice):
            return Seq(self.data[key])
        return self.data[key]

    def __len__(self):
        return len(self.data)

    def __repr__(self):
        return "Seq(%r)" % (self.data,)


q = Seq([1, 2, 3, 4, 5])
print(q[1:3], q[::-1], q[2], q[-1], len(q[::2]))
print(list(q), [x for x in q])
