import sys


def show(label, f):
    try:
        r = f()
        print(label, "ok", r)
    except BaseException as e:
        print(label, type(e).__name__, e.msg if isinstance(e, SyntaxError) else e)


print(sys.get_int_max_str_digits(), sys.flags.int_max_str_digits)
print(sys.int_info.default_max_str_digits, sys.int_info.str_digits_check_threshold)

for v in (-1, 5, 639, "a", 1.5, None):
    show("set %r" % (v,), lambda: sys.set_int_max_str_digits(v))

sys.set_int_max_str_digits(640)
print(sys.get_int_max_str_digits(), sys.flags.int_max_str_digits)

show("10**639", lambda: len(str(10**639)))
show("-10**639", lambda: len(str(-10**639)))
show("10**640", lambda: len(str(10**640)))
show("-10**640", lambda: len(str(-10**640)))
show("repr", lambda: len(repr(10**640)))
show("format", lambda: len(format(10**640, "d")))
show("format _", lambda: len(format(10**640, "_d")))
show("fstring", lambda: len(f"{10**640}"))
show("percent", lambda: len("%d" % 10**640))
show("percent s", lambda: len("%s" % 10**640))
show("str.format", lambda: len("{}".format(10**640)))
show("list repr", lambda: len(repr([10**640])))
show("hex", lambda: len(hex(10**640)))
show("oct", lambda: len(oct(10**640)))
show("bin", lambda: len(bin(10**640)))
show("format x", lambda: len(format(10**640, "x")))

show("int 640", lambda: int("1" * 640) > 0)
show("int 641", lambda: int("1" * 641))
show("int spaces", lambda: int("  " + "1" * 700 + "  "))
show("int leading zeros", lambda: int("0" * 300 + "1" * 700))
show("int sign", lambda: int("-" + "1" * 700))
show("int underscores", lambda: int("1_" * 400 + "1") > 0)
show("int bytes", lambda: int(b"1" * 700))
show("int base 0", lambda: int("1" * 700, 0))
show("int base 0 hex", lambda: int("0x" + "1" * 700, 0) > 0)
for base in (2, 3, 4, 7, 8, 10, 16, 32, 36):
    show("int base %d" % base, lambda: int("1" * 700, base) > 0)
show("float", lambda: float("1" * 700) > 0)

show("literal", lambda: eval("1" * 700))
show("literal 640", lambda: eval("1" * 640) > 0)
show("hex literal", lambda: eval("0x" + "1" * 700) > 0)
show("exec", lambda: exec("x = " + "1" * 700))

sys.set_int_max_str_digits(maxdigits=700)
print(sys.get_int_max_str_digits(), sys.flags.int_max_str_digits)
show("700", lambda: len(str(10**699)))
show("700+1", lambda: len(str(10**700)))

sys.set_int_max_str_digits(0)
print(sys.get_int_max_str_digits(), sys.flags.int_max_str_digits)
show("unlimited", lambda: len(str(7**5000)))
show("unlimited int", lambda: len(str(int("9" * 6000))))
