assert True
assert 1 == 1, "never shown"
assert [1], "non-empty list"
assert not [], "empty list is falsy"
assert 1 < 2 < 3
assert isinstance(1, int) and not isinstance(1, str)
print("simple asserts passed")

try:
    assert False
except AssertionError as e:
    print("caught", type(e).__name__, repr(str(e)), e.args)
try:
    assert 1 == 2, "one is not two"
except AssertionError as e:
    print("caught", type(e).__name__, e, e.args)
try:
    assert 0, 42
except AssertionError as e:
    print(e.args, str(e))
try:
    assert None, ("a", 1)
except AssertionError as e:
    print(e.args, e.args[0])
try:
    assert [], [1, 2]
except AssertionError as e:
    print(repr(e.args[0]))


class Msg:
    def __str__(self):
        return "custom message"


try:
    assert False, Msg()
except AssertionError as e:
    print(str(e))
log = []


def side(v):
    log.append(v)
    return v


assert side(True), side("unused message")
print(log)
try:
    assert side(False), side("evaluated message")
except AssertionError as e:
    print(e, log)


def checker(x):
    assert x > 0, "x must be positive, got %d" % x
    return x * 2


print(checker(3))
try:
    checker(-1)
except AssertionError as e:
    print(e)
try:
    checker(0)
except Exception as e:
    print(type(e).__name__, e)
print(issubclass(AssertionError, Exception), issubclass(AssertionError, BaseException))
print(AssertionError.__name__, AssertionError("x").args, str(AssertionError()))
try:
    raise AssertionError("manual")
except AssertionError as e:
    print(e)
caught = 0
for v in [1, 0, 2, 0, 3]:
    try:
        assert v, "zero"
    except AssertionError:
        caught += 1
print(caught)
total = 0
for v in range(1, 6):
    assert v > 0
    total += v
print(total)
assert __debug__
print(__debug__)
try:
    assert all(x > 0 for x in [1, -2, 3]), "has non-positive"
except AssertionError as e:
    print(e)
try:
    assert "a" in "bcd", "missing a"
except AssertionError as e:
    print(e)
try:
    try:
        assert False, "inner"
    finally:
        print("finally runs")
except AssertionError as e:
    print("outer", e)
try:
    assert False, "a" "b" f"{1 + 1}"
except AssertionError as e:
    print(e)
