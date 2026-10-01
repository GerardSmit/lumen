class CM:
    def __enter__(self):
        print("enter")
        return self
    def __exit__(self, *a):
        print("exit", a[0].__name__)
        return False

def work():
    try:
        print("in try")
        raise ValueError("first")
    finally:
        print("in finally")

try:
    work()
except ValueError as e:
    print("caught", e)
with CM():
    raise OverflowError("inside with")
