e = StopIteration()
print(e.value, e.args)
e = StopIteration(5)
print(e.value, e.args)
e = StopIteration("a", "b")
print(e.value, e.args)
print(issubclass(StopIteration, Exception))

def g():
    yield 1
    raise StopIteration("bad")

try:
    list(g())
except RuntimeError as ex:
    print("RuntimeError", ex)

class It:
    def __init__(self):
        self.i = 0
    def __iter__(self):
        return self
    def __next__(self):
        self.i += 1
        if self.i > 2:
            raise StopIteration
        return self.i

for v in It():
    print(v)
else:
    print("for-else ran")

for v in It():
    if v == 1:
        break
else:
    print("not printed")
print("after break", v)

it = iter(range(2))
while True:
    try:
        print(next(it))
    except StopIteration:
        print("stopped")
        break

def collect(iterable):
    it = iter(iterable)
    out = []
    while True:
        try:
            out.append(next(it))
        except StopIteration:
            return out

print(collect(range(4)), collect("hi"), collect({}))
print(next(iter([]), None))
print(next(iter([9]), None))
lst = [1, 2, 3, 4]
for x in lst:
    if x == 2:
        lst.remove(x)
    print(x, end=" ")
print()
it2 = iter(lst)
lst.append(99)
print(list(it2))
d = {1: 1}
try:
    for k in d:
        d[k + 1] = 1
except RuntimeError as ex:
    print("RuntimeError", ex)
