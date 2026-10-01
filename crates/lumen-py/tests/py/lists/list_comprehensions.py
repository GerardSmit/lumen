print([x * x for x in range(6)], [x for x in range(10) if x % 3 == 0], [x if x % 2 else -x for x in range(5)])
print([(x, y) for x in range(3) for y in range(x)], [x + y for x in "ab" for y in "12"], [[y for y in range(x)] for x in range(4)])
print([x for x in range(20) if x % 2 == 0 if x % 3 == 0], [x for x in [] if x], [c.upper() for c in "abc" if c != "b"])
print([i for i, c in enumerate("hello") if c == "l"], [a * b for a, b in zip([1, 2, 3], [4, 5, 6])], [s[::-1] for s in ["ab", "cd"]])
matrix = [[1, 2, 3], [4, 5, 6], [7, 8, 9]]
print([row[i] for row in matrix for i in range(3)], [[row[i] for row in matrix] for i in range(3)], [sum(r) for r in matrix], list(zip(*matrix)))
print({x: x * x for x in range(4)}, {x % 3 for x in range(10)} == {0, 1, 2}, {k: v for k, v in [("a", 1), ("b", 2)]}, {c: i for i, c in enumerate("abc")})
g = (x * 2 for x in range(4))
print(next(g), next(g), list(g), list(g))
print(sum(x for x in range(5)), max(x * x for x in range(-3, 3)), tuple(x for x in "ab"), sorted(x for x in {3, 1, 2}), ",".join(str(x) for x in range(3)))
x = 100
print([x for x in range(3)], x)
y = [1, 2, 3]
print([y for y in y], y)
z = 5
print([z + i for i in range(3)], [(lambda: z)() for _ in range(2)])
fs = [lambda i=i: i for i in range(3)]
print([f() for f in fs])
print([v for v in range(5) if v not in [1, 3]], [not v for v in [0, 1, "", "a"]], [v for v in (1, None, 2) if v is not None])
nested = [[1, [2, 3]], [4, [5, [6]]]]
def flat(l):
    return [y for x in l for y in (flat(x) if isinstance(x, list) else [x])]
print(flat(nested))
print([n for n in range(2, 40) if all(n % d for d in range(2, int(n ** 0.5) + 1))])
print([(a, b, c) for a in range(1, 20) for b in range(a, 20) for c in range(b, 20) if a * a + b * b == c * c])
print([x for x in range(3)] == list(range(3)), type([x for x in ()]).__name__, type(x for x in ()).__name__)
print([i * j for i in range(1, 4) for j in range(1, 4) if i != j], [[0] * i for i in range(3)], [None for _ in range(2)])
words = ["apple", "bob", "cat"]
print({w: len(w) for w in words}, {len(w): w for w in words}, [w for w in words if len(w) == 3], [(w, i) for i, w in enumerate(words)], sorted(words, key=lambda w: w[-1], reverse=True))
print([c for w in words for c in w if c in "aeiou"], "".join([c for c in "a1b2c3" if c.isdigit()]), [int(c) for c in "123"], sum(int(c) for c in "999"))
print([[i, j] for i in range(2) for j in range(2)], [(i, j) for i in range(2) for j in range(2) if i == j], [i for i in range(10)][2:8:2])
try:
    [1 / v for v in [1, 0]]
except ZeroDivisionError:
    print("ZeroDivisionError in comp")
print([i for i in range(3)] + [i for i in range(3, 5)], [*range(3), *"ab"], [*[1, 2], 3, *(4, 5)])
