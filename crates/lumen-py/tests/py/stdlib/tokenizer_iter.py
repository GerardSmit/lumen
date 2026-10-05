import io
import token
import _tokenize


def rl(src):
    return io.StringIO(src).readline


def show(label, f):
    try:
        r = f()
        print(label, 'ok', r)
    except Exception as e:
        print(label, "EXC", type(e).__name__)


def toks(src, **kw):
    return [(token.tok_name[t[0]], t[1], t[2], t[3]) for t in _tokenize.TokenizerIter(rl(src), **kw)]


for src in ["x = 1\n", "x = (1,\n 2)  # c\n\n", "if x:\n    y\n", "f'a{b}c' + 1\n", "x = 'a\n", "def f(): pass"]:
    for extra in (True,):
        show(repr(src) + str(extra), lambda: toks(src, extra_tokens=extra))

show('no extra', lambda: _tokenize.TokenizerIter(rl('x\n')))
show('no source', lambda: _tokenize.TokenizerIter(extra_tokens=True))
show('positional', lambda: _tokenize.TokenizerIter(rl('x\n'), True))
show('bad readline', lambda: list(_tokenize.TokenizerIter(lambda: 1, extra_tokens=True)))
show('bytes readline', lambda: list(_tokenize.TokenizerIter(io.BytesIO(b'x = 1\n').readline, extra_tokens=True, encoding='utf-8')))
show('bytes no encoding', lambda: list(_tokenize.TokenizerIter(io.BytesIO(b'x = 1\n').readline, extra_tokens=True)))
show('name', lambda: _tokenize.TokenizerIter.__name__)
show('module', lambda: _tokenize.TokenizerIter.__module__)
show('iter', lambda: iter(_tokenize.TokenizerIter(rl('x\n'), extra_tokens=True)).__class__.__name__)
