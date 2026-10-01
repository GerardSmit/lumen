def gen():
    yield 1
    yield 2
    raise ValueError("generator failed")

g = gen()
print(next(g))
print(next(g))
for v in gen():
    print("got", v)
