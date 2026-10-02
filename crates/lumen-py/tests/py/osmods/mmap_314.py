import mmap
import os
import tempfile

print(sorted(n for n in dir(mmap.mmap) if not n.startswith("__")))
print(mmap.PAGESIZE == mmap.ALLOCATIONGRANULARITY, mmap.PROT_READ | mmap.PROT_WRITE)

with tempfile.TemporaryFile() as f:
    f.write(b"hello world" * 100)
    f.flush()
    m = mmap.mmap(f.fileno(), 0)
    print(len(m), m.seekable(), m.tell(), m.readable() if hasattr(m, "readable") else "-")
    print(m.closed, m.read(5), m.tell(), m.find(b"world"), m.rfind(b"hello"))
    m.seek(-3, os.SEEK_END)
    print(m.read())
    try:
        m.seek(5000)
    except ValueError as e:
        print(e)
    m.close()
    print(m.closed)
    try:
        len(m)
    except ValueError as e:
        print(e)

    m = mmap.mmap(f.fileno(), 20, trackfd=False)
    print(len(m), m.size() if False else "")
    try:
        m.size()
    except (OSError, ValueError) as e:
        print(type(e).__name__, e)
    try:
        m.resize(10)
    except (OSError, ValueError, SystemError) as e:
        print(type(e).__name__, e)
    m.close()

    f.truncate(8192)
    f.flush()
    m = mmap.mmap(f.fileno(), 4096)
    print(m.size())
    m.close()

    m = mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ)
    try:
        m[0] = 1
    except TypeError as e:
        print(e)
    try:
        m.write(b"x")
    except TypeError as e:
        print(e)
    m.close()

    m = mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_COPY)
    m[0:3] = b"abc"
    print(m[:5])
    m.close()

m = mmap.mmap(-1, 64)
m.write(b"xyz")
m.seek(0)
print(m.readline(), m[1], m[1:2], m[::-1][:3])
m.seek(0)
m.write_byte(65)
print(m.read_byte() if False else "", m.tell())
m.move(0, 1, 3)
print(m[:5])
m.madvise(mmap.MADV_NORMAL)
try:
    m.madvise(mmap.MADV_NORMAL, 100)
except (ValueError, OverflowError) as e:
    print(type(e).__name__, e)
try:
    m.madvise(mmap.MADV_NORMAL, 0, 100)
except (ValueError, OverflowError) as e:
    print(type(e).__name__, e)
print(m.flush(), m.flush(0, 10) if False else "")
with m:
    pass
print(m.closed)

for bad in ((-1, 0), (-1, -1)):
    try:
        mmap.mmap(*bad)
    except (ValueError, TypeError, OverflowError, OSError) as e:
        print(type(e).__name__, e)
try:
    mmap.mmap(-1, 10, access=7)
except ValueError as e:
    print(e)
try:
    mmap.mmap(-1, 10, flags=mmap.MAP_PRIVATE, access=mmap.ACCESS_READ)
except ValueError as e:
    print(e)
m = mmap.mmap(-1, 10, mmap.MAP_PRIVATE | mmap.MAP_ANON, mmap.PROT_READ | mmap.PROT_WRITE)
print(len(m))
m.close()
