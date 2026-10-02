import _lsprof


def fact(n):
    return 1 if n <= 1 else n * fact(n - 1)


def leaf():
    return sum([1, 2, 3])


def branch():
    leaf()
    leaf()
    return fact(3)


def python_entries(profiler):
    return sorted(
        (e.code.co_name, e.callcount, e.reccallcount)
        for e in profiler.getstats()
        if not isinstance(e.code, str)
    )


p = _lsprof.Profiler()
p.enable()
branch()
p.disable()
print(python_entries(p))

for e in p.getstats():
    if not isinstance(e.code, str) and e.code.co_name == "branch":
        subs = sorted((s.code.co_name, s.callcount) for s in e.calls if not isinstance(s.code, str))
        print(subs)
        print(e.totaltime >= e.inlinetime >= 0)

p.clear()
print(p.getstats())

timer_calls = []


def fake_timer():
    timer_calls.append(1)
    return len(timer_calls)


q = _lsprof.Profiler(fake_timer, 1.0, False, True)
q.enable()
leaf()
q.disable()
print(len(timer_calls) > 0)
print(python_entries(q))
print([[(s.callcount, s.totaltime) for s in e.calls] for e in q.getstats() if not isinstance(e.code, str) and e.code.co_name == "leaf"])
