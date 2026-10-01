class K:
    def __init__(self, v):
        self.v = v
    def __hash__(self):
        return hash(self.v) % 3
    def __eq__(self, o):
        return isinstance(o, K) and self.v == o.v
    def __repr__(self):
        return "K%d" % self.v
d = {}
for i in range(10):
    d[K(i)] = i
print(len(d), d[K(7)], K(3) in d, K(30) in d, list(d.values()))
d[K(3)] = "three"
print(len(d), d[K(3)])
del d[K(0)]
print(list(d)[:4], len(d))
class NoHash:
    __hash__ = None
try:
    {NoHash(): 1}
except TypeError:
    print("TypeError NoHash")
class Eq:
    def __eq__(self, o):
        return True
try:
    {Eq(): 1}
except TypeError:
    print("TypeError Eq without hash")
class Id:
    pass
a, b = Id(), Id()
m = {a: 1, b: 2}
print(len(m), m[a], m[b], a in m, Id() in m)
print({1: 1} == {1.0: 1.0}, {1: "a"}.get(1.0), {True: "x"}[1], {0: "z"}[False], {(1, 2): 3}[(1, 2)], {(1, (2, 3)): 4}[(1, (2, 3))], {frozenset([1]): 5}[frozenset([1])] if False else 5)
print({"a": 1, "b": 2} == {"b": 2, "a": 1}, {"a": [1]} == {"a": [1]}, {"a": 1} == {"a": 1, "b": 2}, {"a": 1} < {"a": 1} if False else "no ordering")
try:
    {"a": 1} < {"a": 2}
except TypeError:
    print("TypeError dict order")
n = float("nan")
dn = {n: 1}
print(n in dn, dn.get(n), len({n: 1, float("nan"): 2}), list({n: 1}.values()))
print({0.0: "a"}[-0.0], {1: "a", 2: "b"}.get(1 + 0j) if False else "ok", {2 ** 70: "big"}[2 ** 70], {"é": 1}["é".replace("é", "é")])
print(len({1, 1.0, True}), len({"a": 1, "a": 2}), {"a": 1, "a": 2}, {1: 1, 1: 2}["a" == "a"])
print(dict([(1, 2), (1, 3)]), dict(a=1, **{"b": 2}), dict({1: 2}.items()), dict([("a", 1)], a=2))
for bad in (lambda: dict(5), lambda: dict([1]), lambda: dict([(1, 2, 3)]), lambda: dict(1, 2), lambda: dict.fromkeys()):
    try:
        bad()
    except (TypeError, ValueError) as e:
        print(type(e).__name__)
x = {"k": 1}
y = x
y["j"] = 2
print(x, x is y, dict(x) is x, dict(x) == x)
t = {"t": (1, [2])}
print(t, t["t"][1].append(3), t)
print(list({3: 0, 1: 0, 2: 0}), sorted({3: 0, 1: 0, 2: 0}), list(dict.fromkeys([3, 1, 3, 2, 1])))
print({k: v for k, v in sorted({"b": 1, "a": 2, "c": 0}.items(), key=lambda kv: kv[1])})
print(len({i % 7: i for i in range(100)}), {i % 3: i for i in range(10)})
dd = {}
dd[1] = dd[2] = "same"
print(dd, {1: 2}.get(1))
