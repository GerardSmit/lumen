t = (10, 20, 30, 20, 40)
print(t[0], t[-1], t[2], len(t))
print(t.count(20), t.count(99), t.index(20), t.index(20, 2))
try:
    t.index(99)
except ValueError as e:
    print(type(e).__name__)
try:
    t[10]
except IndexError as e:
    print(type(e).__name__, e)
try:
    t[0] = 1
except TypeError as e:
    print(type(e).__name__)

print((1, 2) + (3,), (1,) * 3, 2 * (1, 2), () + (), (1,) * 0)
print((1, 2) < (1, 3), (1, 2) < (1, 2, 0), (2,) > (1, 99), () < (0,))
print((1, 2) == (1, 2), (1, 2) != (2, 1), (1, "a") <= (1, "b"))
print((1, 2, 3) >= (1, 2, 3), (1, [2]) == (1, [2]))
print(hash((1, 2)) == hash((1, 2)), hash(()) == hash(()))
print({(1, 2): "x"}[(1, 2)])
print(len({(1, 2), (1, 2), (2, 1)}))

single = (5,)
print(single, type(single).__name__, len(single))
notuple = (5)
print(notuple, type(notuple).__name__)
print((), len(()), type(()).__name__)
x = 1, 2, 3
print(x, type(x).__name__)
y = 1,
print(y)

nested = ((1, 2), (3, (4, 5)), [6, 7])
print(nested[1][1][0], nested[2][1], nested[0])
print(repr(nested))

print(tuple(), tuple([1, 2]), tuple("abc"), tuple(range(4)))
print(tuple({1: "a", 2: "b"}), tuple(x * 2 for x in range(3)))
print(tuple(t) is t)
print(tuple(map(str, [1, 2])), tuple(sorted({3, 1, 2})))
print(3 in t, 99 in t, 99 not in t)
print(min(t), max(t), sum(t), sorted(t), list(reversed(t)))
print(t[1:3], t[::2], t[::-1], t[10:])
a, b = (1, 2)
print(a, b)
print(str((1, "a", None, 2.5, True)))
print(repr(("it's", 'q"')))
print((1, 2) * -1)
print(t.__len__(), t.__contains__(10))
print(any(()), all(()))
print(tuple(enumerate("ab")))
print(tuple(zip([1, 2], "xy")))
print(max((1, "b"), (1, "a")))
print(sorted([(2, "a"), (1, "z"), (2, "A")]))
