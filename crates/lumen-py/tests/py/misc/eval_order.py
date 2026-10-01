trace = []


def t(name, val=None):
    trace.append(name)
    return val if val is not None else name


def flush(label):
    print(label, trace)
    trace.clear()


def f(*args, **kwargs):
    return args, kwargs


f(t("a"), t("b"), k=t("c"), *[t("d")], **{"z": t("e")})
flush("call")
t("x", 1) + t("y", 2) * t("z", 3)
flush("arith")
t("l") if t("c") else t("r")
flush("ternary")
t("a", 0) and t("b")
t("c", 1) or t("d")
flush("shortcircuit")
{t("k1"): t("v1"), t("k2"): t("v2")}
flush("dict")
[t("e1"), t("e2")]
(t("t1"), t("t2"))
{t("s1"), t("s2")}
flush("collections")
t("o")[0:t("hi", 1)] if False else None
x = [10, 20, 30]
x[t("i", 0)] = t("rhs", 5)
flush("subscript assign")
print(x)
t("a", 1) < t("b", 2) < t("c", 3)
t("a", 3) < t("b", 2) < t("c", 5)
flush("chained")
print({"a": 1, "a": 2}, {1: "x", 1.0: "y"})
print([t(1, 1) for _ in range(2)], trace)
trace.clear()


def default_once(x, acc=[], n=t("default_eval", 5)):
    acc.append(x)
    return acc, n


flush("def time")
print(default_once(1))
print(default_once(2))
flush("call time")


def default_expr(val=len(trace)):
    return val


trace.append("grow")
print(default_expr(), default_expr(7))
trace.clear()
fns = [lambda: i for i in range(3)]
print([fn() for fn in fns])
fns2 = [lambda i=i: i for i in range(3)]
print([fn() for fn in fns2])
adders = []
for n in range(3):
    adders.append(lambda v: v + n)
print([a(10) for a in adders])
n = 100
print([a(10) for a in adders])


def make():
    fs = []
    for k in range(3):
        fs.append(lambda: k)
    k = "late"
    return fs


print([g() for g in make()])


def counter():
    c = 0

    def inc():
        nonlocal c
        c += 1
        return c

    return inc


i1, i2 = counter(), counter()
print(i1(), i1(), i2(), i1())
a = [1, 2, 3]
a[1], a[a[1]] = 0, 99
print(a)
v = 1
v, w = v + 1, v + 10
print(v, w)
print(t("L") + t("R", "!"), trace)
trace.clear()
print((t("p", 2) ** t("q", 3)) ** t("r", 2), trace)
trace.clear()
print(t("a", "x") in t("b", "xyz"), trace)
trace.clear()
d = {}
d[t("key")] = t("value")
flush("dict assign")
class C:
    attr = t("class body")
flush("class")
print(f"{t('f1', 1)}-{t('f2', 2)}", trace)
trace.clear()
print(not t("n", 0), trace)
trace.clear()
print(sorted([t(3, 3), t(1, 1), t(2, 2)], key=lambda v: t("key%d" % v, v)), trace)
trace.clear()
print(any([t("p1", 0), t("p2", 1), t("p3", 1)]), trace)
trace.clear()
print(any(t("g%d" % i, i) for i in range(3)), trace)
