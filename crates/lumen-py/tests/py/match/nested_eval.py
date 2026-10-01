class Num:
    __match_args__ = ("v",)
    def __init__(self, v): self.v = v
class Add:
    __match_args__ = ("l", "r")
    def __init__(self, l, r): self.l, self.r = l, r
class Mul:
    __match_args__ = ("l", "r")
    def __init__(self, l, r): self.l, self.r = l, r
class Neg:
    __match_args__ = ("e",)
    def __init__(self, e): self.e = e

def ev(e):
    match e:
        case Num(v):
            return v
        case Add(l, r):
            return ev(l) + ev(r)
        case Mul(l, r):
            return ev(l) * ev(r)
        case Neg(e):
            return -ev(e)
        case _:
            raise ValueError("bad")

def show(e):
    match e:
        case Num(v):
            return str(v)
        case Add(l, r):
            return f"({show(l)} + {show(r)})"
        case Mul(Num(1), r):
            return show(r)
        case Mul(l, r):
            return f"{show(l)} * {show(r)}"
        case Neg(Neg(x)):
            return show(x)
        case Neg(x):
            return f"-{show(x)}"

t = Add(Mul(Num(2), Num(3)), Neg(Add(Num(1), Num(1))))
print(ev(t), show(t))
print(show(Mul(Num(1), Neg(Neg(Num(7))))))
try:
    ev("x")
except ValueError as e:
    print("ValueError", e)

def json_like(v, depth=0):
    match v:
        case None | True | False:
            return str(v).lower() if v is not None else "null"
        case int(n) | float(n):
            return str(n)
        case str(s):
            return '"' + s + '"'
        case [*items]:
            return "[" + ",".join(json_like(i) for i in items) + "]"
        case {**kv}:
            return "{" + ",".join(f'"{k}":{json_like(x)}' for k, x in kv.items()) + "}"
print(json_like({"a": [1, 2.5, None, True], "b": {"c": "d"}, "e": []}))

def stack_machine(prog):
    st = []
    for ins in prog:
        match ins:
            case ("push", n):
                st.append(n)
            case ("add",):
                st.append(st.pop() + st.pop())
            case ("mul",):
                st.append(st.pop() * st.pop())
            case ("dup",):
                st.append(st[-1])
            case other:
                raise ValueError(other)
    return st
print(stack_machine([("push", 2), ("push", 3), ("add",), ("dup",), ("mul",)]))
