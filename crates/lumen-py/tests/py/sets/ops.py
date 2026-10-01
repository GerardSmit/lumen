a = {1, 2, 3, 4}
b = {3, 4, 5, 6}
print(sorted(a | b))
print(sorted(a & b))
print(sorted(a - b))
print(sorted(b - a))
print(sorted(a ^ b))
print(sorted(a.union(b, {10})))
print(sorted(a.intersection(b, {4, 9})))
print(sorted(a.difference(b, {1})))
print(sorted(a.symmetric_difference(b)))
print(a.issubset({1, 2, 3, 4, 5}), a.issuperset({1, 2}), a.isdisjoint({7, 8}), a.isdisjoint(b))
print(a <= a, a < a, a >= {1}, a > {1}, {1} < a)
print({1, 2} <= {1, 2, 3}, {1, 2, 3} >= {1, 2})
print(a == {4, 3, 2, 1}, a != b)

s = {1, 2, 3}
s.update({3, 4}, [5, 6])
print(sorted(s))
s.intersection_update({2, 3, 4, 5, 9})
print(sorted(s))
s.difference_update([2])
print(sorted(s))
s.symmetric_difference_update({3, 100})
print(sorted(s))
s |= {7}
s &= {4, 5, 7, 100}
s -= {100}
s ^= {5, 8}
print(sorted(s))

s.add(42)
s.add(42)
print(len(s), 42 in s, 43 in s, 43 not in s)
s.discard(999)
s.remove(42)
print(sorted(s))
c = s.copy()
c.add(-1)
print(sorted(s), sorted(c))
c.clear()
print(len(c), c == set(), bool(c), bool(s))

print(set(), len(set()))
print(sorted(set("hello")))
print(sorted(set(range(5))))
print(sorted(set([3, 1, 3, 2, 1])))
print(sorted({x * x for x in range(-3, 4)}))
print(sorted(a | frozenset({9})))
print(type(a | frozenset({9})).__name__, type(frozenset({9}) | a).__name__)
print(sorted(set([(1, 2), (2, 1), (1, 2)])))

try:
    a | [1]
except TypeError as e:
    print(type(e).__name__)
try:
    set([[1]])
except TypeError as e:
    print(type(e).__name__)
try:
    {1}.update(5)
except TypeError as e:
    print(type(e).__name__)
print(sum(a), max(a), min(a), len(a))
print(sorted(map(lambda v: v + 1, a)))
print(1 in a, 1.0 in a, True in a)
print(sorted({1, True, 1.0}))
