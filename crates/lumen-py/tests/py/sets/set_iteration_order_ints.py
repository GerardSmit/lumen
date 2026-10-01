s = {3, 1, 2}
print(s, list(s), sorted(s))
print({5, 4, 3, 2, 1}, {10, 9, 8}, {0}, {-1, -2, 0, 1, 2})
print(list({7, 3, 5}), tuple({2, 1}), [x for x in {3, 2, 1}])
t = set()
for i in (5, 3, 8, 1, 9, 2):
    t.add(i)
print(t, sorted(t), len(t), min(t), max(t), sum(t))
t.discard(3)
t.add(3)
print(sorted(t))
u = {1, 2, 3}
for x in sorted(u, reverse=True):
    print(x, end=" ")
print()
total = 0
for x in {10, 20, 30}:
    total += x
print(total)
seen = set()
dupes = []
for v in [1, 2, 3, 2, 1, 4, 5, 4]:
    if v in seen:
        dupes.append(v)
    seen.add(v)
print(dupes, sorted(seen))
print(sorted({1, 2, 3} | {3, 4, 5}), sorted({1, 2, 3} & {2, 3, 4}), sorted({1, 2, 3} - {2}), sorted({1, 2, 3} ^ {3, 4}))
print(sorted(set([3, 1, 2, 3, 1])), sorted(set("banana")), sorted({(1, 2), (0, 5), (1, 1)}))
print(len(set(x % 10 for x in range(1000))), sorted({x // 10 for x in range(35)}))
s = {1, 2, 3}
try:
    for x in s:
        s.add(x + 10)
except RuntimeError as e:
    print("RuntimeError", e)
s = {1, 2, 3}
for x in list(s):
    s.discard(x)
print(s)
primes = {n for n in range(2, 50)} - {m for n in range(2, 8) for m in range(n * n, 50, n)}
print(sorted(primes))
print(sorted({x for x in range(30) if x % 2 == 0} & {x for x in range(30) if x % 3 == 0}))
print(1 in {1: "a"}, {1, 2} == {1, 2}, {1, 2} is {1, 2}, [1, 2, 3] == list({1, 2, 3}))
print(sorted(set(range(5)) | set(range(3, 8))), sorted(set(map(lambda x: x * x, range(5)))), sorted(set(filter(lambda x: x > 2, range(6)))))
print(sorted({c for c in "hello world" if c != " "}), len({"a", "b", "a"}), sorted({"x", "y"}) == ["x", "y"])
print(set(), {1} - {1}, {1} ^ {1}, set([]) == set(), repr(set()), str({1}), {1, 2}.__len__(), {1, 2}.__contains__(2))
n = 0
fs = frozenset(range(5))
for x in fs:
    n += x
print(n, sorted(fs), fs, [x for x in fs])
print(list(zip(sorted({3, 1, 2}), "abc")), dict.fromkeys(sorted({3, 1, 2}), 0), enumerate(sorted({1})).__next__())
print(sorted({1, 2, 3, 4}, key=lambda v: -v), sorted({-1, 1, -2, 2}, key=abs), min({3, 1, 2}, key=lambda v: -v))
