# Lone surrogates and U+10F000..U+10FFFF are all distinct code points.
import re


def show(f):
    try:
        r = f()
        return r.hex() if isinstance(r, bytes) else repr(r)
    except Exception as e:
        return type(e).__name__ + ':' + str(e)


cps = [0xD800, 0xD801, 0xDB7F, 0xDBFF, 0xDC00, 0xDC80, 0xDCFF, 0xDFFF,
       0x10F000, 0x10F7FF, 0x10F800, 0x10F801, 0x10FBFF, 0x10FC00, 0x10FFFE, 0x10FFFF]
for cp in cps:
    c = chr(cp)
    print(hex(cp), len(c), ord(c) == cp, repr(c), ascii(c),
          show(lambda: c.encode('utf-8')),
          show(lambda: c.encode('utf-8', 'surrogatepass')),
          show(lambda: c.encode('utf-16-le', 'surrogatepass')),
          show(lambda: c.encode('utf-32-be', 'surrogatepass')),
          show(lambda: c.encode('utf-8', 'surrogateescape')),
          show(lambda: c.encode('utf-8', 'surrogatepass').decode('utf-8', 'surrogatepass') == c),
          show(lambda: c.encode('utf-16-be', 'surrogatepass').decode('utf-16-be', 'surrogatepass') == c),
          show(lambda: c.encode('utf-8', 'surrogateescape').decode('utf-8', 'surrogateescape') == c))

every = [chr(cp) for cp in list(range(0xD800, 0xE000)) + list(range(0x10F000, 0x110000))]
print(len(set(every)), len(every), all(hash(c) == hash(chr(ord(c))) for c in every))
print(sorted(every) == every, all(a < b for a, b in zip(every, every[1:])))
d = {c: i for i, c in enumerate(every)}
print(all(d[c] == i for i, c in enumerate(every)))
for p in ['퟿', '', '￿', '\U00010000', '\U0010efff']:
    print(repr(p), [(p < c) for c in ('\ud800', '\udfff', '\U0010f7ff', '\U0010ffff')])

pair = '\udbff' + '\udfff'
print(len(pair), pair == chr(0x10FFFF), repr(pair), pair < chr(0x10FFFF), hash(pair) == hash(chr(0x10FFFF)))
print(show(lambda: pair.encode('utf-8')), show(lambda: pair.encode('utf-16-le', 'surrogatepass')))
print(show(lambda: b'\xed\xaf\xbf\xed\xbf\xbf'.decode('utf-8', 'surrogatepass')))
print(show(lambda: b'\xff\xdb\xff\xdf'.decode('utf-16-le', 'surrogatepass')))
print(show(lambda: b'\xf4\x8f\xbf\xbf'.decode('utf-8')), show(lambda: b'\xf4\x8f\x9f\xbf\x80'.decode('utf-8', 'surrogateescape')))

s = 'a' + chr(0x10FFFF) + '\ud800' + chr(0x10F800) + '\udfff' + 'z'
print(len(s), [hex(ord(c)) for c in s], repr(s[1]), repr(s[1:4]), repr(s[::-1]), repr(s[::2]))
print(s.find('\ud800'), s.find(chr(0x10F800)), s.find('\udfff'), s.count(chr(0x10FFFF)), s.index('z'))
print(repr(s.split('\ud800')), repr(s.replace(chr(0x10F800), '-')), '\udfff' in chr(0x10FFFF), chr(0x10F800) in s)
print(repr(s.upper()), s.isprintable(), repr(list(s)), repr(tuple(s)))
print(repr('%c%c' % (0x10FFFF, 0xD800)), repr('{:c}'.format(0x10F7FF)), repr(s.translate({0x10FFFF: 0xDBFF, 0xD800: None})))
print(repr(s.center(10, '*')), repr(s.strip('a' + chr(0x10FFFF))), repr(s.partition(chr(0x10F800))))

t = chr(0x10F800) + chr(0x10FFFF) + 'x' + chr(0x10FFFE) + chr(0x10F801)
print(repr(t.strip(chr(0x10F800) + chr(0x10F801))), repr(t.lstrip(chr(0x10FFFF) + chr(0x10F800))),
      repr(t.rstrip(chr(0x10FFFF) + chr(0x10F801))), repr(t.strip(t)))
F = chr(0x10FFFF)
print(repr('ab'.center(6, F)), repr('ab'.ljust(4, '\ud800')), repr(format('ab', F + '^6')), repr(format(5, F + '>4')))
print(repr('%-3s|' % F), repr(format(F, '>3')), repr(format(F + 'abc', '.2')))

print(re.findall('.', s) == list(s), re.search('\ud800', s).span(), re.search(chr(0x10F800), s).span())
print(re.search('[\udc00-\udfff]', s).span(), repr(re.sub('[\U0010f000-\U0010ffff]', '#', s)), repr(re.split('\ud800', s)))
print(re.match('a(.)(.)', s).groups() == (chr(0x10FFFF), '\ud800'), repr(re.sub('(.)', r'\1\1', s)))
