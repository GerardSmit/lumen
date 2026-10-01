a = [1, 2, 3]
b = a
b.append(4)
print(a, b, a is b, a == b)
c = a[:]
d = a.copy()
e = list(a)
c.append(5)
print(a, c, d, e, a is c, a == d, a is d, a is e)
n = [[1, 2], [3, 4]]
s = n[:]
s[0].append(99)
s.append([5])
print(n, s, n[0] is s[0], n is s)
dd = [row[:] for row in n]
dd[0].append(0)
print(n, dd)
trap = [[0] * 3] * 3
trap[0][0] = 1
print(trap, trap[0] is trap[1], trap[1] is trap[2])
ok = [[0] * 3 for _ in range(3)]
ok[0][0] = 1
print(ok, ok[0] is ok[1])
rep = [[]] * 2
rep[0].append("x")
print(rep)
rep2 = [[] for _ in range(2)]
rep2[0].append("x")
print(rep2)
times = [1, 2] * 2
times[0] = 9
print(times)
lst = [1, 2, 3, 4, 5, 6]
for x in lst[:]:
    if x % 2 == 0:
        lst.remove(x)
print(lst)
lst = [1, 2, 3, 4, 5, 6]
lst = [x for x in lst if x % 2]
print(lst)
lst = [1, 2, 3, 4, 5, 6]
for i in range(len(lst) - 1, -1, -1):
    if lst[i] % 3 == 0:
        del lst[i]
print(lst)
lst = list(range(10))
del lst[0]
del lst[-1]
del lst[2:4]
del lst[::2]
print(lst)
del lst[:]
print(lst)
def mutate(v):
    v.append("m")
    v = [0]
    return v
orig = [1]
res = mutate(orig)
print(orig, res)
def default(v, acc=[]):
    acc.append(v)
    return acc
print(default(1), default(2), default(3, []))
x = y = [1]
x += [2]
print(x, y, x is y)
t = (1, 2)
u = t
t += (3,)
print(t, u)
xs = [1, 2, 3]
ys = xs
xs = xs + [4]
print(xs, ys)
zs = [1, 2, 3]
ws = zs
zs += [4]
print(zs, ws)
m = {"k": [1]}
m2 = dict(m)
m2["k"].append(2)
print(m, m2)
import_copy = [[1, 2], [3]]
flat = [i for r in import_copy for i in r]
flat.append(0)
print(import_copy, flat)
lst = [1, 2, 3]
lst2 = lst
lst[:] = [7, 8]
print(lst, lst2, lst is lst2)
lst = [1, 2]
lst.append(lst)
print(len(lst), lst[2] is lst, lst[2][2][0])
lst = [0] * 3
for i in range(3):
    lst[i] += i
print(lst)
grid = [[0] * 2 for _ in range(2)]
for r in range(2):
    for c in range(2):
        grid[r][c] = r * 2 + c
print(grid, [list(r) for r in zip(*grid)])
