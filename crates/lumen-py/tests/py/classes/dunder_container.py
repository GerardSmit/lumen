class Bag:
    def __init__(self, *items):
        self.items = list(items)
    def __repr__(self):
        return "Bag(%s)" % ", ".join(repr(i) for i in self.items)
    def __str__(self):
        return "<bag of %d>" % len(self.items)
    def __len__(self):
        return len(self.items)
    def __getitem__(self, i):
        return self.items[i]
    def __setitem__(self, i, v):
        self.items[i] = v
    def __delitem__(self, i):
        del self.items[i]
    def __contains__(self, v):
        return v in self.items
    def __iter__(self):
        return iter(self.items)
    def __bool__(self):
        return len(self.items) > 1
    def __call__(self, *a):
        return sum(self.items) + sum(a)

b = Bag(1, 2, 3)
print(repr(b), str(b), b, [b])
print(len(b), b[0], b[-1], b[0:2], 2 in b, 9 in b, 9 not in b)
b[1] = 20
print(b, list(b), sum(b), max(b))
del b[0]
print(b, len(b))
print(bool(b), bool(Bag(1)), "T" if Bag() else "F")
print(b(), b(1, 2))
print("%s|%r|{}".format(b) % (b, b))
print(f"{b!r} {b!s}")
x, y = b
print(x, y)
print(list(reversed(Bag(1, 2, 3).items)))

class Seq:
    def __getitem__(self, i):
        if i >= 4:
            raise IndexError
        return i * i
print(list(Seq()), 9 in Seq(), tuple(Seq()))
print([v for v in Seq()])

class Sl:
    def __getitem__(self, k):
        return k
s = Sl()
print(s[1], s[1:2], s[::2], s[1, 2], s[...] is Ellipsis)
print(s[1:2].start, s[1:2].stop, s[1:2].step)
class Len:
    def __len__(self):
        return 0
print(bool(Len()), len(Len()))
class BadLen:
    def __len__(self):
        return -1
try:
    len(BadLen())
except ValueError:
    print("ValueError neg len")
class Plain: pass
try:
    len(Plain())
except TypeError:
    print("TypeError len")
try:
    Plain()[0]
except TypeError:
    print("TypeError getitem")
try:
    Plain()()
except TypeError:
    print("TypeError call")
try:
    iter(Plain())
except TypeError:
    print("TypeError iter")
