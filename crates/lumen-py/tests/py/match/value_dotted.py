class Color:
    RED = 1
    GREEN = 2
    BLUE = 3


class Cfg:
    class Inner:
        LIMIT = 10

    NAME = "cfg"


def color(c):
    match c:
        case Color.RED:
            return "red"
        case Color.GREEN | Color.BLUE:
            return "green-or-blue"
        case _:
            return "unknown"


print([color(i) for i in range(5)])


def nested_dotted(v):
    match v:
        case Cfg.Inner.LIMIT:
            return "limit"
        case Cfg.NAME:
            return "name"
        case _:
            return "?"


print(nested_dotted(10), nested_dotted("cfg"), nested_dotted(11))

import math


def mathy(v):
    match v:
        case math.pi:
            return "pi"
        case math.inf:
            return "inf"
        case _:
            return "other"


print(mathy(math.pi), mathy(float("inf")), mathy(3))


def as_pat(x):
    match x:
        case [1, 2] as whole:
            return ("whole", whole)
        case (int() | float()) as num:
            return ("num", num)
        case [a, [b, c] as inner]:
            return ("inner", a, inner, b + c)
        case {"k": (1 | 2) as kv}:
            return ("kv", kv)
        case str() as s if s.startswith("x"):
            return ("x-str", s)
        case _:
            return None


print(as_pat([1, 2]))
print(as_pat(5))
print(as_pat(2.5))
print(as_pat([0, [3, 4]]))
print(as_pat({"k": 2}))
print(as_pat({"k": 3}))
print(as_pat("xyz"))
print(as_pat("abc"))


trace = []


def guard_fallthrough(x):
    match x:
        case n if trace.append(("g1", n)) or n > 10:
            return "big"
        case n if trace.append(("g2", n)) or n > 5:
            return "medium"
        case n:
            trace.append(("last", n))
            return "small"


print(guard_fallthrough(20), guard_fallthrough(7), guard_fallthrough(1))
print(trace)


def bind_then_fail(x):
    match x:
        case [a, b] if a > b:
            return "desc"
        case [a, b] if a < b:
            return "asc"
        case [a, b]:
            return "equal"
        case _:
            return "bad"


print(bind_then_fail([2, 1]), bind_then_fail([1, 2]), bind_then_fail([1, 1]), bind_then_fail([1]))


def nomatch(x):
    match x:
        case 1:
            return "one"
    return "fell out"


print(nomatch(1), nomatch(2))

def last_wild(x):
    match x:
        case Color.RED:
            return "r"
        case _:
            return "wild"


print(last_wild(1), last_wild(99))

x = 5
match x:
    case 5 if x > 10:
        print("not here")
    case 5 if x > 1:
        print("here")
    case _:
        print("never")

point = (1, 2)
match point:
    case (0, 0):
        print("origin")
    case (x, 0) | (0, x):
        print("axis", x)
    case (x, y):
        print("plane", x, y)

match 0:
    case 0 | 1 | 2 as v:
        print("v", v)
