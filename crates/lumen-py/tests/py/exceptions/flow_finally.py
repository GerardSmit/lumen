def f1():
    try:
        return "try"
    finally:
        print("finally f1")
print(f1())

def f2():
    try:
        return "try"
    finally:
        return "finally"
print(f2())

def f3():
    try:
        raise ValueError("x")
    except ValueError:
        return "except"
    else:
        return "else"
    finally:
        print("fin f3")
print(f3())

def f4():
    try:
        pass
    except Exception:
        return "except"
    else:
        return "else"
    finally:
        print("fin f4")
print(f4())

def f5():
    for i in range(3):
        try:
            if i == 1:
                continue
            print("body", i)
        finally:
            print("fin", i)
f5()

def f6():
    for i in range(5):
        try:
            if i == 2:
                break
        finally:
            print("fin6", i)
    return i
print(f6())

def f7():
    try:
        raise KeyError("k")
    finally:
        return "swallowed"
print(f7())

def f8():
    x = 1
    try:
        return x
    finally:
        x = 2
print(f8())

def f9():
    try:
        try:
            raise ValueError("inner")
        finally:
            print("inner finally")
    except ValueError as e:
        print("caught", e)
    else:
        print("no")
    finally:
        print("outer finally")
f9()

def f10():
    while True:
        try:
            return "ret"
        finally:
            break
    return "after break"
print(f10())

try:
    try:
        raise ValueError("a")
    finally:
        raise TypeError("b")
except TypeError as e:
    print(type(e).__name__, e, type(e.__context__).__name__)

def order():
    out = []
    try:
        out.append("try")
        raise IndexError
    except IndexError:
        out.append("except")
    else:
        out.append("else")
    finally:
        out.append("finally")
    return out
print(order())
try:
    pass
except:
    print("never")
else:
    print("else runs")
for i in range(2):
    try:
        continue
    finally:
        print("cont fin", i)
