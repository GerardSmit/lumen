class Adder:
    def __init__(self, n):
        self.n = n

    def __call__(self, x, *rest):
        return self.n + x + sum(rest)


add5 = Adder(5)
print(add5(1), add5(1, 2, 3), callable(add5), callable(5))
print(list(map(add5, [1, 2])))


class Flag:
    def __init__(self, v):
        self.v = v

    def __bool__(self):
        return self.v


print(bool(Flag(True)), bool(Flag(False)), "yes" if Flag(False) else "no")
print(Flag(True) and "and", Flag(False) or "or", not Flag(False))


class Sized:
    def __init__(self, n):
        self.n = n

    def __len__(self):
        return self.n


print(len(Sized(3)), bool(Sized(0)), bool(Sized(2)))


class Both:
    def __bool__(self):
        return True

    def __len__(self):
        return 0


print(bool(Both()), len(Both()))


class Seq:
    def __init__(self, *items):
        self.items = list(items)

    def __getitem__(self, i):
        print("getitem", i)
        return self.items[i]


for x in Seq("a", "b"):
    print("iter", x)
print(list(Seq(1, 2)))
print(2 in Seq(1, 2, 3))
print(max(Seq(3, 9, 4)))
a, b = Seq("p", "q")
print(a, b)


class Iter:
    def __init__(self, n):
        self.n = n

    def __iter__(self):
        return IterState(self.n)

    def __contains__(self, x):
        print("contains", x)
        return x == 42


class IterState:
    def __init__(self, n):
        self.i = 0
        self.n = n

    def __iter__(self):
        return self

    def __next__(self):
        if self.i >= self.n:
            raise StopIteration
        self.i += 1
        return self.i


it = Iter(3)
print(list(it), list(it), sum(it))
print(42 in it, 7 in it, 7 not in it)


class Rep:
    def __repr__(self):
        return "Rep()"


class Str(Rep):
    def __str__(self):
        return "Str!"


print(repr(Rep()), str(Rep()), repr(Str()), str(Str()))
print([Str(), Rep()])
print("%s %r" % (Str(), Str()))
print(f"{Str()} {Str()!r}")
print(format(Str(), ""), "{}".format(Rep()))


class Item:
    def __init__(self):
        self.d = {}

    def __getitem__(self, k):
        return self.d[k]

    def __setitem__(self, k, v):
        self.d[k] = v

    def __delitem__(self, k):
        del self.d[k]

    def __contains__(self, k):
        return k in self.d


m = Item()
m["a"] = 1
m[1, 2] = "tuple key"
print(m["a"], m[1, 2], "a" in m, "z" in m)
del m["a"]
try:
    m["a"]
except KeyError as e:
    print("KeyError", e)


class Eq:
    def __init__(self, v):
        self.v = v

    def __eq__(self, o):
        return isinstance(o, Eq) and self.v == o.v

    def __hash__(self):
        return hash(self.v)

    def __lt__(self, o):
        return self.v < o.v


print(Eq(1) == Eq(1), Eq(1) != Eq(2), Eq(1) == 1)
print(len({Eq(1), Eq(1), Eq(2)}), sorted([Eq(3), Eq(1)])[0].v)


class NoHash:
    def __eq__(self, o):
        return True


try:
    hash(NoHash())
except TypeError:
    print("unhashable")


class Num:
    def __init__(self, v):
        self.v = v

    def __add__(self, o):
        return Num(self.v + (o.v if isinstance(o, Num) else o))

    def __radd__(self, o):
        return Num(o + self.v)

    def __neg__(self):
        return Num(-self.v)

    def __int__(self):
        return self.v

    def __repr__(self):
        return "Num(%d)" % self.v


print(Num(1) + Num(2), Num(1) + 5, 5 + Num(1), -Num(3), int(Num(7)), sum([Num(1), Num(2)]))
