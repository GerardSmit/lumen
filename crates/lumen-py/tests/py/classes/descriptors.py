class Data:
    def __set_name__(self, owner, name):
        self.name = name
        print("set_name", owner.__name__, name)

    def __get__(self, obj, objtype=None):
        if obj is None:
            print("class access", self.name, objtype.__name__)
            return self
        print("get", self.name)
        return obj.__dict__.get("_" + self.name, "unset")

    def __set__(self, obj, value):
        print("set", self.name, value)
        obj.__dict__["_" + self.name] = value

    def __delete__(self, obj):
        print("delete", self.name)
        obj.__dict__.pop("_" + self.name, None)


class NonData:
    def __set_name__(self, owner, name):
        self.name = name

    def __get__(self, obj, objtype=None):
        if obj is None:
            return self
        return "nondata:" + self.name


class Model:
    a = Data()
    b = NonData()
    plain = 1


m = Model()
print(m.a)
m.a = 5
print(m.a)
print(sorted(m.__dict__.items()))
del m.a
print(m.a)

print(m.b)
m.b = "shadow"
print(m.b)
print(sorted(m.__dict__.items()))
del m.b
print(m.b)

m.__dict__["a"] = "from dict"
print(m.a)

print(type(Model.a).__name__)
print(Model.b is Model.__dict__["b"])
print(Model.plain, m.plain)


class Child(Model):
    pass


c = Child()
c.a = 9
print(c.a)
print(Child.a is Model.a)


class Typed:
    def __init__(self, kind):
        self.kind = kind

    def __set_name__(self, owner, name):
        self.attr = "_t_" + name

    def __get__(self, obj, objtype=None):
        if obj is None:
            return self
        return getattr(obj, self.attr, None)

    def __set__(self, obj, value):
        if not isinstance(value, self.kind):
            raise TypeError("%s must be %s" % (self.attr[3:], self.kind.__name__))
        setattr(obj, self.attr, value)


class Person:
    name = Typed(str)
    age = Typed(int)

    def __init__(self, name, age):
        self.name = name
        self.age = age


p = Person("ann", 30)
print(p.name, p.age)
try:
    p.age = "old"
except TypeError as e:
    print(e)
print(p.age, sorted(p.__dict__))


class ReadOnly:
    def __init__(self, v):
        self.v = v

    def __get__(self, obj, objtype=None):
        return self.v

    def __set__(self, obj, value):
        raise AttributeError("read-only")


class Holder:
    const = ReadOnly(3)


h = Holder()
print(h.const)
try:
    h.const = 4
except AttributeError as e:
    print(e)

class Cached:
    def __init__(self, fn):
        self.fn = fn

    def __set_name__(self, owner, name):
        self.name = name

    def __get__(self, obj, objtype=None):
        if obj is None:
            return self
        val = self.fn(obj)
        obj.__dict__[self.name] = val
        return val


class Calc:
    calls = 0

    @Cached
    def value(self):
        Calc.calls += 1
        return 42


k = Calc()
print(k.value, k.value, Calc.calls)

print(isinstance(Calc.value, Cached))


class Bound:
    def method(self):
        return "m"


print(type(Bound.method).__name__, type(Bound().method).__name__)
print(Bound.method(Bound()), Bound.__dict__["method"].__get__(Bound(), Bound)())
