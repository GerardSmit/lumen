import sys


def func(out):
    out.append(1)
    out.append(2)
    out.append(3)


base = func.__code__.co_firstlineno


def skip_forward(frame, event, arg):
    if frame.f_code is not func.__code__:
        return None
    if event == "line" and frame.f_lineno == base + 1:
        frame.f_lineno = base + 3
    return skip_forward


out = []
sys.settrace(skip_forward)
func(out)
sys.settrace(None)
print(out)


def repeat(out):
    out.append(1)
    out.append(2)


base2 = repeat.__code__.co_firstlineno
jumped = []


def go_back(frame, event, arg):
    if frame.f_code is not repeat.__code__:
        return None
    if event == "line" and frame.f_lineno == base2 + 2 and not jumped:
        jumped.append(True)
        frame.f_lineno = base2 + 1
    return go_back


out = []
sys.settrace(go_back)
repeat(out)
sys.settrace(None)
print(out)


def bad_targets(frame, event, arg):
    if frame.f_code is not func.__code__:
        return None
    if event == "line" and frame.f_lineno == base + 1:
        for target in (base - 50, base + 1000):
            try:
                frame.f_lineno = target
            except ValueError as e:
                print("ValueError")
        try:
            frame.f_lineno = "x"
        except (TypeError, ValueError) as e:
            print(type(e).__name__)
    return bad_targets


out = []
sys.settrace(bad_targets)
func(out)
sys.settrace(None)
print(out)

try:
    sys._getframe().f_lineno = 1
except ValueError:
    print("not from a trace function")
