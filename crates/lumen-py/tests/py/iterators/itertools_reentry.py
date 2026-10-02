# Re-entering a running chain, and tee.__copy__.
import itertools
import copy


def t(f):
    try:
        print(f())
    except Exception as e:
        print(type(e).__name__, e)


def gen():
    yield 1
    next(c)
    yield 2


c = itertools.chain(gen())
t(lambda: list(c))


def gen2():
    yield 1
    next(c2)
    yield 2


c2 = itertools.chain.from_iterable([gen2()])
t(lambda: list(c2))



a, b = itertools.tee(range(5))
next(a)
a2 = copy.copy(a)
a3 = a.__copy__()
print(list(a), list(a2), list(a3), list(b))
a, = itertools.tee(iter([1, 2, 3]), 1)
print(type(a).__name__, hasattr(a, "__copy__"))
c = a.__copy__()
print(next(a), list(c), list(a))
x, y = itertools.tee([1, 2])
print(type(x) is type(y), type(itertools.tee(x)[0]) is type(x))
x1, x2 = itertools.tee(x)
