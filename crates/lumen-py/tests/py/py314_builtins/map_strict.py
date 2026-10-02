def t(f):
    try:
        print(repr(f()))
    except BaseException as e:
        print(type(e).__name__, e)


def add(*a):
    return sum(a)


t(lambda: list(map(add, [1, 2], [1, 2, 3], strict=True)))
t(lambda: list(map(add, [1, 2, 3], [1, 2], strict=True)))
t(lambda: list(map(add, [1, 2, 3], [1, 2, 3], [1], strict=True)))
t(lambda: list(map(add, [1, 2, 3], [1, 2, 3], [1, 2, 3, 4], strict=True)))
t(lambda: list(map(add, [1, 2, 3], [1, 2, 3, 4], [1, 2, 3], strict=True)))
t(lambda: list(map(add, [1, 2], [1, 2], strict=True)))
t(lambda: list(map(add, [1, 2], strict=True)))
t(lambda: list(map(add, [1, 2], [1], strict=False)))
t(lambda: list(map(add, [1, 2], [1])))
t(lambda: list(map(add, [], [], strict=True)))
t(lambda: map(len))
t(lambda: map())
t(lambda: map(len, [1], strict=True).__reduce__()[2])
t(lambda: len(map(len, [1]).__reduce__()))

import pickle
m = map(add, iter([1, 2]), iter([3, 4, 5]), strict=True)
m2 = pickle.loads(pickle.dumps(m))
t(lambda: list(m2))
