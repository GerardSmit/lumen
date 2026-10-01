class L:
    def __init__(self, *v):
        self.v = list(v)
    def __repr__(self):
        return "L%r" % (self.v,)
    def __iadd__(self, o):
        print("iadd")
        self.v.extend(o)
        return self
    def __add__(self, o):
        print("add")
        return L(*(self.v + list(o)))
    def __imul__(self, o):
        self.v *= o
        return self
a = L(1)
b = a
a += [2, 3]
print(a, b, a is b)
c = a + [9]
print(c, a is c)
a *= 2
print(a, b)

class NoI:
    def __init__(self, x): self.x = x
    def __add__(self, o): return NoI(self.x + o)
    def __repr__(self): return "NoI(%d)" % self.x
n = NoI(1)
m = n
n += 5
print(n, m, n is m)

x = [1, 2]
y = x
x += [3]
print(x, y)
x = x + [4]
print(x, y)
t = (1, 2)
u = t
t += (3,)
print(t, u)
s = "ab"
s *= 3
print(s)
i = 10
i //= 3; print(i)
i **= 3; print(i)
i %= 5; print(i)
i <<= 4; print(i)
i >>= 2; print(i)
i |= 1; print(i)
i &= 6; print(i)
i ^= 15; print(i)
i -= 20; print(i)
f = 7.0
f /= 2; print(f)
d = {"a": 1}
d |= {"b": 2}
print(d)
ls = [1, 2, 3]
ls[1] += 10
print(ls)
dd = {"k": [1]}
dd["k"] += [2]
print(dd)
class O:
    n = 1
o = O()
o.n += 1
print(o.n, O.n)
lst = [[0]] * 2
lst[0] += [1]
print(lst)
z = [1]
z *= 0
print(z)
sset = {1, 2}
sset |= {3}
sset -= {1}
sset &= {2, 3, 4}
sset ^= {4}
print(sorted(sset))
