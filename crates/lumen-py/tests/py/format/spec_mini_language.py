print("{:<8}|{:>8}|{:^8}|{:*^8}|{:=8}|".format("ab", "ab", "ab", "ab", 42))
print("{:+d} {:-d} {: d} {:+d} {: d}".format(5, 5, 5, -5, -5))
print(format(255, "b"), format(255, "o"), format(255, "x"), format(255, "X"), format(255, "d"), format(65, "c"))
print(format(255, "#b"), format(255, "#o"), format(255, "#x"), format(255, "#X"), format(-255, "#x"))
print(format(5, "08b"), format(255, "#010x"), format(-5, "06d"), format(5, "<06d"), format(5, "^6d") + "|")
print(format(1234567, ","), format(1234567, "_"), format(1234567.5, ",.1f"), format(0xABCDEF, "_x"), format(1234, ",d"))
print(format(3.14159, "e"), format(3.14159, "E"), format(3.14159, ".2e"), format(31415.9, ".3E"), format(0.0, "e"))
print(format(3.14159, "f"), format(3.14159, "F"), format(3.14159, ".0f"), format(3.14159, "10.3f"), format(-3.14159, "010.3f"))
print(format(3.14159, "g"), format(3.14159, "G"), format(1e20, "g"), format(1e-20, "G"), format(123456789.0, "g"), format(0.00001234, "g"))
print(format(0.5, "%"), format(0.123456, ".2%"), format(2, "%"), format(0.5, "8.1%"), format(1.5, "+.0%"))
print(format(float("inf"), "f"), format(float("-inf"), "e"), format(float("nan"), "g"), format(float("inf"), "F"), format(float("nan"), "F"))
print(format(float("inf"), "010"), format(float("nan"), ">6"), format(float("inf"), "+"))
print(format("abc", "s"), format("abc", ""), format("abc", ">5"), format("abc", ".1"), format("abc", "^7.2"), format("abc", "x^7"))
print(format(42, ""), format(4.5, ""), format(True, ""), format(None, ""), format([1, 2], ""), format((1,), ""))
print(format(1234, "n"), format(12.5, "n"), format(0.1, "n"), format(1e20, "n"))
print(format(42, "c") == "*", format(0x41, "c"), format(1, "5c") + "|")
print(format(2 ** 70, ","), format(2 ** 70, "x"), format(2 ** 70, "_b")[:12], format(-(2 ** 70), "_d"))
print(format(1e15, "f"), format(1e15, ".2f"), format(1.5e300, "e"), format(5e-324, "e"))
print(format(123.456, ".0e"), format(123.456, "#.0e"), format(0.0, "g"), format(100.0, "g"), format(-0.0, "g"))
print(format(1.5, ".0f"), format(2.5, ".0f"), format(0.125, ".2f"), format(0.375, ".2f"), format(1e-9, ".3f"))
print("{0}{1}{0}".format("a", "b"), "{a}{b}".format(a=1, b=2), "{}{}".format(1, 2), "{0[0]}{0[1]}".format("xy"))
print("{0.real} {0.imag}".format(5), "{x[k]}".format(x={"k": "v"}), "{x.__class__.__name__}".format(x=1))
print("{:{}}|".format("a", 5), "{:{}.{}f}".format(3.14159, 8, 2), "{0:{1}}|".format("a", 4), "{:{w}}|".format("a", w=3))
print("{!r} {!s} {!a}".format("é", "é", "é"), "{!r:>6}".format("a"), "{0!r}".format(1.0))
print("{{}} {{{}}}".format(1), "{{0}}".format())
print("{:>{w}.{p}}".format("abcdef", w=6, p=3) + "|")
print("{0:,} {0:_} {0:+,}".format(12345), "{:,.2f}".format(0.5), "{:,}".format(999), "{:,}".format(1000))
print("{:08.3f}".format(-3.14159), "{:+08.3f}".format(3.14159), "{: 08.3f}".format(3.14159), "{:<8.3f}|".format(1.5))
print("{:#x} {:#o} {:#b}".format(0, 0, 0), "{:#06x}".format(255), "{:x}".format(-1), "{:b}".format(-3))
print("{:.3}".format("abcdef"), "{:.10}".format("abc"), "{:.0}".format("abc") == "")
print("{:.3}".format(3.14159), "{:.3}".format(1234.5678), "{:10.3}".format(0.00012345), "{:.1}".format(0.05))
print("{:5}|{:<5}|{:>5}|{:^5}|".format(1, 2, 3, 4), "{:5}|{:<5}|{:>5}|{:^5}|".format("a", "b", "c", "d"))
print("{:^6}|{:^7}|{:^5.1f}|".format("ab", "ab", 1.25), "{:*>+8.2f}".format(3.14159))
for spec, val in (("d", "s"), ("f", "s"), ("s", 1), ("z", 1), (",s", "a"), ("c", 1.5), ("x", 1.5), ("d", 1.5), ("=5", "a"), ("+", "a"), ("0=5", "a")):
    try:
        print(format(val, spec))
    except (ValueError, TypeError) as e:
        print(type(e).__name__)
try:
    "{}".format()
except IndexError as e:
    print(type(e).__name__, e)
try:
    "{a}".format()
except KeyError as e:
    print(type(e).__name__, e)
try:
    "{0.nope}".format(1)
except AttributeError as e:
    print(type(e).__name__)
try:
    "{".format(1)
except ValueError as e:
    print(type(e).__name__, e)
try:
    "}".format(1)
except ValueError as e:
    print(type(e).__name__, e)
