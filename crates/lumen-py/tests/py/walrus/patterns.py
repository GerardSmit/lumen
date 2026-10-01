def parse(tokens):
    out = []
    while tokens:
        if (t := tokens.pop(0)).isdigit():
            out.append(int(t))
        elif (u := t.upper()) in ("PLUS", "MINUS"):
            out.append(u)
        else:
            out.append(("?", t))
    return out
print(parse(["1", "plus", "x", "22"]))

def first_long(words, n):
    for w in words:
        if (l := len(w)) >= n:
            return w, l
    return None
print(first_long(["a", "bcd", "efghi"], 3))

cache = {}
def memo(n):
    if (v := cache.get(n)) is not None:
        return v
    cache[n] = r = n * n
    return r
print(memo(4), memo(4), cache)

lines = ["x=1", "bad", "y=22"]
res = {}
for ln in lines:
    if (idx := ln.find("=")) != -1:
        res[ln[:idx]] = int(ln[idx + 1:])
print(res)

vals = [4, 9, 16]
print([r for v in vals if (r := v ** 0.5) == int(r)])
print({(h := v // 2): h * 2 for v in vals})
n = 0
while (n := n + 3) < 10:
    print(n, end=" ")
print()
print(n)
import math
if (d := math.sqrt(49)) == 7:
    print("sqrt", d)
print((x := [1, 2]), x.append(3), x)
text = "a,b,c"
if (parts := text.split(",")) and (cnt := len(parts)) > 2:
    print(parts, cnt)
