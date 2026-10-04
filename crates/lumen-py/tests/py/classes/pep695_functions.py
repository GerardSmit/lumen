# PEP 695 generic functions and type aliases (no `typing` import needed).

def ident[T](x: T) -> T:
    return x

print(ident(3), ident.__name__, ident.__qualname__)
(T,) = ident.__type_params__
print(type(T).__name__, T.__name__, repr(T), T.__bound__, T.__constraints__)
print(T.__covariant__, T.__contravariant__, T.__infer_variance__, T.__module__)
print(ident.__annotations__["x"] is T, ident.__annotations__["return"] is T)
print(ident.__type_params__ is ident.__type_params__)

def plain(): pass
print(plain.__type_params__)

def many[A, B: int, C: (str, bytes), *Ts, **P](a: A, *args: *Ts, **kw: P.kwargs) -> B: pass
A, B, C, Ts, P = many.__type_params__
print([type(x).__name__ for x in many.__type_params__])
print(repr(A), repr(B), repr(C), repr(Ts), repr(P))
print(B.__bound__, C.__constraints__, A.__bound__, A.__constraints__)
print(P.args, P.kwargs, P.args.__origin__ is P, P.args == P.args, P.args == P.kwargs)
print(many.__annotations__["kw"])


def defaults[T](a: T = 1, *, b: T = 2) -> T:
    return a, b

print(defaults(), defaults(5, b=6), defaults.__defaults__, defaults.__kwdefaults__)


# Bounds are evaluated lazily, in their own scope.
def lazy[T: Undefined]():
    pass

(LT,) = lazy.__type_params__
try:
    LT.__bound__
except NameError as e:
    print("NameError", e)
Undefined = int
print(LT.__bound__)


class Outer:
    def method[T](self, x: T) -> T:
        return x

    async def amethod[T](self): pass

print(Outer.method.__qualname__, Outer().method(7), Outer.amethod.__qualname__)


def nested():
    def inner[T](): return T
    return inner

print(nested().__qualname__, nested()().__name__)


# Type parameters close over enclosing function scopes.
def make():
    x = "outer"
    def f[T](a: T) -> T:
        return x, T.__name__
    return f

print(make()(0))


# Decorators run after the generic function is built.
def deco(f):
    print("decorating", f.__name__, f.__type_params__)
    return f

@deco
def decorated[X, Y](): pass


# type statements
type Alias = list[int]
print(type(Alias).__name__, Alias, repr(Alias), Alias.__name__, Alias.__module__)
print(Alias.__value__, Alias.__type_params__, Alias.__parameters__)

type Lazy = Later
Later = dict
print(Lazy.__value__)

type Pair[K, V] = tuple[K, V]
K, V = Pair.__type_params__
print(Pair.__type_params__, Pair.__parameters__, Pair.__value__)
print(Pair[int, str], type(Pair[int, str]).__name__)
try:
    Alias[int]
except TypeError as e:
    print("TypeError", e)

type Rec[T] = list[Rec[T]]
print(Rec.__value__)

print(Alias | int, int | Alias, (Alias | None).__args__)
print(Alias.__reduce__())

# soft keyword
type = 5
print(type)
del type
print(type(1))


# type params of a generic alias are visible inside its value only
type Inner[T] = T
print(Inner.__value__ is Inner.__type_params__[0])


def g[T]():
    T = 1
    return T

print(g())


def shadow[T]():
    return [T for _ in range(1)][0]

print(shadow().__name__)


def errors():
    for src in [
        "def f[T, T](): pass",
        "class C[T, *T]: pass",
        "type A[T, **T] = int",
        "def f[T: (x := 1)](): pass",
        "type A = (yield)",
        "class C[T]((x := 1)): pass",
        "def f[T: (yield)](): pass",
        "def f[*Ts: int](): pass",
    ]:
        try:
            compile(src, "<test>", "exec")
            print("ok", src)
        except SyntaxError as e:
            print("SyntaxError:", e.msg)

errors()
