import fcntl
import os
import select
import termios
import tty

master = os.posix_openpt(os.O_RDWR | os.O_NOCTTY)
os.grantpt(master)
os.unlockpt(master)
slave = os.open(os.ptsname(master), os.O_RDWR | os.O_NOCTTY)

attrs = termios.tcgetattr(slave)
print(len(attrs), type(attrs[6]).__name__, len(attrs[6]) == termios.NCCS)
print(attrs[0] & termios.ICRNL != 0, attrs[3] & termios.ECHO != 0, attrs[3] & termios.ICANON != 0)
print(attrs[4] == attrs[5], attrs[4] in (termios.B9600, termios.B38400, termios.B115200))
print(termios.tcgetwinsize(slave))
termios.tcsetwinsize(slave, (30, 100))
print(termios.tcgetwinsize(slave))
try:
    termios.tcsetwinsize(slave, (1,))
except (TypeError, ValueError, OverflowError) as e:
    print(type(e).__name__, e)
try:
    termios.tcsetwinsize(slave, (1 << 20, 1))
except (TypeError, ValueError, OverflowError) as e:
    print(type(e).__name__, e)

raw = list(attrs)
raw[3] &= ~(termios.ECHO | termios.ICANON)
raw[6] = list(raw[6])
raw[6][termios.VMIN] = 1
raw[6][termios.VTIME] = 0
termios.tcsetattr(slave, termios.TCSANOW, raw)
print(termios.tcgetattr(slave)[3] & termios.ICANON)
termios.tcflush(slave, termios.TCIOFLUSH)
termios.tcsendbreak(slave, 0)
termios.tcflow(slave, termios.TCOON)
print(termios.tcgetattr(slave)[6][termios.VMIN])

for bad in ((5, termios.TCSANOW, raw), (slave, 99, raw), (slave, termios.TCSANOW, raw[:5]), (slave, termios.TCSANOW, 5)):
    try:
        termios.tcsetattr(*bad)
    except (TypeError, termios.error, ValueError, OSError) as e:
        print(type(e).__name__, e)
try:
    termios.tcgetattr(os.open("/dev/null", os.O_RDONLY))
except termios.error as e:
    print("termios.error", e.args)
print(termios.error.__mro__[1].__name__, termios.error.__module__)

os.set_blocking(master, False)
os.write(slave, b"out\n")
r, _, _ = select.select([master], [], [], 1)
print(bool(r), os.read(master, 100))
os.write(master, b"in")
print(os.read(slave, 10))
print(os.get_terminal_size(slave) if os.isatty(slave) else None)

buf = bytearray(8)
print(fcntl.ioctl(slave, termios.TIOCGWINSZ, buf), bytes(buf))
print(fcntl.ioctl(slave, termios.TIOCGWINSZ, b"\0" * 8))
fl = fcntl.fcntl(slave, fcntl.F_GETFL)
fcntl.fcntl(slave, fcntl.F_SETFL, fl | os.O_NONBLOCK)
print(bool(fcntl.fcntl(slave, fcntl.F_GETFL) & os.O_NONBLOCK))
print(fcntl.fcntl(slave, fcntl.F_GETFD) & fcntl.FD_CLOEXEC)
print(fcntl.fcntl(slave, fcntl.F_DUPFD_CLOEXEC, 50) >= 50)
print(fcntl.fcntl(slave, fcntl.F_GETFL, 0) == fcntl.fcntl(slave, fcntl.F_GETFL))
try:
    fcntl.fcntl(-1, fcntl.F_GETFL)
except (OSError, ValueError) as e:
    print(type(e).__name__, e)
try:
    fcntl.fcntl(slave, fcntl.F_GETFL, "x")
except TypeError as e:
    print(e)
print(fcntl.fcntl(slave, fcntl.F_SETFD, 0))

import tempfile
with tempfile.TemporaryFile() as f:
    fcntl.flock(f, fcntl.LOCK_EX | fcntl.LOCK_NB)
    fcntl.flock(f, fcntl.LOCK_UN)
    fcntl.lockf(f, fcntl.LOCK_SH)
    fcntl.lockf(f, fcntl.LOCK_UN, 10, 5, 1)
    try:
        fcntl.lockf(f, 99)
    except ValueError as e:
        print(e)
    print(fcntl.F_GETLK, fcntl.F_SETLK, fcntl.F_SETLKW)

old = termios.tcgetattr(slave)
tty.setraw(slave)
print(termios.tcgetattr(slave)[3] & (termios.ECHO | termios.ICANON | termios.ISIG))
tty.setcbreak(slave)
print(termios.tcgetattr(slave)[3] & termios.ICANON)
tty.cfmakeraw(old)
print(old[3] & termios.ECHO)
tty.cfmakecbreak(old)
print(old[6][termios.VMIN], old[6][termios.VTIME])
os.close(slave)
os.close(master)
