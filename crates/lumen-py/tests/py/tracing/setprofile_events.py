import sys


def h():
    return len("ab")


events = []


def profiler(frame, event, arg):
    if event in ("call", "return") and frame.f_code.co_name == "h":
        events.append((event, arg))
    elif event.startswith("c_") and getattr(arg, "__name__", "") == "len":
        events.append((event, arg.__name__))


print(sys.getprofile())
sys.setprofile(profiler)
print(sys.getprofile() is profiler)
h()
sys.setprofile(None)
print(events)
print(sys.getprofile())


def failing():
    raise KeyError("k")


seen = []


def profiler2(frame, event, arg):
    if frame.f_code.co_name == "failing" and event in ("call", "return"):
        seen.append((event, arg))


sys.setprofile(profiler2)
try:
    failing()
except KeyError:
    pass
sys.setprofile(None)
print(seen)
