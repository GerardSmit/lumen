import random
import _random


class Sub(random.Random):
    def __init__(self, x=None):
        super().__init__(x)
        self.extra = "ok"

    def random(self):
        return 0.25


s = Sub(3)
print(s.extra, s.random(), s.randint(1, 4) in range(1, 5))


class Plain(random.Random):
    pass


p = Plain(10)
q = random.Random(10)
print([p.random() for _ in range(3)] == [q.random() for _ in range(3)])

n = _random.Random(7)
m = _random.Random()
m.seed(7)
print(n.random() == m.random(), n.getrandbits(0), n.getrandbits(1) in (0, 1), n.getrandbits(64).bit_length() <= 64)
for bad in (-1, "x", 1.5):
    try:
        n.getrandbits(bad)
    except (TypeError, ValueError) as e:
        print(type(e).__name__)
try:
    n.setstate(5)
except TypeError as e:
    print("TypeError")
try:
    n.setstate((1, 2))
except ValueError as e:
    print("ValueError")
try:
    n.setstate((1,) * 624 + (625,))
except ValueError as e:
    print("ValueError")
st = n.getstate()
x = n.random()
n.setstate(st)
print(x == n.random(), len(st))

try:
    random.Random(1, 2)
except TypeError as e:
    print("TypeError")
try:
    random.Random([])
except TypeError as e:
    print("TypeError")

for bad in (lambda: random.randint(5, 1), lambda: random.choice([]), lambda: random.sample([1, 2], 3), lambda: random.randrange(0), lambda: random.getrandbits(-1)):
    try:
        bad()
    except (ValueError, IndexError) as e:
        print(type(e).__name__)

sr = random.SystemRandom()
v = sr.random()
print(0.0 <= v < 1.0, 0 <= sr.randrange(10) < 10, sr.getrandbits(16) < 65536, len(sr.randbytes(5)), sr.choice([1]))
try:
    sr.seed(1)
    print("seed ignored")
    sr.getstate()
except NotImplementedError:
    print("NotImplementedError")

random.seed(2024)
print(random.sample(range(10**6), 4), random.randrange(10**20), random.randint(-5, 5))
random.seed(7)
d = [random.random() for _ in range(3)]
random.seed(7)
print(d == [random.random() for _ in range(3)])

