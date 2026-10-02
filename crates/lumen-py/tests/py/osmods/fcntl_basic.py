import array
import fcntl
import os
import struct
import termios

r, w = os.pipe()
flags = fcntl.fcntl(r, fcntl.F_GETFD)
print(flags & fcntl.FD_CLOEXEC == fcntl.FD_CLOEXEC)
fcntl.fcntl(r, fcntl.F_SETFD, 0)
print(fcntl.fcntl(r, fcntl.F_GETFD))
fl = fcntl.fcntl(r, fcntl.F_GETFL)
fcntl.fcntl(r, fcntl.F_SETFL, fl | os.O_NONBLOCK)
print(bool(fcntl.fcntl(r, fcntl.F_GETFL) & os.O_NONBLOCK))
print(fcntl.fcntl(r, fcntl.F_DUPFD, 10) >= 10)

os.write(w, b"12345")
buf = fcntl.ioctl(r, termios.FIONREAD, struct.pack("i", 0))
print(struct.unpack("i", buf)[0])
arr = array.array("i", [0])
print(fcntl.ioctl(r, termios.FIONREAD, arr, True) >= 0, arr[0])

path = "/tmp/lumen-fcntl-test-%d" % os.getpid()
fd = os.open(path, os.O_RDWR | os.O_CREAT, 0o600)
fd2 = os.open(path, os.O_RDWR)
fcntl.flock(fd, fcntl.LOCK_EX)
try:
    fcntl.flock(fd2, fcntl.LOCK_EX | fcntl.LOCK_NB)
except OSError as e:
    print("flock busy", e.errno in (11, 35, 13))
fcntl.flock(fd, fcntl.LOCK_UN)
fcntl.flock(fd2, fcntl.LOCK_EX | fcntl.LOCK_NB)
fcntl.flock(fd2, fcntl.LOCK_UN)
fcntl.lockf(fd, fcntl.LOCK_SH)
fcntl.lockf(fd, fcntl.LOCK_UN)
fcntl.lockf(fd, fcntl.LOCK_EX | fcntl.LOCK_NB, 10, 0, 0)
fcntl.lockf(fd, fcntl.LOCK_UN, 10, 0, 0)
try:
    fcntl.lockf(fd, 0)
except ValueError as e:
    print("ValueError", e)
try:
    fcntl.fcntl(fd, fcntl.F_GETFD, b"x" * 2000)
except ValueError:
    print("ValueError")
try:
    fcntl.fcntl(-1, fcntl.F_GETFD)
except ValueError as e:
    print("ValueError", e)
try:
    fcntl.fcntl(9999, fcntl.F_GETFD)
except OSError as e:
    print("OSError", e.errno)
for x in (r, w, fd, fd2):
    os.close(x)
os.unlink(path)
