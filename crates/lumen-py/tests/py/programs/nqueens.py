def solve_all(n):
    solutions = []
    cols = []

    def place(row, cmask, d1, d2):
        if row == n:
            solutions.append(tuple(cols))
            return
        avail = ((1 << n) - 1) & ~(cmask | d1 | d2)
        while avail:
            bit = avail & -avail
            avail ^= bit
            col = bit.bit_length() - 1
            cols.append(col)
            place(row + 1, cmask | bit, ((d1 | bit) << 1) & ((1 << n) - 1), (d2 | bit) >> 1)
            cols.pop()

    place(0, 0, 0, 0)
    return solutions


def solve_gen(n, prefix=()):
    row = len(prefix)
    if row == n:
        yield prefix
        return
    for c in range(n):
        if all(c != pc and abs(c - pc) != row - pr for pr, pc in enumerate(prefix)):
            yield from solve_gen(n, prefix + (c,))


def board(sol):
    n = len(sol)
    return "\n".join("".join("Q" if c == col else "." for c in range(n)) for col in sol)


def is_valid(sol):
    n = len(sol)
    return (
        len(set(sol)) == n
        and len({r + c for r, c in enumerate(sol)}) == n
        and len({r - c for r, c in enumerate(sol)}) == n
    )


def symmetric_classes(sols):
    def rot(s):
        n = len(s)
        out = [0] * n
        for r, c in enumerate(s):
            out[c] = n - 1 - r
        return tuple(out)

    def refl(s):
        return tuple(len(s) - 1 - c for c in s)

    seen = set()
    classes = []
    for s in sols:
        if s in seen:
            continue
        orbit = set()
        cur = s
        for _ in range(4):
            orbit.add(cur)
            orbit.add(refl(cur))
            cur = rot(cur)
        seen |= orbit
        classes.append(len(orbit))
    return classes


expected = {1: 1, 2: 0, 3: 0, 4: 2, 5: 10, 6: 4, 7: 40, 8: 92}
for n in range(1, 9):
    sols = solve_all(n)
    print(n, len(sols), len(sols) == expected[n], all(is_valid(s) for s in sols))

s8 = solve_all(8)
print(s8[0], s8[-1])
print(board(s8[0]))
print()
print(board(solve_all(5)[0]))
print(sorted(s8) == s8, s8 == sorted(solve_gen(8)))
print(next(solve_gen(10)), next(solve_gen(12)))
g = solve_gen(6)
print(next(g), next(g), list(g))
classes = symmetric_classes(s8)
print(len(classes), sorted(classes), sum(classes))
print(sorted(set(len(symmetric_classes(solve_all(n))) for n in range(4, 8))))
print(len(solve_all(9)), len(solve_all(10)))
first_col = {}
for s in s8:
    first_col[s[0]] = first_col.get(s[0], 0) + 1
print(sorted(first_col.items()))
print(min(s8, key=lambda s: (sum(abs(a - b) for a, b in zip(s, s[1:])), s)))
