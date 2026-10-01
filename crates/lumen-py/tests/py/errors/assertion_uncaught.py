def check(n):
    assert n > 0, "n must be positive"
    return n


print(check(1))
print(check(0) if False else "skipping")
assert False, "message"
print("unreachable")
