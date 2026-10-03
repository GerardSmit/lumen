import sys


def f(n):
    total = 0
    for i in range(n):
        total += i
    return total


def g():
    return f(2)


events = []


def trace(frame, event, arg):
    name = frame.f_code.co_name
    if name in ("f", "g"):
        rel = frame.f_lineno - frame.f_code.co_firstlineno
        events.append((name, event, rel, arg if event == "return" else None))
    return trace


print(sys.gettrace())
sys.settrace(trace)
print(sys.gettrace() is trace)
g()
sys.settrace(None)
for e in events:
    print(e)
print(sys.gettrace())


def boom():
    raise ValueError("x")


seen = []


def exc_trace(frame, event, arg):
    if frame.f_code.co_name == "boom":
        if event == "exception":
            seen.append((event, arg[0].__name__, str(arg[1])))
        else:
            seen.append(event)
    return exc_trace


sys.settrace(exc_trace)
try:
    boom()
except ValueError:
    pass
sys.settrace(None)
print(seen)
