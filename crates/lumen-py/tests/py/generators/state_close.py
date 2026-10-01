def g():
    yield 1
    yield 2

x = g()
print(x.gi_running, x.gi_frame is not None)
print(next(x))
print(list(x))
print(x.gi_frame is None)
try:
    next(x)
except StopIteration:
    print("exhausted")

def withfinally():
    try:
        print("start")
        yield 1
        yield 2
    finally:
        print("cleanup")

w = withfinally()
print(next(w))
w.close()
print("closed")
try:
    next(w)
except StopIteration:
    print("stop after close")

w2 = withfinally()
w2.close()
print("closed unstarted")

def catches_exit():
    try:
        yield 1
    except GeneratorExit:
        print("got GeneratorExit")
        raise

c = catches_exit()
next(c)
c.close()

def bad():
    try:
        yield 1
    except GeneratorExit:
        yield 2

b = bad()
next(b)
try:
    b.close()
except RuntimeError as e:
    print("RuntimeError", e)

def gen_ret():
    yield 1
    return 42

r = gen_ret()
next(r)
try:
    next(r)
except StopIteration as e:
    print(e.value, e.args)

def nested_finally():
    try:
        yield 1
    finally:
        print("fin1")

z = nested_finally()
next(z)
z.close()
print("end")
