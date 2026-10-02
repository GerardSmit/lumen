import itertools, json, struct, time

def t(f):
    try:
        print(f())
    except BaseException as e:
        print(type(e).__name__, e)

t(lambda: list(itertools.batched("abcdefg", 3, strict=True)))
t(lambda: list(itertools.batched("abcdef", 3, strict=True)))
t(lambda: list(itertools.batched("abcdefg", 3)))
t(lambda: itertools.batched.__text_signature__)
t(lambda: itertools.batched("abc", 2, True))

t(lambda: struct.pack("<F", 1))
t(lambda: struct.calcsize("<F"))
t(lambda: struct.calcsize("@bD"))
t(lambda: struct.calcsize("@bF"))
t(lambda: struct.pack("<2D", 1, 2j))
t(lambda: struct.unpack("<2D", struct.pack("<2D", 1.5, 2j)))
t(lambda: struct.unpack(">F", struct.pack(">F", 1 + 2j)))
t(lambda: struct.pack("<F", "x"))
t(lambda: struct.pack("<D", 1e308 + 1e308j))

t(lambda: json.dumps({"a": [1, 2.5, None], "b": {}, "c": [], "d": {"e": [1]}}, indent=1))
t(lambda: json.dumps([1, [2, 3]], indent="\t"))
t(lambda: json.dumps({"a": 1, "b": [1, 2]}, indent=2, separators=(",", ": "), sort_keys=True))
t(lambda: json.dumps([], indent=2))
t(lambda: json.dumps([1], indent=0))

import csv, zlib
t(lambda: list(csv.reader(['1,,"2",""'], quoting=csv.QUOTE_NOTNULL)))
t(lambda: list(csv.reader(['1,,"2",""'], quoting=csv.QUOTE_STRINGS)))
t(lambda: list(csv.reader(['a'], quoting=csv.QUOTE_STRINGS)))
t(lambda: list(csv.reader(['a,"b"'], quoting=csv.QUOTE_NOTNULL)))
t(lambda: hasattr(__import__("_csv"), "__version__"))
t(lambda: zlib.crc32.__text_signature__)
t(lambda: zlib.compressobj.__text_signature__)
t(lambda: zlib.compressobj().flush.__text_signature__)
