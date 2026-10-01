import enum


class Color(enum.Enum):
    RED = 1
    GREEN = 2
    BLUE = 3

    def describe(self):
        return "%s=%d" % (self.name, self.value)


print(Color.RED, repr(Color.GREEN), Color(3), Color["RED"])
print(list(Color), len(Color), Color.RED in Color)
print(Color.RED.describe(), Color.BLUE.name, Color.BLUE.value)
print(Color.RED is Color(1), Color.RED == Color.RED, Color.RED != Color.GREEN)
try:
    Color(9)
except ValueError as e:
    print("ValueError", e)


class Level(enum.IntEnum):
    LOW = 1
    HIGH = 2


print(Level.LOW < Level.HIGH, Level.HIGH + 1, int(Level.LOW), Level(2))


class Mode(enum.Flag):
    R = 1
    W = 2
    X = 4


rw = Mode.R | Mode.W
print(rw, Mode.R in rw, Mode.X in rw, bool(Mode(0)))


class Auto(enum.Enum):
    A = enum.auto()
    B = enum.auto()


print([m.value for m in Auto])


class S(enum.StrEnum):
    X = "x"
    Y = enum.auto()


print(S.X, S.Y, S.X == "x")


class Alias(enum.Enum):
    A = 1
    B = 1
    C = 2


print(Alias.B is Alias.A, list(Alias), Alias.__members__["B"])


@enum.unique
class U(enum.Enum):
    P = 1
    Q = 2


print(len(U))
try:
    @enum.unique
    class Bad(enum.Enum):
        P = 1
        Q = 1
except ValueError as e:
    print("ValueError")
try:
    Color.RED = 5
except AttributeError:
    print("AttributeError")
