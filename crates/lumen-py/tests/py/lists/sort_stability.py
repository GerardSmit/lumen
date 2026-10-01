people = [("bob", 25), ("alice", 30), ("carol", 25), ("dave", 30), ("eve", 25), ("frank", 20)]
print(sorted(people))
print(sorted(people, key=lambda p: p[1]))
print(sorted(people, key=lambda p: p[1], reverse=True))
print(sorted(people, key=lambda p: (p[1], p[0])))
print(sorted(people, key=lambda p: (-p[1], p[0])))
print(sorted(people, key=lambda p: len(p[0])))
print(sorted(people, key=lambda p: p[0][-1]))
words = ["banana", "Apple", "cherry", "apple", "Banana", "cherry"]
print(sorted(words))
print(sorted(words, key=str.lower))
print(sorted(words, key=str.lower, reverse=True))
print(sorted(words, key=len))
print(sorted(words, key=lambda w: (len(w), w)))
print(sorted(set(words)) == sorted(set(words), key=lambda w: w))
l = [5, 2, 9, 1, 5, 6]
l.sort()
print(l)
l.sort(reverse=True)
print(l)
l.sort(key=lambda v: v % 3)
print(l)
l.sort(key=lambda v: v % 3, reverse=True)
print(l)
recs = [{"n": "a", "k": 1}, {"n": "b", "k": 0}, {"n": "c", "k": 1}, {"n": "d", "k": 0}]
print([r["n"] for r in sorted(recs, key=lambda r: r["k"])])
print([r["n"] for r in sorted(recs, key=lambda r: r["k"], reverse=True)])
print(sorted([(1, "b"), (1, "a"), (0, "z"), (1, "a")]))
print(sorted([(1, 2, 3), (1, 2), (1,), ()]))
print(sorted([[2, 1], [1, 5], [1, 2], []]))
print(sorted(["10", "9", "2", "1"]), sorted(["10", "9", "2", "1"], key=int))
print(sorted([3, 1.5, 2, -1, 2.0]), sorted([True, False, 2, 0]))
print(sorted("hello"), sorted({3: "a", 1: "b"}), sorted((3, 1, 2)), sorted(range(5, 0, -2)))
print(sorted([0.5, -0.0, 0.0, -1.5]))
calls = []
def key(v):
    calls.append(v)
    return v % 2
print(sorted([4, 3, 2, 1], key=key), calls)
print(sorted([3, 1, 2], key=None), sorted([], key=len), sorted([1]))
data = [(1, "x"), (1, "y"), (1, "z")]
print(sorted(data, key=lambda t: t[0], reverse=True))
print(list(reversed(sorted(data, key=lambda t: t[0]))))
pairs = [("a", 2), ("b", 1), ("c", 2), ("d", 1)]
print(sorted(pairs, key=lambda p: -p[1]))
print(min(pairs, key=lambda p: p[1]), max(pairs, key=lambda p: p[1]), min(pairs), max(pairs))
idx = sorted(range(len(pairs)), key=lambda i: (pairs[i][1], -i))
print(idx)
ranks = {}
for r, (name, _) in enumerate(sorted(pairs, key=lambda p: p[1])):
    ranks[name] = r
print(ranks)
src = [3, 1, 2]
out = sorted(src)
print(src, out, src is out)
big = [(i % 5, i) for i in range(20)]
print(sorted(big, key=lambda t: t[0])[:8])
print(all(a[0] < b[0] or (a[0] == b[0] and a[1] < b[1]) for a, b in zip(sorted(big, key=lambda t: t[0]), sorted(big, key=lambda t: t[0])[1:])))
try:
    sorted([1, "a"])
except TypeError as e:
    print(type(e).__name__, e)
try:
    sorted([1, 2], key=lambda v: None if v == 1 else 1)
except TypeError as e:
    print(type(e).__name__, e)
try:
    sorted([3, 1], reverse=1, key=5)
except TypeError as e:
    print(type(e).__name__, e)
