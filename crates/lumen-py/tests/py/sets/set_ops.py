a = {1, 2, 3, 4}
b = {3, 4, 5, 6}
print(a | b, a & b, a - b, b - a, a ^ b, a.union(b), a.intersection(b), a.difference(b), a.symmetric_difference(b))
print(a.union([9], (10,)), a.intersection([1, 2, 99], range(2)), a.difference([1], [2]), a | {0})
print(sorted(a | b), len(a | b), len(a & b), 3 in a, 9 in a, 9 not in a)
print(a <= b, {3, 4} <= a, {3, 4} < a, a < a, a <= a, a >= {1}, a > {1, 2, 3, 4}, a > {1}, a == {4, 3, 2, 1}, a != b)
print(a.issubset({1, 2, 3, 4, 5}), a.issuperset({1}), a.isdisjoint({7, 8}), a.isdisjoint({4}), set().issubset(a), a.issubset(range(10)))
c = set()
c.add(1)
c.add(1)
c.add(2)
print(c, len(c), c.discard(5), c.discard(1), c)
c.update([5, 6], {7})
print(c)
print(c.remove(5), c)
try:
    c.remove(99)
except KeyError as e:
    print("KeyError", e)
x = c.pop()
print(x in {2, 6, 7}, len(c))
c.clear()
print(c, bool(c), set(), len(set()))
s = {1, 2, 3}
s |= {4}
s &= {1, 2, 4, 9}
s -= {1}
s ^= {2, 10}
print(s)
s.intersection_update({4, 10, 11})
print(s)
s.difference_update({10})
print(s)
s.symmetric_difference_update({4, 5})
print(s)
print(set("hello") == {"h", "e", "l", "o"}, sorted(set("hello")), len(set("mississippi")), set([1, 1, 1]), set(range(3)), set((1, 2)), set({1: "a"}))
print({1, 2} == {2, 1}, {1} == {1.0}, {1, 1.0, True}, {0, False}, {(1, 2), (1, 2)}, {2 ** 70, 2 ** 70})
print({x for x in range(10) if x % 3 == 0}, {x % 4 for x in range(100)}, {x * x for x in range(-3, 4)})
f = frozenset([1, 2, 3])
g = frozenset({3, 4})
print(f, f | g, f & g, f - g, f ^ g, f == {1, 2, 3}, {f: 1}[frozenset([3, 2, 1])], f <= frozenset(range(5)), type(f | {9}).__name__, type({9} | f).__name__)
print(frozenset(), set(), repr(frozenset([1])), len(f), 2 in f, sorted(f), hash(f) == hash(frozenset([3, 2, 1])))
print({frozenset([1]): "a", frozenset([2]): "b"}[frozenset({1})], {frozenset([1, 2]), frozenset([2, 1])}, {(1, frozenset([1]))})
for bad in (lambda: {[1]}, lambda: {1}.add([2]), lambda: {{}}, lambda: {1} | [1], lambda: {1} < [1], lambda: set(5), lambda: {1}[0], lambda: f.add(1)):
    try:
        bad()
    except (TypeError, AttributeError) as e:
        print(type(e).__name__)
print(sum({1, 2, 3}), max({1, 5, 2}), min({4, 2}), sorted({3, 1, 2}, reverse=True), any({0}), all({0, 1}), list(sorted({5, 3}))[0])
print({1, 2, 3}.copy() == {1, 2, 3}, {1}.copy() is not None, {1, 2} ^ {2, 3} == {1, 3}, {1, 2} - {1, 2} == set())
big = set(range(0, 100, 3)) & set(range(0, 100, 5))
print(sorted(big), len(set(range(1000)) - set(range(0, 1000, 2))), sum(set(range(100)) ^ set(range(50, 150))))
