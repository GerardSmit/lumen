d = {"a": 1, "b": 2, "c": 3}
try:
    for k in d:
        d[k + "x"] = 0
except RuntimeError as e:
    print(type(e).__name__, e)
print(len(d) >= 4)
d = {"a": 1, "b": 2, "c": 3}
try:
    for k in d:
        del d[k]
except RuntimeError as e:
    print(type(e).__name__, e)
print(d)
d = {"a": 1, "b": 2, "c": 3}
try:
    for k, v in d.items():
        d.pop(k)
except RuntimeError as e:
    print(type(e).__name__, e)
d = {"a": 1, "b": 2, "c": 3}
try:
    for v in d.values():
        d["new"] = v
except RuntimeError as e:
    print(type(e).__name__, e)
d = {"a": 1, "b": 2, "c": 3}
for k in d:
    d[k] = d[k] * 10
print(d)
for k in list(d):
    if d[k] > 10:
        del d[k]
print(d)
d = {"a": 1, "b": 2, "c": 3}
d = {k: v for k, v in d.items() if v != 2}
print(d)
d = {"a": 1}
for k in d:
    d[k] = 2
    d["a"] = 3
print(d)
d = {"a": 1, "b": 2}
it = iter(d)
print(next(it))
d["c"] = 3
try:
    next(it)
except RuntimeError as e:
    print(type(e).__name__, e)
d = {"a": 1, "b": 2}
it = iter(d)
d["a"] = 5
print(next(it), next(it))
d = {"a": 1}
it = iter(d)
d.clear()
try:
    next(it)
except RuntimeError as e:
    print(type(e).__name__, e)
except StopIteration:
    print("StopIteration")
d = {"a": 1, "b": 2}
for k in d:
    d.pop(k)
    break
print(d)
d = {1: 1, 2: 2, 3: 3}
try:
    for k in d:
        d.pop(k)
        d[k + 10] = 1
except RuntimeError as e:
    print(type(e).__name__, e)
for f in (lambda: {}["missing"], lambda: {1: 2}[2], lambda: {"a": 1}.pop("b"), lambda: {}.popitem(), lambda: {(1, 2): 3}[(2, 1)]):
    try:
        f()
    except KeyError as e:
        print("KeyError", e)
e = {}
try:
    e["x"]
except KeyError as ex:
    print(repr(ex), ex.args, str(ex))
try:
    {}.pop("k")
except KeyError as ex:
    print(ex.args)
try:
    {}.popitem()
except KeyError as ex:
    print(ex)
try:
    del {}["a"]
except KeyError as ex:
    print(ex)
class Missing(dict):
    def __missing__(self, key):
        return key * 2
m = Missing(a=1)
print(m["a"], m["zz"], m.get("zz"), len(m), "zz" in m)
class Counter(dict):
    def __missing__(self, key):
        self[key] = 0
        return 0
c = Counter()
for ch in "abca":
    c[ch] += 1
print(c)
try:
    {}.update(1)
except TypeError as ex:
    print(type(ex).__name__)
try:
    dict(a=1).update({1: 2}, 5)
except TypeError as ex:
    print(type(ex).__name__)
try:
    {}.update([(1, 2, 3)])
except ValueError as ex:
    print(type(ex).__name__, ex)
try:
    {}.update([1])
except TypeError as ex:
    print(type(ex).__name__)
