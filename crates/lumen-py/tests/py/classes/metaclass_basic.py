registry = {}


class Meta(type):
    def __new__(mcs, name, bases, ns):
        print("Meta.__new__", name, sorted(k for k in ns if not k.startswith("__")))
        cls = super().__new__(mcs, name, bases, ns)
        registry[name] = cls
        cls.created_by = mcs.__name__
        return cls

    def __init__(cls, name, bases, ns):
        print("Meta.__init__", name)
        super().__init__(name, bases, ns)

    def __call__(cls, *args, **kwargs):
        print("Meta.__call__", cls.__name__, args)
        obj = super().__call__(*args, **kwargs)
        obj.tagged = True
        return obj

    def describe(cls):
        return "class " + cls.__name__


class Base(metaclass=Meta):
    def __init__(self, v=0):
        self.v = v

    def hello(self):
        return "hello"


class Child(Base):
    extra = 1


print(sorted(registry))
print(Base.created_by, Child.created_by)
print(type(Base).__name__, type(Child).__name__)
b = Base(5)
print(b.v, b.tagged)
c = Child()
print(c.v, c.tagged)
print(Base.describe(), Child.describe())
try:
    b.describe()
except AttributeError:
    print("metaclass methods not on instances")
print(isinstance(Base, Meta), isinstance(b, Meta), issubclass(Child, Base))

Dyn = type("Dyn", (object,), {"x": 1, "get": lambda self: self.x * 2})
d = Dyn()
print(Dyn.__name__, d.x, d.get(), type(Dyn).__name__)

Dyn2 = type("Dyn2", (Dyn,), {"x": 10})
print(Dyn2().get(), Dyn2.__mro__[1].__name__, issubclass(Dyn2, Dyn))

Dyn3 = Meta("Dyn3", (Base,), {"z": 3})
print(Dyn3.z, Dyn3.created_by, "Dyn3" in registry)
print(Dyn3(1).v)


class Singleton(type):
    _inst = {}

    def __call__(cls, *a, **k):
        if cls not in cls._inst:
            cls._inst[cls] = super().__call__(*a, **k)
        return cls._inst[cls]


class Conf(metaclass=Singleton):
    def __init__(self):
        print("Conf init")
        self.n = 0


c1, c2 = Conf(), Conf()
print(c1 is c2)


class Upper(type):
    def __new__(mcs, name, bases, ns):
        ns = {(k.upper() if not k.startswith("__") else k): v for k, v in ns.items()}
        return super().__new__(mcs, name, bases, ns)


class Shout(metaclass=Upper):
    def greet(self):
        return "hi"
    value = 5


print(hasattr(Shout, "greet"), Shout.GREET(Shout()), Shout.VALUE)


class Plugin(type):
    plugins = []

    def __init__(cls, name, bases, ns):
        super().__init__(name, bases, ns)
        if bases:
            Plugin.plugins.append(name)


class PBase(metaclass=Plugin):
    pass


class P1(PBase):
    pass


class P2(P1):
    pass


print(Plugin.plugins)
print(type(type).__name__, type(Meta).__name__, Meta.__mro__[1].__name__)
print(type(5).__name__, type("s") is str, type(None).__name__)
