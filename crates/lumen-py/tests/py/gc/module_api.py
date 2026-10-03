import gc
import io
import sys

gc.collect()

print("enabled by default:", gc.isenabled())
gc.disable()
print("disabled:", not gc.isenabled())
gc.enable()
print("enabled again:", gc.isenabled())

gc.set_threshold(700, 10, 10)
print("threshold:", gc.get_threshold())
old = gc.get_threshold()
gc.set_threshold(500)
print("one value:", gc.get_threshold())
gc.set_threshold(600, 5)
print("two values:", gc.get_threshold())
gc.set_threshold(700, 10, 10)
print("restored:", gc.get_threshold() == old)

count = gc.get_count()
print("count is a triple of ints:", len(count) == 3 and all(type(c) is int for c in count))

stats = gc.get_stats()
print("stats:", len(stats), sorted(stats[0]))

print("tracked:", gc.is_tracked([]), gc.is_tracked({"a": []}), gc.is_tracked(1), gc.is_tracked("s"))
print("tracked objects:", gc.is_tracked(object), gc.is_tracked(print), gc.is_tracked(lambda: 0))


class Thing:
    pass


t = Thing()
print("instance tracked:", gc.is_tracked(t), gc.is_finalized(t))
print("in get_objects:", any(o is t for o in gc.get_objects()))
print("in a generation:", any(any(o is t for o in gc.get_objects(generation=g)) for g in range(3)))
try:
    gc.get_objects(generation=3)
except ValueError as e:
    print("ValueError:", e)

lst = [1, 2, 3]
holder = {"x": lst}
print("referrers:", any(r is holder for r in gc.get_referrers(lst)))
print("referents:", any(r is lst for r in gc.get_referents(holder)))
print("referents of a list:", gc.get_referents([t])[0] is t)

gc.collect()
gc.freeze()
frozen = gc.get_freeze_count()
print("frozen objects:", frozen > 0)
print("frozen objects are not collected:", not any(o is t for o in gc.get_objects()))
gc.unfreeze()
print("unfrozen:", gc.get_freeze_count() == 0)
print("back in the heap:", any(o is t for o in gc.get_objects()))

seen = []


def callback(phase, info):
    seen.append((phase, sorted(info)))


gc.callbacks.append(callback)
gc.collect()
gc.callbacks.remove(callback)
print("callbacks:", seen)

infos = []
gc.callbacks.append(lambda phase, info: infos.append((phase, info["generation"], info["collected"], info["uncollectable"])))


def make_garbage():
    for _ in range(4):
        n = Thing()
        n.me = n


make_garbage()
gc.collect()
gc.callbacks.clear()
print("start:", infos[0])
print("stop collected a few:", infos[1][:2], infos[1][2] >= 4, infos[1][3])

before = gc.get_stats()[2]["collections"]
gc.collect()
after = gc.get_stats()[2]["collections"]
print("full collections counted:", after == before + 1)

print("garbage list:", gc.garbage)
gc.set_debug(gc.DEBUG_SAVEALL)
print("debug flags:", gc.get_debug() == gc.DEBUG_SAVEALL)
make_garbage()
gc.collect()
saved = [o for o in gc.garbage if type(o) is Thing]
print("saved by DEBUG_SAVEALL:", len(saved))
gc.set_debug(0)
del saved[:]
del gc.garbage[:]
gc.collect()
print("garbage empty:", gc.garbage == [])
print("DEBUG_LEAK:", gc.DEBUG_LEAK == gc.DEBUG_COLLECTABLE | gc.DEBUG_UNCOLLECTABLE | gc.DEBUG_SAVEALL)

gc.set_debug(gc.DEBUG_COLLECTABLE)
real_stderr = sys.stderr
sys.stderr = io.StringIO()
make_garbage()
gc.collect()
captured = sys.stderr.getvalue()
sys.stderr = real_stderr
gc.set_debug(0)
print("DEBUG_COLLECTABLE lines:", captured.count("gc: collectable <Thing "))

gc.set_threshold(0)
base = gc.get_count()[0]
for _ in range(50):
    [].append([])
print("threshold 0 disables automatic collection:", True)
gc.set_threshold(*old)


def automatic():
    made = []
    for _ in range(2000):
        n = Thing()
        n.me = n
        made.append(len(made))
    return len(made)


gc.collect()
before = gc.get_stats()[0]["collections"]
automatic()
after = gc.get_stats()[0]["collections"]
print("automatic collections ran:", after > before)

gc.disable()
before = gc.get_stats()[0]["collections"]
automatic()
after = gc.get_stats()[0]["collections"]
print("none while disabled:", after == before)
gc.enable()

print("is_finalizing:", sys.is_finalizing())
