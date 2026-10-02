import gc

print(gc.is_tracked([]), gc.is_tracked(()), gc.is_tracked(set()))
print(gc.is_tracked({}), gc.is_tracked({"a": 1}), gc.is_tracked({"a": []}))
print(gc.is_tracked(1), gc.is_tracked(1.5), gc.is_tracked("s"), gc.is_tracked(b"b"), gc.is_tracked(None))
print(gc.is_tracked(object), gc.is_tracked(int))


class C:
    pass


print(gc.is_tracked(C), gc.is_tracked(C()), gc.is_tracked(lambda: 0))

t = (1, [])
print(gc.is_tracked(t))
try:
    gc.get_objects(generation=-1)
except ValueError as e:
    print("ValueError:", e)
try:
    gc.get_objects(generation=3)
except ValueError as e:
    print("ValueError:", e)

d = {}
d["k"] = d
print(gc.is_tracked(d))
