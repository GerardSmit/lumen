__all__ = ["f", "double", "Box", "CONSTANT"]

print("load util", __name__)

CONSTANT = 7
_private = "hidden"
not_exported = "not in __all__"
counter = 0


def f(x):
    return x + 1


def double(x):
    return x * 2


def bump():
    global counter
    counter += 1
    return counter


class Box:
    def __init__(self, v):
        self.v = v

    def __repr__(self):
        return "Box(%r)" % (self.v,)
