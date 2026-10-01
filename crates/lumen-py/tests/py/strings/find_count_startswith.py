s = "hello world, hello there"
print(s.find("hello"), s.find("hello", 1), s.find("x"), s.find(""), s.find("", 100), s.find("o", 5, 8), s.find("o", -5))
print(s.rfind("hello"), s.rfind("o"), s.rfind("o", 0, 5), s.rfind("zz"), s.rfind(""))
print(s.index("world"), s.rindex("hello"))
for f in (lambda: s.index("zz"), lambda: s.rindex("zz")):
    try:
        f()
    except ValueError as e:
        print("ValueError", e)
print(s.count("hello"), s.count("l"), s.count("ll"), s.count(""), s.count("l", 5), s.count("l", 0, 4), "aaaa".count("aa"), "".count(""))
print(s.startswith("hello"), s.startswith("world", 6), s.startswith(("x", "hel")), s.startswith(""), s.startswith("hello", 1), s.endswith("there"), s.endswith(("a", "re")), s.endswith("o", 0, 5), s.endswith(""))
print("abc".startswith("abcd"), "abc".endswith("abcd"), "abc".startswith("c", -1), "abc".endswith("a", -3, -2))
for bad in (lambda: "abc".startswith(1), lambda: "abc".find(1), lambda: "a" + 1, lambda: "a" * "b", lambda: "abc".count()):
    try:
        bad()
    except TypeError:
        print("TypeError")
print(ord("a"), ord("A"), ord("0"), ord("\n"), ord("é"), ord("€"), ord("😀"), ord("\x00"))
print(chr(97), chr(65), chr(8364), chr(128512), repr(chr(0)), repr(chr(10)), repr(chr(127)), repr(chr(160)), chr(0x1F600) == "😀")
print([ord(c) for c in "aZ09 "], "".join(chr(ord(c) + 1) for c in "HAL"), "".join(chr(i) for i in range(65, 91)))
try:
    chr(-1)
except ValueError:
    print("ValueError chr")
try:
    chr(0x110000)
except ValueError:
    print("ValueError chr2")
try:
    ord("ab")
except TypeError:
    print("TypeError ord")
print("abc"[0], "abc"[-1], "abc"[1:], "abc"[:-1], "abc"[::-1], "abcdef"[::2], "abcdef"[1:5:2], "abc"[5:], "abc"[-10:2])
try:
    "abc"[3]
except IndexError as e:
    print("IndexError", e)
try:
    "abc"[0] = "x"
except TypeError:
    print("immutable")
print(len(""), len("abc"), len("é"), len("😀"), len("́"), len("\n"), len("\\n"), len(r"\n"))
print("abc".encode(), "é".encode(), "€".encode("utf-8"), "😀".encode(), b"\xc3\xa9".decode(), b"abc".decode("ascii"), "é".encode("utf-8").hex(), len("€".encode()))
print("é".encode("latin-1"), "abc".encode("ascii"), bytes([104, 105]).decode(), "héllo".encode("ascii", "ignore"), "héllo".encode("ascii", "replace"), "héllo".encode("ascii", "backslashreplace"))
try:
    "é".encode("ascii")
except UnicodeEncodeError:
    print("UnicodeEncodeError")
try:
    b"\xff".decode()
except UnicodeDecodeError:
    print("UnicodeDecodeError")
print(b"\xff".decode(errors="replace") == "�", b"a\xffb".decode("utf-8", "ignore"))
