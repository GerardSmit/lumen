def seq(x):
    match x:
        case []:
            return "empty"
        case [a]:
            return "one:%r" % (a,)
        case [a, b]:
            return "two:%r,%r" % (a, b)
        case [first, *rest] if len(rest) > 3:
            return "long:%r+%d" % (first, len(rest))
        case [first, *middle, last]:
            return "many:%r|%r|%r" % (first, middle, last)


for v in [[], [1], [1, 2], [1, 2, 3], [1, 2, 3, 4, 5, 6], (7, 8), (), "ab", "a"]:
    print(repr(v), seq(v))


def star_first(x):
    match x:
        case [*init, 0]:
            return ("ends0", init)
        case [*_, 9]:
            return "ends9"
        case [_, _, *tail]:
            return ("tail", tail)
        case _:
            return "none"


print(star_first([1, 2, 0]), star_first([9]), star_first([1, 2, 3, 4]), star_first([1]))


def mapping(x):
    match x:
        case {"type": "point", "x": px, "y": py}:
            return ("point", px, py)
        case {"type": "circle", **rest}:
            return ("circle", sorted(rest.items()))
        case {"k": v}:
            return ("k", v)
        case {}:
            return "any-mapping"
        case _:
            return "not-mapping"


print(mapping({"type": "point", "x": 1, "y": 2, "z": 3}))
print(mapping({"type": "circle", "r": 5, "c": (0, 0)}))
print(mapping({"k": None}))
print(mapping({"other": 1}))
print(mapping({}))
print(mapping([1]))
print(mapping("str"))


def nested(x):
    match x:
        case {"user": {"name": str(name), "tags": [first, *_]}}:
            return (name, first)
        case {"items": [{"id": i}, *others]}:
            return ("items", i, len(others))
        case [(a, b), [c, d]]:
            return ("pairs", a + b + c + d)
        case [[x1, y1], {"z": z1}]:
            return ("mixed", x1, y1, z1)
        case _:
            return "nope"


print(nested({"user": {"name": "ann", "tags": ["t1", "t2"]}}))
print(nested({"user": {"name": 5, "tags": ["t"]}}))
print(nested({"items": [{"id": 3}, {"id": 4}, {"id": 5}]}))
print(nested([(1, 2), [3, 4]]))
print(nested([[1, 2], {"z": 9}]))
print(nested(5))

match [1, [2, 3]]:
    case [a, [b, c]]:
        print(a, b, c)

match {"a": 1, "b": 2}:
    case {"a": 1, **others}:
        print(others)

d = {"a": 1}
match d:
    case {"a": 1, **others}:
        print(others, d)

match (1, 2, 3):
    case (x, *ys):
        print(x, ys)
