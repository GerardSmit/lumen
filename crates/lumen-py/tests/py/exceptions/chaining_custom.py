class AppError(Exception):
    pass
class DbError(AppError):
    def __init__(self, msg, code=0):
        super().__init__(msg)
        self.code = code
class NotFound(DbError):
    pass

try:
    raise NotFound("row", code=404)
except AppError as e:
    print(type(e).__name__, e, e.code, e.args, isinstance(e, DbError))
print(NotFound.__mro__[1].__name__, issubclass(NotFound, Exception), issubclass(AppError, BaseException))

try:
    try:
        raise ValueError("low")
    except ValueError as e:
        raise RuntimeError("high") from e
except RuntimeError as e:
    print(e, repr(e.__cause__), repr(e.__context__), e.__suppress_context__)

try:
    try:
        raise ValueError("low")
    except ValueError:
        raise RuntimeError("high")
except RuntimeError as e:
    print(e.__cause__, repr(e.__context__), e.__suppress_context__)

try:
    try:
        raise ValueError("low")
    except ValueError:
        raise RuntimeError("high") from None
except RuntimeError as e:
    print(e.__cause__, repr(e.__context__), e.__suppress_context__)

try:
    raise ValueError("a") from KeyError("b")
except ValueError as e:
    print(repr(e.__cause__))

def reraise():
    try:
        raise ValueError("orig")
    except ValueError:
        print("handling")
        raise
try:
    reraise()
except ValueError as e:
    print("outer got", e)

try:
    raise AppError
except AppError as e:
    print(repr(e), e.args, str(e) == "")
print(repr(AppError("m")), repr(AppError("m", 1)), str(AppError("m", 1)))
print(repr(ValueError()), repr(KeyError("k")), str(KeyError("k")), repr(OSError("x")))
e = ValueError("v")
print(e.__cause__, e.__context__, e.__traceback__)
e.add = 1
print(e.add, e.args)
e.args = (5, 6)
print(e, e.args)
class Custom(Exception):
    def __str__(self):
        return "custom!"
print(Custom("ignored"), repr(Custom("r")))
try:
    raise Custom
except Exception as ex:
    print(ex)
try:
    raise ValueError
except Exception as e:
    try:
        raise TypeError
    except TypeError as e2:
        print(type(e2.__context__).__name__)
try:
    raise 5
except TypeError:
    print("cannot raise int")
try:
    raise ValueError from 3
except TypeError:
    print("bad cause")
