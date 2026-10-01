def mp(x):
    match x:
        case {}:
            return "empty-or-any-dict"
        case _:
            return "x"
print(mp({}), mp({1: 2}), mp([]))

def cfg(x):
    match x:
        case {"type": "circle", "r": r}:
            return f"circle {r}"
        case {"type": "rect", "w": w, "h": h}:
            return f"rect {w}x{h}"
        case {"type": t, **rest}:
            return f"unknown {t} {rest}"
        case {"items": [first, *_]}:
            return f"first item {first}"
        case _:
            return "bad"

print(cfg({"type": "circle", "r": 2}))
print(cfg({"type": "rect", "w": 3, "h": 4, "extra": 1}))
print(cfg({"type": "tri", "a": 1, "b": 2}))
print(cfg({"items": [7, 8]}))
print(cfg({"items": []}))
print(cfg(5))

class Point:
    __match_args__ = ("x", "y")
    def __init__(self, x, y):
        self.x, self.y = x, y

class Circle:
    __match_args__ = ("center", "r")
    def __init__(self, center, r):
        self.center, self.r = center, r

def shape(s):
    match s:
        case Point(0, 0):
            return "origin"
        case Point(x, 0):
            return f"on x-axis at {x}"
        case Point(x=0, y=y):
            return f"on y-axis at {y}"
        case Point(x, y) if x == y:
            return f"diagonal {x}"
        case Point(x, y):
            return f"point {x},{y}"
        case Circle(Point(cx, cy), r):
            return f"circle at {cx},{cy} r={r}"
        case _:
            return "unknown"

for s in [Point(0, 0), Point(3, 0), Point(0, 4), Point(2, 2), Point(1, 2), Circle(Point(1, 1), 5), "x"]:
    print(shape(s))

class NoArgs:
    def __init__(self):
        self.v = 1
match NoArgs():
    case NoArgs(v=1):
        print("kw match")
try:
    match NoArgs():
        case NoArgs(1):
            print("x")
except TypeError:
    print("TypeError positional")

def kinds(x):
    match x:
        case int() | float():
            return "number"
        case str():
            return "string"
        case list() | tuple():
            return "seq"
        case dict():
            return "dict"
        case bool():
            return "unreachable"
        case _:
            return "other"
print([kinds(v) for v in [1, 2.0, "s", [1], (1,), {}, None, True]])
