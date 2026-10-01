PUZZLE = [
    "530070000",
    "600195000",
    "098000060",
    "800060003",
    "400803001",
    "700020006",
    "060000280",
    "000419005",
    "000080079",
]


class Sudoku:
    def __init__(self, rows):
        self.grid = [[int(c) for c in row] for row in rows]
        self.rows = [0] * 9
        self.cols = [0] * 9
        self.boxes = [0] * 9
        self.nodes = 0
        for r in range(9):
            for c in range(9):
                v = self.grid[r][c]
                if v:
                    bit = 1 << v
                    b = (r // 3) * 3 + c // 3
                    if (self.rows[r] | self.cols[c] | self.boxes[b]) & bit:
                        raise ValueError(f"conflict at {r},{c}")
                    self.rows[r] |= bit
                    self.cols[c] |= bit
                    self.boxes[b] |= bit

    def candidates(self, r, c):
        used = self.rows[r] | self.cols[c] | self.boxes[(r // 3) * 3 + c // 3]
        return [v for v in range(1, 10) if not used & (1 << v)]

    def place(self, r, c, v):
        bit = 1 << v
        self.grid[r][c] = v
        self.rows[r] |= bit
        self.cols[c] |= bit
        self.boxes[(r // 3) * 3 + c // 3] |= bit

    def unplace(self, r, c):
        v = self.grid[r][c]
        bit = ~(1 << v)
        self.grid[r][c] = 0
        self.rows[r] &= bit
        self.cols[c] &= bit
        self.boxes[(r // 3) * 3 + c // 3] &= bit

    def pick(self):
        best = None
        best_cands = None
        for r in range(9):
            for c in range(9):
                if self.grid[r][c] == 0:
                    cands = self.candidates(r, c)
                    if best is None or len(cands) < len(best_cands):
                        best, best_cands = (r, c), cands
                        if len(cands) <= 1:
                            return best, best_cands
        return best, best_cands

    def solve(self):
        self.nodes += 1
        cell, cands = self.pick()
        if cell is None:
            return True
        r, c = cell
        for v in cands:
            self.place(r, c, v)
            if self.solve():
                return True
            self.unplace(r, c)
        return False

    def count_solutions(self, limit=2):
        cell, cands = self.pick()
        if cell is None:
            return 1
        r, c = cell
        total = 0
        for v in cands:
            self.place(r, c, v)
            total += self.count_solutions(limit - total)
            self.unplace(r, c)
            if total >= limit:
                break
        return total

    def valid(self):
        full = sum(1 << v for v in range(1, 10))
        for i in range(9):
            if self.rows[i] != full or self.cols[i] != full or self.boxes[i] != full:
                return False
        return True

    def render(self):
        lines = []
        for r, row in enumerate(self.grid):
            if r % 3 == 0 and r:
                lines.append("------+-------+------")
            parts = []
            for c in range(0, 9, 3):
                parts.append(" ".join(str(x) if x else "." for x in row[c:c + 3]))
            lines.append(" | ".join(parts))
        return "\n".join(lines)


s = Sudoku(PUZZLE)
print(s.render())
givens = sum(1 for row in s.grid for v in row if v)
print("givens:", givens)
print("candidates r0c2:", s.candidates(0, 2))
print("unique:", s.count_solutions())
ok = s.solve()
print("solved:", ok, "valid:", s.valid(), "nodes:", s.nodes)
print(s.render())
print("row sums:", [sum(r) for r in s.grid])
print("diag:", [s.grid[i][i] for i in range(9)])
print("first row:", "".join(map(str, s.grid[0])))

for rows in (["11" + "0" * 7] + ["0" * 9] * 8,):
    try:
        Sudoku(rows)
    except ValueError as ex:
        print("ValueError:", ex)

hard = [
    "000000907",
    "000420180",
    "000705026",
    "100904000",
    "050000040",
    "000507009",
    "920108000",
    "034059000",
    "507000000",
]
h = Sudoku(hard)
print(h.solve(), h.valid(), h.nodes)
print("\n".join("".join(map(str, r)) for r in h.grid))

empty = Sudoku(["0" * 9] * 9)
print(empty.solve(), empty.valid())
print("".join(map(str, empty.grid[0])), "".join(map(str, empty.grid[8])))
