def classify(n):
    match n:
        case x if x < 0:
            return "negative"
        case 0:
            return "zero"
        case x if x % 2 == 0:
            return f"even {x}"
        case x:
            return f"odd {x}"
print([classify(i) for i in [-5, 0, 4, 7]])

def cmd(c):
    match c.split():
        case ["go", ("north" | "south") as d]:
            return "heading " + d
        case ["go", d]:
            return "cannot go " + d
        case ["take", *items] if items:
            return "taking " + ",".join(items)
        case ["quit" | "exit"]:
            return "bye"
        case []:
            return "nothing"
        case [verb, *_]:
            return "unknown verb " + verb
for c in ["go north", "go west", "take a b", "take", "quit", "", "dance now"]:
    print(repr(c), cmd(c))

LIMIT = 10
class K:
    A = 1
    B = 2
def dotted(v):
    match v:
        case K.A:
            return "K.A"
        case K.B:
            return "K.B"
        case _:
            return "none"
print(dotted(1), dotted(2), dotted(3))

def capture_wild(v):
    match v:
        case [_, _, *others] if len(others) > 1:
            return ("many", others)
        case [first, _]:
            return ("pair", first)
        case _:
            return "wild"
print(capture_wild([1, 2, 3, 4, 5]), capture_wild([1, 2]), capture_wild(3))

def walk(tree):
    match tree:
        case ("leaf", v):
            return v
        case ("node", l, r):
            return walk(l) + walk(r)
        case ("neg", t):
            return -walk(t)
print(walk(("node", ("leaf", 3), ("neg", ("node", ("leaf", 1), ("leaf", 1))))))

seen = []
def side(v):
    seen.append(v)
    return v > 5
match 3:
    case n if side(n):
        print("no")
    case n if side(n + 10):
        print("second guard", n)
print(seen)
match {"a": 1, "b": 2}:
    case {"a": 1} as d:
        print("sub-dict", sorted(d))
match 1:
    case 1 if False:
        print("no")
    case _:
        print("fallthrough")
x = 5
match x:
    case y:
        print("bound", y, x)
match [1, 2]:
    case [a, b] | [a, b, _]:
        print("or-seq", a, b)
print("done")
