class Money:
    def __init__(self, cents):
        self.cents = cents
    def __format__(self, spec):
        if spec == "":
            return "$%d.%02d" % divmod(self.cents, 100)
        if spec == "c":
            return str(self.cents) + "c"
        if spec.startswith(">"):
            return format(self, "").rjust(int(spec[1:]))
        raise ValueError("bad spec %r" % spec)
    def __str__(self):
        return "Money(%d)" % self.cents
    def __repr__(self):
        return "<Money %d>" % self.cents
m = Money(12345)
print(format(m), format(m, ""), format(m, "c"), format(m, ">10"), "{}".format(m), "{:c}".format(m), "{!s}".format(m), "{!r}".format(m))
print(f"{m}|{m:c}|{m:>9}|{m!s}|{m!r}|{m!r:>12}|{m!s:>12}")
print("%s %r" % (m, m), str(m), repr(m), "%s" % m)
try:
    format(m, "zzz")
except ValueError as e:
    print("ValueError", e)
class Plain:
    pass
try:
    format(Plain(), "d")
except TypeError:
    print("TypeError plain")
print(format(Plain(), "") == str(Plain()))
class Str:
    def __str__(self):
        return "S!"
print(format(Str()), f"{Str()}", f"{Str():}", "{:}".format(Str()))
try:
    f"{Str():>5}"
except TypeError:
    print("TypeError width on object")
class Tmpl:
    def __init__(self, v):
        self.v = v
    def __format__(self, spec):
        return spec.replace("V", str(self.v)) if spec else "T"
print(f"{Tmpl(3):VV-V}", f"{Tmpl(3)}", "{:V+V}".format(Tmpl(9)), format(Tmpl(1), "<V>"))
print(format(Tmpl(2), "{}"), "{0:{1}}".format(Tmpl(5), "V!"))
class IntSub(int):
    pass
print(format(IntSub(5), "03d"), f"{IntSub(255):#x}", str(IntSub(5)), repr(IntSub(5)), format(IntSub(5)))
class StrSub(str):
    def __format__(self, spec):
        return "<" + super().__format__(spec) + ">"
print(f"{StrSub('ab'):>4}", format(StrSub("x")), "{}".format(StrSub("y")), "%s" % StrSub("z"))
class Both:
    def __repr__(self):
        return "REPR"
    def __str__(self):
        return "STR"
b = Both()
print(b, [b], (b,), {"k": b}, str([b]), "%s|%r|%a" % (b, b, b), f"{b}|{b!r}|{b!s}|{b!a}", "{}|{!r}".format(b, b))
print(format(1, "") , format("é", "!<4"), format(3.0, ""), format(-1, "x"), format(1 + 1j, ""), format(range(3), ""))
print(type(format(1, "d")).__name__, "{:{}}".format(1, 3) + "|", "{0:{0}}".format(4) + "|")
print(m.__format__(""), (5).__format__("03d"), "x".__format__(">3"), (1.5).__format__(".3f"), True.__format__(""), None.__format__(""))
