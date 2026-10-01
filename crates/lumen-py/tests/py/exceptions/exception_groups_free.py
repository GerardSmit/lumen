import sys


def gen():
    try:
        yield 1
        yield 2
    except ValueError as e:
        print("gen caught", e)
        yield 99
    finally:
        print("gen finally")


g = gen()
print(next(g))
print(g.throw(ValueError("thrown")))
try:
    next(g)
except StopIteration:
    print("gen exhausted")

g = gen()
next(g)
g.close()
print("closed")


def failing():
    yield 1
    raise KeyError("in gen")


try:
    for x in failing():
        print("got", x)
except KeyError as e:
    print("outer saw", e)


class Ctx:
    def __enter__(self):
        print("enter")
        return self

    def __exit__(self, et, ev, tb):
        print("exit", et.__name__ if et else None)
        raise RuntimeError("from exit")


try:
    with Ctx():
        print("body")
except RuntimeError as e:
    print("caught", e, e.__context__)

try:
    with Ctx():
        raise ValueError("body error")
except RuntimeError as e:
    print("caught", e, repr(e.__context__))

try:
    [1 / x for x in [1, 0]]
except ZeroDivisionError:
    print("comp zero")

try:
    {k: [][k] for k in range(2)}
except IndexError:
    print("dictcomp index")

try:
    list(1 / x for x in [2, 0])
except ZeroDivisionError:
    print("genexp zero")


def kbd():
    raise KeyboardInterrupt


res = []
for fn in (lambda: sys.exit(2), kbd):
    try:
        try:
            fn()
        except Exception:
            res.append("Exception")
    except BaseException as e:
        res.append(type(e).__name__)
print(res)

try:
    raise SystemExit(5)
except SystemExit as e:
    print("code", e.code)

try:
    sys.exit()
except SystemExit as e:
    print("code", e.code)

try:
    raise KeyboardInterrupt
except Exception:
    print("not here")
except KeyboardInterrupt:
    print("kbd")

print(issubclass(SystemExit, Exception), issubclass(KeyboardInterrupt, Exception),
      issubclass(GeneratorExit, Exception), issubclass(Exception, BaseException))


def g2():
    try:
        yield 1
    except GeneratorExit:
        print("GeneratorExit")
        raise


x = g2()
next(x)
x.close()


def thrower():
    yield 1


t = thrower()
next(t)
try:
    t.throw(ZeroDivisionError("tt"))
except ZeroDivisionError as e:
    print("thrown through", e)
try:
    next(t)
except StopIteration:
    print("dead")
