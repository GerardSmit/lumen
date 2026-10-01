it = iter([1, 2])
print(next(it), next(it), next(it, "done"), next(it, None))
try:
    next(it)
except StopIteration as e:
    print(type(e).__name__)
try:
    next(iter(""))
except StopIteration:
    print("stop")
try:
    iter(5)
except TypeError as e:
    print(type(e).__name__)
try:
    next([1])
except TypeError as e:
    print(type(e).__name__)
print(list(iter("ab")), list(iter((1, 2))), list(iter({1: 0})), iter(it) is it)
counter = [0]


def step():
    counter[0] += 1
    return counter[0]


print(list(iter(step, 4)))
print(list(enumerate(iter("xy"))))
print(len([]), len("héllo"), len({1: 2}), len(range(7)), len(b"abc"), len((1, 2)))
try:
    len(5)
except TypeError as e:
    print(type(e).__name__)
print(abs(-3), chr(65), chr(0x263a) == "☺", ord("a"), ord("é"), ord("\n"), chr(97) + chr(98))
try:
    chr(-1)
except ValueError as e:
    print(type(e).__name__)
try:
    ord("ab")
except TypeError as e:
    print(type(e).__name__)
print(bin(10), oct(64), hex(4095), bin(0), oct(0), hex(-1), bin(-0b101), bin(2**70))
print(divmod(17, 5), divmod(-17, 5))
print(repr("a'b"), repr('a"b'), repr("a'b\"c"), repr("\n\t\\"), repr(""), repr("é"), repr("\x00"))
print(str(1), str(1.0), str(None), str(True), str([1, "a"]), str(("a",)), str({"k": (1,)}))
print(repr([1, "a", None]), repr({"a": [1]}), repr(1.5), repr(-0), repr(10**20))
print(repr(b"a\x00'"), repr(bytearray(b"hi")), repr(range(3)), repr(slice(1)), repr(...), repr(NotImplemented))
print(str(b"x"), str(type(1)), repr(int), str(len)[:20], repr(print)[:25])
print(hash(1) == hash(1.0), hash(5) == 5, hash(-1) == -2, hash(0) == 0, hash(True) == 1)
print(hash(2**61 - 1) == 0, hash(2**61) == 1, hash(-5), hash(1000))
print(hash((1, 2)) == hash((1, 2)), hash((1, 2)) != hash((2, 1)), hash(1.5) == hash(1.5), hash(None) == hash(None))
print(1 == 1.0, 1 == True, 2 == 2.0000000000000001, 0 == False, 1 == "1", None == 0)
print({1: "a"}.get(1.0), {1.0: "b"}.get(1), {True: "c"}.get(1), len({1, 1.0, True}))
print(isinstance(True, int), isinstance(False, int), True + True, True * 5, False - 1, True / 2)
print(1 < 2 < 3, 1 < 2 > 3, 3 > 2 > 1, 1 == 1 < 2, 1 < 2 == 2, 5 > 3 >= 3 > 2)
print(0 < 1 < 2 < 3 < 4, 1 < 3 < 2 < 4, "a" < "b" < "c", 1 <= 1 <= 1, 1 != 2 != 1)
print(1 in [1] in [[1]], [] is not None is not [])
x = 5
print(0 < x < 10, 0 < x < 5, x < 10 > 3, -1 < -x < 0 or "neg")
calls = []


def tr(v):
    calls.append(v)
    return v


print(tr(1) < tr(0) < tr(5), calls)
print(None is None, [] is not [], "a" "b" == "ab", type(None) is type(None))
print(callable(iter), all(map(callable, [len, abs, str])))
print(sorted([3, 1, 2], key=None), list(map(str, range(3))), sum(range(4)), min("bca"), max([2, 9, 4]))
print(round(7.5), round(8.5), -(-7 // 2), ~5, ~-1, -5 >> 1, 5 << 3, 6 & 3, 6 | 3, 6 ^ 3)
print(isinstance(len, type(abs)), isinstance(print, object), id(1) == id(1) or True)
print(pow(2, 8), 2 ** 8, 2 ** -2, (-2) ** 3, -2 ** 2, 2 ** 3 ** 2)
