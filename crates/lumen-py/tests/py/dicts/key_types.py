d = {1: "int", 1.0: "float", True: "bool"}
print(d, len(d), d[1], d[1.0], d[True])
d = {0: "a", False: "b", 0.0: "c", -0.0: "d"}
print(d, len(d))
d = {2: "x"}
d[2.0] = "y"
d[True + 1] = "z"
print(d, list(d), type(next(iter(d))).__name__)
d = {True: 1}
d[1] = 2
print(d, type(next(iter(d))).__name__)
d = {1.5: "a", 1.5 + 0: "b", 3 / 2: "c"}
print(d)
print({(1, 2): "t", (1, 2.0): "u", (True, 2): "v"}, {(): 0, ((),): 1})
print({"a": 1, "a": 2}, {1: "x", 1: "y"})
k = (1, "a", (2.5, None))
dd = {k: "v"}
print(dd[(1, "a", (2.5, None))], dd.get((1, "a", (2.5, 0))), k in dd)
print({None: 1, True: 2, "s": 3, 4: 4, 5.5: 5, (6,): 6, b"b": 7, frozenset([8]): 8})
print({frozenset([1, 2]): "fs"}[frozenset([2, 1])], {b"a": 1}[b"a"], {"é": 1}["é"])
print(len({1, 1.0, True, 2, 2.0}), {1, 1.0, True}, {0, False, 0.0}, len({"a", "a"}))
print({i: 1 for i in (1, 1.0, True, 2, 2.0)})
print(1 == 1.0 == True, hash(1) == hash(1.0) == hash(True), hash(2) == hash(2.0), hash(0) == hash(False) == hash(0.0), hash(-1) == hash(-2))
print(hash(1.5) == hash(1.5 + 0), hash((1, 2)) == hash((1, 2)), hash(()) == hash(()), hash(None) == hash(None))
nan = float("nan")
m = {nan: 1}
m[nan] = 2
print(len(m), m[nan], nan in m, float("nan") in m)
m2 = {float("nan"): 1, float("nan"): 2}
print(len(m2))
big = 2 ** 64
print({big: "b", float(big): "f"}, {10 ** 20: 1}[10 ** 20], {2 ** 100: "x"}.get(2 ** 100))
class Key:
    def __init__(self, v):
        self.v = v
    def __hash__(self):
        return hash(self.v)
    def __eq__(self, other):
        return isinstance(other, Key) and self.v == other.v
    def __repr__(self):
        return f"K{self.v}"
kd = {Key(1): "a", Key(2): "b"}
kd[Key(1)] = "c"
print(kd, Key(2) in kd, Key(3) in kd, kd[Key(1)])
class Bare:
    pass
b1, b2 = Bare(), Bare()
bd = {b1: 1, b2: 2}
print(len(bd), bd[b1], bd[b2], b1 in bd, Bare() in bd)
class Unh:
    __hash__ = None
for bad in ([1], {1: 2}, {1}, [[]], ([],), {"k": []}, Unh(), (1, [2]), bytearray(b"x")):
    try:
        {bad: 1}
    except TypeError as e:
        print(type(e).__name__)
try:
    d = {}
    d[[1, 2]] = 3
except TypeError as e:
    print(type(e).__name__)
try:
    {}.get([1])
except TypeError as e:
    print(type(e).__name__)
try:
    [1] in {}
except TypeError as e:
    print(type(e).__name__)
try:
    {}.setdefault({}, 1)
except TypeError as e:
    print(type(e).__name__)
try:
    {1: 2}[[1]]
except TypeError as e:
    print(type(e).__name__)
try:
    hash([1])
except TypeError as e:
    print(type(e).__name__, e)
try:
    {1: 2}[3]
except KeyError as e:
    print(type(e).__name__, e, e.args)
try:
    {(1, 2): 0}[(1, 3)]
except KeyError as e:
    print(type(e).__name__, e)
try:
    {"s": 0}["t"]
except KeyError as e:
    print(type(e).__name__, e)
