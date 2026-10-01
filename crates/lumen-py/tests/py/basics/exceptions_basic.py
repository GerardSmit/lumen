def t(fn):
    try:
        return fn()
    except Exception as e:
        return type(e).__name__
print(t(lambda: 1 / 0))
print(t(lambda: [][1]))
print(t(lambda: {}["a"]))
print(t(lambda: int("x")))
print(t(lambda: None.foo))
print(t(lambda: undefined_name))
print(t(lambda: 1 + "a"))
print(t(lambda: len(5)))
print(t(lambda: next(iter([]))))
print(t(lambda: (1, 2).index(9)))
try:
    raise ValueError("boom")
except ValueError as e:
    print(e, e.args, repr(e))
class MyErr(Exception):
    pass
class Sub(MyErr):
    def __init__(self, code):
        super().__init__("code %d" % code)
        self.code = code
try:
    raise Sub(7)
except MyErr as e:
    print(type(e).__name__, e, e.code, isinstance(e, Exception))
try:
    try:
        raise KeyError("k")
    except KeyError as e:
        raise RuntimeError("wrapped") from e
except RuntimeError as e:
    print(e, type(e.__cause__).__name__, repr(e.__cause__))
try:
    try:
        1 / 0
    except ZeroDivisionError:
        raise ValueError("again")
except ValueError as e:
    print(type(e.__context__).__name__)
def order():
    out = []
    try:
        out.append("try")
        raise OSError
    except (ValueError, OSError):
        out.append("except")
    else:
        out.append("else")
    finally:
        out.append("finally")
    return out
print(order())
def fin():
    try:
        return 1
    finally:
        print("in finally")
print(fin())
def override():
    try:
        return 1
    finally:
        return 2
print(override())
try:
    assert 1 == 2, "msg"
except AssertionError as e:
    print("assert", e)
print(issubclass(KeyError, LookupError), issubclass(ZeroDivisionError, ArithmeticError))
try:
    raise
except RuntimeError as e:
    print("no active", e)
