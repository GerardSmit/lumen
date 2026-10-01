from enum import Enum, IntEnum, auto, Flag, IntFlag, unique
from typing import List, Dict, Optional, Tuple, Union, Callable


class Color(Enum):
    RED = 1
    GREEN = 2
    BLUE = 3

    def describe(self) -> str:
        return self.name.lower() + "=" + str(self.value)

    @classmethod
    def parse(cls, text: str) -> "Color":
        return cls[text.upper()]


class Level(IntEnum):
    LOW = 1
    MID = 5
    HIGH = 10


class Auto(Enum):
    FIRST = auto()
    SECOND = auto()
    THIRD = auto()


class Perm(Flag):
    R = 4
    W = 2
    X = 1


class Mode(IntFlag):
    A = 1
    B = 2
    C = 4


class Planet(Enum):
    MERCURY = (3.303e23, 2.4397e6)
    EARTH = (5.976e24, 6.37814e6)

    def __init__(self, mass: float, radius: float) -> None:
        self.mass = mass
        self.radius = radius

    @property
    def gravity(self) -> float:
        return round(6.67300e-11 * self.mass / (self.radius * self.radius), 2)


@unique
class Shape(Enum):
    CIRCLE = "circle"
    SQUARE = "square"
    ALIAS_OF_NOTHING = "other"


def total(xs: List[int], weights: Optional[Dict[str, int]] = None) -> int:
    return sum(xs) * (weights or {"w": 1})["w"]


def pair(x: Union[int, str], f: Callable[[int], int]) -> Tuple[int, str]:
    return f(int(x)), str(x)


print(total([1, 2, 3]), total([1, 2], {"w": 10}), pair("7", lambda n: n * 2))
print(total.__annotations__["return"] is int, sorted(total.__annotations__))

print(Color.RED, Color.RED.name, Color.RED.value, repr(Color.GREEN), Color(2), Color["BLUE"])
print(list(Color), [c.name for c in Color], len(Color), Color.RED in Color)
print(Color.RED is Color(1), Color.RED == Color.RED, Color.RED != Color.BLUE, Color.RED == 1)
print(Color.parse("green").describe(), type(Color.RED).__name__, isinstance(Color.RED, Color))
print({c: c.value for c in Color}[Color.BLUE], sorted(Color, key=lambda c: -c.value)[0])
print(Color.__members__["RED"] is Color.RED, list(Color.__members__))
try:
    Color(99)
except ValueError:
    print("ValueError bad value")
try:
    Color["NOPE"]
except KeyError:
    print("KeyError bad name")
try:
    Color.RED = 5
except AttributeError:
    print("AttributeError immutable member")

print(Level.LOW < Level.HIGH, Level.MID == 5, Level.HIGH + 1, sorted([Level.HIGH, Level.LOW, Level.MID]))
print(int(Level.MID), Level(10), max(Level), [l.value for l in Level], f"{Level.MID}", Level.MID * 2)
print(Auto.FIRST.value, Auto.SECOND.value, Auto.THIRD.value, list(Auto))

rw = Perm.R | Perm.W
print(rw, rw.value, Perm.R in rw, Perm.X in rw, bool(Perm.R & Perm.W), (rw & Perm.W) == Perm.W, Perm(7))
print(Mode.A | Mode.C, int(Mode.A | Mode.B | Mode.C), Mode(3), Mode.B in Mode(3), Mode.A | 4)

print(Planet.EARTH.gravity, Planet.MERCURY.gravity, Planet.EARTH.mass, Planet["EARTH"].radius)
print([s.value for s in Shape], Shape("circle"), Shape.SQUARE.name)


class Alias(Enum):
    ONE = 1
    UNO = 1
    TWO = 2

print(Alias.UNO is Alias.ONE, list(Alias), Alias.UNO.name, Alias(1))
