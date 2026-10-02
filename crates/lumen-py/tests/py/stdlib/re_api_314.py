import copy
import pickle
import re
import _sre


def show(label, f):
    try:
        print(label, repr(f()))
    except Exception as e:
        print(label, 'EXC', type(e).__name__, e)


print(re.NOFLAG, int(re.NOFLAG), re.IGNORECASE | re.NOFLAG)
print(re.Pattern[str], re.Match[bytes])
print(re.PatternError is re.error, re.PatternError.__mro__[1].__name__)
show('pattern error attrs', lambda: re.compile('(a'))
try:
    re.compile('a(b')
except re.PatternError as e:
    print(e.msg, e.pattern, e.pos, e.lineno, e.colno)

# empty charsets, any-all sets and possessive/atomic constructs
for p, s in [('[^\\s\\S]', 'ab'), ('[\\s\\S]+', 'a\nb'), ('[\\S\\s]', '\n'), ('(?:[^\\s\\S]|a)b', 'ab'),
             ('a*+a', 'aaa'), ('(?>a*)a', 'aaa'), ('a++b', 'aaab'), ('(a|ab)*+c', 'abac'),
             ('(?>(a+))b', 'aab'), ('x?+x', 'x'), ('a{2,3}+a', 'aaaa'), ('(?i:A)b', 'aB'), ('(?i:A)b', 'ab'),
             ('(?-i:a)B', 'AB'), ('(?s:.)', '\n'), ('(?a:\\w)', '\u00e9'), ('(?u:\\w)', '\u00e9'),
             ('\\B', ''), ('\\b', ''), ('\\B', 'a'), ('a\\B', 'a'), ('\\B$', 'a '), ('\\Ba', 'a'), (r'(?a)\B', '')]:
    show('%s %r' % (p, s), lambda: [(m.span(), m.group()) for m in re.finditer(p, s)])
show('empty at pos', lambda: re.compile('\\B').search('abc', 3, 3))
show('empty at endpos', lambda: re.compile('\\B').match('abc', 0, 0))
show('bytes \\B empty', lambda: re.compile(b'\\B').match(b''))

# templates
text = 'abc def'
pat = re.compile(r'(?P<w>\w)(\w+)(x)?')
for t in [r'\g<0>', r'\g<w>', r'\g<1>-\2', r'\1\3\2', r'\\', r'\n', r'\t\a\b\f\v\r', r'\0', r'\01', r'\x41', r'\g<w', r'\g<>', r'\g<3>',
          r'\g<4>', r'\g<-1>', r'\4', r'\g<nope>', r'\q', r'\u1234', r'\8', r'\1' * 3, '', 'plain', r'\g< w>', r'\g<1x>', r'\18', r'\400', r'\777', '\\\n']:
    show('sub %r' % t, lambda: pat.sub(t, text))
    show('expand %r' % t, lambda: pat.match(text).expand(t))
show('sub bytes', lambda: re.sub(b'(a)', b'[\\1]', b'banana'))
show('sub bytes mixed', lambda: re.sub(b'(a)', '[\\1]', b'banana'))
show('sub str mixed', lambda: re.sub('(a)', b'[\\1]', 'banana'))
show('sub callable', lambda: re.sub('a', lambda m: m.group().upper(), 'banana', count=2))
show('sub callable bad', lambda: re.sub('a', lambda m: 5, 'banana'))
show('sub empty', lambda: re.sub('x*', '-', 'abxd'))
show('sub empty 2', lambda: re.sub('(?:)', '-', 'ab'))
show('subn', lambda: re.subn('a|(b)', r'<\1>', 'abab'))
show('sub count kw', lambda: pat.sub('Z', text, count=1))
show('sub count pos', lambda: pat.sub('Z', text, 1))
show('sub unmatched group', lambda: re.sub('(a)|(b)', r'[\2]', 'ab'))
show('split maxsplit', lambda: re.compile('(,)|;').split('a,b;c,d', maxsplit=2))
show('split empty', lambda: re.split('x*', 'axbc'))
show('split pos maxsplit', lambda: re.compile(',').split('a,b,c', 1))

# Pattern / Match API
p = re.compile(r'(?P<a>\d)(\d)?(?P<c>x)?')
m = p.search('z1 23')
print(m, m.group(), m.group(1, 2), m.group('a'), m[0], m['a'], m.groups(), m.groups('d'), m.groupdict(), m.groupdict('d'))
print(m.start(), m.end(), m.span(), m.start('a'), m.end(2), m.span('c'), m.lastindex, m.lastgroup, m.regs, m.pos, m.endpos, m.re is p, m.string)
show('bad group', lambda: m.group(5))
show('bad group name', lambda: m.group('zz'))
show('bad getitem', lambda: m[5])
show('group float', lambda: m.group(1.0))
show('group big', lambda: m.group(2**70))
show('start neg', lambda: m.start(-1))
show('span bool', lambda: m.span(True))
show('copy match', lambda: copy.copy(m) is m)
show('deepcopy match', lambda: copy.deepcopy(m) is m)
show('copy pattern', lambda: copy.copy(p) is p)
show('deepcopy pattern', lambda: copy.deepcopy(p) is p)
show('pickle pattern', lambda: pickle.loads(pickle.dumps(p)) == p)
show('pickle match', lambda: pickle.dumps(m))
show('hash eq', lambda: (hash(p) == hash(re.compile(p.pattern, p.flags)), p == re.compile(p.pattern, p.flags), p == re.compile(p.pattern, re.I), p != 1))
show('lt', lambda: p < p)
print(repr(p), repr(re.compile('a', re.I | re.X)), repr(re.compile(b'a', re.A)), repr(re.compile('a' * 100)), repr(re.compile('\n')))
print(repr(m), repr(p.match('1x')), repr(re.compile('').match('')))
print(p.pattern, p.flags, p.groups, p.groupindex, dict(p.groupindex))
show('groupindex immutable', lambda: p.groupindex.__setitem__('x', 1))
for s, pos, endpos in [('abcabc', 0, 6), ('abcabc', 2, 5), ('abcabc', -5, 100), ('abcabc', 4, 2), ('abcabc', 7, 9), ('abc', None, None)]:
    show('findall %r %r %r' % (s, pos, endpos), lambda: re.compile('b|c').findall(s, *([pos, endpos] if pos is not None else [])))
    show('search %r %r %r' % (s, pos, endpos), lambda: re.compile('^b|c$').search(s, *([pos, endpos] if pos is not None else [])))
show('search kw', lambda: re.compile('b').search(string='abc', pos=1, endpos=3))
show('search no args', lambda: re.compile('b').search())
show('search bad type', lambda: re.compile('b').search(5))
show('search bytes on str pattern', lambda: re.compile('b').search(b'abc'))
show('search str on bytes pattern', lambda: re.compile(b'b').search('abc'))
show('search bytearray', lambda: re.compile(b'b').search(bytearray(b'abc')))
show('search memoryview', lambda: re.compile(b'b').search(memoryview(b'abc')))
show('pos float', lambda: re.compile('b').search('abc', 1.5))
show('pos huge', lambda: re.compile('b').search('abc', 2**70))
show('findall bad', lambda: re.compile('b').findall('abc', 'x'))

# scanner
sc = re.compile(r'(\d)|(\w)').scanner('1a 2')
print([sc.match(), sc.match(), sc.match(), sc.search(), sc.search()])
show('scanner pattern', lambda: sc.pattern is not None)
sc = re.compile('x*').scanner('abc')
print([sc.search(), sc.search(), sc.search(), sc.search(), sc.search()])
sc = re.compile('a|').scanner('ba', 1)
print([sc.match(), sc.match(), sc.match()])
show('scanner type', lambda: type(sc).__name__)
show('scanner new', lambda: type(sc)())
show('finditer type', lambda: type(re.finditer('a', 'a')).__name__)
it = re.finditer('a', 'aa')
print(next(it), next(it))
show('finditer end', lambda: next(it))

# module level helpers
show('module sub flags pos', lambda: re.sub('a', 'b', 'aA', flags=re.I))
show('module split', lambda: re.split(r'\W+', 'Words, words, words.'))
show('module split capture', lambda: re.split(r'(\W+)', 'Words, words.'))
show('escape', lambda: re.escape('a.b*c\n\x00-_ é'))
show('escape bytes', lambda: re.escape(b'a.b\xff'))
show('purge', lambda: re.purge())
show('fullmatch', lambda: re.fullmatch('a|ab', 'ab').span())
show('compile pattern flags', lambda: re.compile(re.compile('a'), re.I))
show('compile bad type', lambda: re.compile(5))
show('compile str flags bytes', lambda: re.compile(b'a', re.U))
show('compile ascii unicode', lambda: re.compile('a', re.A | re.U))
show('compile locale str', lambda: re.compile('a', re.L))
show('compile locale ascii', lambda: re.compile(b'a', re.L | re.A))
show('flags repr', lambda: re.I | re.M)
show('flags all', lambda: list(re.RegexFlag))
show('flags value', lambda: re.compile('a', re.I | re.M | re.S | re.X | re.A).flags)
show('flags debug', lambda: re.compile('a', 0).flags)
show('template flag', lambda: hasattr(re, 'TEMPLATE') or hasattr(re, 'T'))
show('sre flag template', lambda: hasattr(_sre, 'SRE_FLAG_TEMPLATE'))

# compiled code shapes of the 3.14 compiler
import re._compiler as _c
for pat in ['[^\\s\\S]', '[\\s\\S]', '[]', '[^]', '[\\d\\D]', 'a|b', '(?:)', 'a++', '(?>a)', '(?i:a)', '[a-c]', '[^a]', '\\B', 'x{2,5}', '(?P<n>a)(?(n)b|c)']:
    show('code %s' % pat, lambda: _c._code(re._parser.parse(pat), 0))
    show('compile code %s' % pat, lambda: [m for m in [re.compile(pat).match('a')]] and 1)

# _sre.template & compile argument shapes
show('_sre.template', lambda: type(_sre.template('a', ['x', 1, 'y'])).__name__)
show('_sre.template empty', lambda: type(_sre.template('a', [])).__name__)
show('_sre.template bad', lambda: _sre.template('a', 5))
show('_sre.template odd', lambda: _sre.template('a', ['x', 1]))
show('_sre.compile bad code', lambda: _sre.compile('a', 0, [1, 2, 3], 0, {}, ()))
show('_sre.getcodesize', lambda: _sre.getcodesize())
show('_sre.ascii_iscased', lambda: (_sre.ascii_iscased(ord('a')), _sre.ascii_iscased(ord('1')), _sre.unicode_iscased(0xe9), _sre.ascii_tolower(ord('A')), _sre.unicode_tolower(0xc9)))
