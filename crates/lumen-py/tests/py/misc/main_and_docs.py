"""Module docstring."""
import sys

print(__name__)
print(__doc__)
print(len(sys.argv))
print(__name__ == "__main__")

def f():
    """Function doc."""
    pass

def g():
    pass

class C:
    """Class doc."""
    def m(self):
        """Method doc."""

print(f.__doc__, g.__doc__, C.__doc__, C.m.__doc__, C().m.__doc__)
print(f.__name__, f.__qualname__, C.m.__qualname__, C.__module__, f.__module__)

def main():
    print("in main")
    return 0

if __name__ == "__main__":
    print("running as script")
    rc = main()
    print("rc", rc)

x = ...
print(x, ..., type(...).__name__, x is Ellipsis, bool(...))
print(None is None, True is True, type(None).__name__, None == None, bool(None))
print(True + 1, False * 5, int(True), str(False), True == 1)
print("ab" * 3, "-" * 10, 3 * "x", "a" * 0 + "|", "ab" * -1 + "|", [1, 2] * 2, (0,) * 3)
print("=" * 5 + ">" + " " * 2 + "<")

def stub():
    pass
def stub2():
    ...
print(stub(), stub2())

for _ in range(2):
    pass
while False:
    pass
if 0:
    pass
else:
    print("else branch")
print(sys.version_info[0] == 3, sys.version_info >= (3, 12), type(sys.argv).__name__)
print(sys.maxsize == 2 ** 63 - 1, sys.byteorder in ("little", "big"), isinstance(sys.platform, str))
print(globals().get("__name__"), "x" in globals(), callable(globals))
