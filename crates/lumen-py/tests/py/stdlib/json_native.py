# The _json accelerator: scanner, encoder and string quoting, against CPython.
import json, _json, json.decoder, json.encoder, json.scanner
from collections import OrderedDict
print(json.decoder.scanstring is _json.scanstring, json.scanner.make_scanner is _json.make_scanner)
print(_json.make_scanner.__name__, _json.make_encoder.__name__, json.encoder.c_make_encoder is _json.make_encoder)
print(json.loads('{"a": [1, 2.5, -0, 12345678901234567890, true, null, "x\\u00e9\\ud834\\udd20"], "b": {}}'))
print(json.loads('[NaN, Infinity, -Infinity, -0.0, 1e400]'))
print(json.loads('{"a": 1, "a": 2}', object_pairs_hook=OrderedDict))
print(json.loads('{"x": 1}', object_hook=lambda d: sorted(d)))
print(json.loads('1.5', parse_float=lambda s: ('f', s)), json.loads('7', parse_int=lambda s: ('i', s)))
a, b = json.loads('[{"k": 1}, {"k": 2}]')
print(list(a)[0] is list(b)[0])
for bad in ['', '[', '[1,', '{"a"', '{"a":}', '"abc', '"\\x"', '[1 2]', '{1:2}', '"\x01"', '1 2', 'é[', '"é\\u12"']:
    try:
        json.loads(bad)
    except json.JSONDecodeError as e:
        print(repr(bad), e.msg, e.pos)
print(json.dumps({'a': [1, 2.5, None, True, 'é\ud834', (1, 2)], 3: 'x', 1.5: 'y', None: 0, False: 1}))
print(json.dumps({'a': 'é\U0001d120'}, ensure_ascii=False), json.dumps('é\U0001d120\x7f\x1f'))
print(json.dumps({'b': 1, 'a': 2}, sort_keys=True, separators=(',', ':')))
print(json.dumps(OrderedDict([('z', 1), ('y', 2)])))
print(json.dumps([float('nan'), float('inf'), -float('inf'), 2**70]))
for v, kw in [(float('nan'), {'allow_nan': False}), ({(1,): 2}, {}), (object(), {})]:
    try:
        json.dumps(v, **kw)
    except (ValueError, TypeError) as e:
        print(type(e).__name__, e)
print(json.dumps({(1,): 2, 'a': 1}, skipkeys=True))
x = []; x.append(x)
try:
    json.dumps(x)
except ValueError as e:
    print(e)
print(json.dumps(1j, default=lambda o: [o.real, o.imag]))
print(json.loads('[' * 200 + ']' * 200) == eval('[' * 200 + ']' * 200))
a = []
print(json.dumps(a * 3, default=lambda o: a.clear()))
print(_json.scanstring('"abc"', 1), _json.encode_basestring_ascii('a"\nሴ'), _json.encode_basestring('a"\nሴ'))
try:
    _json.make_encoder(1, None, None, None, ': ', ', ', False, False, False)
except TypeError as e:
    print(e)
try:
    _json.scanstring(b"xxx", 2**64)
except OverflowError as e:
    print('overflow', e)
s = _json.make_scanner(json.JSONDecoder())
print(s.strict, s.parse_float, s('  [1]', 2))
try:
    s('abc', 0)
except StopIteration as e:
    print('stop', e.value)
