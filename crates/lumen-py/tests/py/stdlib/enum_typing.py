from enum import Enum, IntEnum, Flag, auto
from typing import List, Dict, Optional, Union, Callable, Tuple, Any, TypeVar, Generic

class Color(Enum):
    RED = 1
    GREEN = 2
    BLUE = 3

class Pri(IntEnum):
    LOW = 1
    HIGH = 10

class Perm(Flag):
    R = auto()
    W = auto()
    X = auto()

print(Color.RED, Color.RED.name, Color.RED.value, Color(2), Color["BLUE"], repr(Color.GREEN))
print(list(Color), len(Color), Color.RED is Color(1), Color.RED == Color.GREEN, Color.RED in Color)
print(Pri.HIGH > Pri.LOW, Pri.HIGH + 1, int(Pri.LOW), sorted([Pri.HIGH, Pri.LOW]))
print(Perm.R | Perm.W, (Perm.R | Perm.W) & Perm.W == Perm.W, Perm.X.value, bool(Perm.R & Perm.W))
for c in Color:
    print(c.name, c.value, end="; ")
print()
try:
    Color(9)
except ValueError:
    print("ValueError")

T = TypeVar("T")
class Stack(Generic[T]):
    def __init__(self) -> None:
        self.items: List[T] = []
    def push(self, x: T) -> None:
        self.items.append(x)
    def pop(self) -> Optional[T]:
        return self.items.pop() if self.items else None

def f(a: int, b: Union[int, str] = 0, *c: Any, cb: Callable[[int], int] = lambda x: x) -> Dict[str, Tuple[int, ...]]:
    return {"r": (a, 1)}

s: Stack[int] = Stack()
s.push(1); s.push(2)
print(s.pop(), s.pop(), s.pop(), f(1), f.__annotations__["a"].__name__)
x: int = 5
print(x, __annotations__["x"] if "__annotations__" in dir() else int)
