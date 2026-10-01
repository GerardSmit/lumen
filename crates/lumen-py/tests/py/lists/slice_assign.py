l = [0, 1, 2, 3, 4, 5]
l[1:3] = ["a", "b", "c"]
print(l)
l[1:4] = []
print(l)
l[1:1] = [9, 9]
print(l)
l[len(l):] = [7]
print(l)
l[:0] = [-1]
print(l)
l[-2:] = ["x"]
print(l)
l[:] = [1, 2, 3]
print(l)
l[1:2] = "hi"
print(l)
l[0:1] = (5, 6)
print(l)
l[10:20] = ["tail"]
print(l)
l[-100:1] = ["head"]
print(l)
l = list(range(10))
l[::2] = ["e"] * 5
print(l)
l[1::3] = [-1, -2, -3]
print(l)
l[::-1] = list(range(10))
print(l)
l[8:2:-2] = "xyz"
print(l)
del l[::2]
print(l)
l = list(range(10))
del l[1:3]
print(l)
del l[-2:]
print(l)
del l[::3]
print(l)
del l[100:]
print(l)
del l[:2], l[-1]
print(l)
l = list(range(6))
l[1:5:2] = [10, 20]
print(l)
l[::-2] = [0, 0, 0]
print(l)
l = list(range(5))
l[2:2] = l
print(l)
l = [1, 2, 3]
l[1:] = l[:1]
print(l)
l[:] = reversed(l)
print(l)
l[0:0] = range(3)
print(l)
l = [1, 2, 3, 4]
l[1:3] = [l[1] * 10]
print(l)
l[:2] = [[0]]
print(l)
l = list("abcdef")
l[1:-1] = "XY"
print("".join(l), l)
l[::2] = "12"
print(l)
l = [1, 2, 3]
l[0], l[2] = l[2], l[0]
print(l)
l[1:2] = [[]]
print(l)
l[-1:] = []
print(l)
l[5:] = [0]
print(l)
l = list(range(8))
l[2:6] = l[4:2:-1]
print(l)
for f in (lambda: l.__setitem__(slice(None, None, 2), [1]), lambda: l.__setitem__(slice(0, 4, 2), [1, 2, 3]), lambda: l.__setitem__(slice(0, 2), 5), lambda: l.__delitem__(100), lambda: l.__setitem__(100, 1), lambda: l.__setitem__(slice(None, None, 0), [])):
    try:
        f()
    except (ValueError, TypeError, IndexError) as e:
        print(type(e).__name__)
try:
    l = list(range(4))
    l[::2] = [1]
except ValueError as e:
    print(type(e).__name__, e)
try:
    l = list(range(4))
    l[0:2] = 7
except TypeError as e:
    print(type(e).__name__, e)
try:
    l = list(range(4))
    l[10] = 1
except IndexError as e:
    print(type(e).__name__, e)
try:
    l = list(range(4))
    del l[10]
except IndexError as e:
    print(type(e).__name__, e)
