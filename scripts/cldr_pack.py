"""Emit a CLDR table as a packed Rust module (see crates/lumen/src/cldr_pack.rs).

A table of string tuples costs 16 bytes per `&str` field plus a base relocation for each in
the binary (~10x the text itself). Packed, every distinct string is stored once in a sorted
pool (`text` + `u32` end offsets) and each row is a fixed number of `u16` pool ids, so a
lookup resolves its key strings to ids by binary search and then compares integers.

Used by the gen-cldr-*.py generators; `python scripts/cldr_pack.py --repack` converts the
checked-in tuple-form tables (the older generator output) in place.
"""

import os
import re
import sys


def rust_str(s):
    return '"' + s.replace("\\", "\\\\").replace('"', '\\"') + '"'


def pool_lines(strings):
    """The `POOL` static: the sorted distinct strings, concatenated, with end offsets."""
    ends, n = [], 0
    for s in strings:
        n += len(s.encode("utf-8"))
        ends.append(n)
    out = ["use crate::cldr_pack::Pool;", "", "#[rustfmt::skip]", "static POOL: Pool = Pool {",
           "    text: concat!("]
    line = ""
    for s in strings:
        line += s
        if len(line) > 100:
            out.append(f"        {rust_str(line)},")
            line = ""
    if line:
        out.append(f"        {rust_str(line)},")
    out += ["    ),", "    ends: &["]
    for i in range(0, len(ends), 16):
        out.append("        " + ", ".join(map(str, ends[i:i + 16])) + ",")
    out += ["    ],", "};", ""]
    return out


def table_lines(name, rows, ids, int_fields=()):
    """A `[u16; rows * fields]` static: string fields as pool ids, `int_fields` as-is."""
    flat = []
    for r in rows:
        for i, v in enumerate(r):
            flat.append(int(v) if i in int_fields else ids[v])
    out = [f"/// {len(rows[0])} fields per row.", "#[rustfmt::skip]",
           f"static {name}: [u16; {len(flat)}] = ["]
    for i in range(0, len(flat), 20):
        out.append("    " + ", ".join(map(str, flat[i:i + 20])) + ",")
    out += ["];", ""]
    return out


def emit(header, tables, fns):
    """`tables`: (name, rows, int_fields): rows are tuples of strings, except the fields listed
    in `int_fields` (small ints stored as-is). All tables share one pool. `fns`: the accessors."""
    strings = sorted({v for _, rows, ints in tables for r in rows
                      for i, v in enumerate(r) if i not in ints})
    if len(strings) > 0xFFFF:
        raise SystemExit(f"{len(strings)} distinct strings: more than u16 ids")
    ids = {s: i for i, s in enumerate(strings)}
    out = list(header) + [""] + pool_lines(strings)
    for name, rows, ints in tables:
        out += table_lines(name, rows, ids, ints)
    out += fns
    return "\n".join(out) + "\n"


TUPLE = re.compile(r'^\s*\((.*)\),\s*$')
FIELD = re.compile(r'"([^"\\]*)"|(\d+)')


MONTH_FNS = [
    "/// The month name (1-based) for a locale/calendar/width, if shipped.",
    "pub fn month_name(loc: &str, cal: &str, width: &str, month: u8) -> Option<&'static str> {",
    "    let (l, c, w) = (POOL.id(loc)?, POOL.id(cal)?, POOL.id(width)?);",
    "    MONTH_ROWS",
    "        .chunks_exact(5)",
    "        .find(|r| r[0] == l && r[1] == c && r[2] == w && r[3] == month as u16)",
    "        .map(|r| POOL.get(r[4]))",
    "}",
    "",
    "/// The era name for a locale/calendar/width/era-key, if shipped.",
    "pub fn era_name(loc: &str, cal: &str, width: &str, era: &str) -> Option<&'static str> {",
    "    let (l, c, w, e) = (POOL.id(loc)?, POOL.id(cal)?, POOL.id(width)?, POOL.id(era)?);",
    "    ERA_ROWS",
    "        .chunks_exact(5)",
    "        .find(|r| r[0] == l && r[1] == c && r[2] == w && r[3] == e)",
    "        .map(|r| POOL.get(r[4]))",
    "}",
]

LIKELY_FNS = [
    "/// The maximized tag for a likelySubtags search key, if present.",
    "pub fn likely(key: &str) -> Option<&'static str> {",
    "    let k = POOL.id(key)?;",
    "    // Rows are sorted by key, and pool ids by string: the ids are sorted too.",
    "    let (mut lo, mut hi) = (0, ROWS.len() / 2);",
    "    while lo < hi {",
    "        let mid = (lo + hi) / 2;",
    "        match ROWS[mid * 2].cmp(&k) {",
    "            std::cmp::Ordering::Less => lo = mid + 1,",
    "            std::cmp::Ordering::Greater => hi = mid,",
    "            std::cmp::Ordering::Equal => return Some(POOL.get(ROWS[mid * 2 + 1])),",
    "        }",
    "    }",
    "    None",
    "}",
]

UNIT_FNS = [
    "/// The unit display pattern for a CLDR locale key, unit, style and plural category.",
    "pub fn unit_pattern(loc: &str, unit: &str, style: &str, plural: &str) -> Option<&'static str> {",
    "    let (l, u, s, p) = (POOL.id(loc)?, POOL.id(unit)?, POOL.id(style)?, POOL.id(plural)?);",
    "    ROWS.chunks_exact(5)",
    "        .find(|r| r[0] == l && r[1] == u && r[2] == s && r[3] == p)",
    "        .map(|r| POOL.get(r[4]))",
    "}",
]


def repack(src_dir):
    dates = os.path.join(src_dir, "cldr_dates.rs")
    text = open(dates, encoding="utf-8").read()
    if "static MONTHS:" in text:
        head, eras_part = text.split("static ERAS:")
        months = parse_rows_text(head)
        eras = parse_rows_text(eras_part)
        hdr = ["//! CLDR month/era names (packed by scripts/cldr_pack.py). DO NOT EDIT."]
        open(dates, "w", encoding="utf-8", newline="\n").write(
            emit(hdr, [("MONTH_ROWS", months, (3,)), ("ERA_ROWS", eras, ())], MONTH_FNS))
        print(f"cldr_dates: {len(months)} months, {len(eras)} eras", file=sys.stderr)
    likely = os.path.join(src_dir, "cldr_likely.rs")
    text = open(likely, encoding="utf-8").read()
    if "static LIKELY:" in text:
        rows = parse_rows_text(text)
        if [r[0] for r in rows] != sorted(r[0] for r in rows):
            raise SystemExit("likely rows are not sorted by key")
        hdr = ["//! CLDR likelySubtags table (packed by scripts/cldr_pack.py). DO NOT EDIT."]
        open(likely, "w", encoding="utf-8", newline="\n").write(emit(hdr, [("ROWS", rows, ())], LIKELY_FNS))
        print(f"cldr_likely: {len(rows)} rows", file=sys.stderr)
    units = os.path.join(src_dir, "cldr_units.rs")
    text = open(units, encoding="utf-8").read()
    if "static UNIT_PATTERNS:" in text:
        rows = parse_rows_text(text)
        write_units(rows, units)


def write_units(rows, dest):
    hdr = [
        "//! CLDR unit display patterns, generated by scripts/gen-cldr-units.py. DO NOT EDIT.",
        "//! `{0}` is the number placeholder.",
    ]
    open(dest, "w", encoding="utf-8", newline="\n").write(emit(hdr, [("ROWS", rows, ())], UNIT_FNS))
    print(f"cldr_units: {len(rows)} rows", file=sys.stderr)


def parse_rows_text(text):
    rows = []
    for ln in text.splitlines():
        m = TUPLE.match(ln)
        if m:
            rows.append(tuple(a if b == "" else b for a, b in FIELD.findall(m[1])))
    return rows


if __name__ == "__main__":
    if sys.argv[1:] == ["--repack"]:
        repack(os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                            "crates", "lumen", "src"))
    else:
        raise SystemExit("usage: cldr_pack.py --repack")
