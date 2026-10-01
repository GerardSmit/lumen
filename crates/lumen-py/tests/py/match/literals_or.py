def classify(x):
    match x:
        case 0:
            return "zero"
        case 1 | 2:
            return "one-or-two"
        case 3 | 4 | 5 as n:
            return "three-to-five:%d" % n
        case -1:
            return "minus one"
        case 1.5:
            return "float 1.5"
        case "hello":
            return "greeting"
        case "a" | "b":
            return "a-or-b"
        case True:
            return "true"
        case False:
            return "false"
        case None:
            return "none"
        case b"bytes":
            return "bytes"
        case n if isinstance(n, int) and n > 100:
            return "big:%d" % n
        case int(n):
            return "int:%d" % n
        case _:
            return "other"


for v in [0, 1, 2, 3, 5, 6, -1, 1.5, "hello", "a", "b", "c", None, 1000, b"bytes", [], 2.5]:
    print(repr(v), classify(v))

print(classify(True), classify(False))


def guard(x):
    match x:
        case n if n < 0:
            return "neg"
        case n if n == 0:
            return "zero"
        case n if n % 2 == 0:
            return "even"
        case n:
            return "odd %d" % n


print([guard(i) for i in range(-1, 5)])


def capture(x):
    match x:
        case y:
            return ("captured", y)


print(capture(7), capture("s"))


def wild(x):
    match x:
        case 1:
            return "one"
        case _:
            return "wild"


print(wild(1), wild(2), wild(None))

match 5:
    case 5:
        print("five")

match "x":
    case "y":
        print("y")
    case "x":
        print("x")

match 3:
    case 1:
        pass
print("no match falls through")


def str_prefix(cmd):
    match cmd.split():
        case ["go", d]:
            return "go " + d
        case ["quit"]:
            return "quit"
        case _:
            return "unknown"


print(str_prefix("go north"), str_prefix("quit"), str_prefix("dance now"))

calls = []


def side(v):
    calls.append(v)
    return v


match side(2):
    case 1:
        pass
    case 2:
        pass
print(calls)
