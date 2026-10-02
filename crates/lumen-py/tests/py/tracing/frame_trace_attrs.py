import sys


def inner():
    x = 1
    y = 2
    return x + y


lines = []


def local(frame, event, arg):
    lines.append((event, frame.f_lineno - frame.f_code.co_firstlineno))
    return local


def make_global(trace_lines):
    def glob(frame, event, arg):
        if frame.f_code is inner.__code__:
            frame.f_trace_lines = trace_lines
            return local
    return glob


for flag in (True, False):
    lines.clear()
    sys.settrace(make_global(flag))
    inner()
    sys.settrace(None)
    print(flag, lines)


def probe():
    frame = sys._getframe()
    return (
        frame.f_code.co_name,
        frame.f_back.f_code.co_name,
        frame.f_trace,
        frame.f_trace_lines,
        frame.f_trace_opcodes,
        frame.f_lasti >= 0,
    )


def caller():
    return probe()


print(caller())


def set_f_trace(frame, event, arg):
    if frame.f_code is inner.__code__:
        frame.f_trace = local
    return None


lines.clear()
sys.settrace(set_f_trace)
inner()
sys.settrace(None)
print(lines)
