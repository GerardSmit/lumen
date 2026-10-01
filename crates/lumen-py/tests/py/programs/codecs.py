B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
B64_REV = {c: i for i, c in enumerate(B64)}


def b64encode(data):
    out = []
    for i in range(0, len(data), 3):
        chunk = data[i:i + 3]
        n = int.from_bytes(chunk + b"\x00" * (3 - len(chunk)), "big")
        chars = [B64[(n >> s) & 63] for s in (18, 12, 6, 0)]
        pad = 3 - len(chunk)
        if pad:
            chars[4 - pad:] = "=" * pad
        out.append("".join(chars))
    return "".join(out)


def b64decode(text):
    if len(text) % 4:
        raise ValueError("bad length")
    out = bytearray()
    for i in range(0, len(text), 4):
        quad = text[i:i + 4]
        pad = quad.count("=")
        n = 0
        for ch in quad.replace("=", "A"):
            if ch not in B64_REV:
                raise ValueError(f"bad char {ch!r}")
            n = (n << 6) | B64_REV[ch]
        out += n.to_bytes(3, "big")[:3 - pad]
    return bytes(out)


def rle_encode(s):
    if not s:
        return ""
    out = []
    prev, count = s[0], 1
    for ch in s[1:]:
        if ch == prev:
            count += 1
        else:
            out.append(f"{count}{prev}" if count > 1 else prev)
            prev, count = ch, 1
    out.append(f"{count}{prev}" if count > 1 else prev)
    return "".join(out)


def rle_decode(s):
    out = []
    num = ""
    for ch in s:
        if ch.isdigit():
            num += ch
        else:
            out.append(ch * int(num or 1))
            num = ""
    return "".join(out)


def lzw_compress(text):
    table = {chr(i): i for i in range(256)}
    nxt = 256
    w = ""
    out = []
    for ch in text:
        if w + ch in table:
            w += ch
        else:
            out.append(table[w])
            table[w + ch] = nxt
            nxt += 1
            w = ch
    if w:
        out.append(table[w])
    return out


def lzw_decompress(codes):
    table = {i: chr(i) for i in range(256)}
    nxt = 256
    w = table[codes[0]]
    out = [w]
    for k in codes[1:]:
        entry = table[k] if k in table else w + w[0]
        out.append(entry)
        table[nxt] = w + entry[0]
        nxt += 1
        w = entry
    return "".join(out)


def hexdump(data, width=8):
    lines = []
    for off in range(0, len(data), width):
        chunk = data[off:off + width]
        hexs = " ".join(f"{b:02x}" for b in chunk)
        asc = "".join(chr(b) if 32 <= b < 127 else "." for b in chunk)
        lines.append(f"{off:04x}  {hexs:<{width * 3 - 1}}  |{asc}|")
    return "\n".join(lines)


def crc32(data):
    crc = 0xFFFFFFFF
    for b in data:
        crc ^= b
        for _ in range(8):
            crc = (crc >> 1) ^ (0xEDB88320 if crc & 1 else 0)
    return crc ^ 0xFFFFFFFF


def caesar(s, k):
    def sh(c):
        if "a" <= c <= "z":
            return chr((ord(c) - 97 + k) % 26 + 97)
        if "A" <= c <= "Z":
            return chr((ord(c) - 65 + k) % 26 + 65)
        return c
    return "".join(map(sh, s))


for sample in [b"", b"f", b"fo", b"foo", b"foob", b"fooba", b"foobar", bytes(range(10)), b"Many hands make light work."]:
    enc = b64encode(sample)
    print(repr(sample), enc, b64decode(enc) == sample)

print(b64encode("héllo wörld".encode("utf-8")))
print(b64decode("aMOpbGxv").decode("utf-8"))
for bad in ["abc", "ab$d", "a==="]:
    try:
        print(b64decode(bad))
    except ValueError as ex:
        print("ValueError:", ex)

for s in ["", "a", "aaabccdddd", "abc", "zzzzzzzzzzzz", "aabbaabb"]:
    e = rle_encode(s)
    print(repr(s), repr(e), rle_decode(e) == s)

text = "TOBEORNOTTOBEORTOBEORNOT"
codes = lzw_compress(text)
print(codes, lzw_decompress(codes) == text)
long_text = "abracadabra " * 20
lc = lzw_compress(long_text)
print(len(long_text), len(lc), lzw_decompress(lc) == long_text)

data = "The quick brown fox\tjumps\n over 13 lazy dogs!".encode()
print(hexdump(data))
print(f"{crc32(b'123456789'):08x} {crc32(b''):08x} {crc32(data):08x}")
print(caesar("Hello, World!", 3), caesar(caesar("Hello, World!", 3), -3))
print(bytes([72, 105]), bytearray(b"abc") + b"d", b"abc"[1], b"abc"[1:], list(b"hi"))
print(b"a,b,,c".split(b","), b"  pad ".strip(), b"x".join([b"1", b"2", b"3"]))
print("é".encode(), "é".encode("utf-8").decode("latin-1"), len("日本語"), len("日本語".encode()))
print(int.from_bytes(b"\x01\x00", "little"), (258).to_bytes(2, "big"), (255).to_bytes(1, "big").hex())
print(bytes.fromhex("48656c6c6f"), b"Hello".hex())
