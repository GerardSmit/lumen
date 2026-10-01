def reraise():
    try:
        raise ValueError("orig")
    except ValueError:
        print("caught once")
        raise


try:
    reraise()
except ValueError as e:
    print("outer", e)

try:
    try:
        raise KeyError("k")
    except KeyError:
        try:
            raise TypeError("t")
        except TypeError:
            print("inner")
        raise
except Exception as e:
    print(type(e).__name__, e.args)

try:
    try:
        raise OSError("a")
    except OSError:
        try:
            raise
        except OSError as e2:
            print("nested bare", e2)
            raise
except OSError as e:
    print("top", e)

try:
    raise
except RuntimeError as e:
    print("no active:", type(e).__name__)

try:
    try:
        raise ValueError("v")
    except ValueError as e:
        saved = e
    print(saved)
    print(e)
except NameError as n:
    print("NameError", n.name)

e = "outer"
try:
    raise ValueError("v")
except ValueError as e:
    pass
try:
    print(e)
except NameError:
    print("e was deleted")

try:
    try:
        raise ValueError("in finally outer")
    finally:
        print("in finally")
except ValueError as e:
    print("propagated", e)


def raise_in_finally():
    try:
        raise KeyError("first")
    except KeyError:
        try:
            pass
        finally:
            raise
    return "unreached"


try:
    raise_in_finally()
except KeyError as e:
    print("finally bare raise", e.args)


def count_reraise(n):
    try:
        if n == 0:
            raise IndexError("bottom")
        count_reraise(n - 1)
    except IndexError:
        print("unwind", n)
        raise


try:
    count_reraise(3)
except IndexError as e:
    print("done", e)

try:
    try:
        raise ValueError("a")
    except ValueError as e:
        raise e
except ValueError as e:
    print("raise e", e)

for i in range(2):
    try:
        raise ValueError(i)
    except ValueError:
        if i == 0:
            continue
        print("second iter")


def swallow():
    try:
        raise ValueError("s")
    except ValueError:
        pass
    try:
        raise
    except RuntimeError:
        return "no leak"


print(swallow())
