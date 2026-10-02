import bisect

a = [1, 3, 3, 5, 7]
print(bisect.bisect_left(a, 3), bisect.bisect_right(a, 3), bisect.bisect(a, 4))
print(bisect.bisect_left(a, 0), bisect.bisect_right(a, 9))
print(bisect.bisect_left(a, 3, 2), bisect.bisect_right(a, 3, 0, 1))
bisect.insort(a, 4)
bisect.insort_left(a, 3)
bisect.insort_right(a, 3)
print(a)
words = ["apple", "Banana", "cherry"]
print(bisect.bisect_left(words, "banana", key=str.lower))
bisect.insort(words, "BLUEBERRY", key=str.lower)
print(words)
try:
    bisect.bisect_left(a, 1, -1)
except ValueError as e:
    print("ValueError", e)
