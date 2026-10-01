log = []


class R:
    def __init__(self, name, fail_enter=False, fail_exit=False, swallow=False):
        self.name = name
        self.fail_enter = fail_enter
        self.fail_exit = fail_exit
        self.swallow = swallow

    def __enter__(self):
        log.append("enter " + self.name)
        if self.fail_enter:
            raise RuntimeError("enter " + self.name)
        return self.name

    def __exit__(self, et, ev, tb):
        log.append("exit %s %s" % (self.name, et.__name__ if et else None))
        if self.fail_exit:
            raise OSError("exit " + self.name)
        return self.swallow


def flush(title):
    print(title, log)
    log.clear()


with R("a") as a, R("b") as b, R("c") as c:
    print(a, b, c)
flush("multi")

with (R("p") as p, R("q") as q):
    print(p, q)
flush("paren")

with (
    R("x") as x,
    R("y") as y,
    R("z"),
):
    print(x, y)
flush("paren3")

try:
    with R("a"), R("b", fail_enter=True), R("c"):
        print("never")
except RuntimeError as e:
    print("caught", e)
flush("fail enter")

try:
    with R("a"), R("b"), R("c"):
        raise ValueError("body")
except ValueError as e:
    print("caught", e)
flush("body error")

try:
    with R("a"), R("b", fail_exit=True), R("c"):
        raise ValueError("body")
except Exception as e:
    print("caught", type(e).__name__, e, type(e.__context__).__name__)
flush("exit error")

with R("a"), R("b", swallow=True), R("c"):
    raise ValueError("inner swallowed")
print("after swallow")
flush("swallow")

with R("a"), R("b", swallow=True) as q:
    pass
flush("swallow no error")

try:
    with R("outer"):
        with R("inner"):
            raise KeyError("deep")
except KeyError as e:
    print("caught", e)
flush("nested")

with R("a"):
    try:
        with R("b"):
            raise ValueError("v")
    except ValueError:
        print("handled inside")
flush("handled")

try:
    with R("a", fail_exit=True), R("b", fail_exit=True):
        pass
except OSError as e:
    print("caught", e)
flush("both exit fail")

with R("a") as a:
    with R("b") as b:
        print(a + b)
flush("sum")
