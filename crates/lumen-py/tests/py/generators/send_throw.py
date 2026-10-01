def acc():
    total = 0
    while True:
        try:
            x = yield total
        except ValueError as e:
            print("caught", e)
            x = 100
        if x is None:
            break
        total += x
    return total

g = acc()
print(next(g))
print(g.send(5))
print(g.send(10))
print(g.throw(ValueError("boom")))
try:
    g.send(None)
except StopIteration as e:
    print("stop", e.value)

def g2():
    try:
        yield 1
        yield 2
    except KeyError:
        print("keyerror inside")
        yield "recovered"
    yield "end"

x = g2()
print(next(x))
print(x.throw(KeyError("a")))
print(next(x))
try:
    next(x)
except StopIteration:
    print("done")

y = g2()
try:
    y.send(1)
except TypeError as e:
    print(type(e).__name__)
print(next(y))
try:
    y.throw(RuntimeError("unhandled"))
except RuntimeError as e:
    print("propagated", e)
print(list(y))
