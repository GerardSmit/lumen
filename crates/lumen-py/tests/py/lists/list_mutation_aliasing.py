a = [1, 2, 3]
b = a
b.append(4)
print(a, b, a is b, a == b)
c = a[:]
c.append(5)
print(a, c, a is c)
def mutate(l):
    l.append("m")
    l = [0]
    return l
print(mutate(a), a)
nested = [[1, 2], [3, 4]]
shallow = nested[:]
shallow[0].append(99)
shallow[1] = "replaced"
print(nested, shallow)
deep = [r[:] for r in nested]
deep[0].append(100)
print(nested, deep)
l = [1, 2, 3]
l.append(l)
print(len(l), l[3] is l, l[3][3][3][0], l)
l2 = [1]
l2.append(l2)
print(l2, str(l2), repr(l2))
d = {"k": 1}
d["self"] = d
print(d)
l = [1, 2, 3, 4, 5, 6]
for x in l:
    if x % 2 == 0:
        l.remove(x)
print(l)
l = [1, 2, 3, 4, 5, 6]
l = [x for x in l if x % 2]
print(l)
l = [1, 2, 3]
for i in range(len(l)):
    l[i] *= 10
print(l)
l = [0]
for x in l:
    if len(l) < 5:
        l.append(x + 1)
print(l)
l = [1, 2, 3]
l[0], l[2] = l[2], l[0]
print(l)
l = [1, 2, 3]
l[1:2] = [7, 8, 9]
print(l)
i = 0
l = [10, 20, 30]
i, l[i] = 1, 99
print(i, l)
l = [1, 2, 3]
r = reversed(l)
l.append(4)
print(list(r))
it = iter(l)
l.pop()
print(list(it))
x = [1, 2]
y = x + [3]
z = x
x += [3]
print(x, y, z, x is z)
t = ([1], 2)
t[0].append(5)
print(t)
try:
    t[0] += [6]
except TypeError:
    print("TypeError")
print(t)
s = [3, 1, 2]
r = sorted(s)
s.sort()
print(r == s, r is s)
def default(a, acc=[]):
    acc.append(a)
    return acc
print(default(1), default(2), default(3, []))
print([1, 2, 3] is [1, 2, 3], [] == [], [[]] == [[]], id([]) != id([]))
big = list(range(1000))
del big[10:990]
print(big, len(big))
del big[::2]
print(big)
del big[0]
del big[-1]
print(big)
