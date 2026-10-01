def primes(n):
    return [p for p in range(2, n) if all(p % d for d in range(2, int(p ** 0.5) + 1))]

print(primes(50))
triples = [(a, b, c) for c in range(1, 21) for b in range(1, c) for a in range(1, b) if a * a + b * b == c * c]
print(triples)
print({n: [d for d in range(1, n + 1) if n % d == 0] for n in range(1, 11)})
flat = [x for sub in [[1, [2]], [3], []] for x in sub]
print(flat)
words = "the quick brown fox jumps over the lazy dog".split()
by_len = {}
for w in words:
    by_len.setdefault(len(w), []).append(w)
print(sorted(by_len.items()))
print({w[0]: sum(1 for x in words if x[0] == w[0]) for w in words if w[0] in "tqb"})
print(sorted({len(w) for w in words}))
print(sum(sum(r) for r in [[1, 2], [3, 4], [5]]))
print(max((len(w), w) for w in words))
print([[i * j for j in range(1, 4)] for i in range(1, 4)])
print(list(zip(*[[1, 2, 3], [4, 5, 6]])))
print([x for x in range(30) if x % 2 == 0 if x % 3 == 0 if x > 0])
print(dict([(i, chr(97 + i)) for i in range(5)]))
print("".join([c.upper() if i % 2 == 0 else c for i, c in enumerate("comprehension")]))
print([(x, y) for x in range(2) for y in "ab"])
print(list(range(3)) == [i for i in range(3)])
