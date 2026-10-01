t = (1, "a", 2.5, None, [1])
print(t, len(t), t[0], t[-1], t[1:3], t[::-1], type(t).__name__)
print((), (1,), (1, 2), ((1, 2), (3,)), (1,) * 3, (1, 2) + (3,), () + (), (0,) * 0, tuple(), tuple([1, 2]), tuple("ab"), tuple(range(3)))
print((1, 2, 3) == (1, 2, 3), (1) == 1, (1,) == (1,), type((1)).__name__, type((1,)).__name__, type(()).__name__)
print((1, 2) < (1, 3), (1, 2) < (1, 2, 0), (2,) > (1, 9), () < (0,), (1, "a") < (1, "b"), (1, 2) == (1, 2), (1, 2) == [1, 2], (1, 2) != (2, 1), (1, 2) == (1.0, 2.0))
print((1, 2, 3).index(2), (1, 2, 2, 3).count(2), (1, 2).count(5), 2 in (1, 2), 5 not in (1, 2), (1, 2).__len__(), (1, 2).__contains__(1))
try:
    (1, 2).index(9)
except ValueError:
    print("ValueError")
try:
    t[0] = 5
except TypeError:
    print("TypeError assign")
try:
    t[10]
except IndexError as e:
    print("IndexError", e)
try:
    del t[0]
except TypeError:
    print("TypeError del")
try:
    (1,).append(2)
except AttributeError:
    print("AttributeError")
t[4].append(2)
print(t)
a = (1, 2)
b = a
a += (3,)
print(a, b, a is b)
print(hash((1, 2)) == hash((1, 2)), hash(()) == hash(()), {(1, 2): "x"}[(1, 2)], {(1, (2, 3)): 1}[(1, (2, 3))], len({(1, 2), (2, 1), (1, 2)}))
try:
    hash((1, [2]))
except TypeError:
    print("TypeError hash")
print(sorted((3, 1, 2)), max((1, 5, 2)), min((4, 2)), sum((1, 2, 3)), list((1, 2)), set((1, 1, 2)) == {1, 2}, any((0, 1)), all((1, 0)))
print(tuple(x * 2 for x in range(3)), tuple([x] for x in range(2)), tuple(reversed((1, 2, 3))), tuple(sorted("cab")), tuple(map(str, (1, 2))), tuple(zip("ab", (1, 2))))
print(tuple(enumerate("ab")), tuple({1: 2}), tuple({3, 4}) == (3, 4), (1, 2, 3)[1], ((1, 2), 3)[0][1], (1, (2, (3, 4)))[1][1][0])
print(repr((1,)), repr(()), repr((1, 2)), str(("a", "b")), repr(((),)), repr((None,)), repr((1.5, True)))
def f():
    return 1, 2
r = f()
print(r, type(r).__name__, f()[1], len(f()))
print(divmod(7, 2), (3, 4)[0] ** 2 + (3, 4)[1] ** 2, tuple(i for i in range(3)) == (0, 1, 2), (1, 2) * 2, 2 * (1, 2))
class Pt(tuple):
    @property
    def x(self):
        return self[0]
p = Pt((3, 4))
print(p, p.x, len(p), p == (3, 4), isinstance(p, tuple), p + (5,), type(p + (5,)).__name__, p[::-1])
