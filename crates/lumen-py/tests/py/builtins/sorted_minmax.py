print(sorted([3, 1, 2]), sorted([3, 1, 2], reverse=True), sorted([]), sorted("hello"))
print(sorted([3, 1, 2], key=lambda x: -x))
words = ["bb", "a", "ccc", "dd", "e", "fff"]
print(sorted(words, key=len))
print(sorted(words, key=len, reverse=True))
print(sorted(words))
pairs = [(1, "b"), (0, "z"), (1, "a"), (0, "y")]
print(sorted(pairs, key=lambda p: p[0]))
print(sorted(pairs, key=lambda p: p[0], reverse=True))
print(sorted(pairs))
print(sorted([2.5, 1, True, 0]))
print(sorted({3: "a", 1: "b"}))
print(sorted(range(5), key=lambda x: x % 2))
print(sorted([[2, 1], [1, 5], [1]]))
print(sorted([-3, 2, -1], key=abs))
print(sorted(["B", "a", "C"], key=str.lower))
print(sorted(["B", "a", "C"]))
data = [5, 3, 5, 1]
res = sorted(data)
print(data, res, res is data)
data.sort(reverse=True)
print(data)
print(data.sort(), data)
try:
    sorted([1, "a"])
except TypeError as e:
    print(type(e).__name__)
try:
    sorted([None, 1])
except TypeError as e:
    print(type(e).__name__)
try:
    sorted([1, 2], 5)
except TypeError as e:
    print(type(e).__name__)
try:
    sorted([3, 1], key=lambda x: "a" if x == 3 else 1)
except TypeError as e:
    print(type(e).__name__)

print(min(3, 1, 2), max(3, 1, 2), min([4, 2]), max("abc"))
print(min([], default="none"), max([], default=0), min([1], default=5))
print(min(["bb", "a", "cc"], key=len), max(["bb", "a", "cc"], key=len))
print(max(["bb", "dd", "a"], key=len), min(["bb", "dd", "x", "y"], key=len))
print(max([(1, "a"), (1, "b")]), min([(2, 1), (1, 9)]))
print(max(1, 2.0), max(2, 2.0), max(2.0, 2), min(1, 1.0), min(1.0, 1))
print(max(range(5), key=lambda x: -x), min(range(1, 5), key=lambda x: (x - 3) ** 2))
print(max({"a": 3, "b": 9}, key={"a": 3, "b": 9}.get))
print(max(x for x in range(5)), min((x for x in range(3, 8)), default=None))
print(max([], default=None))
try:
    min([])
except ValueError as e:
    print(type(e).__name__, e)
try:
    max([])
except ValueError as e:
    print(type(e).__name__, e)
try:
    max()
except TypeError as e:
    print(type(e).__name__)
try:
    max(1)
except TypeError as e:
    print(type(e).__name__)
try:
    min(1, "a")
except TypeError as e:
    print(type(e).__name__)

print(sum([1, 2, 3]), sum([]), sum([1, 2], 10), sum([0.5, 0.25]), sum(range(10)))
print(sum([[1], [2]], []), sum([(1,), (2,)], ()), sum([1, 2.5]), sum([True, True]))
print(sum(x * x for x in range(4)), sum({1, 2, 3}), sum({1: 100, 2: 200}))
print(sum([0.1] * 10) == 1.0, sum([1e100, 1.0, -1e100]))
print(sum([1, 2], start=5))
try:
    sum(["a", "b"])
except TypeError as e:
    print(type(e).__name__)
try:
    sum([1, None])
except TypeError as e:
    print(type(e).__name__)
print(sorted(["b", "a"], reverse=True)[0], sorted([1, 1.0, True]))
print(list(reversed(sorted([2, 3, 1]))))
