f1 = frozenset([1, 2, 3])
f2 = frozenset([3, 2, 1])
print(f1 == f2, hash(f1) == hash(f2))
d = {f1: "a"}
print(d[f2], f2 in d)
d[frozenset()] = "empty"
print(d[frozenset([])], len(d))

outer = {frozenset([1]), frozenset([1]), frozenset([2])}
print(len(outer))
print(sorted(sorted(x) for x in outer))
print(frozenset([1]) in outer, {1} == frozenset([1]), frozenset([1]) == {1})
print(hash(frozenset()) == hash(frozenset([])))

sq = {x % 5 for x in range(100)}
print(sorted(sq))
pairs = {(i % 3, i % 2) for i in range(12)}
print(sorted(pairs))
print(sorted({(x, y) for x in range(3) for y in range(x)}))

s = {10, 20, 30}
try:
    s.remove(99)
except KeyError as e:
    print(type(e).__name__, e)
s.discard(99)
print(sorted(s))
try:
    set().pop()
except KeyError as e:
    print(type(e).__name__)
one = {7}
print(one.pop(), len(one))
tmp = {1, 2, 3}
got = sorted([tmp.pop(), tmp.pop(), tmp.pop()])
print(got, len(tmp))

fs = frozenset({1, 2})
print(sorted(fs | {3}), sorted(fs & {2, 5}), sorted(fs - {1}), sorted(fs ^ {2, 3}))
print(type(fs | {3}).__name__)
print(fs.issubset({1, 2, 3}), fs <= frozenset({1, 2}))
try:
    fs.add(3)
except AttributeError as e:
    print(type(e).__name__)
try:
    {[1, 2]: 1}
except TypeError as e:
    print(type(e).__name__)
try:
    {{1}: 1}
except TypeError as e:
    print(type(e).__name__)
try:
    hash({1})
except TypeError as e:
    print(type(e).__name__)
print(frozenset("aab") == frozenset("ba"))
print(sorted(frozenset("hello")))
print(len(frozenset(range(100))))
print(frozenset({1}) == {1}, {1} == {1.0}, frozenset([1]) != frozenset([2]))
print(repr(frozenset()), repr(set()))
print(repr(frozenset([5])), repr({5}))
nested = {(1, frozenset([2, 3])): "x"}
print(nested[(1, frozenset([3, 2]))])
print(sorted(frozenset(x for x in range(5) if x % 2)))
print(bool(frozenset()), bool(frozenset([0])))
