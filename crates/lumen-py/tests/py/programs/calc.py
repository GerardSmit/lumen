import math


class ParseError(Exception):
    def __init__(self, msg, pos):
        super().__init__(f"{msg} at {pos}")
        self.msg = msg
        self.pos = pos


class Token:
    __slots__ = ("kind", "value", "pos")

    def __init__(self, kind, value, pos):
        self.kind = kind
        self.value = value
        self.pos = pos

    def __repr__(self):
        return f"Token({self.kind}, {self.value!r}, {self.pos})"


def tokenize(src):
    tokens = []
    i = 0
    n = len(src)
    while i < n:
        c = src[i]
        if c.isspace():
            i += 1
        elif c.isdigit() or (c == "." and i + 1 < n and src[i + 1].isdigit()):
            j = i
            seen_dot = False
            while j < n and (src[j].isdigit() or (src[j] == "." and not seen_dot)):
                if src[j] == ".":
                    seen_dot = True
                j += 1
            text = src[i:j]
            tokens.append(Token("num", float(text) if seen_dot else int(text), i))
            i = j
        elif c.isalpha() or c == "_":
            j = i
            while j < n and (src[j].isalnum() or src[j] == "_"):
                j += 1
            tokens.append(Token("id", src[i:j], i))
            i = j
        elif c == "*" and src[i:i + 2] == "**":
            tokens.append(Token("op", "**", i))
            i += 2
        elif c in "+-*/%^(),":
            tokens.append(Token("op", c, i))
            i += 1
        else:
            raise ParseError(f"unexpected character {c!r}", i)
    tokens.append(Token("eof", None, n))
    return tokens


FUNCS = {
    "sqrt": math.sqrt,
    "abs": abs,
    "max": max,
    "min": min,
    "floor": math.floor,
    "ceil": math.ceil,
    "round": round,
    "pow": pow,
}
CONSTS = {"pi": math.pi, "e": math.e}


class Parser:
    def __init__(self, src, variables=None):
        self.tokens = tokenize(src)
        self.i = 0
        self.vars = dict(variables or {})

    def peek(self):
        return self.tokens[self.i]

    def next(self):
        t = self.tokens[self.i]
        self.i += 1
        return t

    def accept(self, *ops):
        t = self.peek()
        if t.kind == "op" and t.value in ops:
            self.i += 1
            return t.value
        return None

    def parse(self):
        v = self.expr()
        t = self.peek()
        if t.kind != "eof":
            raise ParseError(f"unexpected token {t.value!r}", t.pos)
        return v

    def expr(self):
        v = self.term()
        while True:
            op = self.accept("+", "-")
            if op is None:
                return v
            r = self.term()
            v = v + r if op == "+" else v - r

    def term(self):
        v = self.unary()
        while True:
            op = self.accept("*", "/", "%")
            if op is None:
                return v
            pos = self.peek().pos
            r = self.unary()
            if op == "*":
                v = v * r
            elif r == 0:
                raise ParseError("division by zero", pos)
            elif op == "/":
                v = v / r
            else:
                v = v % r

    def unary(self):
        if self.accept("-"):
            return -self.unary()
        if self.accept("+"):
            return self.unary()
        return self.power()

    def power(self):
        base = self.atom()
        if self.accept("**", "^"):
            return base ** self.unary()
        return base

    def atom(self):
        t = self.next()
        if t.kind == "num":
            return t.value
        if t.kind == "id":
            if self.accept("("):
                args = []
                if not self.accept(")"):
                    while True:
                        args.append(self.expr())
                        if self.accept(")"):
                            break
                        if not self.accept(","):
                            raise ParseError("expected , or )", self.peek().pos)
                if t.value not in FUNCS:
                    raise ParseError(f"unknown function {t.value}", t.pos)
                return FUNCS[t.value](*args)
            if t.value in self.vars:
                return self.vars[t.value]
            if t.value in CONSTS:
                return CONSTS[t.value]
            raise ParseError(f"unknown name {t.value}", t.pos)
        if t.kind == "op" and t.value == "(":
            v = self.expr()
            if not self.accept(")"):
                raise ParseError("expected )", self.peek().pos)
            return v
        raise ParseError(f"unexpected {t.value!r}" if t.kind != "eof" else "unexpected end", t.pos)


def evaluate(src, **variables):
    return Parser(src, variables).parse()


cases = [
    "1 + 2 * 3",
    "(1 + 2) * 3",
    "2 ** 3 ** 2",
    "-2 ** 2",
    "10 / 4",
    "10 % 4 + 7 % 3",
    "sqrt(16) + abs(-3)",
    "max(1, 5, 3) - min(4, 2)",
    "floor(3.7) + ceil(3.2)",
    "x * x + y",
    "1.5 * 4",
    ".5 + .25",
    "2 * pi",
    "round(2.567, 2)",
    "pow(2, 10)",
    "((((1))))",
    "--3",
    "1 +",
    "2 * (3 + 4",
    "foo(1)",
    "bar + 1",
    "5 / (2 - 2)",
    "3 $ 4",
    "1 2",
    "2 ^ 10",
    "100000000000 * 100000000000",
]
for c in cases:
    try:
        r = evaluate(c, x=7, y=2)
        if isinstance(r, float):
            print(f"{c:30} = {r:.6g}")
        else:
            print(f"{c:30} = {r}")
    except ParseError as ex:
        print(f"{c:30} ! {ex} [{ex.msg}|{ex.pos}]")

print([repr(t) for t in tokenize("a1+22.5")])
total = 0
for k in range(1, 30):
    total += evaluate(f"{k} * ({k} + 1) / 2")
print("sum triangular", total)
print(math.isclose(evaluate("sqrt(2) ** 2"), 2.0))
