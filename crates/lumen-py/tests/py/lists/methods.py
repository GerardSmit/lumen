l = [3, 1, 2]
l.append(4)
print(l, len(l))
l.extend([5, 6])
l.extend((7,))
l.extend("ab")
print(l)
l.insert(0, 0)
l.insert(-1, "x")
l.insert(100, "end")
l.insert(-100, "start")
print(l)
print(l.pop(), l.pop(0), l.pop(-1), l)
l.remove("x")
print(l, l.index(3), l.index(3, 1), l.count(3), l.count("zzz"))
l = [5, 3, 8, 1]
l.sort()
print(l)
l.reverse()
print(l)
l.sort(reverse=True)
print(l, sorted(l), l)
c = l.copy()
c.append(99)
print(l, c)
l.clear()
print(l, len(l), bool(l))
a = [1, 2]
a += [3]
a *= 2
print(a, a + [0], [0] * 3, 2 * [1, 2], a * 0)
print(a.index(3), a.index(3, 3), a.count(1))
print(max(a), min(a), sum(a), len(a), sorted(a, reverse=True))
print(list(range(5)), list("abc"), list((1, 2)), list({"k": 1}), list(), list([1]))
print([1, 2, 3].__len__(), [1, 2].__contains__(2), [1, 2, 3].__getitem__(-1))
x = [1, 2, 3]
y = x.append(4)
print(y, x)
print(x.sort(), x.reverse(), x.clear(), x)
print(list(reversed([1, 2, 3])), list(enumerate("ab", 1)), list(zip([1, 2], "ab")))
print(any([0, 0, 1]), all([1, 1, 0]), any([]), all([]))
print(3 in [1, 2, 3], 4 not in [1, 2, 3], [1, 2] in [[1, 2], [3]])
s = [1, 2, 3, 4, 5]
print(s[1:3], s[::2], s[::-1], s[-2:], s[:-2], s[10:], s[-10:2])
print(s.pop(2), s, s.pop(-1), s)
nested = [[1, 2], [3, 4]]
print([i for row in nested for i in row], [[i * 2 for i in row] for row in nested])
print(list(map(str, [1, 2])), list(filter(None, [0, 1, "", "a", None])))
print(sorted(["b", "a", "C"]), sorted([3, 1, 2], key=lambda v: -v), min([3, 1, 2], key=lambda v: -v))
for f in (lambda: [].pop(), lambda: [1].pop(5), lambda: [1].remove(2), lambda: [1].index(2), lambda: [1, 2].insert("a", 1), lambda: [1][5], lambda: [1][str]):
    try:
        f()
    except (IndexError, ValueError, TypeError) as e:
        print(type(e).__name__, e)
try:
    [1, "a"].sort()
except TypeError as e:
    print(type(e).__name__, e)
try:
    [].append()
except TypeError as e:
    print(type(e).__name__)
print([1, 2, 3].index(3, -1), [1, 2, 3].index(1, 0, 1))
