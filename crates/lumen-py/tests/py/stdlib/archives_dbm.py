import bz2
import dbm
import dbm.dumb
import io
import lzma
import os
import plistlib
import tarfile
import tempfile
import xmlrpc.client
import zipfile

data = b"The quick brown fox jumps over the lazy dog. " * 40

c = bz2.compress(data)
print(c[:3], bz2.decompress(c) == data)
d = bz2.BZ2Decompressor()
print(d.decompress(c[:20]) + d.decompress(c[20:]) == data, d.eof, d.unused_data)
print(bz2.decompress(c + bz2.compress(b"tail")) == data + b"tail")
try:
    bz2.decompress(b"not bz2 data at all")
except OSError as e:
    print("OSError", e)

x = lzma.compress(data)
print(x[:6], lzma.decompress(x) == data)
alone = lzma.compress(data, format=lzma.FORMAT_ALONE)
print(lzma.decompress(alone) == data)
raw = lzma.compress(data, format=lzma.FORMAT_RAW, filters=[{"id": lzma.FILTER_LZMA2, "preset": 3}])
print(lzma.decompress(raw, format=lzma.FORMAT_RAW, filters=[{"id": lzma.FILTER_LZMA2, "preset": 3}]) == data)
try:
    lzma.decompress(b"garbage garbage")
except lzma.LZMAError as e:
    print("LZMAError", e)
try:
    lzma.LZMACompressor(format=lzma.FORMAT_RAW)
except ValueError as e:
    print("ValueError", e)

with tempfile.TemporaryDirectory() as tmp:
    path = os.path.join(tmp, "f.bz2")
    with bz2.open(path, "wt") as f:
        f.write("hello\nworld\n")
    with bz2.open(path, "rt") as f:
        print(f.read().split())
    path = os.path.join(tmp, "f.xz")
    with lzma.open(path, "wb") as f:
        f.write(data)
    with lzma.open(path, "rb") as f:
        print(f.read() == data)

    zpath = os.path.join(tmp, "a.zip")
    for comp in (zipfile.ZIP_STORED, zipfile.ZIP_DEFLATED, zipfile.ZIP_BZIP2, zipfile.ZIP_LZMA):
        with zipfile.ZipFile(zpath, "w", comp) as z:
            z.writestr("dir/one.txt", data)
            z.writestr("two.txt", "two")
        with zipfile.ZipFile(zpath) as z:
            print(comp, z.namelist(), z.read("dir/one.txt") == data, z.testzip())

    for mode in ("w:", "w:gz", "w:bz2", "w:xz"):
        tpath = os.path.join(tmp, "a.tar")
        with tarfile.open(tpath, mode) as t:
            info = tarfile.TarInfo("hello.txt")
            info.size = len(data)
            t.addfile(info, io.BytesIO(data))
        with tarfile.open(tpath) as t:
            print(mode, t.getnames(), t.extractfile("hello.txt").read() == data)

    db = dbm.dumb.open(os.path.join(tmp, "db"), "c")
    db[b"a"] = b"1"
    db["b"] = "2"
    print(sorted(db.keys()), db[b"a"], len(db))
    del db[b"a"]
    db.close()
    db = dbm.dumb.open(os.path.join(tmp, "db"), "r")
    print(list(db.keys()), b"b" in db)
    db.close()

    db = dbm.open(os.path.join(tmp, "db2"), "c")
    db[b"k"] = b"v"
    print(db[b"k"])
    db.close()

pl = {"s": "text", "i": 7, "f": 1.5, "b": True, "l": [1, 2], "d": {"k": b"bin"}}
print(plistlib.loads(plistlib.dumps(pl)) == pl)
print(plistlib.loads(plistlib.dumps(pl, fmt=plistlib.FMT_BINARY)) == pl)

req = xmlrpc.client.dumps((1, "two", [3.5], {"k": True}), "method")
print(req.count("<value>"))
params, name = xmlrpc.client.loads(req)
print(params, name)
resp = xmlrpc.client.dumps(xmlrpc.client.Fault(4, "oops"), methodresponse=True)
try:
    xmlrpc.client.loads(resp)
except xmlrpc.client.Fault as f:
    print("Fault", f.faultCode, f.faultString)
