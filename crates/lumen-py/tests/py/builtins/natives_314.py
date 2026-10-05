import _codecs, heapq, operator, math, functools, array, codecs, contextvars, collections

def t(f):
    try:
        print(f())
    except BaseException as e:
        print(type(e).__name__, e)

h = []
for x in (3, 1, 4, 1, 5):
    heapq.heappush_max(h, x)
print(h, heapq.heappop_max(h), heapq.heappushpop_max(h, 9), heapq.heapreplace_max(h, 0))
l = [1, 5, 2]
heapq.heapify_max(l)
print(l)
print(operator.is_none(None), operator.is_not_none(None), operator.is_none(0))
print(math.fma(2.0, 3.0, 1.0))
t(lambda: math.fma(math.inf, 0.0, 1.0))
t(lambda: math.fma(1e308, 10.0, 0.0))

P = functools.Placeholder
print(repr(P), type(P).__name__, type(P)() is P)
t(lambda: type(P)(1))
p = functools.partial(pow, P, 2)
print(p(5), p.args, repr(p))
t(lambda: p())
t(lambda: functools.partial(pow, 1, P))
t(lambda: functools.partial(pow, a=P))
q = functools.partial(p, 7)
print(q(), q.args)
print(p.__reduce__()[2][1:3])

a = array.array("w", "héllo")
print(a, a.tounicode(), a.itemsize)
a.clear()
print(a, len(a))
t(lambda: array.array("w", [1]))
t(lambda: array.array("z"))
print(array.array[int])

t(lambda: _codecs._unregister_error("strict"))
t(lambda: _codecs._unregister_error("nope"))

v = contextvars.ContextVar("v")
with v.set(3) as tok:
    print(v.get(), tok.var is v)
print(v.get(None))

d = collections.defaultdict(list)
print(d.default_factory)
d.default_factory = int
print(d["x"])
