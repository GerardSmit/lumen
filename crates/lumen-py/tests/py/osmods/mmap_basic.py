import mmap
import os
import sys

m = mmap.mmap(-1, 16)
print(len(m), m.closed, m.tell())
m[0:5] = b"hello"
m.seek(5)
m.write(b" world")
print(m.tell(), m[:11])
m.seek(0)
print(m.read(5), m.readline(), m.tell())
print(m.find(b"world"), m.rfind(b"o"), m.find(b"zzz"), m.find(b"o", 5))
print(m[0], m[-1], m[2:8:2])
m[1] = 69
print(m[:5])
m.move(0, 6, 5)
print(m[:11])
mv = memoryview(m)
print(len(mv), bytes(mv[:5]))
try:
    m.close()
except BufferError as e:
    print("BufferError", e)
mv.release()
m.close()
print(m.closed)
try:
    m[0]
except ValueError as e:
    print("ValueError", e)

path = "/tmp/lumen-mmap-test-%d" % os.getpid()
fd = os.open(path, os.O_RDWR | os.O_CREAT | os.O_TRUNC, 0o600)
os.write(fd, b"abcdefghij" * 10)
with mmap.mmap(fd, 0) as mf:
    print(len(mf), mf[:10], mf.size())
    mf[10:13] = b"XYZ"
    mf.flush()
    resized = True
    if sys.platform.startswith("linux"):
        mf.resize(150)
        resized = len(mf) == 150 and mf.size() == 150
    print(resized)
ro = mmap.mmap(fd, 20, access=mmap.ACCESS_READ)
print(ro[:15])
try:
    ro[0] = 1
except TypeError as e:
    print("TypeError", e)
ro.close()
cp = mmap.mmap(fd, 20, access=mmap.ACCESS_COPY)
cp[0:3] = b"###"
print(cp[:12])
cp.close()
os.lseek(fd, 0, 0)
print(os.read(fd, 15))
os.close(fd)
os.unlink(path)
print(mmap.PAGESIZE > 0, mmap.ACCESS_DEFAULT, mmap.ACCESS_READ, mmap.ACCESS_WRITE, mmap.ACCESS_COPY)
