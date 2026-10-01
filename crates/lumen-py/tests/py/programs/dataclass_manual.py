MISSING = object()


class Field:
    def __init__(self, name, default=MISSING, factory=None, repr=True, compare=True):
        self.name = name
        self.default = default
        self.factory = factory
        self.repr = repr
        self.compare = compare


def field(default=MISSING, factory=None, repr=True, compare=True):
    return Field(None, default, factory, repr, compare)


def make_init(fields):
    def __init__(self, *args, **kwargs):
        if len(args) > len(fields):
            raise TypeError(f"{type(self).__name__}() takes {len(fields)} positional arguments but {len(args)} were given")
        values = dict(zip((f.name for f in fields), args))
        for k, v in kwargs.items():
            if k in values:
                raise TypeError(f"{type(self).__name__}() got multiple values for argument {k!r}")
            if k not in {f.name for f in fields}:
                raise TypeError(f"{type(self).__name__}() got an unexpected keyword argument {k!r}")
            values[k] = v
        for f in fields:
            if f.name in values:
                v = values[f.name]
            elif f.factory is not None:
                v = f.factory()
            elif f.default is not MISSING:
                v = f.default
            else:
                raise TypeError(f"{type(self).__name__}() missing required argument: {f.name!r}")
            setattr(self, f.name, v)
        post = getattr(self, "__post_init__", None)
        if post:
            post()
    return __init__


def make_repr(fields):
    def __repr__(self):
        parts = ", ".join(f"{f.name}={getattr(self, f.name)!r}" for f in fields if f.repr)
        return f"{type(self).__name__}({parts})"
    return __repr__


def make_eq(fields):
    def __eq__(self, other):
        if other.__class__ is not self.__class__:
            return NotImplemented
        return all(getattr(self, f.name) == getattr(other, f.name) for f in fields if f.compare)
    return __eq__


def make_order(fields):
    def key(o):
        return tuple(getattr(o, f.name) for f in fields if f.compare)

    def __lt__(self, other):
        if other.__class__ is not self.__class__:
            return NotImplemented
        return key(self) < key(other)

    def __le__(self, other):
        if other.__class__ is not self.__class__:
            return NotImplemented
        return key(self) <= key(other)
    return __lt__, __le__


def dataclass(cls=None, *, order=False, frozen=False):
    def wrap(cls):
        fields = []
        for name, default in getattr(cls, "__annotations__", {}).items():
            raw = cls.__dict__.get(name, MISSING)
            if isinstance(raw, Field):
                raw.name = name
                f = raw
                if f.default is not MISSING:
                    setattr(cls, name, f.default)
                elif hasattr(cls, name):
                    delattr(cls, name)
            else:
                f = Field(name, raw)
            fields.append(f)
        cls.__fields__ = tuple(fields)
        cls.__init__ = make_init(fields)
        cls.__repr__ = make_repr(fields)
        cls.__eq__ = make_eq(fields)
        cls.__hash__ = None if not frozen else (lambda self: hash(tuple(getattr(self, f.name) for f in fields)))
        if order:
            cls.__lt__, cls.__le__ = make_order(fields)
        if frozen:
            def __setattr__(self, name, value):
                if name in {f.name for f in fields} and hasattr(self, "_frozen_done"):
                    raise AttributeError(f"cannot assign to field {name!r}")
                object.__setattr__(self, name, value)
            cls.__setattr__ = __setattr__
            orig_init = cls.__init__

            def __init__(self, *a, **k):
                orig_init(self, *a, **k)
                object.__setattr__(self, "_frozen_done", True)
            cls.__init__ = __init__
        return cls
    return wrap if cls is None else wrap(cls)


def asdict(obj):
    return {f.name: getattr(obj, f.name) for f in obj.__fields__}


def replace(obj, **changes):
    d = asdict(obj)
    d.update(changes)
    return type(obj)(**d)


@dataclass
class Point:
    x: int
    y: int = 0


@dataclass(order=True)
class Version:
    major: int
    minor: int = 0
    patch: int = 0
    label: str = field(default="", compare=False)


@dataclass
class Bag:
    name: str
    items: list = field(factory=list)
    secret: str = field(default="hunter2", repr=False)

    def __post_init__(self):
        self.name = self.name.strip().title()

    def add(self, x):
        self.items.append(x)
        return self


@dataclass(frozen=True)
class Coord:
    lat: int
    lon: int


p = Point(1, 2)
print(p, Point(5), Point(x=3, y=4))
print(p == Point(1, 2), p == Point(1, 3), p == (1, 2), p != Point(9))
print(asdict(p), Point.__fields__[1].default)
print(replace(p, y=10), p)
print(Point.__hash__)

vs = [Version(1, 2, 3), Version(1, 0), Version(0, 9, 9, "old"), Version(1, 2, 3, "dup"), Version(2)]
print(sorted(vs))
print(vs[0] == vs[3], vs[0] < vs[3], vs[0] <= vs[3], max(vs))

b1 = Bag("  alice ")
b2 = Bag(" bob")
b1.add(1).add(2)
print(b1, b2, b1.items is b2.items)
print(b1.secret)

c = Coord(1, 2)
print(c, hash(c) == hash(Coord(1, 2)), {c: "here"}[Coord(1, 2)])
try:
    c.lat = 5
except AttributeError as ex:
    print("AttributeError:", ex)

for thunk in (lambda: Point(), lambda: Point(1, 2, 3), lambda: Point(1, x=2), lambda: Point(1, z=3)):
    try:
        thunk()
    except TypeError as ex:
        print("TypeError:", ex)

print(sorted([Point(3, 1), Point(1, 1)], key=lambda q: (q.x, q.y)))
print(len(Point.__fields__), Point.__name__, type(p).__name__)
