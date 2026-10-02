import io
import pickle
import _pickle
import copyreg


class Point:
    def __init__(self, x, y):
        self.x = x
        self.y = y

    def __eq__(self, other):
        return type(other) is Point and (self.x, self.y) == (other.x, other.y)


class Slots:
    __slots__ = ("a", "b")

    def __init__(self, a, b):
        self.a = a
        self.b = b


class MyInt(int):
    pass


class MyList(list):
    pass


class MyDict(dict):
    pass


samples = [
    None, True, False, 0, 1, -1, 255, 256, 65535, 65536, 2**31 - 1, -(2**31), 2**31, 2**100, -(2**100),
    1.5, -0.0, 1e300, "", "abc", "héllo", "€", "\U0001f600", b"", b"xyz",
    (), (1,), (1, 2), (1, 2, 3), (1, 2, 3, 4), [], [1, 2, 3], {}, {"a": 1, "b": [2, 3]},
    set(), {1, 2, 3}, frozenset(), frozenset({4, 5}), bytearray(b"ba"), ...,
]

for proto in range(6):
    print("protocol", proto)
    for obj in samples:
        data = pickle.dumps(obj, proto)
        back = pickle.loads(data)
        assert back == obj and type(back) is type(obj), (proto, obj)
        print(" ", repr(obj)[:30], data)

shared = [1, 2]
tup = (shared, shared)
for proto in range(6):
    data = pickle.dumps(tup, proto)
    back = pickle.loads(data)
    assert back[0] is back[1]
    print(proto, data)

cyc = []
cyc.append(cyc)
for proto in range(6):
    back = pickle.loads(pickle.dumps(cyc, proto))
    assert back[0] is back
print("cycle ok")

for proto in range(6):
    p = Point(1, [2, 3])
    data = pickle.dumps(p, proto)
    assert pickle.loads(data) == p
    print("point", proto, data)

for proto in range(2, 6):
    s = pickle.loads(pickle.dumps(Slots(1, "x"), proto))
    print("slots", proto, s.a, s.b)

for proto in range(6):
    for obj in (MyInt(5), MyList([1, 2]), MyDict(a=1)):
        back = pickle.loads(pickle.dumps(obj, proto))
        print("subclass", proto, type(back).__name__, back)

big = list(range(2500))
for proto in range(6):
    data = pickle.dumps(big, proto)
    assert pickle.loads(data) == big
    print("batched", proto, len(data))

bigd = {i: str(i) for i in range(2500)}
for proto in range(6):
    data = pickle.dumps(bigd, proto)
    assert pickle.loads(data) == bigd
    print("batched dict", proto, len(data))

payload = b"x" * 200000
for proto in (3, 4, 5):
    data = pickle.dumps(payload, proto)
    assert pickle.loads(data) == payload
    print("large bytes", proto, len(data))
text = "y" * 200000
data = pickle.dumps(text, 4)
assert pickle.loads(data) == text
print("large str", len(data))

buf = io.BytesIO()
p = pickle.Pickler(buf, 4)
p.dump([1, 2])
p.dump([1, 2])
print(buf.getvalue())
buf.seek(0)
u = pickle.Unpickler(buf)
print(u.load(), u.load())
try:
    u.load()
except EOFError as e:
    print("EOFError", e)

p = pickle.Pickler(io.BytesIO(), 4)
p.fast = True
print(p.fast, p.bin)
p.fast = False


class PIDPickler(pickle.Pickler):
    def persistent_id(self, obj):
        if isinstance(obj, Point):
            return ("pt", obj.x)
        return None


class PIDUnpickler(pickle.Unpickler):
    def persistent_load(self, pid):
        return Point(pid[1], "loaded")


for proto in range(6):
    buf = io.BytesIO()
    try:
        PIDPickler(buf, proto).dump([Point(7, 8), 1])
    except pickle.PicklingError as e:
        print("pers", proto, "PicklingError", e)
        continue
    buf.seek(0)
    out = PIDUnpickler(buf).load()
    print("pers", proto, out[0].x, out[0].y, out[1])

try:
    pickle.loads(pickle.dumps(Point(1, 2), 4).replace(b"Point", b"Pxint"))
except Exception as e:
    print(type(e).__name__)

try:
    pickle.dumps(lambda: 0)
except pickle.PicklingError as e:
    print("PicklingError")

try:
    pickle.dumps(1, protocol=6)
except ValueError as e:
    print("ValueError", e)

for bad in (b"", b"\x80\x04", b"K", b"(", b"\x95\x10\x00\x00\x00\x00\x00\x00\x00N."):
    try:
        pickle.loads(bad)
    except Exception as e:
        print(type(e).__name__, e)

try:
    pickle.loads(b"\xff")
except pickle.UnpicklingError as e:
    print("UnpicklingError", e)

try:
    pickle.loads(b"h\x05.")
except pickle.UnpicklingError as e:
    print("UnpicklingError", e)

print(pickle.loads(b"I01\n."), pickle.loads(b"I00\n."), pickle.loads(b"I12\n."), pickle.loads(b"L123L\n."))
print(pickle.loads(b"F1.5\n."), pickle.loads(b"S'abc'\n."), pickle.loads(b"Vabc\\u20ac\n."))
print(pickle.loads(b"S'a\\xe9'\n.", encoding="latin1"), pickle.loads(b"S'abc'\n.", encoding="bytes"))
print(pickle.loads(b"(lp0\nI1\naI2\na."), pickle.loads(b"(dp0\nS'k'\nI1\ns."))

print(pickle.loads(pickle.dumps(range(3), 2)), pickle.loads(pickle.dumps(1 + 2j, 2)))

print(pickle.loads(pickle.dumps(Point, 2)).__name__, pickle.loads(pickle.dumps(len, 2)).__name__)
print(pickle.dumps(Point, 0), pickle.dumps(len, 2), pickle.dumps(len, 4))

import collections
od = collections.OrderedDict([("a", 1), ("b", 2)])
dq = collections.deque([1, 2, 3], maxlen=5)
dd = collections.defaultdict(list, {"z": [1]})
for proto in range(6):
    assert pickle.loads(pickle.dumps(od, proto)) == od
    assert list(pickle.loads(pickle.dumps(dq, proto))) == [1, 2, 3]
    assert pickle.loads(pickle.dumps(dd, proto)) == dd
print("collections ok")

p = pickle.Pickler(io.BytesIO(), 4)
p.dump("memo me")
memo = p.memo
print(type(memo).__name__, len(memo.copy()))
copy = memo.copy()
p.clear_memo()
print(len(p.memo.copy()))
p.memo = copy
print(len(p.memo.copy()))
print(type(memo.__reduce__()[0]).__name__)

u = pickle.Unpickler(io.BytesIO(pickle.dumps(["m"], 4)))
u.load()
print(type(u.memo).__name__, u.memo.copy())
u.memo.clear()
print(u.memo.copy())

pb = pickle.PickleBuffer(bytearray(b"abcdef"))
print(bytes(pb.raw()), bytes(memoryview(pb)))
pb.release()
try:
    pb.raw()
except ValueError as e:
    print("ValueError", e)
try:
    pickle.PickleBuffer("str")
except TypeError as e:
    print("TypeError", e)

for obj in (b"ro-bytes", bytearray(b"rw-bytes")):
    pb = pickle.PickleBuffer(obj)
    data = pickle.dumps(pb, protocol=5)
    back = pickle.loads(data)
    print(type(back).__name__, bytes(back))
    collected = []
    data = pickle.dumps(pb, protocol=5, buffer_callback=collected.append)
    print(data, len(collected))
    back = pickle.loads(data, buffers=collected)
    print(type(back).__name__, bytes(back), back.readonly if hasattr(back, "readonly") else None)
try:
    pickle.dumps(pickle.PickleBuffer(b"x"), protocol=4)
except pickle.PicklingError as e:
    print("PicklingError", e)
try:
    pickle.loads(pickle.dumps(pickle.PickleBuffer(b"x"), protocol=5, buffer_callback=lambda b: None))
except pickle.UnpicklingError as e:
    print("UnpicklingError", e)


class Reducer:
    def __init__(self, v):
        self.v = v

    def __reduce__(self):
        return (Reducer, (self.v,))


class Override(pickle.Pickler):
    def reducer_override(self, obj):
        if isinstance(obj, Point):
            return Reducer, (obj.x,)
        return NotImplemented


buf = io.BytesIO()
Override(buf, 4).dump([Point(1, 2)])
print(pickle.loads(buf.getvalue())[0].v)


class Dispatch(pickle.Pickler):
    dispatch_table = copyreg.dispatch_table.copy()
    dispatch_table[Point] = lambda p: (Reducer, (p.x + 100,))


buf = io.BytesIO()
Dispatch(buf, 4).dump(Point(5, 6))
print(pickle.loads(buf.getvalue()).v)

copyreg.add_extension("builtins", "len", 240)
print(pickle.dumps(len, 2), pickle.loads(pickle.dumps(len, 2)) is len)
copyreg.remove_extension("builtins", "len", 240)

print(pickle.dumps(1, 0, fix_imports=True), pickle.dumps(collections.OrderedDict, 2))
print(pickle.dumps(range, 2, fix_imports=True), pickle.dumps(range, 2, fix_imports=False))
print(pickle.loads(b"c__builtin__\nlen\n."))
try:
    pickle.loads(b"c__builtin__\nlen\n.", fix_imports=False)
except ImportError as e:
    print("ImportError", e)

print(_pickle.dumps is pickle.dumps, _pickle.Pickler is pickle.Pickler)
print(pickle.PickleError.__module__, pickle.PicklingError.__mro__[1].__name__, pickle.UnpicklingError.__bases__[0].__name__)
