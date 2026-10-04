# PEP 695 generic classes and annotation scopes inside classes.
import typing


class Box[T]:
    def __init__(self, item: T):
        self.item = item

    def get(self) -> T:
        return self.item


(T,) = Box.__type_params__
print(Box.__type_params__, Box.__bases__, Box.__orig_bases__, Box.__parameters__)
print(Box.__mro__)
print(Box[int], Box[int]().__class__ if False else Box(3).get())
print(Box.__init__.__annotations__["item"] is T, Box.get.__qualname__)
print(isinstance(Box(1), Box), issubclass(Box, typing.Generic))


class Base: pass

class Child[T, *Ts, **P](Base, metaclass=type):
    pass

print(Child.__type_params__, Child.__bases__, Child.__orig_bases__, Child.__parameters__)


class Sub(Box[int]):
    pass

print(Sub.__type_params__, Sub.__orig_bases__, Sub.__parameters__)


class Sub2[U](Box[U]):
    pass

print(Sub2.__orig_bases__, Sub2.__parameters__, Sub2[str])

try:
    class Bad[T](typing.Generic[T]): pass
except TypeError as e:
    print("TypeError", e)


# Annotation scopes inside a class see the class namespace.
class Scoped:
    Attr = int
    type Alias = Attr
    def m[V: Attr](self, x: Attr) -> V: pass
    type GAlias[W] = dict[W, Attr]

print(Scoped.Alias.__value__, Scoped.m.__type_params__[0].__bound__)
print(Scoped.m.__annotations__["x"], Scoped.GAlias.__value__)
Scoped.Attr = str
print(Scoped.Alias.__value__, Scoped.Late.__value__ if hasattr(Scoped, "Late") else None)

class Live:
    X = int
    type A = X
Live.X = str
print(Live.A.__value__)


# Lazy evaluation falls back from the class namespace to globals.
GLOBAL = "global"
class Fallback:
    type A = GLOBAL
print(Fallback.A.__value__)

# Names bound in an enclosing function are visible too.
def outer():
    local = bytes
    class C:
        type A = local
        def m[T: local](self): pass
    return C

C = outer()
print(C.A.__value__, C.m.__type_params__[0].__bound__)

# Name mangling applies inside the type-parameter scope of a class.
class __Mangled[__T]:
    pass
print(__Mangled.__type_params__)

class M:
    def f[__U](self, x: __U): pass
print(M.f.__type_params__, M.f.__annotations__)

# super() keeps working in generic methods and classes.
class Parent:
    def hello(self):
        return "parent"

class Kid[T](Parent):
    def hello[S](self):
        return "kid+" + super().hello()

print(Kid().hello())

# __type_params__ is per class and writable.
class Plain: pass
print(Plain.__type_params__, int.__type_params__)
Plain.__type_params__ = (T,)
print(Plain.__type_params__)

def f(): pass
f.__type_params__ = (T,)
print(f.__type_params__)

# Starred bases
bases = (Base,)
class Star[T](*bases): pass
print(Star.__bases__)
class Star2(*bases, **{}): pass
print(Star2.__bases__)

# TypeVar operations through typing
print((T | int).__args__, (int | T).__args__, typing.Optional[T].__args__)
print(typing.get_type_hints(Box.__init__))
