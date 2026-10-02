import os
import termios
import tty

master, slave = os.openpty()
print(os.isatty(master), os.isatty(slave))
print(os.get_inheritable(master), os.get_inheritable(slave))

attrs = termios.tcgetattr(slave)
print(len(attrs), len(attrs[6]), all(isinstance(x, int) for x in attrs[:6]))
print(isinstance(attrs[6][termios.VMIN], int), isinstance(attrs[6][termios.VINTR], bytes))

attrs[3] &= ~termios.ECHO
termios.tcsetattr(slave, termios.TCSANOW, attrs)
print(bool(termios.tcgetattr(slave)[3] & termios.ECHO))
attrs[3] |= termios.ECHO
termios.tcsetattr(slave, termios.TCSADRAIN, attrs)
print(bool(termios.tcgetattr(slave)[3] & termios.ECHO))

termios.tcsetwinsize(slave, (24, 80))
print(termios.tcgetwinsize(slave))
termios.tcflush(slave, termios.TCIOFLUSH)
termios.tcdrain(slave)

tty.setraw(slave)
raw = termios.tcgetattr(slave)
print(bool(raw[3] & termios.ICANON), bool(raw[3] & termios.ECHO))
tty.setcbreak(slave)
print(bool(termios.tcgetattr(slave)[3] & termios.ICANON))

try:
    termios.tcgetattr(os.pipe()[0])
except termios.error as e:
    print("termios.error", e.args[0] == 25 or e.args[0] == 19)
try:
    termios.tcsetattr(slave, termios.TCSANOW, [1, 2, 3])
except TypeError as e:
    print("TypeError")
print(termios.error.__name__, issubclass(termios.error, Exception))

os.write(master, b"ping\n")
print(os.read(slave, 5))
os.close(master)
os.close(slave)
