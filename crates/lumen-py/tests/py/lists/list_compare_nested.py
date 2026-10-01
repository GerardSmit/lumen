print([1, 2, 3] == [1, 2, 3], [1, 2] == [1, 2, 3], [] == [], [1] != [2], [1, 2] == (1, 2))
print([1, 2] < [1, 3], [1, 2] < [1, 2, 0], [2] > [1, 9, 9], [] < [0], [1, 2] <= [1, 2], [3] >= [3, 0])
print(["a", "b"] < ["a", "c"], [[1, 2], [3]] < [[1, 2], [4]], [1, [2]] == [1, [2]], [1.0, 2] == [1, 2.0], [True] == [1])
print([0] * 3 == [0, 0, 0], [None] == [None], [float("nan")] == [float("nan")] , [1, 2] > [1])
nan = float("nan")
print([nan] == [nan], [nan] == [1], (lambda a: a == a)([nan]), (lambda a: [a] == [a])(nan))
n = [1, [2, [3, [4]]]]
print(n, n[1][1][1][0], len(n), n[1][1], [x for x in n if isinstance(x, list)])
n[1][1][1].append(5)
print(n)
mixed = [1, "a", 2.5, None, True, (1, 2), [3], {"k": 1}, {1, 2}, b"x"]
print(mixed, len(mixed))
print([type(x).__name__ for x in mixed])
r = [1, 2]
r.append(r)
print(r, len(r), r[2] is r, r[2][2][0])
print(str(r), repr(r) == str(r), r == r)
r2 = []
r2.append(r2)
print(r2, [r2], r2[0] is r2)
r3 = [1]
r3.append([r3])
print(r3)
d = {"k": 1}
d["self"] = d
print(d, [d])
t = [(1, [2, 3])]
t[0][1].append(4)
print(t)
def flatten(x):
    out = []
    for i in x:
        if isinstance(i, list):
            out.extend(flatten(i))
        else:
            out.append(i)
    return out
print(flatten([1, [2, [3, [4, [5]]]], 6, []]))
print(flatten([[], [[]], [[], [[]]]]))
depth = []
for _ in range(50):
    depth = [depth]
print(len(repr(depth)), repr(depth)[:6], repr(depth)[-6:])
print([[]] == [[]], [[]] == [], [[], []] == [[]] * 2, [()] == [[]])
print([1, 2, 3].index(2), [[1], [2]].index([2]), [[1], [2]].count([1]), [1, 1.0, True].count(1), [0, False, ""].count(0))
for f in (lambda: [1, 2].index(5), lambda: [1, 2].remove(5), lambda: [].remove(1), lambda: [1, 2].count(), lambda: [[1]].index([2]), lambda: [1, 2, 3].index(1, 1)):
    try:
        f()
    except (ValueError, TypeError) as e:
        print(type(e).__name__)
for f in (lambda: [1, 2].index(5), lambda: [1, 2].remove(5), lambda: [].pop(), lambda: [1][1], lambda: [1] < [None], lambda: [1] < ["a"], lambda: [1] + (2,), lambda: [1] * 1.5):
    try:
        f()
    except (ValueError, IndexError, TypeError) as e:
        print(type(e).__name__, e)
print(max([[1, 2], [1, 3], [0, 9]]), min([[1, 2], [1, 3], [0, 9]]), sorted([[2], [1, 5], [1]]))
print([1, 2] + [3], [[1]] + [[2]], [1] * 2 + [2] * 2, [[]] + [[]], len([[]] * 4))
print(list(map(len, [[], [1], [1, 2]])), sum([[1], [2]], []), sum([[1, 2], [3]], [0]))
print(list(zip(*[[1, 2, 3], [4, 5, 6]])), [list(p) for p in zip([1, 2], [3, 4])])
print(all([[1], [2]]), any([[], []]), bool([[]]), bool([]), not [0], not [])
print(tuple([1, [2]]), list((1, (2, 3))), [(1, 2)] == [(1, 2)], [(1, 2)] == [[1, 2]])
