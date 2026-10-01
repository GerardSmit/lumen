class Life:
    def __init__(self, width, height, wrap=True):
        self.w = width
        self.h = height
        self.wrap = wrap
        self.cells = set()
        self.generation = 0

    def seed(self, pattern, ox=0, oy=0):
        for y, row in enumerate(pattern):
            for x, ch in enumerate(row):
                if ch in "#O*":
                    self.cells.add(((x + ox) % self.w, (y + oy) % self.h))
        return self

    def neighbours(self, x, y):
        count = 0
        for dy in (-1, 0, 1):
            for dx in (-1, 0, 1):
                if dx == 0 and dy == 0:
                    continue
                nx, ny = x + dx, y + dy
                if self.wrap:
                    nx %= self.w
                    ny %= self.h
                elif not (0 <= nx < self.w and 0 <= ny < self.h):
                    continue
                if (nx, ny) in self.cells:
                    count += 1
        return count

    def step(self):
        new = set()
        for y in range(self.h):
            for x in range(self.w):
                n = self.neighbours(x, y)
                alive = (x, y) in self.cells
                if n == 3 or (alive and n == 2):
                    new.add((x, y))
        self.cells = new
        self.generation += 1

    def render(self):
        rows = []
        for y in range(self.h):
            rows.append("".join("#" if (x, y) in self.cells else "." for x in range(self.w)))
        return rows

    def show(self):
        print("gen %d (%d alive)" % (self.generation, len(self.cells)))
        for row in self.render():
            print(row)

    def snapshot(self):
        return frozenset(self.cells)


GLIDER = [".#.", "..#", "###"]
BLINKER = ["###"]
BLOCK = ["##", "##"]
RPENTOMINO = [".##", "##.", ".#."]


def main():
    life = Life(8, 8).seed(GLIDER, 1, 1)
    life.show()
    for _ in range(4):
        life.step()
    life.show()
    for _ in range(4):
        life.step()
    life.show()
    for _ in range(16):
        life.step()
    print("after wrap-around lap:", life.generation, sorted(life.cells))

    blinker = Life(5, 5).seed(BLINKER, 1, 2)
    states = []
    for _ in range(4):
        states.append(blinker.snapshot())
        blinker.step()
    print("blinker period 2:", states[0] == states[2], states[0] == states[1])
    blinker.show()

    block = Life(6, 6).seed(BLOCK, 2, 2)
    before = block.snapshot()
    for _ in range(5):
        block.step()
    print("block stable:", before == block.snapshot())

    bounded = Life(6, 6, wrap=False).seed(GLIDER, 2, 2)
    counts = []
    for _ in range(12):
        bounded.step()
        counts.append(len(bounded.cells))
    print("bounded glider population:", counts)
    bounded.show()

    toroid = Life(10, 10).seed(GLIDER, 7, 7)
    toroid.step()
    print("wrapped cells:", sorted(toroid.cells))

    r = Life(12, 12).seed(RPENTOMINO, 5, 5)
    history = []
    for _ in range(25):
        r.step()
        history.append(len(r.cells))
    print(history)
    r.show()

    seen = {}
    lf = Life(6, 6).seed(GLIDER, 0, 0)
    while lf.snapshot() not in seen:
        seen[lf.snapshot()] = lf.generation
        lf.step()
    print("cycle starts at", seen[lf.snapshot()], "repeats at", lf.generation)


main()
