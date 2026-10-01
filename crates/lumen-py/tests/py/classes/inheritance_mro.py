class Base:
    def who(self):
        return "Base"
    def hello(self):
        return "hello " + self.who()
class Child(Base):
    def who(self):
        return "Child"
class Grand(Child):
    pass
for c in (Base(), Child(), Grand()):
    print(c.hello())
print(Grand.__mro__ == (Grand, Child, Base, object))
print([k.__name__ for k in Grand.__mro__])
print(issubclass(Grand, Base), issubclass(Base, Grand), isinstance(Grand(), Child))
print(Grand.__bases__ == (Child,), Grand.__base__ is Child)

class A:
    def f(self): return ["A"]
class B(A):
    def f(self): return ["B"] + super().f()
class C(A):
    def f(self): return ["C"] + super().f()
class D(B, C):
    def f(self): return ["D"] + super().f()
print(D().f())
print([k.__name__ for k in D.__mro__])

class P:
    def __init__(self, x):
        self.x = x
class Q(P):
    def __init__(self, x, y):
        super().__init__(x)
        self.y = y
q = Q(1, 2)
print(q.x, q.y, sorted(q.__dict__))

class Animal:
    sound = "..."
    def speak(self):
        return type(self).__name__ + " says " + self.sound
class Dog(Animal):
    sound = "woof"
class Cat(Animal):
    sound = "meow"
print(Dog().speak(), Cat().speak(), Animal().speak())

class X:
    def m(self): return "X"
class Y(X): pass
class Z(X):
    def m(self): return "Z"
class W(Y, Z): pass
print(W().m(), [k.__name__ for k in W.__mro__])
print(super(Y, Y()).m(), super(W, W()).m())
try:
    class Bad(A, B):
        pass
except TypeError:
    print("MRO conflict")
print(isinstance(3, object), type(Base).__name__, Base.__name__, Base.__qualname__)
print(object.__subclasses__ is not None, Base.__dict__.get("who") is not None)
class E(Exception): pass
print(E.__mro__[1].__name__, E.__mro__[-1].__name__)
