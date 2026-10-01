class JSONError(Exception):
    pass


ESCAPES = {'"': '"', "\\": "\\", "/": "/", "b": "\b", "f": "\f", "n": "\n", "r": "\r", "t": "\t"}
REV = {'"': '\\"', "\\": "\\\\", "\b": "\\b", "\f": "\\f", "\n": "\\n", "\r": "\\r", "\t": "\\t"}


def encode_str(s, ensure_ascii=True):
    out = ['"']
    for ch in s:
        if ch in REV:
            out.append(REV[ch])
        elif ord(ch) < 0x20:
            out.append("\\u%04x" % ord(ch))
        elif ensure_ascii and ord(ch) > 126:
            cp = ord(ch)
            if cp > 0xFFFF:
                cp -= 0x10000
                out.append("\\u%04x\\u%04x" % (0xD800 + (cp >> 10), 0xDC00 + (cp & 0x3FF)))
            else:
                out.append("\\u%04x" % cp)
        else:
            out.append(ch)
    out.append('"')
    return "".join(out)


def encode(obj, indent=None, level=0, sort_keys=False):
    if obj is None:
        return "null"
    if obj is True:
        return "true"
    if obj is False:
        return "false"
    if isinstance(obj, int):
        return str(obj)
    if isinstance(obj, float):
        if obj != obj or obj in (float("inf"), float("-inf")):
            raise JSONError("cannot encode non-finite float")
        return repr(obj)
    if isinstance(obj, str):
        return encode_str(obj)
    nl = "" if indent is None else "\n" + " " * (indent * (level + 1))
    end = "" if indent is None else "\n" + " " * (indent * level)
    sep = ", " if indent is None else ","
    if isinstance(obj, (list, tuple)):
        if not obj:
            return "[]"
        items = [encode(x, indent, level + 1, sort_keys) for x in obj]
        return "[" + nl + (sep + nl).join(items) + end + "]"
    if isinstance(obj, dict):
        if not obj:
            return "{}"
        keys = list(obj)
        if sort_keys:
            keys.sort()
        colon = ": "
        items = []
        for k in keys:
            if not isinstance(k, str):
                raise JSONError(f"keys must be strings, got {type(k).__name__}")
            items.append(encode_str(k) + colon + encode(obj[k], indent, level + 1, sort_keys))
        return "{" + nl + (sep + nl).join(items) + end + "}"
    raise JSONError(f"cannot encode {type(obj).__name__}")


class Decoder:
    def __init__(self, text):
        self.s = text
        self.i = 0

    def error(self, msg):
        line = self.s.count("\n", 0, self.i) + 1
        raise JSONError(f"{msg} (pos {self.i}, line {line})")

    def ws(self):
        while self.i < len(self.s) and self.s[self.i] in " \t\r\n":
            self.i += 1

    def decode(self):
        self.ws()
        v = self.value()
        self.ws()
        if self.i != len(self.s):
            self.error("extra data")
        return v

    def value(self):
        if self.i >= len(self.s):
            self.error("unexpected end")
        c = self.s[self.i]
        if c == "{":
            return self.obj()
        if c == "[":
            return self.arr()
        if c == '"':
            return self.string()
        for word, val in (("true", True), ("false", False), ("null", None)):
            if self.s.startswith(word, self.i):
                self.i += len(word)
                return val
        if c == "-" or c.isdigit():
            return self.number()
        self.error(f"unexpected {c!r}")

    def number(self):
        start = self.i
        if self.s[self.i] == "-":
            self.i += 1
        digits_start = self.i
        while self.i < len(self.s) and self.s[self.i].isdigit():
            self.i += 1
        if self.i == digits_start:
            self.error("bad number")
        if self.s[digits_start] == "0" and self.i - digits_start > 1:
            self.error("leading zero")
        is_float = False
        if self.i < len(self.s) and self.s[self.i] == ".":
            is_float = True
            self.i += 1
            fs = self.i
            while self.i < len(self.s) and self.s[self.i].isdigit():
                self.i += 1
            if fs == self.i:
                self.error("bad fraction")
        if self.i < len(self.s) and self.s[self.i] in "eE":
            is_float = True
            self.i += 1
            if self.i < len(self.s) and self.s[self.i] in "+-":
                self.i += 1
            es = self.i
            while self.i < len(self.s) and self.s[self.i].isdigit():
                self.i += 1
            if es == self.i:
                self.error("bad exponent")
        text = self.s[start:self.i]
        return float(text) if is_float else int(text)

    def string(self):
        self.i += 1
        out = []
        while True:
            if self.i >= len(self.s):
                self.error("unterminated string")
            c = self.s[self.i]
            if c == '"':
                self.i += 1
                return "".join(out)
            if c == "\\":
                self.i += 1
                if self.i >= len(self.s):
                    self.error("bad escape")
                e = self.s[self.i]
                if e == "u":
                    hexs = self.s[self.i + 1:self.i + 5]
                    if len(hexs) != 4 or any(h not in "0123456789abcdefABCDEF" for h in hexs):
                        self.error("bad unicode escape")
                    cp = int(hexs, 16)
                    self.i += 5
                    if 0xD800 <= cp < 0xDC00 and self.s.startswith("\\u", self.i):
                        lo = int(self.s[self.i + 2:self.i + 6], 16)
                        if 0xDC00 <= lo < 0xE000:
                            cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00)
                            self.i += 6
                    out.append(chr(cp))
                    continue
                if e not in ESCAPES:
                    self.error(f"bad escape \\{e}")
                out.append(ESCAPES[e])
                self.i += 1
            elif ord(c) < 0x20:
                self.error("control character in string")
            else:
                out.append(c)
                self.i += 1

    def arr(self):
        self.i += 1
        out = []
        self.ws()
        if self.s[self.i:self.i + 1] == "]":
            self.i += 1
            return out
        while True:
            self.ws()
            out.append(self.value())
            self.ws()
            c = self.s[self.i:self.i + 1]
            self.i += 1
            if c == "]":
                return out
            if c != ",":
                self.i -= 1
                self.error("expected , or ]")

    def obj(self):
        self.i += 1
        out = {}
        self.ws()
        if self.s[self.i:self.i + 1] == "}":
            self.i += 1
            return out
        while True:
            self.ws()
            if self.s[self.i:self.i + 1] != '"':
                self.error("expected string key")
            k = self.string()
            self.ws()
            if self.s[self.i:self.i + 1] != ":":
                self.error("expected :")
            self.i += 1
            self.ws()
            out[k] = self.value()
            self.ws()
            c = self.s[self.i:self.i + 1]
            self.i += 1
            if c == "}":
                return out
            if c != ",":
                self.i -= 1
                self.error("expected , or }")


def decode(text):
    return Decoder(text).decode()


doc = {
    "name": "Widget \"Pro\"",
    "tags": ["a", "b\\c", "tab\there", "nl\nx"],
    "price": 12.5,
    "stock": 0,
    "ratio": 1e-7,
    "big": 12345678901234567890,
    "nested": {"deep": [[1, 2], [3, [4, [5]]], {}], "empty": []},
    "unicode": "café 中文 \U0001F600",
    "flags": [True, False, None],
}
compact = encode(doc)
print(compact)
print(encode(doc, indent=2, sort_keys=True))
back = decode(compact)
print("roundtrip equal:", back == doc)
print("pretty roundtrip:", decode(encode(doc, indent=4)) == doc)
print(decode('"\\ud83d\\ude00 \\u00e9 \\/"') == "\U0001F600 é /")

good = ['  [1, 2.5, -3, 1e3, 2E-2, -0.5]  ', '{"a": {"b": {"c": []}}}', '""', '0', '-0', 'true', ' null ', '[[],[[]]]']
for g in good:
    print(repr(g), "->", repr(decode(g)))

bad = ['', '[1,', '[1 2]', '{"a" 1}', '{a: 1}', '"abc', '01', '1.', '-', '[1,]', '{"a":1,}',
       'tru', '"\\x"', '"\\u12"', '[1] x', '"a\nb"', '1e', '{"a":}']
for b in bad:
    try:
        decode(b)
        print(repr(b), "unexpectedly ok")
    except JSONError as ex:
        print(repr(b), "error:", ex)

for obj in [{1: 2}, {"a": {1, 2}}, float("nan"), object]:
    try:
        encode(obj)
    except JSONError as ex:
        print("encode error:", ex)

print(encode_str("é", ensure_ascii=False), encode_str("é"))
print(decode(encode([1.0, 2.5e20, 1e-5, 123456789.125])))
