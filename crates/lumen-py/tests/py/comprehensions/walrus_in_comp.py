data = [1, 5, 2, 8, 3]
res = [y for x in data if (y := x * 2) > 5]
print(res, y)

last = None
vals = [(last := v) for v in range(5)]
print(vals, last)

def f():
    out = [(t := i * i) + t for i in range(4)]
    return out, t

print(f())

total = 0
cum = [(total := total + x) for x in data]
print(cum, total)

print([w for s in ["a b", "c d e"] if (w := s.split())])
d = {k: (n := len(k)) for k in ["a", "bb", "ccc"]}
print(d, n)
print(any((found := x) > 6 for x in data), found)
s = {(m := x % 3) for x in range(10)}
print(sorted(s), m)

def g():
    seen = []
    keep = [x for x in [3, 1, 3, 2, 1] if x not in seen and not seen.append(x)]
    return keep, seen

print(g())
