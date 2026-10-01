"""Helper module."""
print("loading _helper_mod as", __name__)

VALUE = 42
_private = "hidden"
names = ["a", "b"]

def greet(who):
    return "hello " + who

class Box:
    def __init__(self, v):
        self.v = v
    def __repr__(self):
        return f"Box({self.v})"

counter = 0
def bump():
    global counter
    counter += 1
    return counter
