class A:
    def who(self):
        return ["A"]

    def __init__(self):
        print("A.__init__")


class B(A):
    def who(self):
        return ["B"] + super().who()

    def __init__(self):
        print("B.__init__")
        super().__init__()


class C(A):
    def who(self):
        return ["C"] + super().who()

    def __init__(self):
        print("C.__init__")
        super().__init__()


class D(B, C):
    def who(self):
        return ["D"] + super().who()

    def __init__(self):
        print("D.__init__")
        super().__init__()


print([c.__name__ for c in D.__mro__])
print([c.__name__ for c in B.__mro__])
d = D()
print(d.who())
print(B().who())
print([c.__name__ for c in B.__mro__])


class Left(object):
    def __init__(self, **kw):
        print("Left", sorted(kw))
        super().__init__(**kw)


class Right(object):
    def __init__(self, **kw):
        print("Right", sorted(kw))
        super().__init__()


class Both(Left, Right):
    def __init__(self, **kw):
        print("Both", sorted(kw))
        super().__init__(**kw)


Both(a=1)
print([c.__name__ for c in Both.__mro__])


class Counter:
    @classmethod
    def create(cls):
        return [cls.__name__]

    @staticmethod
    def st():
        return "st"


class Sub(Counter):
    @classmethod
    def create(cls):
        return ["Sub"] + super().create()


class SubSub(Sub):
    @classmethod
    def create(cls):
        return ["SubSub"] + super().create()


print(SubSub.create())
print(Sub.create())


class Mixin:
    def hello(self):
        return "mixin:" + super().hello()


class Base:
    def hello(self):
        return "base"


class Mixed(Mixin, Base):
    pass


print(Mixed().hello())
print([c.__name__ for c in Mixed.__mro__])

try:
    class X(A, B):
        pass
except TypeError:
    print("MRO conflict TypeError")

try:
    class Y(object, A):
        pass
except TypeError:
    print("MRO conflict 2")


class Z(B, A):
    pass


print([c.__name__ for c in Z.__mro__])


class P:
    def val(self):
        return 1

    @property
    def prop(self):
        return "P.prop"


class Q(P):
    def val(self):
        return super().val() + 10

    @property
    def prop(self):
        return "Q>" + super().prop


print(Q().val(), Q().prop)


print(super(D, d).who())
print(super(B, d).who())
print(D.__mro__[0] is D, D.__mro__[-1] is object)
print(issubclass(D, A), isinstance(d, C))
