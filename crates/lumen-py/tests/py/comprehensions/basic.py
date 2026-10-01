print([x * x for x in range(10)])
print([x for x in range(20) if x % 3 == 0 if x % 2 == 0])
print({x: x ** 2 for x in range(5)})
print(sorted({x % 4 for x in range(20)}))
print(type({x for x in "ab"}).__name__, type({}).__name__, type({1}).__name__)
print([(i, j) for i in range(3) for j in range(3) if i != j])
print([[j for j in range(i)] for i in range(4)])
matrix = [[1, 2, 3], [4, 5, 6], [7, 8, 9]]
print([row[i] for row in matrix for i in range(3) if i == 1])
print([[row[i] for row in matrix] for i in range(3)])
print([c for word in ["ab", "cd"] for c in word])
print({k: v for k, v in zip("abc", range(3))})
print({v: k for k, v in {"a": 1, "b": 2}.items()})
print([x if x % 2 else -x for x in range(6)])
print(["even" if x % 2 == 0 else "odd" for x in range(4)])
print([f"{x:02d}" for x in range(3)])
print(sum([i for i in range(100)]))
print([x for x in []])
print({}.keys() == {k: 1 for k in []}.keys())
d = {}
for i in range(3):
    d[i] = [j * i for j in range(3)]
print(d)
print(list(map(lambda p: p[0] + p[1], [(a, b) for a in range(2) for b in range(2)])))
words = ["apple", "bob", "cat"]
print({w: len(w) for w in words if len(w) > 2})
print([w.upper() for w in words][::-1])
print([x for x in range(5)][-2:])
print([[0] * 3 for _ in range(2)])
