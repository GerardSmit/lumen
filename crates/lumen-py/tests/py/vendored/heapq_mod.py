import heapq

h = []
for x in [5, 1, 8, 3, 2, 9, 4]:
    heapq.heappush(h, x)
print(h[0], len(h))
print([heapq.heappop(h) for _ in range(len(h))])

data = [7, 2, 9, 4, 1]
heapq.heapify(data)
print(data[0])
print(heapq.heappushpop(data, 0), heapq.heapreplace(data, 6), sorted(data))
print(heapq.nlargest(3, [4, 8, 1, 9, 3]), heapq.nsmallest(2, [4, 8, 1, 9, 3]))
print(heapq.nlargest(2, ["aa", "b", "cccc"], key=len))
print(list(heapq.merge([1, 4, 7], [2, 5, 8], [0, 3, 6])))
print(list(heapq.merge([3, 2, 1], [6, 5], reverse=True)))
try:
    heapq.heappop([])
except IndexError as e:
    print("IndexError")
pairs = [(2, "b"), (1, "a"), (3, "c")]
heapq.heapify(pairs)
print(heapq.heappop(pairs))
