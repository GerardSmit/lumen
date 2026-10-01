# memoryview over bytes / bytearray, and bytearray resizes while a view exports it.
import struct


def show(f):
    try:
        r = f()
        print("ok", repr(r))
    except Exception as e:
        print(type(e).__name__, e)


ba = bytearray(b"abcdef")
m = memoryview(ba)
print(len(m), m.nbytes, m.itemsize, m.format, m.ndim, m.shape, m.strides, m.readonly, m.obj is ba)
print(m[0], m[-1], m[1:4].tobytes(), m[::2].tolist(), bytes(m[::-1]))
m[0] = 65
m[1:3] = b"BC"
print(ba)
ba[3] = 0x44
print(m.tobytes(), m.hex(), m.hex(":", 2))

# resizing while exported fails, in-place writes still work
show(lambda: ba.append(1))
show(lambda: ba.extend(b"xy"))
show(lambda: ba.pop())
show(lambda: ba.insert(0, 1))
show(lambda: ba.remove(65))
show(lambda: ba.clear())


def iadd():
    global ba
    ba += b"z"


def imul():
    global ba
    ba *= 2


show(iadd)
show(imul)


def del_item():
    del ba[0]


def del_slice():
    del ba[1:3]


def grow_slice():
    ba[0:1] = b"xyz"


def same_slice():
    ba[0:2] = b"qr"


show(del_item)
show(del_slice)
show(grow_slice)
show(same_slice)
show(lambda: ba.reverse())
print(ba, len(ba))

# a sub-view and a cast keep the export alive
s = m[1:3]
c = m.cast("B", (2, 3))
m.release()
show(lambda: ba.append(1))
print(c.tolist(), c.shape, c.strides)
s.release()
c.release()
ba.append(0x21)
print(ba)
show(lambda: m[0])
show(lambda: len(m))
print(repr(m).startswith("<released memory"))

# context manager releases
with memoryview(ba) as v:
    show(lambda: ba.append(1))
ba.append(0x22)
print(ba)

# views of views and of bytes
b = b"\x01\x02\x03\x04\x05\x06\x07\x08"
mb = memoryview(b)
print(mb.readonly, mb.obj is b, memoryview(mb).obj is b, hash(mb) == hash(b))
show(lambda: mb.__setitem__(0, 1))
show(lambda: hash(memoryview(bytearray(2))))
ints = mb.cast("i")
print(ints.format, ints.itemsize, len(ints), ints.tolist() == list(struct.unpack("2i", b)))
show(lambda: ints.cast("h"))
show(lambda: mb.cast("B", (3, 3)))
show(lambda: mb.cast("Z"))
show(lambda: memoryview(1))
show(lambda: memoryview())
show(lambda: memoryview(object=b"ab").tolist())
show(lambda: memoryview("x"))
print(memoryview(b"ab") == b"ab", memoryview(b"ab") == bytearray(b"ab"), memoryview(b"ab") != b"ac")
print(list(memoryview(b"xyz")), memoryview(b"xyz")[::-1].tobytes())

w = bytearray(8)
wm = memoryview(w).cast("H")
wm[1] = 0xFFFF
show(lambda: wm.__setitem__(0, -1))
show(lambda: wm.__setitem__(0, 1.5))
print(w)
d = memoryview(bytearray(16)).cast("d")
d[0] = 1.5
d[1] = 7
print(d.tolist())
fq = memoryview(bytearray(2)).cast("?")
fq[0] = [1]
print(fq.tolist())

# struct over buffers, and iter_unpack keeps its buffer exported
buf = bytearray(8)
struct.pack_into("<hh", buf, 2, 1, -2)
print(buf, struct.unpack_from("<hh", buf, 2), struct.unpack("<q", memoryview(buf)))
struct.pack_into("<h", memoryview(buf)[4:], 0, 7)
print(buf)
itr = struct.iter_unpack("<h", buf)
show(lambda: buf.append(0))
print(list(itr))
buf.append(0)
print(len(buf))
show(lambda: struct.pack_into("b", memoryview(b"ab"), 0, 1))
show(lambda: struct.unpack("<h", memoryview(b"abcd")[::2]))
