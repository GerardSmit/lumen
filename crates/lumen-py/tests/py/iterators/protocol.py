class Count:
    def __init__(self, n):
        self.n = n
    def __iter__(self):
        self.i = 0
        return self
    def __next__(self):
        if self.i >= self.n:
            raise StopIteration
        self.i += 1
        return self.i

c = Count(4)
print(list(c))
print(list(c))
it = iter(c)
print(next(it), next(it))
print(it is iter(it))

class Seq:
    def __init__(self, data):
        self.data = data
    def __iter__(self):
        return SeqIter(self.data)

class SeqIter:
    def __init__(self, data):
        self.data = data
        self.pos = 0
    def __iter__(self):
        return self
    def __next__(self):
        if self.pos >= len(self.data):
            raise StopIteration("done")
        v = self.data[self.pos]
        self.pos += 1
        return v

s = Seq("abc")
print(list(s), list(s))
for a in s:
    for b in s:
        print(a + b, end=" ")
print()
print(sorted(Seq([3, 1, 2])), sum(Seq([1, 2, 3])), min(Seq([5, 4])))
x, y, z = Seq([7, 8, 9])
print(x, y, z)
print(list(iter([1, 2, 3])), list(iter("ab")), list(iter((1,))), list(iter({"k": 1})))
i = iter([1])
print(next(i))
print(next(i, "default"))
try:
    next(i)
except StopIteration as e:
    print("StopIteration", e.args)
try:
    iter(5)
except TypeError as e:
    print("TypeError", e)
try:
    next([1])
except TypeError as e:
    print("TypeError", e)
print(3 in Seq([1, 2, 3]), 9 in Seq([1]))
print(tuple(Seq("xy")), list(enumerate(Seq("xy"), 1)))
