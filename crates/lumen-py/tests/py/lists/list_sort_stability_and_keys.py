recs = [("bob", 25), ("alice", 30), ("carol", 25), ("dave", 30), ("eve", 25)]
print(sorted(recs, key=lambda r: r[1]))
print(sorted(recs, key=lambda r: r[1], reverse=True))
print(sorted(recs, key=lambda r: (-r[1], r[0])))
print(sorted(recs, key=lambda r: r[0][-1]))
data = [5, 3, 8, 1, 9, 2, 7, 3, 5]
print(sorted(data), sorted(data, reverse=True), data)
data.sort(key=lambda v: v % 3)
print(data)
calls = []
def key(v):
    calls.append(v)
    return -v
print(sorted([3, 1, 2], key=key), calls)
print(sorted(["b", "A", "c", "B"]), sorted(["b", "A", "c", "B"], key=str.lower), sorted(["10", "9", "2"]), sorted(["10", "9", "2"], key=int))
print(sorted([(1, "b"), (1, "a"), (0, "z")]), sorted([[1, 2], [1], [0, 5], []]), sorted([(2,), (1, 5), (1,)]))
print(sorted([True, False, True]), sorted([0.5, -1, 3, 2.5]), sorted([-0.0, 0.0, -1]), sorted(range(5), key=lambda x: -x))
print(min(recs, key=lambda r: r[1]), max(recs, key=lambda r: r[1]), min([], default="none"), max([3, 1], default=0))
print(min("b", "a", "c"), max((1, 2), (1, 3)), min([[2], [1, 5]]), max("abc", key=ord), max([1, 1.0]), max([1.0, 1]))
import_free = sorted(set([3, 1, 2, 3]))
print(import_free, sorted({"b": 1, "a": 2}.items()), sorted({"b": 1, "a": 2}.items(), key=lambda kv: kv[1]))
big = [(i * 7919) % 1009 for i in range(200)]
s = sorted(big)
print(s[:10], s[-10:], s == sorted(s), len(s), sum(s) == sum(big))
stable = [(i % 5, i) for i in range(20)]
print(sorted(stable, key=lambda p: p[0]))
print([x for _, x in sorted(zip([3, 1, 2], "abc"))], [v for _, v in sorted(zip("cab", range(3)))])
l = [3, 1, 2]
res = l.sort()
print(res, l)
l = [1, 2, 3]
print(sorted(l, key=lambda x: 0), sorted(l, key=lambda x: 0, reverse=True))
class V:
    def __init__(self, v):
        self.v = v
    def __lt__(self, o):
        return self.v < o.v
    def __repr__(self):
        return "V%d" % self.v
print(sorted([V(3), V(1), V(2)]), min(V(3), V(1)), max([V(3), V(1)]), sorted([V(2), V(1)], reverse=True))
try:
    sorted([1, "a"])
except TypeError as e:
    print("TypeError")
try:
    sorted([V(1), 2])
except (TypeError, AttributeError) as e:
    print(type(e).__name__)
try:
    sorted([3, 1], key=5)
except TypeError:
    print("TypeError key")
print(sorted("hello world"), "".join(sorted("hello")), sorted(["é", "e", "z", "a"]), sorted([10 ** 20, 5, -10 ** 20]))
print(list(reversed(sorted([2, 3, 1]))), sorted([2, 3, 1])[::-1], sorted(["bb", "a", "ccc"], key=len)[-1])
