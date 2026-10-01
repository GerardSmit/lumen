n = 1234567
f = 3.14159265
s = "abc"
print(f"[{s:10}]", f"[{s:<10}]", f"[{s:>10}]", f"[{s:^10}]", f"[{s:*^10}]", f"[{s:-<6}]", f"[{s:.2}]", f"[{s:5.2}]")
print(f"[{42:5}]", f"[{42:<5}]", f"[{42:^5}]", f"[{42:05}]", f"[{42:=5}]", f"[{-42:=5}]", f"[{-42:05}]", f"[{-42:x<5}]")
print(f"[{42:+}]", f"[{42:-}]", f"[{42: }]", f"[{-42:+}]", f"[{-42: }]", f"[{42:+5}]", f"[{42:+05}]")
print(f"{n:,}", f"{n:_}", f"{n:,d}", f"{n:_d}", f"{-n:,}", f"{n:15,}", f"{n:015,}", f"{n:>15,}")
print(f"{0xFFFF:_x}", f"{0xFFFF:_b}", f"{0b1010:#_b}", f"{0xFFFFFFFF:#_x}", f"{10**9:,}")
print(f"{255:b}", f"{255:o}", f"{255:x}", f"{255:X}", f"{255:#b}", f"{255:#o}", f"{255:#x}", f"{255:#X}", f"{255:#010b}")
print(f"{-255:x}", f"{-255:#x}", f"{65:c}", f"{0:b}", f"{5:08b}", f"{5:#010b}")
print(f"{f:f}", f"{f:.0f}", f"{f:.1f}", f"{f:.3f}", f"{f:10.2f}", f"{f:<10.2f}|", f"{f:^10.2f}|", f"{f:010.2f}", f"{-f:010.2f}")
print(f"{f:e}", f"{f:.2e}", f"{f:E}", f"{f:g}", f"{f:.3g}", f"{f:G}", f"{1e10:g}", f"{1e-10:g}", f"{123456789.0:g}")
print(f"{0.25:%}", f"{0.25:.0%}", f"{0.12345:.2%}", f"{1:%}", f"{0.5:10.1%}|")
print(f"{f:+.2f}", f"{f:+f}", f"{f: .2f}", f"{f:,.2f}", f"{1234567.891:,.2f}", f"{1234567.891:_.1f}")
print(f"{3:.2f}", f"{3:e}", f"{3:g}", f"{3:%}", f"{True:d}", f"{True}", f"{True:5}|")
w = 12
p = 3
print(f"[{f:{w}.{p}f}]", f"[{f:<{w}.{p}f}]", f"[{s:>{w}}]", f"[{s:^{w}}]", f"[{s:{'*'}^{w}}]")
fill = "."
align = ">"
print(f"[{s:{fill}{align}{w}}]", f"[{n:{fill}{align}{w},}]", f"[{n:{w}}]")
print(f"{n:n}", f"{f:n}", f"{12:n}")
print(f"{1e16:f}", f"{1e-7:f}", f"{123.456:.0f}", f"{0.5:.0f}", f"{1.5:.0f}", f"{2.5:.0f}", f"{2.675:.2f}")
print(f"{0:.2f}", f"{-0.0:.1f}", f"{0.0001:.2e}", f"{1234.5:e}", f"{0.1:.20f}")
print(f"{12345.6789:10.3e}|", f"{12345.6789:<14.3E}|", f"{1.0:g}", f"{1.5:g}", f"{100000.0:g}", f"{1000000.0:g}", f"{0.0001:g}", f"{0.00001:g}")
print(f"{1.0:#g}", f"{1.0:#.3g}", f"{1.0:#.0f}", f"{5:#.1f}")
print(f"{s!r:>8}", f"{s!r:^9}", f"{n!r:>9}")
print(f"{'':5}|", f"{'':^5}|", f"{'x':3}|", f"{12:3}|", f"{1.5:6}|", f"{None!s:>6}|")
print(f"{42:c}" == "*", f"{97:c}", f"{0x1F600:c}")
for bad in ("{:d}", "{:.2f}", "{:,s}", "{:c}"):
    try:
        print(bad.format("str" if "s" not in bad else 5))
    except ValueError as e:
        print(type(e).__name__)
try:
    print(f"{s:d}")
except ValueError as e:
    print(type(e).__name__, e)
try:
    print(f"{f:d}")
except ValueError as e:
    print(type(e).__name__, e)
try:
    print(f"{s:=5}")
except ValueError as e:
    print(type(e).__name__, e)
