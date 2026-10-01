d = {"a": 1, "b": 2, "c": 3}
print(d.setdefault("a", 100), d.setdefault("z", 26), d.setdefault("n"), d)
print(d.pop("a"), d.pop("zz", "none"), d.pop("n", 1), d)
try:
    d.pop("nope")
except KeyError as e:
    print("KeyError", e)
print(d.popitem(), d.popitem(), d)
try:
    {}.popitem()
except KeyError:
    print("KeyError popitem")
d = {"a": 1, "b": 2}
d.update({"b": 20, "c": 30})
d.update(x=1, y=2)
d.update([("p", 1), ("q", 2)])
d.update()
print(d)
k, v, it = d.keys(), d.values(), d.items()
print(k, v, it, len(k), len(v), len(it))
print(type(k).__name__, type(v).__name__, type(it).__name__)
d["new"] = "N"
print(list(k)[-1], list(v)[-1], list(it)[-1], "new" in k, ("new", "N") in it, "N" in v)
print(k == {"a", "b", "c", "x", "y", "p", "q", "new"}, k & {"a", "zz"}, sorted(k | {"zz"}), sorted(k - {"a"}), sorted(k ^ {"a", "zz"}))
print(it & {("a", 1)}, sorted(it | {("e", 5)})[-1], d.keys() & d.keys() == d.keys() if False else "skip", d.keys() == d.keys(), d.keys() <= d.keys(), {"a"} < d.keys())
print(list(reversed(d.keys()))[:2], list(reversed(d.values()))[:2], list(reversed(d.items()))[:1])
print(sorted(d.items(), key=lambda kv: str(kv[1]))[:3], [k for k, v in d.items() if v == 1], max(d.values(), key=str), min(d, key=len))
e = {"x": 1}
print(e.get("x"), e.get("y"), e.get("y", []), e.copy() == e, e.copy() is e)
e.clear()
print(e, len(e))
d = {i: i * i for i in range(5)}
try:
    for key in d:
        if key == 2:
            d[100] = 0
except RuntimeError as ex:
    print("RuntimeError", ex)
try:
    for key in d:
        del d[key]
except RuntimeError as ex:
    print("RuntimeError", ex)
d = {i: i for i in range(5)}
for key in list(d):
    if key % 2:
        del d[key]
print(d)
for key in d:
    d[key] += 1
print(d)
d = {"a": 1}
for key in d:
    d[key] = 2
    d["a"] = 3
print(d)
d = {1: "a", 2: "b"}
vi = iter(d.values())
print(next(vi), next(vi))
try:
    next(vi)
except StopIteration:
    print("StopIteration")
cnt = {}
for ch in "mississippi":
    cnt[ch] = cnt.get(ch, 0) + 1
print(cnt, sorted(cnt.items(), key=lambda kv: (-kv[1], kv[0])))
groups = {}
for w in ["apple", "avocado", "banana", "blueberry", "cherry"]:
    groups.setdefault(w[0], []).append(w)
print(groups)
inv = {}
for kk, vv in {"a": 1, "b": 1, "c": 2}.items():
    inv.setdefault(vv, []).append(kk)
print(inv)
print(dict(sorted({"b": 1, "a": 2}.items())), {**{"a": 1}, "a": 2}, list(zip(d.keys(), d.values())) == list(d.items()))
