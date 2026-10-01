def show(spec, *vals):
    out = []
    for v in vals:
        try:
            out.append(format(v, spec))
        except (ValueError, TypeError) as e:
            out.append(type(e).__name__)
    print(repr(spec), out)
show("", "abc", "")
show("s", "abc")
show("10", "abc", "abcdefghijklmnop")
show(">10", "abc")
show("<10", "abc")
show("^10", "abc", "abcd")
show("*^10", "abc")
show("-<10", "abc")
show("0>10", "abc")
show(".2", "abcdef")
show("10.2", "abcdef")
show(">10.2", "abcdef")
show(".0", "abc")
show("1", "abc")
show("3", "é€😀")
show("^7", "é€😀")
show("d", "abc")
show("+", "abc")
show("=5", "abc")
show("05", "abc")
show(",", "abc")
show("x", "abc")
show("c", "abc")
show("<", "ab")
show("^3", "ab")
print("{:>6}|{:<6}|{:^6}|".format("ab", "cd", "ef"))
print("{0:>{1}}|{0:<{1}}|{0:^{1}}|".format("ab", 6))
print("{:{}}|{:{}{}}|".format("a", 3, "b", ">", 4))
print("{a:{b}}|{a:>{b}.{c}}".format(a="abcdef", b=8, c=3))
print("{0!r:>8}|{0!s:<8}|{0!a:^8}|".format("é"))
print("{0[0]:>4}|{0[1]:<4}|{1[x]:^5}|{1[y]}|".format(["p", "q"], {"x": "a", "y": "b"}))
print("{.real}|{.imag}|{.__name__}".format(5, 2j, int))
class O:
    attr = "val"
    def __init__(self):
        self.lst = [10, 20]
print("{0.attr}|{0.lst[1]}|{o.attr:>5}".format(O(), o=O()))
print("{}{}{}".format(1, 2, 3), "{2}{0}{1}".format(1, 2, 3), "{0}{0}".format("x"))
print("{:%Y}".format(5) if False else "skip", "{:}".format("e"), "{!s:}".format(1))
for bad in ("{", "}", "{0", "{}{1}", "{x}", "{0.nope}", "{0[5]}", "{:z}", "{!x}", "{:{}}"):
    try:
        "".join([bad.format(1)])
    except (ValueError, IndexError, KeyError, AttributeError, TypeError) as e:
        print(repr(bad), type(e).__name__)
print(format("x", ">3"), format(5, ""), format(5), format("x"), format(1.5, ".1f"), format(True), format(None), format([1]), format((1, 2)))
