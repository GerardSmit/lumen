import re

m = re.match(r"(\w+)@(\w+)\.com", "alice@example.com extra")
print(m.group(), m.group(1), m.group(2), m.groups(), m.span(), m.start(1), m.end(2), m.string)
print(re.match(r"\d+", "abc123"), re.match(r"\d+", "123abc").group(), re.match("", "x").span())
print(re.search(r"\d+", "abc123def456").group(), re.search(r"\d+", "abc123def456").span(), re.search("z", "abc"))
print(re.fullmatch(r"\d+", "12345") is not None, re.fullmatch(r"\d+", "123a"), re.fullmatch(r"a|ab", "ab").group())
print(re.findall(r"\d+", "a1 b22 c333"), re.findall(r"(\w)(\d)", "a1 b2 c3"), re.findall(r"x*", "axb"))
print(re.findall(r"(\d)(?:px|em)", "1px 2em 3pt"), re.findall("(a)|(b)", "ab"), re.findall(r"\bw\w*", "word wide not"))
print([m.group() for m in re.finditer(r"[aeiou]", "education")], [m.start() for m in re.finditer("ab", "ababab")])

print(re.sub(r"\d+", "#", "a1 b22 c333"), re.sub(r"(\w+) (\w+)", r"\2 \1", "hello world"))
print(re.sub(r"a", "b", "aaaa", count=2), re.sub(r"\s+", " ", "  many   spaces\there  "), re.sub("x*", "-", "abc"))
print(re.sub(r"\d+", lambda m: str(int(m.group()) * 2), "1 22 333"), re.sub(r"(?P<w>\w+)", r"<\g<w>>", "a b"))
print(re.subn(r"o", "0", "foo boo"), re.subn("z", "y", "abc"))
print(re.split(r"\s*,\s*", "a , b,c ,d"), re.split(r"(-)", "1-2-3"), re.split(r"\d", "a1b2c"), re.split(",", "a,b,c", maxsplit=1))
print(re.split(r"\W+", "Hello, world! How are you?"), re.split("x", ""))

named = re.match(r"(?P<year>\d{4})-(?P<month>\d{2})-(?P<day>\d{2})", "2024-03-15T10:00")
print(named.group("year"), named.group("month", "day"), named.groupdict(), named.lastgroup, named.lastindex)
print(named["day"], named.expand(r"\3/\2/\1"), named.re.groups, sorted(named.re.groupindex.items()))
opt = re.match(r"(a)?(b)?", "b")
print(opt.groups(), opt.group(1), opt.span(1), opt.groups("none"), opt.span(2))

pat = re.compile(r"(?P<key>[a-z]+)=(?P<val>\d+)")
print(pat.pattern, pat.findall("a=1, b=22, C=3"), pat.match("x=5").groupdict(), pat.search(" y=7").span())
print(pat.sub(lambda m: m.group("key").upper() + ":" + m.group("val"), "a=1 b=2"), pat.split("a=1,b=2"))
print([m.groupdict() for m in pat.finditer("p=1 q=2")], pat.match("=1"), pat.fullmatch("abc=12").group("val"))

print(re.findall(r"^\w+", "one\ntwo\nthree", re.M), re.findall(r"\w+$", "one\ntwo\nthree", re.MULTILINE))
print(re.match(r"HELLO", "hello", re.I).group(), re.match(r"a.b", "a\nb"), re.match(r"a.b", "a\nb", re.S).span())
print(re.findall(r"""
    (\d+)   # digits
    \s*     # space
    ([a-z]) # letter
""", "10 a 20 b", re.X), re.search(r"(?i)abc", "xABCx").group())
print(re.findall(r"a+?", "aaa"), re.findall(r"a{2}", "aaaaa"), re.findall(r"a{1,2}", "aaaaa"), re.match(r"<.+?>", "<a><b>").group())
print(re.findall(r"(?<=\$)\d+", "cost $15 and $20"), re.findall(r"\d+(?!px)", "12px 34em"), re.findall(r"(?<!a)b", "ab cb"))
print(re.match(r"(\w)\1", "aab").group(), re.match(r"(?P<c>\w)(?P=c)", "xxy").group(), re.findall(r"(\w)\1", "aabbcd"))
print(re.findall(r"[^\d\s]+", "ab 12 cd"), re.findall(r"[\w.-]+@[\w.-]+", "a.b@c-d.org, x@y.z"), re.findall(r"[a-c-]", "a-d"))
print(re.escape("a.b*c"), re.findall(r"\.", "a.b.c"), re.match(r"\\", "\\x").span())
print(re.findall(r"\d{3}-\d{4}", "555-1234 and 5555-12345"), re.findall(r"é\w", "éa éb"), re.match(r"\w+", "naïve café").group())
print(re.findall(r"\bcat\b", "cat concat cat."), re.findall(r"\Bcat", "cat concat"), re.search(r"\Aab", "abab") is not None)
print(re.search(r"b\Z", "ab\n"), re.search(r"b$", "ab\n").span(), re.findall(r"^", "a\nb"), len(re.findall("", "abc")))

try:
    re.compile("(unclosed")
except re.error:
    print("re.error unclosed")
try:
    re.compile("*bad")
except re.error:
    print("re.error nothing to repeat")
print(isinstance(re.compile("a"), re.Pattern), isinstance(re.match("a", "a"), re.Match), re.compile("a", re.I).flags & re.I != 0)
print(re.match("(a)(b)(c)", "abc").group(0, 2), re.match("(a)(b)(c)", "abc").regs)
