TEXT = """It was the best of times, it was the worst of times, it was the age of
wisdom, it was the age of foolishness, it was the epoch of belief, it was the
epoch of incredulity, it was the season of Light, it was the season of Darkness,
it was the spring of hope, it was the winter of despair. We had everything
before us, we had nothing before us; we were all going direct to Heaven, we
were all going direct the other way."""

STOP = {"it", "was", "the", "of", "we", "had", "were", "all", "us", "to"}


def words(text):
    out = []
    cur = []
    for ch in text.lower():
        if ch.isalpha() or ch == "'":
            cur.append(ch)
        elif cur:
            out.append("".join(cur))
            cur = []
    if cur:
        out.append("".join(cur))
    return out


def frequencies(ws):
    freq = {}
    for w in ws:
        freq[w] = freq.get(w, 0) + 1
    return freq


def top(freq, n, skip=()):
    items = [(w, c) for w, c in freq.items() if w not in skip]
    items.sort(key=lambda p: (-p[1], p[0]))
    return items[:n]


def wrap(text, width):
    lines = []
    cur = ""
    for w in text.split():
        if cur and len(cur) + 1 + len(w) > width:
            lines.append(cur)
            cur = w
        else:
            cur = (cur + " " + w) if cur else w
    if cur:
        lines.append(cur)
    return lines


def justify_line(line, width):
    ws = line.split(" ")
    if len(ws) == 1:
        return line.ljust(width)
    total = sum(len(w) for w in ws)
    gaps = len(ws) - 1
    spaces, extra = divmod(width - total, gaps)
    out = []
    for i, w in enumerate(ws):
        out.append(w)
        if i < gaps:
            out.append(" " * (spaces + (1 if i < extra else 0)))
    return "".join(out)


def justify(text, width):
    lines = wrap(text, width)
    res = [justify_line(l, width) for l in lines[:-1]]
    res.append(lines[-1].ljust(width))
    return res


def caesar(text, shift):
    out = []
    for ch in text:
        if "a" <= ch <= "z":
            out.append(chr((ord(ch) - 97 + shift) % 26 + 97))
        elif "A" <= ch <= "Z":
            out.append(chr((ord(ch) - 65 + shift) % 26 + 65))
        else:
            out.append(ch)
    return "".join(out)


def is_palindrome(s):
    cleaned = [c.lower() for c in s if c.isalnum()]
    return cleaned == cleaned[::-1]


def anagram_groups(ws):
    groups = {}
    for w in ws:
        groups.setdefault("".join(sorted(w)), []).append(w)
    result = [sorted(set(g)) for g in groups.values()]
    result = [g for g in result if len(g) > 1]
    result.sort(key=lambda g: (-len(g), g))
    return result


def main():
    ws = words(TEXT)
    print(len(ws), "words,", len(set(ws)), "unique")
    freq = frequencies(ws)
    for w, c in top(freq, 8):
        print("%-12s %3d %s" % (w, c, "#" * c))
    print("without stop words:")
    for w, c in top(freq, 6, STOP):
        print("  %s=%d" % (w, c))
    lengths = {}
    for w in ws:
        lengths.setdefault(len(w), set()).add(w)
    for n in sorted(lengths):
        print(n, sorted(lengths[n]))
    longest = max(ws, key=lambda w: (len(w), w))
    print("longest:", longest, "avg len: %.3f" % (sum(map(len, ws)) / len(ws)))
    letters = frequencies([c for c in TEXT.lower() if c.isalpha()])
    print("".join(c for c, _ in top(letters, 26)))

    print("--- wrap 30")
    for l in wrap(TEXT, 30)[:5]:
        print("|" + l.ljust(30) + "|")
    print("--- justify 38")
    for l in justify(" ".join(TEXT.split()[:30]), 38):
        print("|" + l + "|")

    msg = "Hello, World! The quick brown fox."
    enc = caesar(msg, 3)
    print(enc, caesar(enc, -3) == msg, caesar(msg, 26) == msg, caesar("xyz XYZ", 4))
    print(caesar(caesar("Round Trip", 13), 13))

    for s in ["A man, a plan, a canal: Panama", "No lemon, no melon", "Hello", "", "Was it a car or a cat I saw?"]:
        print(repr(s), is_palindrome(s))

    cand = ["listen", "silent", "enlist", "google", "gogole", "cat", "act", "tac", "dog", "god", "odg", "inlets", "x"]
    for g in anagram_groups(cand):
        print(g)
    print(anagram_groups(words(TEXT)))
    pal_words = sorted(set(w for w in ws if len(w) > 1 and w == w[::-1]))
    print(pal_words)
    print(sorted(set(w.capitalize() for w in ws if w.startswith("w"))))


main()
