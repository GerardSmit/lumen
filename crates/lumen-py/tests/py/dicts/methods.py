d = {"a": 1, "b": 2}
print(d.get("a"), d.get("z"), d.get("z", 0), d.get("a", 99), d["b"], len(d))
print(d.setdefault("a", 100), d.setdefault("c", 3), d.setdefault("d"), d)
print(d.pop("c"), d.pop("zz", "dflt"), d.pop("d"), d)
d["e"] = 5
print(d.popitem(), d.popitem(), d)
d.update({"x": 1, "y": 2})
d.update(z=3)
d.update([("w", 0), ("x", 10)])
print(d, sorted(d))
print(list(d.items()), list(d.keys()), list(d.values()))
c = d.copy()
c["new"] = 1
print("new" in d, "new" in c, d == c, d is c)
print(dict.fromkeys("abc"), dict.fromkeys([1, 2], 0), dict.fromkeys(range(3), "v"), dict.fromkeys([]))
shared = dict.fromkeys("ab", [])
shared["a"].append(1)
print(shared)
print(dict(a=1, b=2), dict([("a", 1)]), dict({"a": 1}, b=2), dict(zip("ab", [1, 2])), dict())
print({}, {1: "a"}, {"k": [1, {"n": None}]}, {(1, 2): "t"})
print(len({}), bool({}), bool({0: 0}), "a" in d, "q" not in d, 10 in {10: 0})
d = {"a": 1}
d["b"] = 2
d["a"] = 10
del d["b"]
print(d)
d.clear()
print(d, len(d))
counts = {}
for ch in "mississippi":
    counts[ch] = counts.get(ch, 0) + 1
print(counts, sorted(counts.items(), key=lambda kv: (-kv[1], kv[0])))
groups = {}
for w in ["apple", "avocado", "banana", "blueberry", "cherry"]:
    groups.setdefault(w[0], []).append(w)
print(groups)
inv = {v: k for k, v in {"a": 1, "b": 2}.items()}
print(inv)
print(max({"a": 3, "b": 9}, key=lambda k: {"a": 3, "b": 9}[k]), sum({1: 5, 2: 6}), sorted({"b": 1, "a": 2}.values()))
print(dict.get({"a": 1}, "a"), {}.get(None), {None: 1}.get(None), {"a": None}.get("a", 5))
x = {"a": 1}
print(x.update({"b": 2}), x.setdefault("c", 3), x.pop("c"), x)
print(x.items() == {"a": 1, "b": 2}.items(), dict(x) == x, dict(x) is x)
for f in (lambda: {}.pop("k"), lambda: {}.popitem(), lambda: {}["k"], lambda: dict([1]), lambda: dict([(1, 2, 3)]), lambda: {}.get(), lambda: dict(1)):
    try:
        f()
    except (KeyError, TypeError, ValueError) as e:
        print(type(e).__name__, e if isinstance(e, KeyError) else "")
try:
    {1: 2}.update(5)
except TypeError as e:
    print(type(e).__name__)
try:
    del x["nope"]
except KeyError as e:
    print(type(e).__name__, e, e.args)
