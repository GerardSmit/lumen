d = {}
d["z"] = 1
d["a"] = 2
d["m"] = 3
print(d, list(d), list(d.values()))
d["z"] = 100
print(d)
del d["a"]
d["a"] = 0
print(d)
print({"b": 1, "a": 2} == {"a": 2, "b": 1}, list({"b": 1, "a": 2}) == list({"a": 2, "b": 1}))
print(list(reversed(d)), list(reversed(d.items())), list(reversed(d.values())), list(reversed(d.keys())))
k = d.keys()
v = d.values()
it = d.items()
print(k, v, it, len(k), len(v), len(it))
d["new"] = 7
print(k, v, it, len(k))
del d["z"]
print(list(k), list(v), list(it))
print("m" in k, "zz" in k, 7 in v, ("m", 3) in it, ("m", 4) in it)
a = {"a": 1, "b": 2, "c": 3}
b = {"b": 20, "c": 30, "d": 40}
print(sorted(a.keys() & b.keys()), sorted(a.keys() | b.keys()), sorted(a.keys() - b.keys()), sorted(a.keys() ^ b.keys()))
print(sorted(a.keys() & {"a", "z"}), sorted(a.keys() | ["q"]), sorted(a.keys() - ["a"]), a.keys() & [])
print(sorted(a.items() & {("a", 1), ("b", 9)}), sorted(a.items() | {("q", 0)}), sorted(a.items() - {("a", 1)}))
print(a.keys() == {"a", "b", "c"}, a.keys() <= {"a", "b", "c", "d"}, a.keys() < {"a", "b", "c"}, a.keys() >= {"a"}, a.keys().isdisjoint({"x"}), a.keys().isdisjoint(["a"]))
print(a.values() == a.values(), list(a.values()) == [1, 2, 3], a.items() == dict(a).items())
print(a | b, b | a, a, b)
a |= {"z": 26, "a": -1}
print(a)
a |= [("y", 25)]
print(a)
print({**a, **b}, {**b, **a}, {**{"x": 1}, "y": 2, "x": 3})
print(dict(a, a=0) | {"n": 1})
m = {"k": 1}
m.update(m)
print(m, list(m.items())[0], next(iter(m)), next(iter(m.values())))
big = {i: i * i for i in range(10)}
for i in range(0, 10, 2):
    del big[i]
big[0] = "back"
print(big, list(big)[-1], list(big)[0])
print(list(dict.fromkeys("hello")), "".join(dict.fromkeys("mississippi")))
seen = {}
for ch in "abracadabra":
    seen.setdefault(ch, len(seen))
print(seen)
print(list(zip(d, d.values())), list(enumerate(d)), sorted(d.items()), dict(sorted(d.items(), reverse=True)))
print(list(map(str, d)), [k for k in d if k > "m"], {k: v for k, v in d.items() if v})
print(popped := d.popitem(), d, d.popitem(), d)
d2 = dict(x=1)
d2["y"] = 2
d2.pop("x")
d2["x"] = 3
print(d2, list(d2.items()))
print(type(k).__name__, type(v).__name__, type(it).__name__, repr(type(iter(d))).count("dict"))
print(list(iter({1: "a", 2: "b"})), list(iter({1: "a", 2: "b"}.items())))
try:
    reversed({}.keys())
    print("reversed ok")
except TypeError as e:
    print(type(e).__name__)
try:
    k[0]
except TypeError as e:
    print(type(e).__name__, e)
