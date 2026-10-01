L = list(range(10))
print(L[::-1])
print(L[::-2])
print(L[::-3])
print(L[8:2:-1])
print(L[8:2:-2])
print(L[2:8:-1])
print(L[-1:-6:-1])
print(L[-2::-2])
print(L[:3:-1])
print(L[:3:-2])
print(L[5::-1])
print(L[5:0:-1])
print(L[5:-100:-1])
print(L[100:5:-1])
print(L[-100::-1])
print(L[0:0:-1])
print(L[3:3:-1])
s = "abcdefgh"
print(s[::-1], s[::-2], s[6:1:-2], s[-1:-4:-1], s[:2:-1])
t = tuple(range(6))
print(t[::-1], t[4:1:-1], t[::-4])
print(b"abcdef"[::-1], b"abcdef"[4::-2])
print(list(range(10))[::-1][::-1] == L)
print("".join(reversed("abc")) == "abc"[::-1])
M = list(range(8))
M[::-1] = list("abcdefgh")
print(M)
M[::-2] = [0, 0, 0, 0]
print(M)
M[5:1:-1] = [1, 2, 3, 4]
print(M)
D = list(range(10))
del D[::-3]
print(D)
D2 = list(range(10))
del D2[7:2:-2]
print(D2)
try:
    L[::0]
except ValueError as e:
    print(type(e).__name__, e)
try:
    M2 = [1, 2, 3]
    M2[::-1] = [1]
except ValueError as e:
    print(type(e).__name__)
print([L[i] for i in range(9, -1, -1)] == L[::-1])
print(len(L[::-1]), len(L[::-4]), len(L[3:8:-1]))
print("palindrome"[::-1])
print(L[-1::-1] == L[::-1])
