# Indices and shapes beyond Py_ssize_t, and memoryview.cast shape-product overflow.
from collections import deque
def t(f):
    try:
        print(f())
    except Exception as e:
        print(type(e).__name__, e)
m = memoryview(b"x" * 8)
for k in (2**70, -2**70, 2**63, 10):
    t(lambda: m[k])
t(lambda: m[2**70:3].tobytes())
t(lambda: m[-2**70:3].tobytes())
mb = memoryview(bytearray(8))
def setit():
    mb[2**70] = 1
t(setit)
for sh in ([2**62, 4], [2**70], [2**63 - 1, 2], [0, 2**62, 4], [4, 2], [2**61, 2**3], [1] * 65, [3], [-1, 8], [2.0, 4]):
    t(lambda: m.cast("B", shape=sh).shape)
t(lambda: m.cast("I", shape=[2]).cast("B", shape=[4, 2]).shape)
t(lambda: m.cast("Q", shape=[]).shape)
t(lambda: memoryview(b"").cast("B", shape=[0]))
t(lambda: memoryview(b"abc").cast("I"))
t(lambda: memoryview(b"abcd").cast("I", shape=[2]))
d = deque([1, 2])
t(lambda: d[2**70])
t(lambda: [1][2**70])
t(lambda: (1,)[2**70])
t(lambda: "a"[2**70])
t(lambda: b"a"[2**70])
t(lambda: bytearray(b"a")[2**70])
t(lambda: range(3)[2**70])
t(lambda: [1] * 2**70)
t(lambda: d.rotate(2**70))
t(lambda: d.insert(2**70, 1))
t(lambda: "abc".find("b", 2**70))
t(lambda: b"abc".find(b"b", 2**70))
t(lambda: deque([], 2**70))
