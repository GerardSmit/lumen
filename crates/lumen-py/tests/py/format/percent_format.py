print("%s" % "a", "%s" % 1, "%s" % None, "%s" % [1, 2], "%s" % (1,), "%s" % ((1, 2),))
print("%s %s" % ("a", "b"), "%s-%s-%s" % (1, 2.5, True), "%d%%" % 50, "%%" % ())
print("%d" % 42, "%d" % -42, "%d" % 3.9, "%d" % True, "%i" % 7, "%u" % 7)
print("%5d|%-5d|%05d|%+d|% d|%+5d|%-+5d|" % (42, 42, 42, 42, 42, 42, 42))
print("%x %X %o %#x %#X %#o %b" % (255, 255, 8, 255, 255, 8, 5) if False else "%x %X %o %#x %#X %#o" % (255, 255, 8, 255, 255, 8))
print("%08.3f|%-8.3f|%+.2f|% .2f|%.0f|%f" % (3.14159, 3.14159, 3.14159, 3.14159, 2.5, 1.5))
print("%e %E %.2e %g %G %.3g %g %g" % (1234.5, 1234.5, 1234.5, 1234.5, 1e-10, 3.14159, 1e20, 100000.0))
print("%s|%10s|%-10s|%.2s|%10.2s|%-10.2s|" % ("abc", "abc", "abc", "abc", "abc", "abc"))
print("%r %r %r %r" % ("a", 1, None, "it's"), "%a" % "é", "%s" % "é")
print("%c%c%c" % (72, "i", 33), "%5c|%-5c|" % ("a", "b"))
print("%(name)s is %(age)d" % {"name": "Bob", "age": 30})
print("%(a)s %(a)s %(b)05.1f" % {"a": "x", "b": 2.55})
print("%(x)r %(y)-4s|%(z)+d" % {"x": "q", "y": "ab", "z": 3})
print("%(a)s%%" % {"a": 10}, "%(a)s" % {"a": [1]})
print("%*d|%-*d|%.*f" % (5, 42, 5, 42, 2, 3.14159))
print("%*s|%.*s|%*.*f|" % (6, "ab", 1, "abc", 8, 2, 3.14159))
print("%*d|" % (-5, 42))
print("%s %s" % ("only", "two"), "%s" % "tuple-free", "%s" % {"k": 1})
print("%d items, %s total, %.1f avg" % (3, "ten", 3.333))
print("%5.1f%%" % 99.55, "%3d%%" % 7)
print("%x" % -255, "%o" % -8, "%d" % 10 ** 20, "%x" % 2 ** 64, "%,d" % 1 if False else "ok")
print("%.3d|%5.3d|%-5.3d|" % (5, 5, 5))
print("%f %f %f" % (float("inf"), float("-inf"), float("nan")), "%5.1f|" % float("inf"), "%e" % float("inf"))
print("%s" % 1e16, "%s" % 1e-5, "%s" % 0.1, "%r" % 0.1, "%s" % 1.0, "%s" % 100.0)
print("%.20s" % "short", "%.0s|" % "x", "%-3s|" % "toolong")
print("%s" % True, "%d" % False, "%r" % True, "%s" % (None,))
print("%s and %r" % ("x", "x"), "%-6r|" % "ab")
print(("%s" * 3) % ("a", "b", "c"), "%s" % ("a" * 3))
print("%#.3g|%#g|%#.0f|%#x" % (1.0, 1.5, 3.0, 0))
print("%5s|%5s|" % (True, None), "%-5s|" % [1])
class T:
    def __str__(self):
        return "T!"
    def __repr__(self):
        return "<T>"
print("%s %r" % (T(), T()), "%10s|" % T(), "%a" % T())
for fmt, arg in (("%d", "x"), ("%s %s", ("a",)), ("%s", ("a", "b")), ("%(k)s", {}), ("%z", 1), ("%c", "ab"), ("%d", None), ("%(k)s", 5), ("%s %(k)s", {"k": 1})):
    try:
        print(fmt % arg)
    except (TypeError, ValueError, KeyError) as e:
        print(type(e).__name__)
try:
    "%d" % "x"
except TypeError as e:
    print(type(e).__name__, e)
try:
    "%s %s" % ("a",)
except TypeError as e:
    print(type(e).__name__, e)
try:
    "%s" % ("a", "b")
except TypeError as e:
    print(type(e).__name__, e)
try:
    "%(k)s" % {}
except KeyError as e:
    print(type(e).__name__, e)
try:
    "%" % ()
except ValueError as e:
    print(type(e).__name__, e)
