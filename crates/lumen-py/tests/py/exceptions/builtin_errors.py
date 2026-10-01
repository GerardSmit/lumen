import sys


def attempt(label, fn, msg=True):
    try:
        fn()
    except BaseException as e:
        if msg:
            print(label, type(e).__name__, e)
        else:
            print(label, type(e).__name__)
    else:
        print(label, "no error")


attempt("type1", lambda: 1 + "a")
attempt("type2", lambda: len(5))
attempt("type3", lambda: (lambda n: n())(None))
attempt("value1", lambda: int("abc"))
attempt("value2", lambda: [].index(1), msg=False)
attempt("index1", lambda: [1, 2][5])
attempt("index2", lambda: "abc"[10])
attempt("index3", lambda: ()[0])
attempt("key1", lambda: {"a": 1}["b"])
attempt("key2", lambda: {}[1])
attempt("attr1", lambda: (1).foo, msg=False)
attempt("attr2", lambda: None.bar, msg=False)
attempt("zero1", lambda: 1 / 0)
attempt("zero2", lambda: 1 // 0)
attempt("zero3", lambda: 1 % 0)
attempt("zero4", lambda: 0 ** -1, msg=False)
attempt("name1", lambda: undefined_name_xyz, msg=False)
attempt("overflow1", lambda: 10.0 ** 1000, msg=False)
attempt("overflow2", lambda: float(10 ** 400), msg=False)
attempt("stop1", lambda: next(iter([])))
attempt("assert1", lambda: exec("assert False"))
attempt("assert2", lambda: exec("assert 1 == 2, 'boom'"))


def unbound():
    print(x)
    x = 1


attempt("unbound", unbound, msg=False)


def rec(n):
    return rec(n + 1)


try:
    rec(0)
except RecursionError as e:
    print("RecursionError", isinstance(e, RuntimeError))


def deep(n):
    if n == 0:
        return 0
    return 1 + deep(n - 1)


print(deep(100))
try:
    deep(10 ** 6)
except RecursionError:
    print("deep failed")

it = iter([1])
next(it)
try:
    next(it)
except StopIteration as e:
    print("StopIteration", e.value, e.args)


def gen():
    yield 1
    return 42


g = gen()
next(g)
try:
    next(g)
except StopIteration as e:
    print("gen done", e.value)

attempt("unpack1", lambda: [a for a, b in [(1,)]], msg=False)
attempt("hash", lambda: {[]: 1}, msg=False)
attempt("hash2", lambda: hash([]), msg=False)
attempt("str-idx", lambda: "abc".index("z"))
attempt("range-step", lambda: range(0, 5, 0))
attempt("chr", lambda: chr(-1), msg=False)
attempt("pop", lambda: [].pop(), msg=False)
attempt("popdict", lambda: {}.popitem(), msg=False)
attempt("neg-shift", lambda: 1 << -1)
print(issubclass(ZeroDivisionError, ArithmeticError), issubclass(KeyError, LookupError),
      issubclass(IndexError, LookupError), issubclass(UnboundLocalError, NameError),
      issubclass(RecursionError, RuntimeError), issubclass(OverflowError, ArithmeticError))
