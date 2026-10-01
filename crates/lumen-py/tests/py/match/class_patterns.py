class Point:
    __match_args__ = ("x", "y")

    def __init__(self, x, y):
        self.x = x
        self.y = y


class Point3(Point):
    __match_args__ = ("x", "y", "z")

    def __init__(self, x, y, z):
        super().__init__(x, y)
        self.z = z


class NoArgs:
    def __init__(self, v):
        self.v = v


def where(p):
    match p:
        case Point3(0, 0, 0):
            return "origin3"
        case Point(0, 0):
            return "origin"
        case Point(x=0, y=y):
            return "on y axis at %r" % y
        case Point(x, 0):
            return "on x axis at %r" % x
        case Point(x=a, y=b) if a == b:
            return "diagonal %r" % a
        case Point(a, b):
            return "point %r,%r" % (a, b)
        case _:
            return "not a point"


for p in [Point(0, 0), Point(0, 5), Point(3, 0), Point(2, 2), Point(1, 2), Point3(0, 0, 0), Point3(1, 2, 3), 5]:
    print(where(p))


def builtin(x):
    match x:
        case bool(b):
            return ("bool", b)
        case int(n):
            return ("int", n)
        case float(f):
            return ("float", f)
        case str() as s:
            return ("str", s)
        case list([first, *_]):
            return ("list", first)
        case tuple():
            return "tuple"
        case dict():
            return "dict"
        case None:
            return "none"
        case _:
            return "other"


for v in [True, 1, 2.5, "s", [9, 8], [], (1,), {}, None, {1}]:
    print(builtin(v))


def kw(x):
    match x:
        case NoArgs(v=1):
            return "v is one"
        case NoArgs(v=[a, b]):
            return "v pair %r %r" % (a, b)
        case NoArgs(v=v):
            return "v other %r" % (v,)


print(kw(NoArgs(1)), kw(NoArgs([1, 2])), kw(NoArgs("z")))

try:
    match NoArgs(1):
        case NoArgs(1):
            pass
except TypeError:
    print("TypeError positional without __match_args__")


def attr_missing(x):
    match x:
        case NoArgs(missing=1):
            return "has missing"
        case _:
            return "no attr"


print(attr_missing(NoArgs(1)))


class Shape:
    pass


class Circle(Shape):
    __match_args__ = ("r",)

    def __init__(self, r):
        self.r = r


class Rect(Shape):
    __match_args__ = ("w", "h")

    def __init__(self, w, h):
        self.w, self.h = w, h


def area(s):
    match s:
        case Circle(r):
            return 3 * r * r
        case Rect(w, h):
            return w * h
        case Shape():
            return "unknown shape"


print(area(Circle(2)), area(Rect(2, 5)), area(Shape()))

match [Point(1, 2), Point(3, 4)]:
    case [Point(x=a), Point(y=b)]:
        print(a, b)

match Point(1, Point(2, 3)):
    case Point(a, Point(b, c)):
        print(a, b, c)
