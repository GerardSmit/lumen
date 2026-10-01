print(repr("hi"), repr('it\'s'), repr("say \"x\""), repr("both ' and \""), repr("tab\tnl\n\\"))
print(repr("é"), ascii("é"), ascii("€ 😀"), ascii("plain"), repr("\x00\x1f\x7f"))
print(repr(1), repr(1.5), repr(None), repr(True), repr([1, "a"]), repr((1,)), repr(()), repr({"k": [1]}))
print(repr({1, 2}), repr(set()), repr(frozenset()) if False else "fs", repr({}), repr([]))
print(repr(b"ab\x00\xff"), repr(bytearray(b"hi")), repr(b"it's"), repr(b'"'))
print(str("x"), str(5), str([1, "a"]), str(("a",)), str(None), str(b"x"))
print(repr(range(3)), repr(slice(1, 2)), repr(3 + 4j), repr(1j), repr(complex(1, -1)))
print(repr(Ellipsis), repr(NotImplemented), repr(len)[:20], repr(int))

class P:
    def __repr__(self):
        return "P()"
class Q:
    def __str__(self):
        return "Q-str"
class R:
    def __repr__(self):
        return "R-repr"
    def __str__(self):
        return "R-str"
print(repr(P()), str(P()), [P(), P()], str(Q()), f"{R()} {R()!r} {R()!s}")
print(repr([R(), "x"]), {"a": R()}, (R(),))
print(f"{'x'!r} {'é'!a} {1.5!r:>8}|{'ab':^6}|{'ab':*<5}|{42:+d}|{255:#x}|{255:08b}|{1234567:_}")
print("%r %s %a" % ("a", "a", "é"), "%5d|%-5d|%05d" % (1, 2, 3), "%x %o %c" % (255, 8, 65))
print("{} {!r} {:>4}".format("a", "b", "c"), "{0}{1}{0}".format("x", "y"), "{k}".format(k=1))
a = [1]
a.append(a)
print(a, repr(a))
d = {}
d["self"] = d
print(d)
print(str(1e100), str(10 ** 30), repr(-0.0), str(True))
print(repr("a" * 3), repr("multi\nline"), "multi\nline")
print(chr(10).__repr__(), "é" == "é", "\N{BULLET}", len("😀"))
print(repr('​'), repr('\xa0'), repr("퟿"))
