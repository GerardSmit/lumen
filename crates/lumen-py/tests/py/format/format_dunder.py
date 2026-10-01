class Temp:
    def __init__(self, deg):
        self.deg = deg
    def __format__(self, spec):
        if spec == "":
            return f"{self.deg}C"
        if spec == "f":
            return f"{self.deg * 9 / 5 + 32:.1f}F"
        if spec.endswith("!"):
            return format(self.deg, spec[:-1])
        raise ValueError("bad spec " + spec)
    def __str__(self):
        return "Temp(str)"
    def __repr__(self):
        return "Temp(repr)"
t = Temp(100)
print(format(t), format(t, ""), format(t, "f"), format(t, ">8.2f!"))
print(f"{t}", f"{t:f}", f"{t!s}", f"{t!r}", f"{t:>6.1f!}|", "{}".format(t), "{:f}".format(t), "{!r}".format(t))
print("%s %r" % (t, t), str(t), repr(t))
try:
    format(t, "zzz")
except ValueError as e:
    print(type(e).__name__, e)

class OnlyStr:
    def __str__(self):
        return "only-str"
class OnlyRepr:
    def __repr__(self):
        return "only-repr"
print(str(OnlyStr()), repr(OnlyStr())[:9], str(OnlyRepr()), repr(OnlyRepr()))
print(format(OnlyStr()), format(OnlyRepr()), f"{OnlyStr()}", f"{OnlyRepr():}", f"{OnlyStr()!r}"[:9])
print(format(OnlyStr(), ""), "[{:}]".format(OnlyRepr()))
try:
    format(OnlyStr(), ">10")
except TypeError as e:
    print(type(e).__name__, e)

class Pad:
    def __init__(self, s):
        self.s = s
    def __str__(self):
        return self.s
    def __format__(self, spec):
        return format(str(self), spec)
print(f"[{Pad('ab'):>5}] [{Pad('ab'):<5}] [{Pad('ab'):^6}] [{Pad('abcdef'):.3}]")

print(format(True), format(False, ""), format(True, "d"), format(True, "5"), format(True, ">6"), format(False, "03d"))
print(format(None), format(None, ""), f"{None}", f"{None!r}", f"{None!s:>6}")
print(f"{True!r}", f"{True:}", f"{True:x}", f"{False:b}", "{:s}".format("x"), "{:}".format(None))
print(format(5), format(5, ""), format(5, "s" if False else ""), format(-5, "+"), format(1.0), format(1e100), format(1e-100))
print(format("s"), format([1, "a"]), format((1, "a")), format({"a": 1}), format({1}), format(range(3)), format(b"a"))
print(format(5, "") == str(5), format(1.5, "") == str(1.5), format("x", "") == "x")
for v in (None, [1], (1,), {1: 1}, b"x", object):
    try:
        format(v, ">5")
    except TypeError as e:
        print("TypeError")
print(int.__format__(5, "03d"), str.__format__("a", ">3"), float.__format__(1.5, ".3f"), (5).__format__("x"), "ab".__format__("^4") + "|")
print(bool.__format__(True, ""), type(format(1)).__name__)

class Money:
    def __init__(self, cents):
        self.cents = cents
    def __format__(self, spec):
        d, c = divmod(self.cents, 100)
        base = f"${d}.{c:02d}"
        return format(base, spec)
    def __repr__(self):
        return f"Money({self.cents})"
m = Money(1234)
print(f"{m}", f"{m:>10}", f"{m:*<10}", f"{m!r}", [m], (m,), {"k": m})
print(f"{[m]}", f"{(m, m)}")
