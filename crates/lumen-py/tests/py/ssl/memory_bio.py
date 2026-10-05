import _ssl

bio = _ssl.MemoryBIO()
print(bio.pending, bio.eof)
print(bio.write(b"hello world"))
print(bio.pending, bio.eof)
print(bio.read(5))
print(bio.pending)
print(bio.read())
print(bio.read(), bio.eof)
bio.write(b"abc")
bio.write_eof()
print(bio.eof)
print(bio.read(2), bio.eof)
print(bio.read(), bio.eof)
try:
    bio.write(b"x")
except _ssl.SSLError as e:
    print(type(e).__name__, e)
bio2 = _ssl.MemoryBIO()
print(bio2.write(bytearray(b"ba")), bio2.write(memoryview(b"mv")))
print(bio2.read(-1))
try:
    bio2.write("text")
except TypeError as e:
    print("TypeError")
try:
    _ssl.MemoryBIO(1)
except TypeError:
    print("TypeError")
