d = {}
d["z"] = 1
d["a"] = 2
d["m"] = 3
print(d, list(d), list(d.values()), list(d.items()), len(d))
d["z"] = 10
print(d)
del d["a"]
d["a"] = 20
print(d, list(d))
print(d["z"], d.get("z"), d.get("q"), d.get("q", "dflt"), "z" in d, "q" in d, "q" not in d, 10 in d.values())
try:
    d["q"]
except KeyError as e:
    print("KeyError", e, e.args)
try:
    del d["q"]
except KeyError:
    print("KeyError del")
print({1: "a", 1.0: "b", True: "c"}, {1: 1, 2: 2} == {2: 2, 1: 1}, {} == {}, {"a": 1} != {"a": 2}, {"a": 1} == {"a": 1.0})
print(dict(a=1, b=2), dict([("x", 1), ("y", 2)]), dict({"k": "v"}, z=1), dict(zip("ab", [1, 2])), dict(), dict(**{"a": 1}), dict([[1, 2], [3, 4]]))
print({"a": 1, **{"b": 2}, **{"a": 3}}, {**{}}, {"x": 1} | {"y": 2}, {"x": 1} | {"x": 2})
e = {"x": 1}
e |= {"y": 2}
e |= [("z", 3)]
print(e)
print(dict.fromkeys("abc"), dict.fromkeys([1, 2], 0), dict.fromkeys([], 1), dict.fromkeys("aa", []))
keys = {(1, 2): "t", "s": "str", 3: "int", None: "none", 2.5: "f", (): "empty", True: "bool"}
print(keys[(1, 2)], keys["s"], keys[3], keys[None], keys[2.5], keys[()], keys[1], len(keys))
for bad in ([1], {1: 2}, {1}, [[]]):
    try:
        {bad: 1}
    except TypeError:
        print("unhashable", type(bad).__name__)
big = {}
for i in range(100):
    big[i * 7 % 101] = i
print(list(big)[:12], len(big), big[7], sum(big.values()), min(big), max(big))
for i in range(0, 100, 2):
    big.pop(i * 7 % 101, None)
print(len(big), list(big)[:8])
nested = {"a": {"b": {"c": 1}}, "l": [1, {"k": 2}]}
print(nested["a"]["b"]["c"], nested["l"][1]["k"], nested)
nested["a"]["b"]["d"] = 2
print(nested)
c = nested.copy()
c["new"] = 1
c["a"]["b"]["c"] = 99
print(nested, "new" in nested)
print(len({}), bool({}), bool({"a": 0}), {"a": 1}.__len__(), type({}).__name__, {1: 2}.__contains__(1), {1: 2}.__getitem__(1))
print(sorted({"b": 1, "a": 2}), sorted({"b": 1, "a": 2}.items()), max({"b": 1, "a": 2}), list(reversed({"a": 1, "b": 2})), dict(reversed(list({"a": 1, "b": 2}.items()))))
print({k: v for k, v in zip("abc", range(3)) if v}, {v: k for k, v in {"a": 1, "b": 2}.items()}, {i: i * i for i in range(4)}, {c: c.upper() for c in "ab"})
print(str({"a": "b"}), repr({"a": 1.5, 2: [1], (1,): None}), {"a": {"b": ()}}, {1: {2: {3: {}}}})
