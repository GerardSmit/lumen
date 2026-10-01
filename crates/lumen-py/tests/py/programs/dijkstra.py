class MinHeap:
    def __init__(self, key=lambda x: x):
        self.data = []
        self.key = key

    def __len__(self):
        return len(self.data)

    def push(self, item):
        self.data.append(item)
        self._up(len(self.data) - 1)

    def pop(self):
        data = self.data
        if not data:
            raise IndexError("pop from empty heap")
        top = data[0]
        last = data.pop()
        if data:
            data[0] = last
            self._down(0)
        return top

    def _up(self, i):
        data, key = self.data, self.key
        item = data[i]
        while i > 0:
            parent = (i - 1) >> 1
            if key(item) < key(data[parent]):
                data[i] = data[parent]
                i = parent
            else:
                break
        data[i] = item

    def _down(self, i):
        data, key = self.data, self.key
        n = len(data)
        item = data[i]
        while True:
            child = 2 * i + 1
            if child >= n:
                break
            if child + 1 < n and key(data[child + 1]) < key(data[child]):
                child += 1
            if key(data[child]) < key(item):
                data[i] = data[child]
                i = child
            else:
                break
        data[i] = item


def dijkstra(graph, source):
    dist = {source: 0}
    prev = {}
    heap = MinHeap(key=lambda t: (t[0], t[1]))
    heap.push((0, source))
    done = set()
    pops = 0
    while heap:
        d, u = heap.pop()
        pops += 1
        if u in done:
            continue
        done.add(u)
        for v, w in graph.get(u, ()):
            nd = d + w
            if v not in dist or nd < dist[v]:
                dist[v] = nd
                prev[v] = u
                heap.push((nd, v))
    return dist, prev, pops


def path_to(prev, source, target):
    path = [target]
    while path[-1] != source:
        if path[-1] not in prev:
            return None
        path.append(prev[path[-1]])
    return path[::-1]


def make_graph(edges, directed=False):
    g = {}
    for a, b, w in edges:
        g.setdefault(a, []).append((b, w))
        g.setdefault(b, [])
        if not directed:
            g[b].append((a, w))
    return g


h = MinHeap()
vals = [5, 3, 8, 1, 9, 2, 7, 3, 6, 4, 0, -1]
for v in vals:
    h.push(v)
out = []
while h:
    out.append(h.pop())
print(out, out == sorted(vals))
try:
    h.pop()
except IndexError as ex:
    print("IndexError:", ex)

edges = [
    ("A", "B", 7), ("A", "C", 9), ("A", "F", 14), ("B", "C", 10), ("B", "D", 15),
    ("C", "D", 11), ("C", "F", 2), ("D", "E", 6), ("E", "F", 9), ("G", "H", 1),
]
g = make_graph(edges)
dist, prev, pops = dijkstra(g, "A")
for node in sorted(g):
    p = path_to(prev, "A", node) if node in dist else None
    print(node, dist.get(node, "inf"), "->".join(p) if p else "unreachable")
print("pops", pops)

grid_rows = [
    "S.#.....",
    ".##.###.",
    "....#...",
    ".####.#.",
    "......#E",
]
H, W = len(grid_rows), len(grid_rows[0])
cells = {}
for r, row in enumerate(grid_rows):
    for c, ch in enumerate(row):
        if ch == "S":
            start = (r, c)
        elif ch == "E":
            goal = (r, c)
        if ch != "#":
            cells[(r, c)] = 1
gg = {}
for (r, c) in cells:
    gg[(r, c)] = []
    for dr, dc in ((0, 1), (1, 0), (0, -1), (-1, 0)):
        nb = (r + dr, c + dc)
        if nb in cells:
            gg[(r, c)].append((nb, 1))
dist2, prev2, _ = dijkstra(gg, start)
print("grid distance", dist2[goal])
p = path_to(prev2, start, goal)
print(len(p), p[:4], p[-2:])
marks = {pt for pt in p}
for r in range(H):
    print("".join("*" if (r, c) in marks and grid_rows[r][c] == "." else grid_rows[r][c] for c in range(W)))

big = []
x = 12345
for i in range(300):
    x = (x * 1103515245 + 12345) % 2147483648
    a = x % 60
    x = (x * 1103515245 + 12345) % 2147483648
    b = x % 60
    x = (x * 1103515245 + 12345) % 2147483648
    big.append((a, b, 1 + x % 20))
bg = make_graph(big, directed=True)
bd, bp, bpops = dijkstra(bg, 0)
print(len(bd), sum(bd.values()), max(bd.values()), bpops)
far = max(bd, key=lambda n: (bd[n], n))
print(far, bd[far], path_to(bp, 0, far))
