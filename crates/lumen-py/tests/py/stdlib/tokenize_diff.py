import io
import token
import tokenize

SOURCES = [
    "",
    "\n",
    "x = 1",
    "x = 1\n",
    "x = 1  # c\n",
    "# only comment\n",
    "# only comment",
    "\n\n  \nx\n",
    "if x:\n    y = 1\n    # c\n\n    z\nw\n",
    "if x:\n\ty\n\tz\n",
    "def f(a, *, b=2, **k) -> int:\n    return a @ b\n",
    "a = (1,\n     2,\n  3)\n",
    "a = 1 + \\\n    2\n",
    "x = 'abc' \"def\" b'x' r'\\n' rb'\\x' Rb'z' f'a{b}c'\n",
    "x = '''multi\nline''' + 1\n",
    "x = 0x1F + 0b101 + 0o17 + 1_000 + 1.5e-3 + 3j + .5 + 5. + 1e10\n",
    "a <<= b >>= c **= d //= e != f -> g := h ... i\n",
    "a = b if c else d\n",
    "async def f():\n    await x\n    async for a in b: pass\n",
    "match x:\n    case 1: pass\n    case _: pass\n",
    "x = é + ñ\n",
    "x = f'{a}'\n",
    "x = f'a{b!r:>{w}}c'\n",
    "x = f'{x=}'\n",
    "x = f'{{}}'\n",
    "x = f'{a:{b}{c}}'\n",
    "x = f\"{f'{y}'}\"\n",
    "x = f'''a\n{b}\nc'''\n",
    "x = rf'\\{a}'\n",
    "x = f'{a + b}' f'c'\n",
    "x = f'{lambda: 1}'\n",
    "x = (f'{a}'\n     'b')\n",
    "x = 1\r\ny = 2\r\n",
    "x = 1\x0c\ny = 2\n",
    "x = 1 ; y = 2\n",
    "$x\n",
    "x = ?\n",
    "x = !\n",
    "x = 'abc\n",
    "x = '''abc\n",
    "x = (1,\n",
    "x = 1)\n",
    "if x:\n  y\n z\n",
    "if x:\ny\n",
    "  x = 1\n",
    "x = 1 \\\n",
    "x = 1 \\",
    "x = 0777\n",
    "x = 1__0\n",
    "x = 1abc\n",
    "x = 0b12\n",
    "x = 'a' 'b'\\\n 'c'\n",
    "class A:\n    def f(self):\n        pass\n\n    x = 1\n",
    "def f():\n    pass\n    \n# c\n",
    "if 1:\n    x\n  # c\n    y\n",
    "x = [\n    1,  # one\n    2,\n]\n",
    "lambda: (yield)\n",
    "print(a, b, sep='')\n",
    "@dec\nclass C: ...\n",
    "x: int = 5\n",
    "with a as b, c as d: pass\n",
    "del a[0]; assert x, 'm'\n",
    "x = not a is not b\n",
    "x = a < b <= c == d >= e > f\n",
    "x = ~a ^ b & c | d % e\n",
    "x = {**a, 'b': 1}\n",
    "x = *a, b\n",
    "type X = int\n",
    "x = 1if y else 2\n",
    "x = 1_0.0_1e1_0j\n",
    "x = '\\\n'\n",
    "x = \"\"\"a\\\nb\"\"\"\n",
    "x = f'{a!r}' f'{b!s:5}'\n",
    "x = f'{a}{b}'\n",
    "x = f'{a:%Y-%m}'\n",
    "x = f'{ a }'\n",
    "x = f'{a:{b:{c}}}'\n",
    "x = f'{\"a\"}'\n",
    "x = f'\\N{DIGIT ONE}'\n",
    "x = f'{a}\n",
    "x = f'}'\n",
    "x = f'a}'\n",
    "x = f'{a}}'\n",
    "x = f'{a:{b}{c}d}'\n",
    "if x:\n  y\n z = 1 + 2\n",
    "x = 1)\n",
    "x = ]\n",
    "x = 0b12\n",
    "x = 1abc\n",
]


def run(src, mode):
    out = []
    try:
        if mode == 'str':
            toks = tokenize.generate_tokens(io.StringIO(src).readline)
        else:
            toks = tokenize.tokenize(io.BytesIO(src.encode('utf-8')).readline)
        for t in toks:
            out.append((token.tok_name[t.type], token.tok_name[t.exact_type], t.string, t.start, t.end, t.line))
    except Exception as e:
        out.append(('EXC', type(e).__name__, str(e.args[0]) if e.args else '', getattr(e, 'args', ())[1:]))
    return out


for src in SOURCES:
    for mode in ('str', 'bytes'):
        print(repr(src), mode)
        for t in run(src, mode):
            print('   ', t)
