falsy = [None, False, 0, 0.0, "", (), [], {}, set(), b"", range(0), frozenset(), 0j if False else 0]
print([bool(x) for x in falsy])
truthy = [True, 1, -1, 0.1, "a", " ", "0", "False", (0,), [0], {0: 0}, {0}, b"x", range(1), float("nan"), object()]
print([bool(x) for x in truthy])
print(not None, not 0, not [], not "a", not [0], not not 5)


class T:
    def __bool__(self):
        return True


class F:
    def __bool__(self):
        return False


class L0:
    def __len__(self):
        return 0


class L2:
    def __len__(self):
        return 2


class Both:
    def __bool__(self):
        return True

    def __len__(self):
        return 0


class Plain:
    pass


print(bool(T()), bool(F()), bool(L0()), bool(L2()), bool(Both()), bool(Plain()))
print(1 and 2, 0 and 2, 1 or 2, 0 or 2, None or "d", "" or [] or 0, 1 and 2 and 3, 1 and 0 and 3)
print([] or {}, [0] and "yes", None and 1, not (1 and 0))
print((0 or None), (None or 0), ("a" and ""), ("" and "a"))
x = None
print(x is None, x is not None, x == None, [] is None)
print(None is None, True is True, type(None) is type(None))
a = []
b = a
c = []
print(a is b, a is c, a == c, a is not c)
print(True == 1, False == 0, 1 == 1.0, "a" == "a")
print((1, 2) == (1, 2), [1] == [1], {1: 2} == {1: 2}, {1} == {1}, None == None)
print(1 if [] else 2, 1 if [0] else 2, "t" if "" else "f")
print(len([x for x in range(10) if x % 3]), [x for x in [0, 1, 2, None, "", "a"] if x])
print(all([]), any([]), all([0]), any([0]), all([[]]), any([[], [1]]))
print(bool(0.0), bool(1e-300), bool(-0.0), bool(float("inf")), bool(2**100), bool(-2**100))
print(bool(True) is True, bool(0) is False, bool("") is False, bool([1]) is True)
print(True + True, True * 3, True & False, True | False, True ^ True, -True, False or True)
print(isinstance(True, int), type(True).__name__, type(1 == 1).__name__, type(not 0).__name__)
print(1 < 2 == True, (1 < 2) is True, not 1 == 2)
print(None.__class__.__name__, repr(None), str(None), bool(None), None.__bool__())
print(NotImplemented is NotImplemented, ... is Ellipsis, bool(NotImplemented) if False else "skip")
print((lambda: 0)() or "zero", (lambda: 1)() and "one")
if []:
    print("empty list truthy")
else:
    print("empty list falsy")
if [[]]:
    print("nested truthy")
while False:
    print("never")
n = 3
while n:
    n -= 1
print(n)
print(0 == False, "0" == False, "" == False, [] == False, None == False)
print(max(0, False), max(False, 0), min(1, True), min(True, 1))
print(sum([True, False, True]), [1, 2, 3][True], "ab"[False], {True: "t"}[1])
d = {}
d[0] = "int"
d[False] = "bool"
d[0.0] = "float"
print(d, len(d))
i1, i2 = 10**30, 10**30
print(i1 == i2, i1 + 1 > i2, i1 is i1)
s1 = "x" * 3
print(s1 == "xxx", s1 is s1)
