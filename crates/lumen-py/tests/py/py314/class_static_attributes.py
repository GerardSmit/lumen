# 3.13: __static_attributes__ and __firstlineno__ on classes; co_firstlineno of decorated defs.
def deco(x):
    return x


class Point:
    def __init__(self, x, y):
        self.x = x
        self.y: int = y
        self.count += 1
        del self.gone
        other = self
        other.not_self = 1

    def move(self):
        self.x, (self.y, *self.rest) = 1, (2, 3)
        for self.loop in range(1):
            pass
        [0 for self.comp in range(1)]

        def inner():
            self.from_inner = 1

        class Nested:
            def m(self):
                self.nested_only = 1

        return Nested


print(Point.__static_attributes__)
print(Point.move(object.__new__(Point)).__static_attributes__)
print(Point.__firstlineno__, Point.__init__.__code__.co_firstlineno)


class Empty:
    pass


print(Empty.__static_attributes__, Empty.__firstlineno__)


@deco

@deco
class Decorated:
    pass


@deco
def decorated_fn():
    pass


print(Decorated.__firstlineno__, decorated_fn.__code__.co_firstlineno)
print("__firstlineno__" in Decorated.__dict__, "__static_attributes__" in Decorated.__dict__)
print([k for k in Point.__dict__ if k.startswith("__") and k not in ("__dict__", "__weakref__")])
