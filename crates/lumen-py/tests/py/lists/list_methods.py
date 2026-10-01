l = [3, 1, 2]
l.append(5)
l.extend([7, 8])
l.extend("ab")
l.insert(0, "z")
l.insert(100, "end")
l.insert(-1, "pen")
print(l, len(l))
print(l.pop(), l.pop(0), l.pop(-1), l)
l = [1, 2, 3, 2, 1]
print(l.index(2), l.index(2, 2), l.index(1, 1), l.count(2), l.count(9))
l.remove(2)
print(l)
l.reverse()
print(l)
l.sort()
print(l)
l.sort(reverse=True)
print(l)
words = ["banana", "Apple", "cherry", "apple"]
print(sorted(words), sorted(words, key=str.lower), sorted(words, key=len, reverse=True), sorted(words, key=lambda w: (len(w), w)))
print(sorted([3, 1, 2], reverse=True), sorted((3, 1, 2)), sorted("cab"), sorted({3: "a", 1: "b"}), sorted([[2, 1], [1, 5], [1, 2]]))
pairs = [(2, "b"), (1, "z"), (2, "a"), (1, "y")]
print(sorted(pairs), sorted(pairs, key=lambda p: p[0]), sorted(pairs, key=lambda p: -p[0]))
print(sorted([2.5, 1, True, 0]), sorted([-1, -10, 5, 0], key=abs), sorted([3, 1, 2], key=lambda x: -x))
c = l.copy()
c.append(99)
print(l, c, l is c, l == c[:-1])
l.clear()
print(l, len(l), bool(l))
for bad in (lambda: [].pop(), lambda: [1].pop(5), lambda: [1].remove(2), lambda: [1].index(2), lambda: [1, "a"].sort(), lambda: [1].insert(), lambda: [1].extend(5)):
    try:
        bad()
    except (IndexError, ValueError, TypeError) as e:
        print(type(e).__name__)
x = [1, 2]
print(x.append(3), x.extend([4]), x.sort(), x.reverse(), x.insert(0, 0), x.clear(), x)
a = [1, 2, 3]
a += [4, 5]
a *= 2
print(a, [0] * 3, [[0] * 2] * 2, [] * 5, 3 * [1, 2], [1, 2] + [3], a.count(1))
m = [[0] * 2 for _ in range(2)]
m[0][0] = 1
s = [[0] * 2] * 2
s[0][0] = 1
print(m, s)
print(list(range(5)), list("abc"), list((1, 2)), list({1: 2}), list(), list(range(10, 0, -3)), list(reversed([1, 2, 3])), list(map(str, [1, 2])), list(filter(None, [0, 1, "", "a"])))
print(max([1, 5, 2]), min([4, 2, 8]), sum([1, 2, 3]), len([[], []]), any([0, 1]), all([1, 0]), any([]), all([]))
print([1, 2] == [1, 2], [1, 2] < [1, 3], [1, 2] < [1, 2, 0], [] < [0], [2] > [1, 9], [1, [2]] == [1, [2]], [1] != [1.0], [float("nan")] == [float("nan")])
n = float("nan")
print([n] == [n], [n].count(n), n in [n], [n].index(n))
print(3 in [1, 2, 3], "a" in ["a"], [1] in [[1]], 5 not in [1], [1, 2].__contains__(2), [1, 2].__len__())
print(list(enumerate(["a", "b"], 5)), list(zip([1, 2, 3], "ab")), list(zip()), list(zip([1], [2], [3])))
