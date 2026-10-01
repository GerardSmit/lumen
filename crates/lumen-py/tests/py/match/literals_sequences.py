def lit(x):
    match x:
        case 0:
            return "zero"
        case 1 | 2 | 3:
            return "small"
        case -1:
            return "neg one"
        case 3.5:
            return "float"
        case "hi":
            return "greeting"
        case "a" | "b":
            return "letter"
        case None:
            return "none"
        case True:
            return "true"
        case False:
            return "false"
        case b"x":
            return "bytes"
        case _:
            return "other"

for v in [0, 1, 3, -1, 3.5, "hi", "b", None, True, False, b"x", 99, "zz", 1.0]:
    print(repr(v), lit(v))

def seq(x):
    match x:
        case []:
            return "empty"
        case [a]:
            return f"one {a}"
        case [a, b]:
            return f"two {a} {b}"
        case [1, *rest]:
            return f"starts 1, rest {rest}"
        case [a, *mid, z]:
            return f"first {a} last {z} mid {mid}"
        case (a, b, c, d):
            return "unreachable"
        case _:
            return "no"

for v in [[], [5], (1, 2), [1, 2, 3], [9, 8, 7, 6], (4, 5, 6), "ab", 5, {1: 2}]:
    print(repr(v), seq(v))

match (1, (2, 3)):
    case (a, (b, c)):
        print(a, b, c)
match [1, 2, 3]:
    case [_, _, _] as whole:
        print("whole", whole)
match "abc":
    case str() as s:
        print("str", s)
match 5:
    case int(n) if n > 3:
        print("big int", n)
match [1, [2, [3]]]:
    case [x, [y, [z]]]:
        print(x + y + z)
