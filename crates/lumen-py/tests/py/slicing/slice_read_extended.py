l = list(range(10))
print(l[2:5], l[:3], l[7:], l[:], l[-3:], l[:-3], l[-5:-2], l[5:2], l[2:2], l[100:], l[:100], l[-100:3])
print(l[::2], l[1::2], l[::3], l[::-1], l[::-2], l[8:2:-2], l[2:8:-1], l[-1:-5:-1], l[:2:-1], l[5::-1], l[:-8:-1], l[-100::-3], l[100::-4])
print(l[None:None:None], l[0:10:1], l[9:-11:-1], l[-1::-1], l[3:-3:2], l[::10], l[::11], l[::-10], l[::-11])
s = "abcdefghij"
print(s[2:5], s[::-1], s[::2], s[-3:], s[:-3], s[8:2:-2], s[100:], s[5:2], s[::-3], s[-1:-100:-1], s[1:-1:3])
t = tuple(range(6))
print(t[1:4], t[::-1], t[::2], t[-2:], t[4:1:-1], t[10:], t[:0])
b = b"abcdef"
print(b[1:3], b[::-1], b[::2], b[-2:], bytearray(b)[1:4])
r = range(10)
print(r[2:5], r[::2], r[::-1], r[-3:], r[8:2:-2], list(r[1:8:3]), r[5:2], len(r[2:9:3]), r[::-1][0], range(0, 100, 7)[3:6], range(5)[::-1])
print(slice(1, 5), slice(5), slice(1, 5, 2), slice(None), slice(None, None, -1), slice(1, 2).start, slice(1, 2).stop, slice(1, 2).step, slice(5).start, slice(5).stop)
sl = slice(1, 8, 3)
print(l[sl], s[sl], t[sl], "abcdefghij".__getitem__(sl), l.__getitem__(slice(None, None, 4)))
print(slice(1, 10, 2).indices(5), slice(None).indices(5), slice(-3, None).indices(10), slice(None, None, -1).indices(4), slice(-100, 100).indices(3), slice(5, 1, -1).indices(10), slice(None, None, -2).indices(7))
print(slice(1, 2) == slice(1, 2), slice(1, 2) != slice(1, 3), slice(1, 2, None) == slice(1, 2), slice(1, 2) < slice(1, 3), repr(slice(1, 2)), str(slice(None, 3)))
for k in (-11, -10, -1, 0, 9):
    print(k, l[k] if -10 <= k < 10 else "oob")
for bad in (lambda: l[10], lambda: l[-11], lambda: l[::0], lambda: l["a"], lambda: l[1.5], lambda: l[1:2:0], lambda: s[::0], lambda: s[1.0:2], lambda: slice()):
    try:
        bad()
    except (IndexError, ValueError, TypeError) as e:
        print(type(e).__name__)
class Idx:
    def __index__(self):
        return 2
print(l[Idx():5], s[Idx()], l[True:3], l[:False], l[None:3], l[0:None])
class G:
    def __getitem__(self, k):
        return k
g = G()
print(g[1:2], g[1:2:3], g[:], g[::], g[1:], g[:2], g[::2], g[1, 2], g[1:2, 3], g[...], g[1:2, ::3], g[()], g[-1:], g[:-1:-1])
print(g[1:2].start, g[1:2:3].step, g[:].start, g[:].stop, g[::2].step, type(g[1:2]).__name__, g[2 ** 70:], g["a":"b"])
words = "the quick brown fox".split()
print(words[1:3], words[::-1], [w[::-1] for w in words], words[-2:], " ".join(words[1:]), [w[:2] for w in words], words[1::2], "".join(w[0] for w in words[::-1]))
pal = "A man a plan"
clean = "".join(c for c in pal.lower() if c.isalpha())
print(clean, clean == clean[::-1], clean[:len(clean) // 2], clean[len(clean) // 2:], clean[::2], clean[1::2])
print(l[1:][1:][1:], l[::2][::2], l[::-1][::-1] == l, l[2:8][1:3], (l[3:] + l[:3]), s[3:] + s[:3], [l[i:i + 3] for i in range(0, 10, 3)], [s[i:i + 4] for i in range(0, len(s), 4)])
