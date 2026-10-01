try:
    {}["x"]
except (KeyError, IndexError) as e:
    print("tuple", type(e).__name__, e.args)
try:
    [][1]
except (KeyError, IndexError) as e:
    print("tuple", type(e).__name__)

try:
    1 / 0
except ArithmeticError as e:
    print(type(e).__name__, isinstance(e, ZeroDivisionError))
try:
    {}["x"]
except LookupError:
    print("LookupError base")
print(ZeroDivisionError.__mro__[1].__name__, KeyError.__mro__[1].__name__, FileNotFoundError.__mro__[1].__name__)
print(UnboundLocalError.__mro__[1].__name__, RecursionError.__mro__[1].__name__, NotImplementedError.__mro__[1].__name__)
print(StopIteration.__mro__[1].__name__, KeyboardInterrupt.__mro__[1].__name__, SystemExit.__mro__[1].__name__)

def nested():
    try:
        try:
            raise ValueError("v")
        except TypeError:
            print("wrong")
        print("skipped")
    except ValueError as e:
        print("outer", e)
        try:
            raise KeyError("inner")
        except KeyError as k:
            print("inner", k)
        print("e still", e)
nested()
e = "preset"
try:
    raise ValueError
except ValueError as e:
    pass
try:
    print(e)
except NameError:
    print("e deleted")

it = iter([1])
print(next(it), next(it, "dflt"))
try:
    next(it)
except StopIteration as s:
    print("StopIteration", s.args, s.value)
class Itr:
    def __init__(self): self.i = 0
    def __iter__(self): return self
    def __next__(self):
        self.i += 1
        if self.i > 3:
            raise StopIteration("done")
        return self.i
print(list(Itr()), [x for x in Itr()])
try:
    assert 1 == 2, "math broke"
except AssertionError as e:
    print("AssertionError", e)
try:
    assert False
except AssertionError as e:
    print("AssertionError", e.args)
assert True, "fine"
try:
    raise SystemExit(3)
except SystemExit as e:
    print("exit", e.code)
try:
    raise KeyboardInterrupt
except BaseException as e:
    print(type(e).__name__)
try:
    try:
        raise Exception("e")
    except ValueError:
        pass
except Exception as e:
    print("passed through", e)

def gen_close():
    try:
        raise ValueError(5)
    except ValueError as e:
        return e.args[0]
print(gen_close())
errs = []
for v in ("1", "x", "3", None):
    try:
        errs.append(int(v))
    except (ValueError, TypeError) as e:
        errs.append(type(e).__name__)
print(errs)
try:
    raise ValueError("a", "b", 3)
except ValueError as e:
    print(e, e.args, len(e.args))
print(str(ValueError(1, 2)), str(ValueError(None)), str(ValueError("")) == "")
print(isinstance(ValueError(), Exception), Exception().args, BaseException("a").args)
print(NotImplementedError("x"), repr(NotImplementedError()))
try:
    raise NotImplemented
except TypeError:
    print("TypeError raise NotImplemented")
