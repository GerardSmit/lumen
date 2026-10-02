def t(f):
    try:
        print(repr(f()))
    except BaseException as e:
        print(type(e).__name__, e)


# memoryview[int]
t(lambda: memoryview[int])
t(lambda: memoryview[int].__origin__)
t(lambda: memoryview[int].__args__)

# NotImplemented in a boolean context
t(lambda: bool(NotImplemented))
t(lambda: not NotImplemented)
t(lambda: 1 if NotImplemented else 2)
t(lambda: NotImplemented and 1)
t(lambda: [x for x in [1] if NotImplemented])
t(lambda: NotImplemented.__bool__())
t(lambda: NotImplemented == NotImplemented)
t(lambda: repr(NotImplemented))

# PythonFinalizationError
t(lambda: PythonFinalizationError.__mro__)
t(lambda: issubclass(PythonFinalizationError, RuntimeError))

# str.replace(count=)
t(lambda: 'aaa'.replace('a', 'b', count=2))
t(lambda: 'aaa'.replace('a', 'b', 1))
t(lambda: 'aaa'.replace('a', 'b', count=0))
t(lambda: 'aaa'.replace('a', 'b', count=-1))
t(lambda: 'aaa'.replace(old='a', new='b'))
t(lambda: b'aaa'.replace(b'a', b'b', count=1))

# eval / exec keywords
t(lambda: eval('x+1', globals={'x': 1}))
t(lambda: eval('x+y', globals={'x': 1}, locals={'y': 5}))
t(lambda: eval('x+y', {'x': 1}, locals={'y': 5}))
t(lambda: eval(source='1'))
d = {}
t(lambda: exec('z=3', globals=d))
print(d.get('z'))
loc = {}
t(lambda: exec('w=4', {}, locals=loc))
print(loc)
t(lambda: exec('v=5', globals=d, locals=loc))
print(loc.get('v'))
t(lambda: eval('1', globals=1))

# int() no longer falls back to __trunc__
class Tr:
    def __trunc__(self):
        return 5


class Idx:
    def __index__(self):
        return 6


class Int:
    def __int__(self):
        return 7


t(lambda: int(Tr()))
t(lambda: int(Idx()))
t(lambda: int(Int()))
t(lambda: int(None))
t(lambda: int(object()))
import math
t(lambda: math.trunc(Tr()))

# three-argument pow() tries __rpow__
class R:
    def __rpow__(self, o, m=None):
        return ('rpow', o, m)

    def __pow__(self, o, m=None):
        return ('pow', o, m)


class R2(R):
    def __rpow__(self, o, m=None):
        return ('rpow2', o, m)


class NI:
    def __pow__(self, o, m=None):
        return NotImplemented

    def __rpow__(self, o, m=None):
        return NotImplemented


t(lambda: pow(2, 3, 5))
t(lambda: pow(2, R(), 5))
t(lambda: pow(2, R()))
t(lambda: pow(R(), 3, 5))
t(lambda: (lambda r: (r[0], r[2]))(pow(R(), R2(), 5)))
t(lambda: (lambda r: (r[0], r[2]))(pow(R(), R(), 5)))
t(lambda: (lambda r: (r[0], r[2]))(pow(NI(), R(), 5)))
t(lambda: pow(NI(), NI(), 5))
t(lambda: pow(2, 3, R()))
t(lambda: pow(2, 3, 'x'))
t(lambda: pow(2.0, 3, 5))
t(lambda: pow(2, 3.0, 5))
t(lambda: pow(2, 3, 5.0))
t(lambda: pow(2, 3, 1j))
t(lambda: pow(1j, 3, 5))
t(lambda: pow('a', 2, 3))
t(lambda: pow(2, 3, None))
