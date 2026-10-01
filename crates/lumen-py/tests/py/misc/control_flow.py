for i in range(5):
    if i == 1:
        continue
    if i == 4:
        break
    print("i", i)
else:
    print("no break")
for i in range(2):
    pass
else:
    print("for else", i)
n = 0
while n < 3:
    n += 1
else:
    print("while else", n)
while True:
    n += 1
    if n > 5:
        break
print(n)

def fin():
    try:
        return "try"
    finally:
        print("finally runs")
print(fin())

def fin2():
    for i in range(3):
        try:
            if i == 1:
                continue
            print("body", i)
        finally:
            print("fin", i)
fin2()

def fin3():
    try:
        raise ValueError("x")
    except ValueError as e:
        return "except " + str(e)
    else:
        return "else"
    finally:
        print("always")
print(fin3())

try:
    pass
except Exception:
    print("no")
else:
    print("else clause")
finally:
    print("finally clause")

try:
    try:
        raise KeyError("inner")
    except KeyError as e:
        raise ValueError("outer") from e
except ValueError as e:
    print(type(e.__cause__).__name__, e.__cause__.args, e.__context__ is e.__cause__)
try:
    1 / 0
except (TypeError, ZeroDivisionError) as e:
    print(type(e).__name__, e)
try:
    raise Exception("a", 1)
except Exception as e:
    print(e.args, str(e), repr(e))
class MyErr(Exception):
    def __init__(self, code):
        super().__init__(f"code {code}")
        self.code = code
try:
    raise MyErr(7)
except MyErr as e:
    print(e, e.code, isinstance(e, Exception), MyErr.__mro__[1].__name__)
x = 3
print("pos" if x > 0 else "neg", [1, 2][x > 2], {True: "t", False: "f"}[x == 3])
class CM:
    def __enter__(self):
        print("enter")
        return self
    def __exit__(self, *a):
        print("exit", a[0].__name__ if a[0] else None)
        return True
with CM() as cm:
    raise ValueError("swallowed")
print(type(cm).__name__)
assert x == 3, "unreachable"
print(isinstance(KeyError(), LookupError), issubclass(ZeroDivisionError, ArithmeticError))
