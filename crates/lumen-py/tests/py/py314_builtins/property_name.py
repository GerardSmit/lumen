def t(f):
    try:
        print(repr(f()))
    except BaseException as e:
        print(type(e).__name__, e)


def g():
    return 1


p = property(g)
t(lambda: p.__name__)
p.__name__ = 'x'
t(lambda: p.__name__)
del p.__name__
t(lambda: p.__name__)
t(lambda: property().__name__)
t(lambda: property(len).__name__)
t(lambda: property(lambda: 0, doc='x').__name__)


class NoGetattr:
    def __getattr__(self, name):
        raise AttributeError(name)


class Boom:
    def __getattr__(self, name):
        raise KeyError(name)


t(lambda: property(NoGetattr()).__name__)
t(lambda: property(Boom()).__name__)


class B:
    @property
    def foo(self):
        return 1

    bar = property(lambda self: 2)
    baz = property(g)


t(lambda: B.foo.__name__)
t(lambda: B.bar.__name__)
t(lambda: B.baz.__name__)
t(lambda: B.foo.setter(lambda self, v: None).__name__)
t(lambda: B.bar.getter(g).__name__)


class Sub(property):
    pass


t(lambda: Sub().__name__)
t(lambda: Sub(g).__name__)
