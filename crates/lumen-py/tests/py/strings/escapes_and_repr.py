print("tab\there", "nl\\n", 'q"uote', "q'uote", "bs\\")
print(repr("a'b"), repr('a"b'), repr("a'b\"c"), repr("plain"), repr(""), repr("\\"), repr("\n\t\r"), repr("\x00\x1f\x7f"))
print(repr("é"), repr("€"), repr("😀"), repr("​"), repr("\xa0"), repr("­"), repr("퟿"), repr("\U0001F600"))
print(ascii("é€😀"), ascii("a'b"), ascii("\x7f\x80"), ascii(["é", "b"]))
print(str("x"), str(1), str(None), str(True), str([1, "a"]), str(("a",)), str({"k": "v"}), str(b"x"))
print("\x41B\U00000043\103\7" == "ABC\a", "\a\b\f\v" == "\x07\x08\x0c\x0b", len("\N{BULLET}") if False else 1)
print(r"\n\t", len(r"\n"), r"a\\b", len(r"\\"), R"x\y", repr(r"\d+"), r"\'" == "\\'", len(r"\""))
print("a" "b" 'c', "x" """y""", ("p"
      "q"))
t = """line1
line2 "quoted" 'single'
  indented"""
print(t, t.count("\n"), repr(t))
print("""a\
b""", len("""\
"""))
print(b"abc", b"\x00\xff", b"a'b", b'a"b', b"\n\t\\", repr(b"\xc3"), bytes(3), bytes([65, 66]), b"ab" + b"cd", b"ab" * 2)
print(b"abc"[0], b"abc"[1:], list(b"hi"), bytes(b"abc").upper(), b"a,b".split(b","), b"abc".find(b"c"), b"abc" == b"abc", b"a" < b"b", len(b"abc"))
print(bytearray(b"abc"), bytearray(3), bytearray([1, 2]) + b"x", repr(bytearray(b"a")))
ba = bytearray(b"hello")
ba[0] = 72
ba.append(33)
ba.extend(b"??")
print(ba, bytes(ba), ba.decode(), len(ba), ba.pop(), ba)
print(repr(str), repr(len)[:20] if False else "skip", type("x").__name__, type(b"x").__name__)
print("%r %s %a" % ("é", "é", "é"))
print(str(b"abc", "utf-8"), str(bytes([0xe2, 0x82, 0xac]), "utf-8"), "abc".__len__(), "abc".__contains__("b"), "a".__add__("b"))
print("é" == "é", "é" == "é", len("é"), "é" < "f", "Z" < "a", "é" > "z", sorted("bAaB"), sorted("éa€z"))
print("ß".upper(), "ǆ".title() if False else "x", "İ".lower() == "i̇", "Σ".lower(), "ﬁ".upper(), "ß".casefold(), "ÀÉ".lower(), "àé".upper())
print("日本語".encode(), len("日本語"), "日本語"[1], "日本語"[::-1], "日本語".encode().decode() == "日本語", len("日本語".encode()))
print("a\x00b", len("a\x00b"), repr("a\x00b"), "a\x00b".split("\x00"))
