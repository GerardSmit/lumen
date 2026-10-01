class K:
    def __init__(self, n, tag=""):
        self.n = n
        self.tag = tag

    def __hash__(self):
        return self.n % 3

    def __eq__(self, other):
        return isinstance(other, K) and self.n == other.n

    def __repr__(self):
        return "K(%d%s)" % (self.n, self.tag)


a, b, c = K(1), K(1, "x"), K(4)
print(hash(a) == hash(b), hash(a) == hash(c), a == b, a == c, a is b)
s = {a, b, c}
print(len(s), sorted(k.n for k in s))
d = {a: "first"}
d[b] = "second"
print(len(d), d[a], d[K(1)], list(d.values()))
print(K(1) in s, K(7) in s, K(4) in {a})
d[c] = "c"
d[K(7)] = "seven"
print(len(d), sorted(k.n for k in d), d[K(7)], d[K(4)])
del d[K(1)]
print(sorted(k.n for k in d))
print(s == {K(1), K(4)}, K(1) in [K(1)], [K(2)].index(K(2)), [K(1), K(2)].count(K(2)))
print(hash(K(3)), hash(K(5)), hash(K(300)))


class NoHash:
    def __init__(self, n):
        self.n = n

    def __eq__(self, other):
        return self.n == other.n


nh = NoHash(1)
print(NoHash.__hash__)
for op in (lambda: hash(nh), lambda: {nh}, lambda: {nh: 1}, lambda: {}.get(nh), lambda: nh in {1}):
    try:
        op()
    except TypeError as e:
        print(type(e).__name__)
print(nh == NoHash(1), nh != NoHash(2), nh in [NoHash(1)])


class Explicit(NoHash):
    __hash__ = object.__hash__


print(len({Explicit(1), Explicit(1)}))


class Inherit(K):
    pass


print(hash(Inherit(5)), Inherit(5) == K(5), len({Inherit(5), K(5)}))


class Override(K):
    def __eq__(self, other):
        return True


try:
    hash(Override(5))
except TypeError as e:
    print(type(e).__name__)


class Unhashable:
    __hash__ = None


try:
    {Unhashable()}
except TypeError as e:
    print(type(e).__name__)


class Const:
    def __hash__(self):
        return 7


print(len({Const() for _ in range(5)}), hash(Const()))


class Eq:
    def __eq__(self, other):
        return True

    def __hash__(self):
        return 1


print(len({Eq(), Eq(), 5}), Eq() == 5, 5 == Eq(), Eq() in [1, 2], 3 in [Eq()])


class BigHash:
    def __hash__(self):
        return 2**70


class NegHash:
    def __hash__(self):
        return -1


print(hash(BigHash()) == hash(2**70), hash(NegHash()))
print(len({1, 1.0, True, K(1)}), len({1: 1, 1.0: 2, True: 3}), {1: 1, 1.0: 2, True: 3})


class BadHash:
    def __hash__(self):
        return "x"


try:
    hash(BadHash())
except TypeError as e:
    print(type(e).__name__)
