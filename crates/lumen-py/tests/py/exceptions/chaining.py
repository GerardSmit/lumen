def show(e):
    print(type(e).__name__, repr(e.__cause__), repr(e.__context__), e.__suppress_context__)


try:
    try:
        raise ValueError("a")
    except ValueError as a:
        raise TypeError("b") from a
except TypeError as e:
    show(e)
    print(e.__cause__ is e.__context__)

try:
    try:
        raise ValueError("a")
    except ValueError:
        raise TypeError("b") from None
except TypeError as e:
    show(e)

try:
    try:
        raise ValueError("a")
    except ValueError:
        raise TypeError("b")
except TypeError as e:
    show(e)

try:
    try:
        raise KeyError("k")
    except KeyError:
        raise RuntimeError("r") from OSError("os")
except RuntimeError as e:
    show(e)

try:
    raise ValueError("plain")
except ValueError as e:
    show(e)

try:
    try:
        raise ValueError("one")
    except ValueError:
        try:
            raise TypeError("two")
        except TypeError:
            raise KeyError("three")
except KeyError as e:
    print(repr(e.__context__))
    print(repr(e.__context__.__context__))
    print(e.__context__.__context__.__context__)

e = ValueError("manual")
e.__cause__ = KeyError("c")
print(e.__suppress_context__, repr(e.__cause__))
e.__suppress_context__ = False
print(e.__suppress_context__)

try:
    raise ValueError("x") from KeyError("y")
except ValueError as e:
    print(repr(e.__cause__), e.__context__)

try:
    try:
        1 / 0
    except ZeroDivisionError as z:
        raise ValueError("wrapped") from z
except ValueError as e:
    print(type(e.__cause__).__name__, e.__cause__.args)

try:
    raise ValueError("a") from 5
except TypeError as e:
    print("TypeError")


def helper():
    try:
        raise OSError("io")
    except OSError:
        raise RuntimeError("from helper")


try:
    helper()
except RuntimeError as e:
    print(repr(e.__context__), e.__cause__)

try:
    try:
        raise ValueError("v")
    finally:
        raise TypeError("t")
except TypeError as e:
    print(repr(e.__context__))
