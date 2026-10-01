x = 5
y = 3
name = "World"
items = [1, 2, 3]
d = {"k": "v", "n": 7}
print(f"Hello, {name}!")
print(f"{x} + {y} = {x + y}", f"{x * y}", f"{x ** y}", f"{x // y} {x % y} {x / y}")
print(f"{x > y}", f"{x if x > y else y}", f"{not x}", f"{x and y}")
print(f"{items}", f"{items[0]}", f"{items[-1]}", f"{items[1:]}", f"{len(items)}")
print(f"{d['k']}", f"{d['n'] + 1}", f'{d["k"]}')
print(f"{name!r}", f"{name!s}", f"{'é'!a}", f"{name!r:>10}", f"{name!s:<8}|")
print(f"{x=}", f"{x + y=}", f"{name=}", f"{name = }", f"{x=!r}", f"{name=!r}", f"{x=:>4}")
print(f"{{literal}}", f"{{{x}}}", f"}}{{", f"{{x}}")
print(f"{'single'}", f"{"double"}", f"{'a' + "b"}")
print(f"{"nested " + f"{x}"}")
print(f"{f'{x}'}", f"{f"{f"{y}"}"}")
print(f"{', '.join(str(i) for i in items)}")
print(f"{[i * 2 for i in items]}", f"{ {i: i * i for i in items} }")
print(f"{(lambda a: a + 1)(x)}")
print(f"{x:d}", f"{3.14159:.2f}", f"{name:>8}", f"{name:*^11}")
print(f"{'a' 'b'}", f"{x}{y}", f"{x} {y}", f"{x}" f"{y}")
print(f"no placeholders", f"", f"{''}")
print(f"{x!r}", f"{items!r}", f"{None}", f"{True}", f"{None!r}")
print(f"{1_000}", f"{0x1F}", f"{1e3}", f"{1+2}", f"{-x}")
print(f"{x:{y}}|", f"{x:<{y}}|")
print(f"a\tb", f"a\\b", f"\x41", f"é", f"{x}\n".encode())
print(rf"\d{x}", fr"{name}\n", Rf"\t{y}")
print(f"{name.upper()}", f"{name.lower().title()}", f"{name[::-1]}", f"{name * 2}")
print(f"{x:b}", f"{x:o}", f"{255:x}", f"{255:X}", f"{255:#x}")
print(f"{len(name):03d}", f"{x:+d}", f"{-x:+d}")
class P:
    def __str__(self):
        return "P-str"
    def __repr__(self):
        return "P-repr"
p = P()
print(f"{p}", f"{p!r}", f"{p!s}", f"{p!a}", f"{[p]}")
print(f"{'a' if x else 'b'}", f"{(x, y)}", f"{(1,)}", f"{[]}", f"{ {} }", f"{()}")
print(f"""{x}
{y}""")
print(f'''{name}''' f"""{x}""")
print(f"{x}" == "5", f"{x!s}" == str(x), type(f"{x}").__name__)
print(f"{'{'}", f"{'}'}", f"{'{}'}")
print(f"{x:^5}|{y:<5}|{x:>5}|")
print(f"{1:{2}}")
print(f"{ x }", f"{x  }", f"{  x}")
print(f"{x !r}")
