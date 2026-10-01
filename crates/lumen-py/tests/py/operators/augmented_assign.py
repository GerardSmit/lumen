a = [1, 2]
b = a
a += [3]
print(a, b, a is b)
a = a + [4]
print(a, b, a is b)
a = b
a += (5, 6)
print(a, b)
a += "xy"
print(a)
a *= 2
print(a, a is b)
t = (1, 2)
u = t
t += (3,)
print(t, u, t is u)
t *= 2
print(t)
s = "ab"
s += "c"
s *= 2
print(s)
n = 5
n += 1
n -= 2
n *= 3
n //= 2
n **= 2
n %= 7
n <<= 3
n >>= 1
n |= 1
n &= 6
n ^= 5
print(n)
f = 7
f /= 2
print(f)
f //= 1
print(f)
st = {1}
st2 = st
st |= {2}
st &= {1, 2, 3}
st -= {3}
st ^= {5}
print(sorted(st), st is st2)
fr = frozenset({1})
fr2 = fr
fr |= {2}
print(sorted(fr), sorted(fr2), fr is fr2)
d = {"a": 1}
d["a"] += 10
d["b"] = d.get("b", 0) + 1
print(sorted(d.items()))
d2 = {"l": [1]}
d2["l"] += [2]
print(d2)
try:
    d["zz"] += 1
except KeyError as e:
    print(type(e).__name__, e)


class Obj:
    def __init__(self):
        self.x = 1
        self.items = []


o = Obj()
o.x += 5
o.items += [1]
o.items += [2]
print(o.x, o.items)
try:
    o.nope += 1
except AttributeError as e:
    print(type(e).__name__)


class IAdd:
    def __init__(self, v):
        self.v = v

    def __add__(self, o):
        print("add")
        return IAdd(self.v + o)

    def __iadd__(self, o):
        print("iadd")
        self.v += o
        return self


class OnlyAdd:
    def __init__(self, v):
        self.v = v

    def __add__(self, o):
        print("add")
        return OnlyAdd(self.v + o)


i = IAdd(1)
j = i
i += 1
print(i is j, i.v)
k = OnlyAdd(1)
m = k
k += 1
print(k is m, k.v, m.v)


class IRet:
    def __iadd__(self, o):
        return "replaced"


r = IRet()
r += 1
print(r)

trace = []


def get_list():
    trace.append("get_list")
    return lst


def get_idx():
    trace.append("get_idx")
    return 0


lst = [10, 20]
get_list()[get_idx()] += 5
print(lst, trace)
trace.clear()


class Counter:
    def __init__(self):
        self.reads = 0

    @property
    def val(self):
        self.reads += 1
        return 10

    @val.setter
    def val(self, v):
        trace.append(("set", v))


c = Counter()
c.val += 1
print(c.reads, trace)
x = [0]
x[0] += x[0] + 1
print(x)
g = 1


def bump():
    global g
    g += 1


bump()
bump()
print(g)
q = [[1], [2]]
for row in q:
    row += [0]
print(q)
nums = [1, 2]
for z in nums:
    z += 10
print(nums)
