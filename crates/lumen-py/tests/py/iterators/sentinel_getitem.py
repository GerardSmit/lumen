data = [3, 2, 1, 0, 5]
pos = [0]
def nxt():
    v = data[pos[0]]
    pos[0] += 1
    return v

print(list(iter(nxt, 0)))
print(pos[0])

class Reader:
    def __init__(self, lines):
        self.lines = list(lines)
    def readline(self):
        return self.lines.pop(0) if self.lines else ""

r = Reader(["a\n", "b\n", "c\n"])
print([l.strip() for l in iter(r.readline, "")])

n = [0]
def counter():
    n[0] += 1
    return n[0]
for v in iter(counter, 4):
    print(v, end=" ")
print()
si = iter(counter, 100)
print(type(si).__name__, next(si), next(si))

class OldStyle:
    def __getitem__(self, i):
        if i >= 4:
            raise IndexError
        return i * 10

print(list(OldStyle()))
for q in OldStyle():
    print(q, end=",")
print()
print(list(reversed([1, 2, 3])), list(reversed("abc")), list(reversed(range(4))))
print(list(reversed((1, 2))))

class Rev:
    def __len__(self):
        return 3
    def __getitem__(self, i):
        return "xyz"[i]
print(list(reversed(Rev())))

class WithReversed:
    def __reversed__(self):
        return iter(["custom", "reversed"])
print(list(reversed(WithReversed())))
try:
    reversed({1: 2}.items()).__next__
    print("dict items reversible")
except TypeError:
    print("no")
try:
    reversed(iter([1]))
except TypeError as e:
    print("TypeError")
