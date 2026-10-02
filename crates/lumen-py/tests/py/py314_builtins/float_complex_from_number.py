def t(f):
    try: print(repr(f()))
    except BaseException as e: print(type(e).__name__, e)
class SS(str): pass
class Fl:
    def __float__(s): return 2.5
class Fl2:
    def __float__(s): return 'x'
class Ix:
    def __index__(s): return 7
class Cx:
    def __complex__(s): return 1+2j
class Cx2:
    def __complex__(s): return 5
class CxF(complex): pass
class FF(float):
    def __new__(cls, v): print('new', v); return super().__new__(cls, v)
class CC(complex):
    def __new__(cls, *a): print('new', a); return super().__new__(cls, *a)
for v in [SS('1'), b'1', bytearray(b'1'), Fl(), Fl2(), Ix(), Cx(), Cx2(), CxF(1,2), 1j, True, 10**400, 2**53+1, memoryview(b'1'), [], float('nan'), -0.0]:
    t(lambda: float.from_number(v))
    t(lambda: complex.from_number(v))
    t(lambda: FF.from_number(v))
    t(lambda: CC.from_number(v))
t(lambda: float.from_number())
t(lambda: float.from_number(1,2))
t(lambda: float.from_number(number=1))
t(lambda: complex.from_number(number=1))
t(lambda: type(complex.from_number(CxF(1,2))))
x = 1+2j
t(lambda: complex.from_number(x) is x)
t(lambda: float.from_number.__doc__)
t(lambda: complex.from_number.__doc__)
t(lambda: float.from_number.__text_signature__)
t(lambda: (1.0).from_number(2))
