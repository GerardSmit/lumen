s = "  Hello, World  "
print(repr(s.strip()), repr(s.lstrip()), repr(s.rstrip()))
print("xxhixx".strip("x"), "xxhixx".lstrip("x"), "xxhixx".rstrip("x"))
print("a,b,c".split(","), "a b  c".split(), "a,b,c".split(",", 1))
print(",".join(["a", "b", "c"]), "".join(["x", "y"]), "-".join([]))
print("hello".replace("l", "L"), "hello".replace("l", "L", 1), "abc".replace("", "-"))
print("hello".find("l"), "hello".find("z"), "hello".rfind("l"), "hello".find("l", 3))
print("hello".index("e"), "hello".rindex("l"))
print("banana".count("a"), "banana".count("an"), "banana".count(""), "banana".count("a", 2))
print("hello".startswith("he"), "hello".endswith("lo"), "hello".startswith(("x", "h")))
print("hello".endswith("l", 0, 4), "hello".startswith("l", 2))
print("a=b=c".partition("="), "a=b=c".rpartition("="), "abc".partition("x"), "abc".rpartition("x"))
print(repr("ab".center(7)), repr("ab".center(7, "*")), repr("ab".ljust(5, ".")), repr("ab".rjust(5, ".")))
print("42".zfill(5), "-42".zfill(5), "+7".zfill(4), "abc".zfill(2))
print("hello world".title(), "HELLO".lower(), "hello".upper(), "hELLo".swapcase())
print("hello world".capitalize(), "HELLO".capitalize())
print("they're bill's".title())
print("abc".isalpha(), "abc1".isalnum(), "123".isdigit(), "  ".isspace(), "".isspace())
print("abc".islower(), "ABC".isupper(), "Abc".istitle(), "".isalpha())
print("a" * 3, "ab" * 0, "ab" * -1)
print("abc"[1], "abc"[-1], "abcdef"[1:4], "abcdef"[::2], "abcdef"[::-1])
print(len("hello"), len(""))
print("a-b_c".replace("-", " ").replace("_", " ").split())
words = "the quick brown fox".split()
print([w.capitalize() for w in words], " ".join(reversed(words)))
print(sorted("hello"), "".join(sorted("hello")))
print("abc" < "abd", "abc" == "abc", "B" < "a")
print("%s and %s" % ("x", "y"))
print("x".join("abc"), "abc".removeprefix("a"), "abc".removesuffix("c"), "abc".removeprefix("z"))
print("a\tb".split("\t"), "a  b".split(" "))
try:
    "abc".index("z")
except ValueError as e:
    print(type(e).__name__, e)
try:
    "abc".rindex("z")
except ValueError as e:
    print(type(e).__name__, e)
try:
    ",".join([1, 2])
except TypeError as e:
    print(type(e).__name__, e)
try:
    "abc"[5]
except IndexError as e:
    print(type(e).__name__, e)
try:
    "abc".split("")
except ValueError as e:
    print(type(e).__name__, e)
