def t1():
    try:
        print("body")
    except Exception:
        print("except")
    else:
        print("else")
    finally:
        print("finally")


def t2():
    try:
        print("body")
        raise ValueError("x")
    except ValueError as e:
        print("except", e)
    else:
        print("else")
    finally:
        print("finally")


def f_ret():
    try:
        return "try"
    finally:
        print("cleanup")


def f_override():
    try:
        return "try"
    finally:
        return "finally"


def f_swallow():
    try:
        raise RuntimeError("lost")
    finally:
        return "swallowed"


def f_else_ret():
    try:
        pass
    except Exception:
        return "except"
    else:
        return "else"
    finally:
        print("fin-else")


def loop_break():
    out = []
    for i in range(5):
        try:
            if i == 3:
                break
            out.append(i)
        finally:
            out.append(("f", i))
    return out


def loop_continue():
    out = []
    for i in range(4):
        try:
            if i % 2:
                continue
            out.append(i)
        finally:
            out.append(("f", i))
    return out


def finally_break():
    for i in range(3):
        try:
            raise ValueError(i)
        finally:
            break
    return "after loop", i


def finally_continue():
    n = 0
    for i in range(3):
        try:
            return "never"
        finally:
            n += 1
            continue
    return n


def nested():
    try:
        try:
            raise KeyError("inner")
        finally:
            print("inner finally")
    except KeyError as e:
        print("outer caught", e.args)
    try:
        try:
            raise KeyError("a")
        except KeyError:
            raise ValueError("b")
        finally:
            print("mid finally")
    except ValueError as e:
        print("outer got", e)


def value_in_finally():
    x = 1
    try:
        return x
    finally:
        x = 2


t1()
t2()
print(f_ret())
print(f_override())
print(f_swallow())
print(f_else_ret())
print(loop_break())
print(loop_continue())
print(finally_break())
print(finally_continue())
nested()
print(value_in_finally())
try:
    try:
        raise ValueError("first")
    finally:
        try:
            raise TypeError("second")
        except TypeError:
            print("handled second")
except ValueError as e:
    print("still", e)
