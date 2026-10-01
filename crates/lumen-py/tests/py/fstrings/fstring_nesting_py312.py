songs = ["a", "b"]
print(f"{", ".join(songs)}")
print(f"{f"{f"{1 + 1}"}"}")
print(f"{'\n'.join(songs)}")
print(f"{"\N{BULLET}"}" == "•", f"{'\\'}", f"{'\t'!r}")
d = {"a": 1, "b": 2}
print(f"{d["a"]} {d['b']}", f"{"x" if d["a"] else "y"}")
print(f"{
    1 + 2
}")
print(f"{[
    i * 2
    for i in range(3)
]}")
print(f"{d  # comment
}" if False else "skipped")
w = 8
print(f"{f'{w}':>{w}}|", f"{f"{w:>{w}}"}|", f"{f'{w!r:^{w}}'}|")
print(f"{'a' 'b'}", f"{'a' + 'b':>{len('abc') + 2}}")
print(f"{(lambda: 'lam')()}", f"{(lambda x, y=2: x * y)(3)}", f"{(x := 5)} {x}", f"{(y := 1) + y}")
print(f"{ 1 }", f"{1 }", f"{ 1}", f"{ 1 = }", f"{3 = :>4}")
print(f"{'{'}", f"{'}'}", f"{{}}", f"{'{}'}", f"{{{1}}}", f"{{{{{2}}}}}")
v = 12
print(f"{v:{'#'}>6}|", f"{v:{'0'}6}|", f"{v:#{'x'}}", f"{v:{"b"}}", f"{v:{w}.{w - 6}f}")
def f(*a, **k):
    return f"{a} {sorted(k.items())}"
print(f(1, 2, a=f"{v}"), f"{f(*[1], **{'z': f'{v}'})}")
print(f"{'a'!r}", f"{'a'!s:>3}", f"{'é'!a}", f"{[1, 'a']!s}", f"{[1, 'a']!r}")
print(f"{1:{2}{3}}|" if False else "x", f"{12345:{'_'}}", f"{1234.5678:{'.'}{2}{'f'}}")
print(f"""{"""triple"""}""" if False else "t", f'''{'''a'''}''' if False else "u", f"""{
'in triple'}""")
name = "n"
print(f"{name}" "{name}" f"{name}", "{name}" f"{name}")
print(f"{ 'A' }", f"{'\x41'}", f"{'\101'}", f"\{name}" if False else "bs")
print(f"{'%s %s' % ('a', 'b')}", f"{'a' in 'abc'}", f"{not 0}", f"{-v}", f"{~v}", f"{v if v else 0:>5}", f"{v or 'z'}")
print(f"{*[1, 2], }" if False else "star", f"{(*[1, 2],)}", f"{[*range(3)]}", f"{ {**{'a': 1}} }")
