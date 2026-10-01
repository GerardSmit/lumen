L = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]
print(L[2:5], L[:3], L[7:], L[:], L[::2], L[1::3], L[2:8:2])
print(L[-3:], L[:-3], L[-5:-2], L[-100:3], L[3:100], L[100:], L[5:2])
print(L[0:0], L[-1:], L[:-1])
s = "hello world"
print(s[0:5], s[6:], s[-5:], s[::2], s[:0] == "", s[3:3] == "")
print(s[100:], s[-100:2], s[2:-2])
t = (1, 2, 3, 4, 5)
print(t[1:4], t[::2], t[-2:])
b = b"abcdef"
print(b[1:4], b[::2], b[-2:])

M = list(range(10))
M[2:5] = ["a", "b"]
print(M)
M[:2] = []
print(M)
M[1:1] = [100, 200]
print(M)
M[::2] = [0] * len(M[::2])
print(M)
N = list(range(6))
N[1:3] = [9, 9, 9, 9]
print(N)
N[-2:] = "xy"
print(N)
N[:] = (7, 8)
print(N)
try:
    O = list(range(6))
    O[::2] = [1, 2]
except ValueError as e:
    print(type(e).__name__)
D = list(range(10))
del D[2:4]
print(D)
del D[::3]
print(D)
del D[:]
print(D)
E = [1, 2, 3]
F = E[:]
F.append(4)
print(E, F, E is F, E == F[:3])
G = [[1, 2], [3, 4]]
H = G[:]
H[0].append(99)
print(G, H)
print(L[True:3], L[:True])
class Idx:
    def __index__(self):
        return 3
print(L[Idx():], L[:Idx()])
try:
    L["a":2]
except TypeError as e:
    print(type(e).__name__)
try:
    L[1.0:2]
except TypeError as e:
    print(type(e).__name__)
print(L[None:None], L[None:3:None])
print("abcdef"[1:-1][1:-1])
print(list(range(20))[5:15][2:8][::2])
print([x for x in L[::3]])
print(len(L[2:7]), len(L[7:2]), len(L[::4]))
