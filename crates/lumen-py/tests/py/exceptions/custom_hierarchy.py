class AppError(Exception):
    pass


class ConfigError(AppError):
    def __init__(self, key, msg="bad config"):
        super().__init__(key, msg)
        self.key = key
        self.msg = msg


class NetError(AppError):
    code = 500

    def __str__(self):
        return "NetError<%s>" % (self.args[0] if self.args else "?")


class Timeout(NetError):
    code = 504


class Plain(Exception):
    pass


def classify(exc):
    try:
        raise exc
    except (ConfigError, Timeout) as e:
        return "tuple:" + type(e).__name__
    except NetError:
        return "net"
    except AppError:
        return "app"
    except Exception:
        return "other"


print(classify(ConfigError("k")))
print(classify(Timeout("t")))
print(classify(NetError("n")))
print(classify(AppError("a")))
print(classify(ValueError("v")))

c = ConfigError("port")
print(c.args, c.key, c.msg)
print(str(c))
print(repr(c))
t = Timeout("host")
print(t.code, NetError.code, str(t), repr(t))
print(str(Plain()), repr(Plain()))
print(str(Plain("x")), repr(Plain("x")))
print(str(Plain("x", 1)), repr(Plain("x", 1)))
print(str(Plain(5)), repr(Plain(5)))
print(isinstance(t, NetError), isinstance(t, AppError), isinstance(t, Exception))
print(issubclass(Timeout, (Plain, NetError)), issubclass(Plain, AppError))
print([c.__name__ for c in Timeout.__mro__])
print(Timeout.__bases__[0].__name__)

try:
    raise Timeout("a", "b")
except NetError as e:
    print(e.args, e)

try:
    raise Plain
except Plain as e:
    print(repr(e), e.args)


class WithData(Exception):
    def __init__(self, payload):
        self.payload = payload


try:
    raise WithData({"a": 1})
except WithData as e:
    print(e.payload, e.args)

e = KeyError("missing")
print(str(e), repr(e))
print(str(KeyError(1, 2)))
print(OSError("x").args, ValueError().args)

try:
    raise Plain("one")
except (TypeError, Plain) as e:
    print("caught", e)

excs = (ValueError, KeyError)
try:
    {}["z"]
except excs as e:
    print("dyn", type(e).__name__)

try:
    try:
        raise Plain("x")
    except ValueError:
        print("wrong")
except Plain:
    print("right")

try:
    try:
        raise Plain
    except 5:
        pass
except TypeError:
    print("bad except spec")
