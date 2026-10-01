def queens(n):
    cols, d1, d2 = set(), set(), set()
    placement = []
    solutions = []

    def place(r):
        if r == n:
            solutions.append(tuple(placement))
            return
        for c in range(n):
            if c in cols or (r - c) in d1 or (r + c) in d2:
                continue
            cols.add(c)
            d1.add(r - c)
            d2.add(r + c)
            placement.append(c)
            place(r + 1)
            placement.pop()
            cols.remove(c)
            d1.remove(r - c)
            d2.remove(r + c)

    place(0)
    return solutions


def show_board(sol):
    n = len(sol)
    return ["".join("Q" if sol[r] == c else "." for c in range(n)) for r in range(n)]


def permutations(items):
    items = list(items)
    if len(items) <= 1:
        yield tuple(items)
        return
    for i in range(len(items)):
        rest = items[:i] + items[i + 1:]
        for p in permutations(rest):
            yield (items[i],) + p


def combinations(items, k):
    items = list(items)

    def rec(start, chosen):
        if len(chosen) == k:
            yield tuple(chosen)
            return
        for i in range(start, len(items)):
            chosen.append(items[i])
            yield from rec(i + 1, chosen)
            chosen.pop()

    yield from rec(0, [])


def subset_sum(nums, target):
    results = []

    def rec(i, remaining, chosen):
        if remaining == 0:
            results.append(tuple(chosen))
            return
        if i == len(nums) or remaining < 0:
            return
        chosen.append(nums[i])
        rec(i + 1, remaining - nums[i], chosen)
        chosen.pop()
        rec(i + 1, remaining, chosen)

    rec(0, target, [])
    return results


def lcs(a, b):
    memo = {}

    def go(i, j):
        if i == len(a) or j == len(b):
            return ""
        key = (i, j)
        if key in memo:
            return memo[key]
        if a[i] == b[j]:
            res = a[i] + go(i + 1, j + 1)
        else:
            x, y = go(i + 1, j), go(i, j + 1)
            res = x if (len(x), y) >= (len(y), x) else y
        memo[key] = res
        return res

    return go(0, 0)


def edit_distance(a, b):
    prev = list(range(len(b) + 1))
    for i in range(1, len(a) + 1):
        cur = [i] + [0] * len(b)
        for j in range(1, len(b) + 1):
            cost = 0 if a[i - 1] == b[j - 1] else 1
            cur[j] = min(prev[j] + 1, cur[j - 1] + 1, prev[j - 1] + cost)
        prev = cur
    return prev[-1]


def knapsack(items, capacity):
    best = [[0] * (capacity + 1) for _ in range(len(items) + 1)]
    for i, (name, w, v) in enumerate(items, 1):
        for c in range(capacity + 1):
            best[i][c] = best[i - 1][c]
            if w <= c and best[i - 1][c - w] + v > best[i][c]:
                best[i][c] = best[i - 1][c - w] + v
    chosen = []
    c = capacity
    for i in range(len(items), 0, -1):
        if best[i][c] != best[i - 1][c]:
            chosen.append(items[i - 1][0])
            c -= items[i - 1][1]
    return best[-1][-1], sorted(chosen)


def main():
    for n in (1, 4, 5, 6, 8):
        sols = queens(n)
        print("n=%d solutions=%d first=%s" % (n, len(sols), sols[0] if sols else None))
    for row in show_board(queens(6)[0]):
        print(row)
    s8 = queens(8)
    print(s8[0], s8[-1])
    print(all(len(set(s)) == 8 for s in s8))

    perms = list(permutations("abc"))
    print(perms)
    print(len(list(permutations(range(6)))), next(permutations([3, 1, 2])))
    print(["".join(p) for p in permutations("abcd")][:7])
    print(list(permutations([])))
    gen = permutations([1, 2, 3])
    print(next(gen), next(gen), list(gen))

    print(list(combinations("abcde", 3)))
    print(len(list(combinations(range(10), 4))), list(combinations([1, 2], 0)), list(combinations([1, 2], 3)))

    print(subset_sum([3, 34, 4, 12, 5, 2], 9))
    print(subset_sum([1, 2, 3, 4, 5, 6, 7, 8, 9, 10], 15)[:5], len(subset_sum(list(range(1, 13)), 20)))
    print(subset_sum([5, 10], 3))

    print(repr(lcs("AGGTAB", "GXTXAYB")), repr(lcs("abcdefgh", "xaybzcdw")), repr(lcs("", "abc")))
    print(len(lcs("ACGTACGTACGTACGTTGCA" * 3, "TGCATGCATGCAACGTACGT" * 3)))
    for a, b in [("kitten", "sitting"), ("flaw", "lawn"), ("", "abc"), ("same", "same"), ("intention", "execution")]:
        print(a, b, edit_distance(a, b))

    items = [("map", 9, 150), ("compass", 13, 35), ("water", 153, 200), ("sandwich", 50, 160),
             ("glucose", 15, 60), ("tin", 68, 45), ("banana", 27, 60), ("apple", 39, 40)]
    print(knapsack(items, 200))
    print(knapsack(items, 10), knapsack(items, 0))


main()
