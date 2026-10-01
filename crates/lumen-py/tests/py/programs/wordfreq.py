TEXT = """
It was the best of times, it was the worst of times, it was the age of wisdom,
it was the age of foolishness, it was the epoch of belief, it was the epoch of
incredulity, it was the season of Light, it was the season of Darkness, it was
the spring of hope, it was the winter of despair, we had everything before us,
we had nothing before us, we were all going direct to Heaven, we were all going
direct the other way -- in short, the period was so far like the present period,
that some of its noisiest authorities insisted on its being received, for good or
for evil, in the superlative degree of comparison only.

There were a king with a large jaw and a queen with a plain face, on the throne
of England; there were a king with a large jaw and a queen with a fair face, on
the throne of France. In both countries it was clearer than crystal to the lords
of the State preserves of loaves and fishes, that things in general were settled
for ever.
"""

STOP = {"a", "the", "of", "it", "was", "in", "on", "to", "we", "and", "with", "for", "its", "or", "so", "that"}


def words(text):
    cleaned = []
    for ch in text.lower():
        cleaned.append(ch if ch.isalpha() or ch == "'" else " ")
    return "".join(cleaned).split()


def count(ws):
    freq = {}
    for w in ws:
        freq[w] = freq.get(w, 0) + 1
    return freq


def top(freq, n):
    return sorted(freq.items(), key=lambda kv: (-kv[1], kv[0]))[:n]


ws = words(TEXT)
freq = count(ws)
print("total words:", len(ws), "unique:", len(freq))
print("longest:", max(sorted(freq), key=lambda w: (len(w), w)))
print("top 10 overall:")
for w, c in top(freq, 10):
    print(f"  {w:<12}{c:>3} {'#' * c}")
content = {w: c for w, c in freq.items() if w not in STOP}
print("top 10 content:", top(content, 10))

by_len = {}
for w in freq:
    by_len.setdefault(len(w), []).append(w)
for n in sorted(by_len):
    ws_n = sorted(by_len[n])
    shown = ", ".join(ws_n[:5]) + (f", ... (+{len(ws_n) - 5})" if len(ws_n) > 5 else "")
    print(f"len {n:2}: {len(ws_n):3} words: {shown}")

bigrams = count(list(zip(ws, ws[1:])))
print("top bigrams:", [(" ".join(k), v) for k, v in top(bigrams, 6)])
trigrams = count(list(zip(ws, ws[1:], ws[2:])))
print("top trigram:", [(" ".join(k), v) for k, v in top(trigrams, 3)])

letters = count([ch for w in ws for ch in w])
print("letters:", "".join(f"{k}{v} " for k, v in top(letters, 8)))
print("vowel ratio: %.3f" % (sum(v for k, v in letters.items() if k in "aeiou") / sum(letters.values())))

first_pos = {}
for i, w in enumerate(ws):
    first_pos.setdefault(w, i)
print("first seen order:", sorted(first_pos, key=first_pos.get)[:12])
last_pos = {w: i for i, w in enumerate(ws)}
print("longest span:", max(freq, key=lambda w: (last_pos[w] - first_pos[w], w)))

sentences = [s.strip() for s in TEXT.replace("\n", " ").replace(";", ".").split(".") if s.strip()]
print(len(sentences), [len(words(s)) for s in sentences])
avg = sum(len(w) for w in ws) / len(ws)
print(f"avg word length {avg:.3f}")

lines = TEXT.strip().splitlines()
widest = max(lines, key=len)
print(len(lines), len(widest), repr(widest[:30]))
print("caps:", sorted({w for w in TEXT.replace(",", " ").split() if w[0].isupper()}))
anagram_groups = {}
for w in freq:
    anagram_groups.setdefault("".join(sorted(w)), []).append(w)
print(sorted(sorted(g) for g in anagram_groups.values() if len(g) > 1))
hist = count([c for c in freq.values()])
for c in sorted(hist):
    print(f"{c:3} -> {hist[c]:3} words")
rank = {w: i + 1 for i, (w, _) in enumerate(top(freq, len(freq)))}
print([(w, rank[w]) for w in ("the", "was", "king", "queen", "fishes")])
print(" ".join(w.capitalize() for w in ("lord", "of", "the", "rings")), "title".center(11, "*"), "x".join(["a", "b"]))
