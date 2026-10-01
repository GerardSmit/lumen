import sys


class LispError(Exception):
    pass


class Symbol:
    __slots__ = ("name",)
    table = {}

    def __init__(self, name):
        self.name = name

    @classmethod
    def intern(cls, name):
        sym = cls.table.get(name)
        if sym is None:
            sym = cls(name)
            cls.table[name] = sym
        return sym

    def __repr__(self):
        return self.name


def tokenize(src):
    tokens = []
    i = 0
    n = len(src)
    while i < n:
        c = src[i]
        if c in " \t\n\r":
            i += 1
        elif c == ";":
            while i < n and src[i] != "\n":
                i += 1
        elif c in "()'":
            tokens.append(c)
            i += 1
        elif c == '"':
            j = i + 1
            while j < n and src[j] != '"':
                j += 1
            tokens.append(src[i:j + 1])
            i = j + 1
        else:
            j = i
            while j < n and src[j] not in " \t\n\r()'":
                j += 1
            tokens.append(src[i:j])
            i = j
    return tokens


def parse_atom(tok):
    if tok.startswith('"'):
        return tok[1:-1]
    try:
        return int(tok)
    except ValueError:
        pass
    try:
        return float(tok)
    except ValueError:
        pass
    if tok == "#t":
        return True
    if tok == "#f":
        return False
    return Symbol.intern(tok)


def parse(tokens, pos=0):
    if pos >= len(tokens):
        raise LispError("unexpected EOF")
    tok = tokens[pos]
    if tok == "(":
        items = []
        pos += 1
        while True:
            if pos >= len(tokens):
                raise LispError("missing )")
            if tokens[pos] == ")":
                return items, pos + 1
            item, pos = parse(tokens, pos)
            items.append(item)
    if tok == ")":
        raise LispError("unexpected )")
    if tok == "'":
        item, pos = parse(tokens, pos + 1)
        return [Symbol.intern("quote"), item], pos
    return parse_atom(tok), pos + 1


def parse_all(src):
    tokens = tokenize(src)
    pos = 0
    forms = []
    while pos < len(tokens):
        form, pos = parse(tokens, pos)
        forms.append(form)
    return forms


class Env:
    def __init__(self, params=(), args=(), outer=None):
        self.vars = dict(zip(params, args))
        self.outer = outer

    def find(self, name):
        env = self
        while env is not None:
            if name in env.vars:
                return env
            env = env.outer
        raise LispError("unbound symbol: " + name.name)

    def lookup(self, name):
        return self.find(name).vars[name]

    def define(self, name, value):
        self.vars[name] = value


class Lambda:
    def __init__(self, params, body, env, name="lambda"):
        self.params = params
        self.body = body
        self.env = env
        self.name = name

    def __repr__(self):
        return "<" + self.name + "/" + str(len(self.params)) + ">"


def sym(name):
    return Symbol.intern(name)


S_QUOTE, S_IF, S_DEFINE, S_LAMBDA = sym("quote"), sym("if"), sym("define"), sym("lambda")
S_LET, S_COND, S_ELSE, S_BEGIN, S_SET = sym("let"), sym("cond"), sym("else"), sym("begin"), sym("set!")
S_AND, S_OR = sym("and"), sym("or")


def evaluate(x, env):
    while True:
        if isinstance(x, Symbol):
            return env.lookup(x)
        if not isinstance(x, list):
            return x
        if not x:
            return []
        head = x[0]
        if head is S_QUOTE:
            return x[1]
        if head is S_IF:
            test = evaluate(x[1], env)
            if test is not False:
                x = x[2]
            elif len(x) > 3:
                x = x[3]
            else:
                return None
            continue
        if head is S_DEFINE:
            target = x[1]
            if isinstance(target, list):
                name = target[0]
                env.define(name, Lambda(target[1:], x[2:], env, name.name))
            else:
                env.define(target, evaluate(x[2], env))
            return None
        if head is S_SET:
            env.find(x[1]).vars[x[1]] = evaluate(x[2], env)
            return None
        if head is S_LAMBDA:
            return Lambda(x[1], x[2:], env)
        if head is S_BEGIN:
            for form in x[1:-1]:
                evaluate(form, env)
            x = x[-1]
            continue
        if head is S_LET:
            names = [b[0] for b in x[1]]
            vals = [evaluate(b[1], env) for b in x[1]]
            env = Env(names, vals, env)
            for form in x[2:-1]:
                evaluate(form, env)
            x = x[-1]
            continue
        if head is S_COND:
            for clause in x[1:]:
                if clause[0] is S_ELSE or evaluate(clause[0], env) is not False:
                    for form in clause[1:-1]:
                        evaluate(form, env)
                    x = clause[-1]
                    break
            else:
                return None
            continue
        if head is S_AND:
            result = True
            for form in x[1:]:
                result = evaluate(form, env)
                if result is False:
                    return False
            return result
        if head is S_OR:
            for form in x[1:]:
                result = evaluate(form, env)
                if result is not False:
                    return result
            return False
        proc = evaluate(head, env)
        args = [evaluate(a, env) for a in x[1:]]
        if isinstance(proc, Lambda):
            if len(args) != len(proc.params):
                raise LispError("%s expects %d args, got %d" % (proc.name, len(proc.params), len(args)))
            env = Env(proc.params, args, proc.env)
            for form in proc.body[:-1]:
                evaluate(form, env)
            x = proc.body[-1]
            continue
        if callable(proc):
            return proc(*args)
        raise LispError("not a procedure: " + show(proc))


def show(v):
    if v is True:
        return "#t"
    if v is False:
        return "#f"
    if v is None:
        return "nil"
    if isinstance(v, list):
        return "(" + " ".join(show(i) for i in v) + ")"
    if isinstance(v, str):
        return '"' + v + '"'
    return repr(v)


def make_global():
    env = Env()
    def num_fold(fn):
        def f(*a):
            r = a[0]
            for v in a[1:]:
                r = fn(r, v)
            return r
        return f
    def cmp_chain(fn):
        def f(*a):
            return all(fn(a[i], a[i + 1]) for i in range(len(a) - 1))
        return f
    def minus(*a):
        if len(a) == 1:
            return -a[0]
        return num_fold(lambda p, q: p - q)(*a)
    def div(a, b):
        if isinstance(a, int) and isinstance(b, int) and a % b == 0:
            return a // b
        return a / b
    table = {
        "+": num_fold(lambda p, q: p + q),
        "-": minus,
        "*": num_fold(lambda p, q: p * q),
        "/": div,
        "mod": lambda a, b: a % b,
        "<": cmp_chain(lambda p, q: p < q),
        ">": cmp_chain(lambda p, q: p > q),
        "<=": cmp_chain(lambda p, q: p <= q),
        ">=": cmp_chain(lambda p, q: p >= q),
        "=": cmp_chain(lambda p, q: p == q),
        "not": lambda v: v is False,
        "car": lambda l: l[0],
        "cdr": lambda l: l[1:],
        "cons": lambda a, l: [a] + l,
        "list": lambda *a: list(a),
        "null?": lambda l: l == [],
        "length": len,
        "append": lambda *ls: [i for l in ls for i in l],
        "eq?": lambda a, b: a is b or (a == b and not isinstance(a, list)),
        "number?": lambda v: isinstance(v, (int, float)) and not isinstance(v, bool),
        "display": lambda v: print(show(v)),
    }
    for k, v in table.items():
        env.define(sym(k), v)
    return env


PROGRAMS = [
    "(+ 1 2 3 4)",
    "(define (fact n) (if (<= n 1) 1 (* n (fact (- n 1))))) (fact 20)",
    "(define (fib n) (if (< n 2) n (+ (fib (- n 1)) (fib (- n 2))))) (fib 15)",
    "(define (map f l) (if (null? l) '() (cons (f (car l)) (map f (cdr l))))) (map (lambda (x) (* x x)) '(1 2 3 4 5))",
    "(define (filter p l) (cond ((null? l) '()) ((p (car l)) (cons (car l) (filter p (cdr l)))) (else (filter p (cdr l))))) (filter (lambda (x) (= (mod x 2) 0)) '(1 2 3 4 5 6 7 8))",
    "(define (make-counter) (let ((n 0)) (lambda () (set! n (+ n 1)) n))) (define c (make-counter)) (c) (c) (c)",
    "(define (compose f g) (lambda (x) (f (g x)))) ((compose (lambda (x) (* 2 x)) (lambda (x) (+ x 1))) 10)",
    "(define (loop i acc) (if (= i 0) acc (loop (- i 1) (+ acc i)))) (loop 5000 0)",
    "(let ((x 2) (y 3)) (let ((x 10)) (* x y)))",
    "(define (reduce f init l) (if (null? l) init (reduce f (f init (car l)) (cdr l)))) (reduce + 0 '(1 2 3 4 5 6 7 8 9 10))",
    "(/ 10 4) (/ 10 5) (- 7)",
    "(and 1 2 #f 3) (or #f #f 7) (and) (or)",
    "(append '(1 2) '(3) '() '(4 5))",
    "(define (range a b) (if (>= a b) '() (cons a (range (+ a 1) b)))) (length (range 0 100))",
    "(car '())",
    "(undefined-fn 1)",
    "(fact 1 2)",
    "(1 2 3)",
    "(+ 1",
    "\"hello\"",
    "(cond (#f 1) (#f 2))",
]


def main():
    sys.setrecursionlimit(5000)
    env = make_global()
    for src in PROGRAMS:
        try:
            forms = parse_all(src)
            results = [evaluate(f, env) for f in forms]
            results = [r for r in results if r is not None]
            print(src[:50].ljust(52), "=>", " | ".join(show(r) for r in results) if results else "(no value)")
        except LispError as e:
            print(src[:50].ljust(52), "!! LispError:", e)
        except IndexError as e:
            print(src[:50].ljust(52), "!! IndexError")
    print("symbols interned:", len(Symbol.table))
    print(sorted(s for s in Symbol.table if s.isalpha())[:12])
    print(tokenize("(a 'b \"s t\" ; comment\n c)"))


main()
