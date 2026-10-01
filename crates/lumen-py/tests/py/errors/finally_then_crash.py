def work():
    try:
        print("working")
        raise ValueError("crash")
    finally:
        print("cleanup runs first")


try:
    print("outer")
finally:
    print("outer finally")
work()
print("unreachable")
