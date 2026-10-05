import sys

m = sys.monitoring
E = m.events
TOOL = m.PROFILER_ID

print(E.NO_EVENTS)
print(m.get_tool(TOOL))
m.use_tool_id(TOOL, "corpus")
print(m.get_tool(TOOL))

try:
    m.use_tool_id(TOOL, "again")
except ValueError:
    print("in use")
try:
    m.use_tool_id(6, "bad")
except ValueError:
    print("bad id")
try:
    m.set_events(m.OPTIMIZER_ID, E.PY_START)
except ValueError:
    print("tool not in use")

print(m.get_events(TOOL))
m.set_events(TOOL, E.PY_START | E.PY_RETURN)
print(m.get_events(TOOL) == E.PY_START | E.PY_RETURN)
m.set_events(TOOL, E.NO_EVENTS)

starts = []
returns = []


def on_start(code, offset):
    starts.append(code.co_name)


def on_return(code, offset, value):
    returns.append((code.co_name, value))


print(m.register_callback(TOOL, E.PY_START, on_start))
print(m.register_callback(TOOL, E.PY_RETURN, on_return) is None)
print(m.register_callback(TOOL, E.PY_START, on_start) is on_start)


def target(x):
    return x * 2


def other():
    return target(21)


m.set_events(TOOL, E.PY_START | E.PY_RETURN)
other()
m.set_events(TOOL, E.NO_EVENTS)
print(starts)
print(returns)

starts.clear()
m.set_local_events(TOOL, target.__code__, E.PY_START)
print(m.get_local_events(TOOL, target.__code__) == E.PY_START)
other()
target(1)
m.set_local_events(TOOL, target.__code__, E.NO_EVENTS)
print(starts)

starts.clear()


def disable_after_first(code, offset):
    starts.append(code.co_name)
    return m.DISABLE


m.register_callback(TOOL, E.PY_START, disable_after_first)
m.set_events(TOOL, E.PY_START)
for _ in range(3):
    target(1)
print(starts)
m.restart_events()
target(1)
m.set_events(TOOL, E.NO_EVENTS)
print(starts)

lines = []


def on_line(code, line):
    if code.co_name == "other" and line > code.co_firstlineno:
        lines.append(line - code.co_firstlineno)


m.register_callback(TOOL, E.LINE, on_line)
m.set_events(TOOL, E.LINE)
other()
m.set_events(TOOL, E.NO_EVENTS)
print(lines)

m.register_callback(TOOL, E.PY_START, None)
m.register_callback(TOOL, E.PY_RETURN, None)
m.register_callback(TOOL, E.LINE, None)
m.free_tool_id(TOOL)
print(m.get_tool(TOOL))
