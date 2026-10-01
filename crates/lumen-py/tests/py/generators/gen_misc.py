class Tree:
    def __init__(self, v, l=None, r=None):
        self.v, self.l, self.r = v, l, r
    def __iter__(self):
        if self.l:
            yield from self.l
        yield self.v
        if self.r:
            yield from self.r

t = Tree(4, Tree(2, Tree(1), Tree(3)), Tree(6, Tree(5), Tree(7)))
print(list(t))
print(sum(t), max(t))

def gen_with_args(a, b=2, *args, **kw):
    yield a
    yield b
    yield args
    yield sorted(kw.items())

print(list(gen_with_args(1, 3, 4, 5, x=1, y=2)))

def lazy():
    print("running")
    yield 1

g = lazy()
print("created")
print(next(g))

def echo():
    received = []
    while len(received) < 3:
        received.append((yield len(received)))
    return received

e = echo()
print(next(e))
for v in "xyz":
    try:
        r = e.send(v)
        print("yielded", r)
    except StopIteration as s:
        print("returned", s.value)

def kv():
    d = {"a": 1, "b": 2}
    for k, v in d.items():
        yield k, v
print(dict(kv()))
a, b, *rest = (i * 2 for i in range(6))
print(a, b, rest)
print(list(zip(range(3), (c for c in "abcdef"))))
print(lazy.__name__)
