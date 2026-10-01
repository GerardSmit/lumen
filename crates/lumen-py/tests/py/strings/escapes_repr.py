print(repr("hello"), repr("it's"), repr('say "hi"'), repr("both ' and \""))
print(repr('a\'b"c'), repr("\\"), repr("a\nb"), repr("tab\there"))
print(repr("\r\0\x01\x7f"), repr("\x1b[0m"), repr("\a\b\f\v"))
print(repr("é"), repr("€"), repr("\u200b"), repr("\U0001F600"), repr("\xa0"))
print(ascii("é"), ascii("€"), ascii("\U0001F600"), ascii("abc"), ascii("it's"))
print(ascii(["é", "a"]), ascii({"k": "ü"}))
print(repr(b"abc"), repr(b"a'b"), repr(b'a"b'), repr(b"\x00\xff\n"), repr(b"a'b\"c"))
print(repr(bytearray(b"hi")), repr(bytes(3)), repr(b""))
print(r"\n", len(r"\n"), r"C:\path\to", r"a\"b", len(r"a\"b"))
print(rb"\x00", len(rb"\x00"))
s = """line1
line2 "quoted" 'single'
line3"""
print(repr(s), s.count("\n"))
t = '''a\
b'''
print(repr(t))
print("a" "b" 'c', ("x"
      "y"))
print("\x41\u0042\U00000043\103\N{LATIN SMALL LETTER A}")
print(repr("\N{EURO SIGN}"), "\N{BULLET}" == "\u2022")
print(len("\\n"), len("\n"), len("\t"), len("\0"))
print(repr(str(1.5)), repr(str(None)), str("x"), repr(repr("x")))
print(repr(repr(repr("a'b"))))
print("%r %r" % ("a", 1), "%a" % "é")
print(["a", "b'c"], ("x",), {"k": 'v"w'})
print(repr("\x80"), repr("\x9f"), repr("\xad"))
print(repr("a" * 3), repr(""))
print("\101\60", "\1" == "\x01")
print("tab\tsep".split("\t"), "nl\nsep".split("\n"))
print(repr("\ud7ff"), repr("\uffff") == "'\\uffff'")
print(chr(0x10FFFF).encode("utf-8").hex())
print(repr(b"\\"), repr(b"\t\r"), repr(b"~\x7f"))
print(len(b"\xff\x00"), b"\101", b"\x41" == b"A")
print("a\
b")
print(repr("'\""), repr("'"), repr('"'))
