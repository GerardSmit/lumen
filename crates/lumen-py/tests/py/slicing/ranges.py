r = range(10)
print(r, len(r), r[0], r[-1], r[3], list(r[2:5]))
print(range(2, 10, 3), len(range(2, 10, 3)), list(range(2, 10, 3)))
print(range(10, 0, -3), list(range(10, 0, -3)), len(range(10, 0, -3)))
print(len(range(0)), len(range(5, 5)), len(range(5, 0)), list(range(5, 0)))
print(range(5)[1:4], range(10)[::2], range(10)[::-1], range(10)[8:2:-2])
print(list(range(10)[::-3]), range(0, 10, 2)[1:], range(10)[100:])
print(range(10)[2:5][1])
print(list(reversed(range(5))), list(reversed(range(1, 10, 4))))
print(3 in range(5), 5 in range(5), -1 in range(5), 4 in range(0, 10, 2), 5 in range(0, 10, 2))
print(10 in range(10, 0, -2), 2 in range(10, 0, -2), 3 in range(10, 0, -2))
print(3.0 in range(5), "a" in range(5))
print(range(5) == range(5), range(0) == range(5, 5), range(0, 10, 2) == range(0, 9, 2))
print(range(1, 10, 3) == range(1, 11, 3), range(5) != range(6))
print(range(3).index(2), range(10, 0, -1).index(4), range(0, 20, 5).count(10), range(5).count(9))
print(r.start, r.stop, r.step, range(2, 8, 3).step)
big = range(10**18, 10**18 + 10)
print(len(big), big[0], big[-1], 10**18 + 3 in big)
huge = range(0, 10**30, 10**20)
print(len(huge), huge[5], huge[-1])
print(len(range(-10**20, 10**20, 10**19)))
print(range(2**64)[-1], 2**64 - 1 in range(2**64))
print(sum(range(101)), max(range(5)), min(range(2, 9)), sorted(range(3, 0, -1)))
print(bool(range(0)), bool(range(1)))
print([i * i for i in range(5)])
print(list(range(-3, 3)), list(range(3, -4, -2)))
print(tuple(range(3)), set(range(3)) == {0, 1, 2})
print(list(zip(range(3), "abc")))
try:
    range(0, 10, 0)
except ValueError as e:
    print(type(e).__name__, e)
try:
    range(10)[10]
except IndexError as e:
    print(type(e).__name__, e)
try:
    range(1.5)
except TypeError as e:
    print(type(e).__name__)
try:
    range(10)["a"]
except TypeError as e:
    print(type(e).__name__)
try:
    range()
except TypeError as e:
    print(type(e).__name__)
try:
    range(3).index(5)
except ValueError as e:
    print(type(e).__name__)
try:
    range(5)[::0]
except ValueError as e:
    print(type(e).__name__)
it = iter(range(3))
print(next(it), next(it), next(it), next(it, "done"))
print(hash(range(3)) == hash(range(3)))
print(isinstance(range(3), range), type(range(3)).__name__)
print(range(5)[True], range(10)[-10])
for i in range(3, 0, -1):
    print(i, end=" ")
print()
