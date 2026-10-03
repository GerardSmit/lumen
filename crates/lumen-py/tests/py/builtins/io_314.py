import _io, io, os, sys, gc, warnings

def t(f):
    try:
        print(f())
    except BaseException as e:
        print(type(e).__name__, e)

t(lambda: io.DEFAULT_BUFFER_SIZE)
b = _io.BytesIO(b"abc")
m = b.getbuffer()
t(lambda: type(m.obj).__name__)
t(lambda: _io._BytesIOBuffer())
t(lambda: b.write(b"x"))
t(lambda: _io._RawIOBase().readinto(bytearray(1)))
t(lambda: _io._RawIOBase().write(b"x"))
t(lambda: _io._RawIOBase().read(1))

import tempfile
path = tempfile.gettempdir() + "/lumen_io_314.tmp"
with open(path, "wb") as f:
    f.write(b"hello")

def leak(kind):
    with warnings.catch_warnings(record=True) as w:
        warnings.simplefilter("always")
        if kind == "raw":
            f = _io.FileIO(path)
        elif kind == "buf":
            f = _io.BufferedReader(_io.FileIO(path))
        elif kind == "text":
            f = open(path)
        else:
            f = open(path, "rb")
        name = repr(f)
        del f
        gc.collect()
        return [(x.category.__name__, str(x.message).split(" at ")[0][:30]) for x in w]

for k in ("raw", "buf", "text", "rb"):
    t(lambda: leak(k))

f = _io.FileIO(path)
t(lambda: f._isatty_open_only())
f.close()
t(lambda: f._isatty_open_only())
t(lambda: f._dealloc_warn(f))
t(lambda: f._finalizing)
g = open(path, "rb")
t(lambda: (g._finalizing, g.raw._finalizing))
g.close()
t(lambda: _io.BufferedReader._dealloc_warn.__name__)
os.unlink(path)
m.release()
