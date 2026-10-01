import _string
import _thread
import atexit
import gc
import sys
from collections import deque, defaultdict, OrderedDict, Counter, ChainMap, namedtuple

print(list(_string.formatter_parser("a{0!r:>5}b{{c}}{name.x[1]}")))
print(_string.formatter_field_name_split("a.b[0].c")[0], list(_string.formatter_field_name_split("a.b[0].c")[1]))

lock = _thread.allocate_lock()
print(lock.acquire(), lock.locked(), lock.acquire(False))
lock.release()
print(lock.locked())
with lock:
    print(lock.locked())
rl = _thread.RLock()
with rl:
    with rl:
        print("reentrant")
print(isinstance(_thread.get_ident(), int))
local = _thread._local()
local.x = 5
print(local.x)

atexit.register(print, "bye", 1)
atexit.register(print, "never")
atexit.unregister(print)
atexit.register(print, "bye", 2)

print(gc.isenabled(), gc.collect() >= 0)
print(sys.version_info[:2] >= (3, 12), sys.version_info.major, sys.implementation.name != "")
print(isinstance(sys.getrecursionlimit(), int), sys.maxsize, sys.byteorder)
f = sys._getframe()
print(f.f_code.co_name, f.f_globals is globals(), f.f_back is None)

d = deque([1, 2, 3], maxlen=3)
d.append(4)
d.appendleft(0)
print(d, d.maxlen, d.pop(), d.popleft(), len(d))
d.extend("ab")
d.rotate(1)
print(list(d), d[0], d[-1])
print(deque(range(5)) == deque(range(5)), deque("ab") < deque("ac"), deque([1]) * 3)
dd = defaultdict(list)
dd["a"].append(1)
print(dd, dd["b"], sorted(dd))
print(Counter("abracadabra").most_common(2))
print(OrderedDict(a=1, b=2), list(ChainMap({"a": 1}, {"b": 2}).items()))
P = namedtuple("P", "x y")
print(P(1, 2), P(1, 2)._replace(x=5), P._fields)
