class Life:
    def __init__(self, width, height, cells=(), wrap=True):
        self.w = width
        self.h = height
        self.wrap = wrap
        self.alive = set(cells)
        self.generation = 0

    @classmethod
    def from_text(cls, text, **kw):
        rows = text.strip("\n").split("\n")
        cells = [(x, y) for y, row in enumerate(rows) for x, ch in enumerate(row) if ch in "#O"]
        return cls(max(len(r) for r in rows), len(rows), cells, **kw)

    def neighbors(self, x, y):
        for dx in (-1, 0, 1):
            for dy in (-1, 0, 1):
                if dx or dy:
                    nx, ny = x + dx, y + dy
                    if self.wrap:
                        yield nx % self.w, ny % self.h
                    elif 0 <= nx < self.w and 0 <= ny < self.h:
                        yield nx, ny

    def step(self):
        counts = {}
        for cell in self.alive:
            for nb in self.neighbors(*cell):
                counts[nb] = counts.get(nb, 0) + 1
        self.alive = {c for c, n in counts.items() if n == 3 or (n == 2 and c in self.alive)}
        self.generation += 1
        return self

    def render(self):
        return "\n".join("".join("#" if (x, y) in self.alive else "." for x in range(self.w)) for y in range(self.h))

    def bbox(self):
        if not self.alive:
            return None
        xs = [c[0] for c in self.alive]
        ys = [c[1] for c in self.alive]
        return (min(xs), min(ys), max(xs), max(ys))

    def signature(self):
        return tuple(sorted(self.alive))


glider = """
.#......
..#.....
###.....
........
........
........
........
........
"""
g = Life.from_text(glider)
print(g.w, g.h, len(g.alive))
for i in range(5):
    print(f"gen {g.generation}: pop={len(g.alive)} bbox={g.bbox()}")
    print(g.render())
    print()
    g.step()
    g.step()

g = Life.from_text(glider)
start = g.signature()
shifted = None
for i in range(1, 100):
    g.step()
    cells = g.signature()
    if len(cells) == len(start):
        dx = (cells[0][0] - start[0][0])
        if sorted(((x - dx) % 8, y) for x, y in cells) != sorted(start) and i < 40:
            continue
    if i % 32 == 0:
        print("gen", g.generation, "equals start:", cells == start)

blinker = Life(5, 5, [(1, 2), (2, 2), (3, 2)], wrap=False)
seen = {}
for i in range(4):
    seen.setdefault(blinker.signature(), i)
    print(blinker.render().replace("\n", "/"))
    blinker.step()
print("period detected at", seen.get(blinker.signature()))

block = Life(6, 6, [(2, 2), (3, 2), (2, 3), (3, 3)], wrap=False)
before = block.signature()
block.step().step().step()
print("block stable:", before == block.signature())

rpent = Life.from_text("""
................
................
................
................
.......##.......
......##........
.......#........
................
................
................
................
................
................
................
................
................
""", wrap=False)
history = []
for _ in range(60):
    history.append(len(rpent.alive))
    rpent.step()
print(history[:20])
print("max pop", max(history), "at gen", history.index(max(history)), "final", len(rpent.alive))
print(rpent.bbox())

empty = Life(3, 3)
print(empty.step().bbox(), empty.generation)
lonely = Life(3, 3, [(1, 1)], wrap=False)
print(lonely.step().signature())
torus = Life(4, 4, [(0, 0), (1, 0), (0, 1), (1, 1)])
print(len(list(torus.neighbors(0, 0))), sorted(torus.neighbors(0, 0))[:3])
print(torus.step().render())
