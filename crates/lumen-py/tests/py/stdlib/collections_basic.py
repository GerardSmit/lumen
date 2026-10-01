from collections import Counter, defaultdict, deque, OrderedDict, namedtuple

print("--- Counter")
c = Counter("abracadabra")
print(sorted(c.items()))
print(c.most_common(3), c["a"], c["z"], len(c))
c.update("aaa")
c.subtract("bb")
print(c["a"], c["b"], sorted(c.elements())[:5])
d = Counter({"x": 3, "y": 1}) + Counter({"x": 1, "z": 2})
print(sorted(d.items()), sorted((Counter("aab") - Counter("ab")).items()))
print(sorted((Counter("aabbc") & Counter("abcc")).items()), sorted((Counter("aab") | Counter("abb")).items()))
print(Counter(["a", "b", "a"]) == Counter("aba"), sum(Counter("hello").values()), Counter().most_common())
words = "the quick brown fox jumps over the lazy dog the end".split()
wc = Counter(words)
print(sorted(wc.most_common(2)), sorted(wc.keys())[:4], wc.total())

print("--- defaultdict")
dd = defaultdict(list)
for w in words:
    dd[len(w)].append(w)
print(sorted(dd.items()))
print(dd[99], 99 in dd, len(dd))
counts = defaultdict(int)
for ch in "mississippi":
    counts[ch] += 1
print(sorted(counts.items()), counts.default_factory is int)
nested = defaultdict(lambda: defaultdict(int))
nested["a"]["x"] += 2
nested["a"]["y"] += 1
nested["b"]["x"] += 5
print({k: dict(sorted(v.items())) for k, v in sorted(nested.items())})
plain = defaultdict(None)
try:
    plain["nope"]
except KeyError:
    print("KeyError with no factory")
print(dd.get(100), 100 in dd, dd.pop(99), sorted(dd))

print("--- deque")
dq = deque([1, 2, 3])
dq.append(4)
dq.appendleft(0)
print(dq, list(dq), len(dq), dq[0], dq[-1])
print(dq.pop(), dq.popleft(), dq)
dq.extend([7, 8])
dq.extendleft([-1, -2])
print(list(dq))
dq.rotate(2)
print(list(dq))
dq.rotate(-3)
print(list(dq), dq.count(7), dq.index(8))
dq.reverse()
print(list(dq))
bounded = deque(maxlen=3)
for i in range(6):
    bounded.append(i)
print(bounded, bounded.maxlen)
bounded.appendleft(99)
print(list(bounded))
dq.clear()
print(len(dq), bool(dq))
try:
    dq.pop()
except IndexError:
    print("IndexError on empty pop")
print(deque("abc") == deque(["a", "b", "c"]), list(reversed(deque([1, 2, 3]))), 2 in deque([1, 2]))
q = deque([(0, 0)])
seen = {(0, 0)}
order = []
while q:
    x, y = q.popleft()
    order.append((x, y))
    for nx, ny in ((x + 1, y), (x, y + 1)):
        if nx < 3 and ny < 3 and (nx, ny) not in seen:
            seen.add((nx, ny))
            q.append((nx, ny))
print(order)

print("--- OrderedDict")
od = OrderedDict()
od["one"] = 1
od["two"] = 2
od["three"] = 3
print(list(od), list(od.items()))
od.move_to_end("one")
print(list(od))
od.move_to_end("three", last=False)
print(list(od))
print(od.popitem(), od.popitem(last=False), list(od))
print(OrderedDict(a=1, b=2) == OrderedDict(b=2, a=1), OrderedDict([("a", 1), ("b", 2)]) == OrderedDict([("b", 2), ("a", 1)]))
print(OrderedDict(a=1, b=2) == {"b": 2, "a": 1})
print(repr(OrderedDict([("k", 1)])))

print("--- namedtuple")
Point = namedtuple("Point", ["x", "y"])
p = Point(3, 4)
print(p, p.x, p[1], p._fields, p._asdict() == {"x": 3, "y": 4})
print(p._replace(x=10), Point._make([5, 6]), tuple(p), len(p))
x, y = p
print(x + y, p == (3, 4), p < Point(3, 5), hash(p) == hash((3, 4)))
Emp = namedtuple("Emp", "name dept salary", defaults=[0])
e = Emp("ann", "eng")
print(e, e.salary, Emp._field_defaults, sorted([Emp("b", "x", 2), Emp("a", "y", 3)]))
try:
    p.x = 1
except AttributeError:
    print("AttributeError immutable")
try:
    Point(1)
except TypeError:
    print("TypeError missing arg")
print(Point(y=1, x=2), isinstance(p, tuple), type(p).__name__)
