text = "the quick brown fox jumps over the lazy dog the end"
freq = {}
for w in text.split():
    freq[w] = freq.get(w, 0) + 1
print(freq)
print(sorted(freq.items(), key=lambda kv: (-kv[1], kv[0]))[:3], max(freq, key=freq.get), sum(freq.values()), len(freq))
anagrams = {}
for w in ["listen", "silent", "enlist", "google", "gogole", "cat", "act"]:
    anagrams.setdefault("".join(sorted(w)), []).append(w)
print(anagrams, sorted(anagrams.values(), key=len))
memo = {}
def fib(n):
    if n < 2:
        return n
    if n not in memo:
        memo[n] = fib(n - 1) + fib(n - 2)
    return memo[n]
print(fib(80), len(memo), list(memo)[:5])
graph = {"a": ["b", "c"], "b": ["d"], "c": ["d", "e"], "d": ["f"], "e": ["f"], "f": []}
def bfs(start):
    seen, order, q = {start}, [], [start]
    while q:
        n = q.pop(0)
        order.append(n)
        for m in graph[n]:
            if m not in seen:
                seen.add(m)
                q.append(m)
    return order
def dfs(n, seen=None):
    seen = {} if seen is None else seen
    seen[n] = True
    for m in graph[n]:
        if m not in seen:
            dfs(m, seen)
    return list(seen)
print(bfs("a"), dfs("a"))
indeg = {n: 0 for n in graph}
for n in graph:
    for m in graph[n]:
        indeg[m] += 1
print(indeg)
topo = []
ready = sorted(n for n in indeg if indeg[n] == 0)
while ready:
    n = ready.pop(0)
    topo.append(n)
    for m in graph[n]:
        indeg[m] -= 1
        if indeg[m] == 0:
            ready.append(m)
print(topo)
two = [2, 7, 11, 15, 3, 6]
seen = {}
for i, v in enumerate(two):
    if 9 - v in seen:
        print("pair", seen[9 - v], i)
    seen[v] = i
lru = {}
def touch(k):
    if k in lru:
        del lru[k]
    lru[k] = True
    if len(lru) > 3:
        del lru[next(iter(lru))]
for k in "abcadbea":
    touch(k)
print(list(lru))
matrix = {(r, c): r * c for r in range(3) for c in range(3)}
print(matrix[(2, 2)], sum(matrix.values()), [matrix[(1, c)] for c in range(3)], len(matrix))
roman = {"M": 1000, "D": 500, "C": 100, "L": 50, "X": 10, "V": 5, "I": 1}
def r2i(s):
    t = 0
    for i, ch in enumerate(s):
        v = roman[ch]
        t += -v if i + 1 < len(s) and roman[s[i + 1]] > v else v
    return t
print([r2i(s) for s in ("III", "IV", "IX", "LVIII", "MCMXCIV")])
merged = {}
for part in ({"a": 1}, {"b": 2}, {"a": 3, "c": 4}):
    merged.update(part)
print(merged)
d = {"x": {"y": 1}}
print(d.get("x", {}).get("y"), d.get("q", {}).get("y"), d["x"].setdefault("z", {}).setdefault("w", 5), d)
print({k: sorted(v) for k, v in {"a": {3, 1}, "b": {2}}.items()})
