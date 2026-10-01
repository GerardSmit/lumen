def attempt(label, fn):
    try:
        fn()
    except Exception as e:
        print(label, type(e).__name__, e)
    else:
        print(label, "ok")


def two(v):
    a, b = v


def three(v):
    a, b, c = v


def star(v):
    a, *b, c = v


def nested(v):
    (a, b), c = v


attempt("too many", lambda: two([1, 2, 3]))
attempt("too many 4", lambda: two((1, 2, 3, 4)))
attempt("too few", lambda: two([1]))
attempt("too few empty", lambda: two([]))
attempt("three few", lambda: three((1, 2)))
attempt("three many", lambda: three(range(5)))
attempt("exact", lambda: three(range(3)))
attempt("str many", lambda: two("abc"))
attempt("str few", lambda: two("a"))
attempt("star few", lambda: star([1]))
attempt("star exact", lambda: star([1, 2]))
attempt("star many", lambda: star(range(10)))
attempt("nested", lambda: nested([(1, 2, 3), 4]))
attempt("nested few", lambda: nested([(1,), 4]))
attempt("not iter", lambda: two(5))
attempt("none", lambda: two(None))
attempt("dict", lambda: two({1: 1, 2: 2, 3: 3}))
attempt("dict ok", lambda: two({1: 1, 2: 2}))
attempt("set few", lambda: two({1}))
attempt("gen", lambda: two(x for x in range(3)))
attempt("gen few", lambda: two(x for x in range(1)))
attempt("bytes", lambda: two(b"abc"))
attempt("int", lambda: three(1))
attempt("float", lambda: two(1.5))


def raising_gen():
    yield 1
    raise RuntimeError("boom")


attempt("gen raises", lambda: two(raising_gen()))
try:
    a, b = [1, 2, 3]
except ValueError as e:
    print(type(e).__name__, e)
try:
    a, b, c, d = "ab"
except ValueError as e:
    print(type(e).__name__, e)
try:
    (a, b) = ()
except ValueError as e:
    print(type(e).__name__, e)
try:
    a, = [1, 2]
except ValueError as e:
    print(type(e).__name__, e)
try:
    a, *b, c = []
except ValueError as e:
    print(type(e).__name__, e)
try:
    for x, y in [(1, 2), (3,)]:
        print(x, y)
except ValueError as e:
    print(type(e).__name__, e)
