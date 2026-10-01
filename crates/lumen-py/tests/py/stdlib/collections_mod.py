from collections import deque, Counter, defaultdict, OrderedDict, namedtuple

d = deque([1, 2, 3], maxlen=4)
d.append(4); d.append(5)
d.appendleft(0)
print(d, list(d), len(d), d.maxlen)
print(d.pop(), d.popleft(), d)
d.rotate(1)
print(d)
d.extend([9]); d.extendleft([7])
print(d, d[0], d[-1])

c = Counter("abracadabra")
print(sorted(c.items()), c["a"], c["z"], c.most_common(2))
c.update("aaa")
print(c["a"], sum(c.values()), sorted(c.elements())[:3])
print(Counter([1, 1, 2]) + Counter([2, 3]), Counter(a=3) - Counter(a=1))

dd = defaultdict(list)
for k, v in [("a", 1), ("b", 2), ("a", 3)]:
    dd[k].append(v)
print(dict(dd), dd["missing"], sorted(dd))
di = defaultdict(int)
for ch in "hello":
    di[ch] += 1
print(sorted(di.items()))

od = OrderedDict()
od["x"] = 1; od["y"] = 2; od["z"] = 3
od.move_to_end("x")
print(list(od), od.popitem(last=False), list(od.items()))

Point = namedtuple("Point", ["x", "y"])
p = Point(1, y=2)
print(p, p.x, p[1], p._asdict(), p._replace(x=10), Point._fields)
x, y = p
print(x, y, p == (1, 2), isinstance(p, tuple))
P3 = namedtuple("P3", "a b c", defaults=[0])
print(P3(1, 2), P3._make([4, 5, 6]))
