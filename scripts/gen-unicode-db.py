"""Generate crates/lumen-common/src/unicode_db.rs: the Unicode character database of a Python
interpreter's `unicodedata` module (UCD 15.0.0 for CPython 3.12).

Every property is read back from the reference interpreter, so the tables reproduce its
answers exactly: per-code-point records (category, bidirectional class, combining class,
mirrored, East Asian width, decimal/digit/numeric values), decomposition mappings, the
canonical composition pairs NFC uses, character names, and the `ucd_3_2_0` deltas.
Name aliases and named sequences are not enumerable through `unicodedata`; they come from the
UCD's NameAliases.txt and NamedSequences.txt and are checked against `unicodedata.lookup`.

The bulky tables go to unicode_db.bin (little-endian u16/u32 arrays), which unicode_db.rs
includes and addresses through `U16s`/`U32s` sections.

Usage: python3.12 scripts/gen-unicode-db.py
"""

import os
import re
import struct
import sys
import unicodedata as U
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "crates", "lumen-common", "src", "unicode_db.rs")
BLOB = os.path.join(ROOT, "crates", "lumen-common", "src", "unicode_db.bin")
SHIFT = 7
HANGUL = range(0xAC00, 0xD7A4)
CJK_PREFIX = "CJK UNIFIED IDEOGRAPH-"


def ucd_file(name):
    url = f"https://www.unicode.org/Public/{U.unidata_version}/ucd/{name}"
    with urllib.request.urlopen(url) as r:
        return r.read().decode("utf-8")


def ucd_lines(name):
    for line in ucd_file(name).splitlines():
        line = line.split("#", 1)[0].strip()
        if line:
            yield [f.strip() for f in line.split(";")]


def interned(table, index, value):
    if value not in index:
        index[value] = len(table)
        table.append(value)
    return index[value]


def two_level(values, default):
    """Split a 0x110000-entry list into (index1, index2) blocks of 1 << SHIFT, sharing blocks."""
    size = 1 << SHIFT
    blocks, seen, index1 = [], {}, []
    for start in range(0, 0x110000, size):
        block = tuple(values[start:start + size])
        if block not in seen:
            seen[block] = len(blocks)
            blocks.append(block)
        index1.append(seen[block])
    index2 = [v for b in blocks for v in b]
    return index1, index2


def rust_list(items, per_line=16):
    items = list(items)
    lines = []
    for i in range(0, len(items), per_line):
        lines.append("    " + ", ".join(items[i:i + per_line]) + ",")
    return "\n".join(lines)


def rust_str(s):
    return '"' + s.replace("\\", "\\\\").replace('"', '\\"') + '"'


def main():
    old = U.ucd_3_2_0
    categories, cat_ix = [], {}
    bidis, bidi_ix = [], {}
    widths, width_ix = [], {}
    numerics, num_ix = [], {}
    records, rec_ix = [], {}

    def record(db, ch):
        num = db.numeric(ch, None)
        return (
            interned(categories, cat_ix, db.category(ch)),
            interned(bidis, bidi_ix, db.bidirectional(ch)),
            db.combining(ch),
            db.mirrored(ch),
            interned(widths, width_ix, db.east_asian_width(ch)),
            db.decimal(ch, -1),
            db.digit(ch, -1),
            0xFFFF if num is None else interned(numerics, num_ix, num),
        )

    rec_of = []
    for cp in range(0x110000):
        rec_of.append(interned(records, rec_ix, record(U, chr(cp))))

    old_unassigned, old_changes, old_norm = [], [], []
    unassigned_old = None
    for cp in range(0x110000):
        ch = chr(cp)
        # `ucd_3_2_0.digit` answers from the current database; old records store no digit.
        cur = records[rec_of[cp]]
        r = record(old, ch)
        assert r[6] == cur[6], hex(cp)
        r = r[:6] + (-1,) + r[7:]
        if r == cur[:6] + (-1,) + cur[7:]:
            continue
        if old.category(ch) == "Cn":
            if unassigned_old is None:
                unassigned_old = r
            assert r == unassigned_old, hex(cp)
            if old_unassigned and old_unassigned[-1][1] == cp - 1:
                old_unassigned[-1][1] = cp
            else:
                old_unassigned.append([cp, cp])
        else:
            old_changes.append((cp, interned(records, rec_ix, r)))
    for cp in range(0x110000):
        ch = chr(cp)
        if old.category(ch) == "Cn" and U.category(ch) != "Cn":
            continue
        a, b = old.normalize("NFD", ch), U.normalize("NFD", ch)
        if a != b:
            assert len(a) == 1, hex(cp)
            old_norm.append((cp, ord(a)))
    unassigned_rec = interned(records, rec_ix, unassigned_old)

    # Decompositions: "<tag> XXXX YYYY" packed as [(prefix << 8) | count, cps...].
    prefixes, prefix_ix = [""], {"": 0}
    decomp_data, decomp_ix = [0], {}
    decomp_of = [0] * 0x110000
    for cp in range(0x110000):
        d = U.decomposition(chr(cp))
        if not d:
            continue
        parts = d.split()
        tag = parts.pop(0) if parts[0].startswith("<") else ""
        key = (interned(prefixes, prefix_ix, tag), tuple(int(p, 16) for p in parts))
        if key not in decomp_ix:
            decomp_ix[key] = len(decomp_data)
            decomp_data.append(key[0] << 8 | len(key[1]))
            decomp_data.extend(key[1])
        decomp_of[cp] = decomp_ix[key]
    assert len(decomp_data) < 0x10000

    compose = []
    for cp in range(0x110000):
        d = U.decomposition(chr(cp))
        if d and not d.startswith("<") and len(d.split()) == 2:
            a, b = (int(p, 16) for p in d.split())
            if U.normalize("NFC", chr(a) + chr(b)) == chr(cp):
                compose.append((a, b, cp))
    compose.sort()

    # Names: runs of "<PREFIX>-<hex code point>" and "<PREFIX>-<ordinal>" become ranges; the rest
    # are word sequences over a lexicon (words split at ' ' and '-').
    ranges = []
    explicit = []
    for cp in range(0x110000):
        if cp in HANGUL:
            continue
        name = U.name(chr(cp), None)
        if name is None:
            continue
        m = re.fullmatch(r"(.+-)([0-9A-F]{4,6})", name)
        if m and int(m.group(2), 16) == cp and m.group(2) == f"{cp:04X}":
            r = ranges[-1] if ranges else None
            if r and r[2] == m.group(1) and r[3] == 0 and r[1] == cp - 1:
                r[1] = cp
            else:
                ranges.append([cp, cp, m.group(1), 0, 0, 0])
            continue
        m = re.fullmatch(r"(.+-)(\d{3})", name)
        if m:
            n = int(m.group(2))
            r = ranges[-1] if ranges else None
            if r and r[2] == m.group(1) and r[3] == 1 and r[1] == cp - 1 and r[4] + (cp - r[0]) == n:
                r[1] = cp
                continue
            if not (r and r[2] == m.group(1)):
                ranges.append([cp, cp, m.group(1), 1, n, 3])
                continue
        explicit.append((cp, name))
    short = [r for r in ranges if r[1] - r[0] < 15]
    ranges = [r for r in ranges if r[1] - r[0] >= 15]
    for lo, hi, *_ in short:
        explicit.extend((cp, U.name(chr(cp))) for cp in range(lo, hi + 1))
    explicit.sort()
    for lo, hi, prefix, kind, start, width in ranges:
        for cp in range(lo, hi + 1):
            want = U.name(chr(cp))
            got = prefix + (f"{cp:04X}" if kind == 0 else f"{start + cp - lo:0{width}d}")
            assert want == got, (hex(cp), want, got)
        if prefix == CJK_PREFIX:
            assert kind == 0

    words, word_ix = [], {}
    freq = {}
    split = [re.split(r"([ -])", name) for _, name in explicit]
    for parts in split:
        for w in parts[0::2]:
            freq[w] = freq.get(w, 0) + 1
    for w in sorted(freq, key=lambda w: (-freq[w], w)):
        interned(words, word_ix, w)
    assert len(words) < 0x4000
    tokens, names = [], []
    for (cp, name), parts in zip(explicit, split):
        names.append((cp, len(tokens)))
        ws, seps = parts[0::2], parts[1::2]
        for i, w in enumerate(ws):
            t = word_ix[w]
            if i == len(ws) - 1:
                t |= 0x8000
            elif seps[i] == "-":
                t |= 0x4000
            tokens.append(t)
    by_name = sorted(range(len(explicit)), key=lambda i: explicit[i][1])
    assert len(explicit) < 0x10000

    aliases = []
    for cp, alias, _kind in ucd_lines("NameAliases.txt"):
        assert U.lookup(alias) == chr(int(cp, 16)), alias
        aliases.append((alias, int(cp, 16)))
    aliases.sort()
    sequences = []
    for name, cps in ucd_lines("NamedSequences.txt"):
        seq = [int(c, 16) for c in cps.split()]
        assert U.lookup(name) == "".join(map(chr, seq)), name
        sequences.append((name, seq))
    sequences.sort()

    idx1, idx2 = two_level(rec_of, 0)
    didx1, didx2 = two_level(decomp_of, 0)
    lexicon = "".join(words)
    offsets = [0]
    for w in words:
        offsets.append(offsets[-1] + len(w))

    blob = bytearray()
    out = []
    w = out.append

    def section(name, kind, values, doc):
        values = list(values)
        fmt = {"U16s": "<H", "U32s": "<I"}[kind]
        offset = len(blob)
        for v in values:
            blob.extend(struct.pack(fmt, v))
        w(f"/// {doc}")
        w(f"pub const {name}: {kind} = {kind}::at({offset}, {len(values)});")

    w(f"//! GENERATED by scripts/gen-unicode-db.py from Python's unicodedata (UCD {U.unidata_version}). Do not edit.")
    w("//!")
    w("//! Two-level tables are indexed as `INDEX2[(INDEX1[cp >> SHIFT] << SHIFT) + (cp & mask)]`.")
    w("")
    w('static BLOB: &[u8] = include_bytes!("unicode_db.bin");')
    w("")
    w("/// A little-endian `u16` array inside the data blob.")
    w("#[derive(Clone, Copy)]")
    w("pub struct U16s {")
    w("    offset: usize,")
    w("    pub len: usize,")
    w("}")
    w("")
    w("impl U16s {")
    w("    const fn at(offset: usize, len: usize) -> Self {")
    w("        U16s { offset, len }")
    w("    }")
    w("")
    w("    pub fn get(self, i: usize) -> u16 {")
    w("        let o = self.offset + 2 * i;")
    w("        u16::from_le_bytes([BLOB[o], BLOB[o + 1]])")
    w("    }")
    w("}")
    w("")
    w("/// A little-endian `u32` array inside the data blob.")
    w("#[derive(Clone, Copy)]")
    w("pub struct U32s {")
    w("    offset: usize,")
    w("    pub len: usize,")
    w("}")
    w("")
    w("impl U32s {")
    w("    const fn at(offset: usize, len: usize) -> Self {")
    w("        U32s { offset, len }")
    w("    }")
    w("")
    w("    pub fn get(self, i: usize) -> u32 {")
    w("        let o = self.offset + 4 * i;")
    w("        u32::from_le_bytes([BLOB[o], BLOB[o + 1], BLOB[o + 2], BLOB[o + 3]])")
    w("    }")
    w("}")
    w("")
    w(f'pub const UNIDATA_VERSION: &str = "{U.unidata_version}";')
    w(f"pub const SHIFT: u32 = {SHIFT};")
    w("")
    w(f"pub static CATEGORIES: &[&str] = &[{', '.join(map(rust_str, categories))}];")
    w(f"pub static BIDIRECTIONAL: &[&str] = &[{', '.join(map(rust_str, bidis))}];")
    w(f"pub static EAST_ASIAN_WIDTHS: &[&str] = &[{', '.join(map(rust_str, widths))}];")
    w(f"pub static NUMERIC: &[f64] = &[\n{rust_list((repr(float(n)) for n in numerics), 8)}\n];")
    w("")
    w("/// (category, bidirectional, combining, mirrored, east_asian_width, decimal, digit, numeric);")
    w("/// -1 is no decimal/digit value and `u16::MAX` no numeric value.")
    w("pub static RECORDS: &[(u8, u8, u8, u8, u8, i8, i8, u16)] = &[")
    w(rust_list((f"({', '.join(map(str, r))})" for r in records), 4))
    w("];")
    section("INDEX1", "U16s", idx1, "Record index, first level.")
    section("INDEX2", "U16s", idx2, "Record index, second level.")
    w("")
    w(f"pub static DECOMP_PREFIXES: &[&str] = &[{', '.join(map(rust_str, prefixes))}];")
    section("DECOMP_DATA", "U32s", decomp_data,
            "At each offset: `(prefix << 8) | count`, then `count` code points. Offset 0 is no mapping.")
    section("DECOMP_INDEX1", "U16s", didx1, "`DECOMP_DATA` offset, first level.")
    section("DECOMP_INDEX2", "U16s", didx2, "`DECOMP_DATA` offset, second level.")
    w("")
    w("/// (first, second, composite), sorted: the pairs canonical composition joins.")
    w(f"pub static COMPOSE: &[(u32, u32, u32)] = &[\n{rust_list((f'({a:#x}, {b:#x}, {c:#x})' for a, b, c in compose), 6)}\n];")
    w("")
    w("/// (first, last, prefix, kind, start, width): names `prefix` + the code point in hex (kind 0)")
    w("/// or + `start + (cp - first)` in `width` decimal digits (kind 1).")
    w("pub static NAME_RANGES: &[(u32, u32, &str, u8, u32, u8)] = &[")
    w(rust_list((f"({lo:#x}, {hi:#x}, {rust_str(p)}, {k}, {s}, {wd})" for lo, hi, p, k, s, wd in ranges), 1))
    w("];")
    w(f"pub static LEXICON: &str = {rust_str(lexicon)};")
    section("LEXICON_OFFSETS", "U32s", offsets, "Word `i` is `LEXICON[LEXICON_OFFSETS[i]..LEXICON_OFFSETS[i + 1]]`.")
    section("NAME_TOKENS", "U16s", tokens,
            "Word indices; bit 15 marks a name's last word, bit 14 a '-' (not ' ') after the word.")
    section("NAME_CODES", "U32s", (c for c, _ in names), "Code points with a stored name, ascending.")
    section("NAME_STARTS", "U32s", (t for _, t in names), "The first `NAME_TOKENS` entry of each `NAME_CODES` name.")
    section("NAMES_BY_NAME", "U16s", by_name, "Indices into `NAME_CODES`, sorted by name.")
    w(f"pub static ALIASES: &[(&str, u32)] = &[\n{rust_list((f'({rust_str(a)}, {c:#x})' for a, c in aliases), 1)}\n];")
    w("pub static NAMED_SEQUENCES: &[(&str, &[u32])] = &[")
    w(rust_list((f"({rust_str(n)}, &[{', '.join(hex(c) for c in s)}])" for n, s in sequences), 1))
    w("];")
    w("")
    w("/// UCD 3.2.0: ranges of code points it leaves unassigned, which read as `OLD_UNASSIGNED_RECORD`.")
    w(f"pub static OLD_UNASSIGNED: &[(u32, u32)] = &[\n{rust_list((f'({lo:#x}, {hi:#x})' for lo, hi in old_unassigned), 6)}\n];")
    w(f"pub const OLD_UNASSIGNED_RECORD: u16 = {unassigned_rec};")
    w("/// UCD 3.2.0: (code point, record) where its properties differ; `digit` is never stored.")
    w(f"pub static OLD_CHANGES: &[(u32, u16)] = &[\n{rust_list((f'({c:#x}, {r})' for c, r in old_changes), 8)}\n];")
    w("/// UCD 3.2.0: singleton decompositions that differ, applied before the current mappings.")
    w(f"pub static OLD_NORMALIZATION: &[(u32, u32)] = &[\n{rust_list((f'({a:#x}, {b:#x})' for a, b in old_norm), 6)}\n];")
    assert len(records) < 0x10000
    with open(OUT, "w") as f:
        f.write("\n".join(out) + "\n")
    with open(BLOB, "wb") as f:
        f.write(blob)
    print(f"{OUT}: {len(records)} records, {len(explicit)} names, {len(words)} words, "
          f"{len(ranges)} name ranges, {len(compose)} compositions, {len(old_changes)} 3.2.0 changes")


if __name__ == "__main__":
    if U.unidata_version != "15.0.0":
        sys.exit(f"needs CPython 3.12 (UCD 15.0.0), not UCD {U.unidata_version}")
    main()
