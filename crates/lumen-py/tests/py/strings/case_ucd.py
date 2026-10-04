# str predicates and case mappings follow CPython 3.12's character database (UCD 15.0.0),
# including full mappings, titlecase digraphs and the final-sigma rule.
import hashlib

methods = ["isalnum", "isalpha", "isdecimal", "isdigit", "islower", "isnumeric", "isspace",
           "istitle", "isupper", "isprintable", "isidentifier", "lower", "upper", "title",
           "casefold", "swapcase"]
for m in methods:
    h = hashlib.sha1()
    for cp in list(range(0x2000)) + list(range(0x1E900, 0x1E960)) + [0x1C89, 0x1C8A, 0xA7CB, 0x10D50]:
        h.update(repr(getattr(chr(cp), m)()).encode("utf-8", "surrogatepass"))
    print(m, h.hexdigest())

samples = ["ΑΣ", "ΑΣ'", "ΑΣ'Β", "Σ", "aΣ.", "ǆungla ǅ", "ﬃ straße İstanbul", "hello world's",
           "ΣΑΣ ΣΑ", "ʰΣ", "ß", "xͅΣ", "\ud800a", "ǅ", "Ǆa"]
for s in samples:
    print(repr(s), [getattr(s, m)() for m in ["lower", "upper", "title", "casefold", "capitalize",
                                               "swapcase", "istitle", "isupper", "islower"]])
print("a b\x1cc".split(), repr(" \x1c\x1d x\x85".strip()), " ".isprintable(), " ".isspace())
print("٣७".isdecimal(), "²".isdigit(), "²".isdecimal(), "Ⅷ".isnumeric(), "Ⅷ".isdigit(), "ª".islower())
