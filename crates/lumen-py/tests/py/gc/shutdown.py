import atexit
import sys


class Loud:
    def __init__(self, name):
        self.name = name

    def __del__(self):
        print("del", self.name)


a = Loud("a")
b = Loud("b")
cyc = Loud("cycle")
cyc.me = cyc
atexit.register(lambda: print("atexit 1"))
atexit.register(lambda: print("atexit 2, finalizing:", sys.is_finalizing()))
print("end of script, finalizing:", sys.is_finalizing())
