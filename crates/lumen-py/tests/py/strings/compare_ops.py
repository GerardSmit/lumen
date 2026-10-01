print("a" < "b", "a" <= "a", "b" > "a", "abc" >= "abd", "a" != "b", "" < "a")
print("Z" < "a", "10" < "9", "apple" < "apple pie", "é" > "z")
print("a" in "banana", "nan" in "banana", "" in "abc", "x" not in "abc", "ba" in "banana")
print("ab" * 3, 3 * "ab", "x" * 0, "x" * -2, "-" * 10)
print("a" + "b" + "c", "abc" + "", "" + "")
s = "abcdefghij"
print(s[2:5], s[:3], s[7:], s[-3:], s[:-3], s[-5:-2])
print(s[::-1], s[::-2], s[8:2:-2], s[-1:-4:-1], s[5:1:-1], s[1:5:-1] == "")
print(s[::3], s[1::3], s[100:], s[-100:2], s[:100], s[3:3] == "")
print(s[None:None:None], s[slice(2, 8, 2)])
print(s[9], s[-10])
x = "a"
x += "b"
x *= 2
print(x)
print(max("abc"), min("abc"), sorted("cab"), list("abc"), tuple("ab"))
print("abc" == "abc", "a" == 1, "1" == 1)
print(("a", 1) < ("a", 2), "b" in ["a", "b"], "b" in ("a", "b"), "bc" in ["abc"])
print("abc".__contains__("b"), "abc".__len__(), "abc".__add__("d"), "ab".__mul__(2))
print("abc".__eq__("abc"), "abc".__lt__("abd"), "a".__getitem__(0))
print(sum(1 for c in "hello" if c in "aeiou"))
print([c for c in "abc"], [c * 2 for c in "abc"], {c: i for i, c in enumerate("abc")})
print("".join(c.upper() if i % 2 else c for i, c in enumerate("abcdef")))
print(any(c.isdigit() for c in "ab3"), all(c.isalpha() for c in "ab3"))
print("a" < "b" < "c", "a" < "b" > "c")
print(str(1) + str(2), "x" * 2 + "y" * 2)
print(list(reversed("abc")), list(enumerate("ab")), list(zip("ab", "xy")))
for bad in (lambda: "a" + 1, lambda: "a" * "b", lambda: "a" < 1, lambda: 1 in "abc"):
    try:
        bad()
    except TypeError as e:
        print("TypeError", e)
try:
    w = "abc"
    w[1] = "x"
except TypeError as e:
    print(type(e).__name__, e)
try:
    key = "a"
    "abc"[key]
except TypeError as e:
    print(type(e).__name__, e)
try:
    "abc"[::0]
except ValueError as e:
    print(type(e).__name__, e)
