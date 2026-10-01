MISSING = object()


class FrozenInstanceError(AttributeError):
    pass


class Field:
    def __init__(self, default=MISSING, default_factory=None, compare=True, repr=True):
        self.default = default
        self.default_factory = default_factory
        self.compare = compare
        self.repr = repr


def field(**kwargs):
    return Field(**kwargs)


def collect_fields(cls):
    fields = []
    seen = {}
    for base in reversed(cls.__mro__[1:]):
        for f in getattr(base, "__fields__", []):
            seen[f[0]] = f
    for name, f in seen.items():
        fields.append(f)
    own = cls.__annotations__
    for name, typ in own.items():
        default = cls.__dict__.get(name, MISSING)
        if isinstance(default, Field):
            spec = default
        else:
            spec = Field(default=default)
        entry = (name, typ, spec)
        replaced = False
        for idx, old in enumerate(fields):
            if old[0] == name:
                fields[idx] = entry
                replaced = True
        if not replaced:
            fields.append(entry)
    return fields


def dataclass_like(cls=None, *, frozen=False, order=False, repr=True):
    def wrap(cls):
        fields = collect_fields(cls)
        seen_default = False
        for name, typ, spec in fields:
            has_default = spec.default is not MISSING or spec.default_factory is not None
            if has_default:
                seen_default = True
            elif seen_default:
                raise TypeError("non-default argument %r follows default argument" % name)
        cls.__fields__ = fields

        def __init__(self, *args, **kwargs):
            if len(args) > len(fields):
                raise TypeError("__init__() takes %d positional arguments but %d were given" % (len(fields) + 1, len(args) + 1))
            values = {}
            for (name, typ, spec), arg in zip(fields, args):
                values[name] = arg
            for key, val in kwargs.items():
                if key in values:
                    raise TypeError("got multiple values for argument %r" % key)
                if key not in [f[0] for f in fields]:
                    raise TypeError("unexpected keyword argument %r" % key)
                values[key] = val
            for name, typ, spec in fields:
                if name in values:
                    continue
                if spec.default_factory is not None:
                    values[name] = spec.default_factory()
                elif spec.default is not MISSING:
                    values[name] = spec.default
                else:
                    raise TypeError("missing required argument: %r" % name)
            for name, val in values.items():
                object.__setattr__(self, name, val)

        cls.__init__ = __init__

        if repr:
            def __repr__(self):
                parts = ["%s=%r" % (n, getattr(self, n)) for n, t, s in fields if s.repr]
                return "%s(%s)" % (type(self).__name__, ", ".join(parts))
            cls.__repr__ = __repr__

        def key(self):
            return tuple(getattr(self, n) for n, t, s in fields if s.compare)

        def __eq__(self, other):
            if other.__class__ is not self.__class__:
                return NotImplemented
            return key(self) == key(other)
        cls.__eq__ = __eq__

        if order:
            def check(self, other):
                return other.__class__ is self.__class__
            cls.__lt__ = lambda self, other: key(self) < key(other) if check(self, other) else NotImplemented
            cls.__le__ = lambda self, other: key(self) <= key(other) if check(self, other) else NotImplemented
            cls.__gt__ = lambda self, other: key(self) > key(other) if check(self, other) else NotImplemented
            cls.__ge__ = lambda self, other: key(self) >= key(other) if check(self, other) else NotImplemented

        if frozen:
            def __setattr__(self, name, value):
                raise FrozenInstanceError("cannot assign to field %r" % name)

            def __delattr__(self, name):
                raise FrozenInstanceError("cannot delete field %r" % name)
            cls.__setattr__ = __setattr__
            cls.__delattr__ = __delattr__
            cls.__hash__ = lambda self: hash(key(self))
        else:
            cls.__hash__ = None
        return cls

    if cls is None:
        return wrap
    return wrap(cls)


def astuple(obj):
    return tuple(getattr(obj, f[0]) for f in obj.__fields__)


def asdict(obj):
    return {f[0]: getattr(obj, f[0]) for f in obj.__fields__}


@dataclass_like
class Point:
    x: int
    y: int = 0


@dataclass_like(order=True)
class Version:
    major: int
    minor: int = 0
    patch: int = 0
    label: str = field(default="", compare=False)


@dataclass_like(frozen=True)
class Color:
    r: int
    g: int
    b: int

    def hex(self):
        return "#%02x%02x%02x" % (self.r, self.g, self.b)


@dataclass_like
class Bag:
    name: str
    items: list = field(default_factory=list)
    secret: str = field(default="s3", repr=False)


@dataclass_like
class Point3(Point):
    z: int = 0


def main():
    p = Point(3)
    print(p, Point(1, 2), Point(y=5, x=4))
    print(p == Point(3, 0), p == Point(3, 1), p == (3, 0))
    print(astuple(p), asdict(Point(7, 8)))
    print(Point.__fields__[0][0], [f[0] for f in Point.__fields__])
    try:
        hash(p)
    except TypeError:
        print("Point unhashable")
    p.x = 10
    print(p)

    versions = [Version(1, 2, 3), Version(1, 0), Version(0, 9, 9, "beta"), Version(1, 2, 3, "rc"), Version(2)]
    print(sorted(versions))
    print(Version(1, 2, 3) == Version(1, 2, 3, "other"))
    print(Version(1, 2) < Version(1, 3), Version(1, 2) >= Version(1, 2), Version(1) > Version(0, 99))
    print(max(versions), min(versions))
    try:
        Point(1) < Point(2)
    except TypeError:
        print("Point has no ordering")

    c = Color(255, 128, 0)
    print(c, c.hex())
    try:
        c.r = 1
    except FrozenInstanceError as e:
        print("FrozenInstanceError:", e)
    try:
        del c.g
    except AttributeError as e:
        print("AttributeError:", e)
    print({c: "orange"}[Color(255, 128, 0)], hash(c) == hash(Color(255, 128, 0)))

    b1, b2 = Bag("a"), Bag("b")
    b1.items.append(1)
    print(b1, b2, b1.secret)

    p3 = Point3(1, 2, 3)
    print(p3, [f[0] for f in Point3.__fields__])
    print(Point3(1) == Point3(1, 0, 0), Point3(1) == Point(1))

    for bad in [lambda: Point(), lambda: Point(1, 2, 3), lambda: Point(1, z=3),
                lambda: Point(1, x=3)]:
        try:
            bad()
        except TypeError as e:
            print("TypeError:", e)
    try:
        @dataclass_like
        class Bad:
            a: int = 1
            b: int
    except TypeError as e:
        print("TypeError:", e)
    print(Point.__annotations__ == {"x": int, "y": int})


main()
