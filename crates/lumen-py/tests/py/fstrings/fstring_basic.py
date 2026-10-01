name = "World"
n = 42
pi = 3.14159265
items = [1, 2, 3]
d = {"k": "v", "n": 7}
print(f"Hello, {name}!", f"{n} {n + 1} {n * 2}", f"{name!r} {name!s} {'é'!a}", f"{{literal}} {{{n}}}")
print(f"{items} {items[0]} {items[-1]} {items[1:]} {len(items)}", f"{d['k']} {d["n"]}", f"{name.upper()} {name.lower()[:2]}")
print(f"{pi:.2f} {pi:8.3f}| {pi:<10.1f}| {n:5d}| {n:<5d}| {n:^7d}| {n:05d} {n:+d} {n:x} {n:#x} {n:b} {n:o} {n:e}")
print(f"{1000000:,} {1000000:_} {0.5:%} {0.123:.1%} {1234.5:,.2f} {255:#010b} {-5:05d} {3:>+5}")
print(f"{name:>10}|{name:<10}|{name:^10}|{name:*^11}|{name:->8}|{name:.3}|{name:10.3}|")
width, prec = 10, 3
print(f"{pi:{width}.{prec}f}|{name:>{width}}|{n:0{width}d}|{name:{'^'}{width}}|{pi:{width}}")
print(f"{n=} {name=} {n + 1=} {items=} {pi=:.2f} {name = } {n=!r}")
print(f"{'a' if n > 40 else 'b'} {(lambda x: x * 2)(4)} {[i * i for i in range(4)]} {sum(i for i in range(5))} {n > 3 and 'yes'}")
print(f"{n:{'>' if n else '<'}6}|", f"{{{{}}}}", f"a{''}b", f"{'':5}|", f"{None} {True} {1.0} {1e10} {1e-5} {2 ** 70}")
print(f"{3.0} {3.5e20} {-0.0} {float('inf')} {float('nan'):>6} {1/3}", f"{'nested {x}'}", f'{"dq"}')
x = 5
print(f"{x}{x}{x}", f"{x:>3}{x:<3}|", f"{x!r:>4}", f"{'a'!r:>5}", f"{x**2:4}", f"{-x:4}", f"{x:4}")
print(f"a\tb\\n{x}\n---", f"\x41B{x}", f"{x}" f"{x + 1}" "plain", rf"\n{x}", fr"{x}\t")
print(f"""multi
line {x}
  {name!r}""")
print(f"{'x' * 3}", f"{'%s' % 'pct'}", f"{'{}'.format('fmt')}", f"{x:{x}}|", f"{x:{x}.{x - 3}f}|")
print(f"{ord('a'):#06x} {chr(97)} {int('12') + 1} {round(2.675, 2)} {abs(-3)} {max(1, 2)} {str(1).zfill(3)} {[1, 2][1]}")
class C:
    def __format__(self, spec):
        return "C<" + spec + ">"
    def __str__(self):
        return "str-C"
    def __repr__(self):
        return "repr-C"
print(f"{C()} {C():abc} {C()!s} {C()!r} {C():}|{C()!r:>10}|")
print(f"{'tricky: }'}", f"{'a:b'}", f"{d['k']!r}", f"{ {'a': 1}['a'] }", f"{(1, 2)}", f"{ {1, 2} }", f"{'{'}{'}'}")
