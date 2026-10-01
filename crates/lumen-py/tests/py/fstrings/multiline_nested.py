rows = [("apple", 3, 0.5), ("banana", 12, 0.25), ("cherry", 100, 12.125)]
header = f"""{'name':<10}|{'qty':>5}|{'price':>8}"""
print(header)
print("-" * len(header))
for name, qty, price in rows:
    print(f"{name:<10}|{qty:>5}|{price:>8.2f}")
total = sum(q * p for _, q, p in rows)
print(f"""Total: {
    total:.3f}""")
msg = f"""
Line one: {rows[0][0]}
Line two: {rows[1][1] + rows[2][1]}
Line three: {'yes' if total > 10 else 'no'}
"""
print(msg)
print(repr(msg))
text = (
    f"a={1 + 1} "
    f"b={'x' * 3} "
    "plain {not_interpolated} "
    f"c={[i for i in range(3)]}"
)
print(text)
width = 8
for i in range(1, 4):
    print(f"{i:{width}d}|{i * i:<{width}d}|{'*' * i:^{width}}|")
print(f"{f'{1 + 1}' + f'{2 + 2}'}")
print(f"{'outer ' + f'inner {1 + 2}'}")
print(f"{f'{f"{1}"}'}")
inner = 5
print(f"{f'{inner:>{inner}}'}|")
print(f"{ {'a': 1}['a'] }", f"{[1, 2, 3][1]}")
print(f"{'\n'.join(['a', 'b'])}")
print(f"{'\\'}", f"{'\t'!r}")
print(f"""{
    'multi'
    + 'line'
}""")
print(f"{(
    1
    + 2
)}")
print(f"{'a' # comment
}")
d = {"x": 1, "y": 2}
print(", ".join(f"{k}={v}" for k, v in d.items()))
print([f"{i}:{i * i}" for i in range(4)])
print({f"k{i}": f"{i:02}" for i in range(3)})
fmt = "^7"
print(f"{'mid':{fmt}}|", f"{'mid':{'*' + fmt}}|")
print(f"{3.14159:{'.3f'}}", f"{42:{'>'}{6}}")
def f(a, b=2):
    return f"{a}-{b}"
print(f(1), f"{f(3, b=4)}", f"{f('z')!r}")
print(f"{ {1, 2}!r}" == "{1, 2}")
print(f"{(lambda: 'lam')()}")
print(f'{"a" "b"}' f"{'c'}")
lines = ["a", "b", "c"]
print(f"{len(lines)} lines: {', '.join(f'<{l}>' for l in lines)}")
print(f"""{'''triple'''}""")
print(f"{f"{f"{f"deep"}"}"}")
