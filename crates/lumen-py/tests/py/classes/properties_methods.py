class Temp:
    def __init__(self, c):
        self._c = c
    @property
    def celsius(self):
        return self._c
    @celsius.setter
    def celsius(self, v):
        if v < -273:
            raise ValueError("too cold")
        self._c = v
    @celsius.deleter
    def celsius(self):
        print("deleting")
        del self._c
    @property
    def fahrenheit(self):
        return self._c * 9 / 5 + 32
t = Temp(100)
print(t.celsius, t.fahrenheit)
t.celsius = 0
print(t.celsius, t.fahrenheit)
try:
    t.celsius = -300
except ValueError as e:
    print("ValueError", e)
try:
    t.fahrenheit = 3
except AttributeError:
    print("read-only")
del t.celsius
print(hasattr(t, "_c"))
try:
    t.celsius
except AttributeError:
    print("AttributeError after del")
print(type(Temp.celsius).__name__, Temp.celsius.fget is not None)

class Counter:
    count = 0
    def __init__(self):
        Counter.count += 1
    @classmethod
    def make(cls):
        return cls()
    @classmethod
    def total(cls):
        return cls.count
    @staticmethod
    def helper(a, b):
        return a + b
class Sub(Counter):
    pass
Counter(); Counter.make(); s = Sub.make()
print(Counter.total(), Sub.total(), type(s).__name__, Counter.helper(1, 2), s.helper(3, 4))
print(Sub.count, s.count)
s.count = 100
print(s.count, Counter.count, Sub.count)

class Shared:
    items = []
    def __init__(self):
        self.own = []
a, b = Shared(), Shared()
a.items.append(1); a.own.append(2)
print(b.items, b.own, Shared.items)
b.items = ["mine"]
print(a.items, b.items, Shared.items)
m = Counter().total
print(m(), Counter.total.__name__)
class Meth:
    def f(self): return "f"
mm = Meth()
bound = mm.f
print(bound(), Meth.f(mm), bound.__self__ is mm, bound.__name__)
print(Meth.f.__name__, type(Meth.f).__name__, type(bound).__name__)
