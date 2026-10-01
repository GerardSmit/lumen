# The main features of the vendored typing.py.
import typing
from typing import (
    Annotated, Any, Callable, ClassVar, Concatenate, Dict, Final, Generic, List, Literal,
    NamedTuple, NewType, Optional, ParamSpec, Protocol, Tuple, TypeAlias, TypedDict, TypeVar,
    TypeVarTuple, Union, Unpack, cast, get_args, get_origin, get_type_hints, overload,
    runtime_checkable,
)

T = TypeVar("T")
KT = TypeVar("KT", bound=str)
CT = TypeVar("CT", int, str)
T_co = TypeVar("T_co", covariant=True)
P = ParamSpec("P")
Ts = TypeVarTuple("Ts")

print(repr(T), repr(T_co), repr(TypeVar("T_contra", contravariant=True)), T.__module__)
print(KT.__bound__, CT.__constraints__, P.__module__, repr(P.args), repr(Ts))
print(T.__name__, T.__reduce__(), [*Ts])


class Stack(Generic[T]):
    def __init__(self) -> None:
        self.items: list[T] = []

    def push(self, item: T) -> None:
        self.items.append(item)

    def pop(self) -> T:
        return self.items.pop()


s = Stack[int]()
s.push(1)
print(s.pop(), Stack.__parameters__, Stack[int], get_origin(Stack[int]), get_args(Stack[int]))
print(s.__orig_class__ if hasattr(s, "__orig_class__") else "no orig")


class Pair(Generic[KT, T]):
    pass

print(Pair[str, int], Pair[str, int].__args__, Pair.__parameters__)
try:
    Pair[str]
except TypeError as e:
    print("TypeError", e)
try:
    Generic[int]
except TypeError as e:
    print("TypeError", e)
try:
    class Bad(Generic): pass
except TypeError as e:
    print("TypeError", e)


@runtime_checkable
class Closable(Protocol):
    def close(self) -> None: ...


class File:
    def close(self) -> None:
        pass


print(isinstance(File(), Closable), isinstance(1, Closable), issubclass(File, Closable))


class Movie(TypedDict):
    title: str
    year: int


class Partial(TypedDict, total=False):
    rating: float


m = Movie(title="Blade Runner", year=1982)
print(m, type(m), Movie.__required_keys__ == {"title", "year"}, Partial.__optional_keys__)
print(get_type_hints(Movie))


class Point(NamedTuple):
    x: int
    y: int = 0

p = Point(1)
print(p, p._fields, p._field_defaults, Point.__annotations__, p._replace(y=5))
Emp = NamedTuple("Emp", [("name", str), ("id", int)])
print(Emp("a", 1))

print(get_args(Union[int, str]), get_origin(Union[int, str]) is Union)
print(get_args(Optional[int]), Union[int, int] is int, Union[int, str] == Union[str, int])
print(get_args(int | str), get_args(Callable[[int, str], bool]), get_args(Callable[..., int]))
print(get_args(Literal[1, "a"]), get_args(Annotated[int, "meta"]), get_origin(Annotated[int, "m"]) is Annotated)
print(Callable[P, T][[int], str].__args__, get_args(Callable[Concatenate[int, P], T]))
print(List[int], Dict[str, int], Tuple[int, ...], get_args(Tuple[int, ...]))
print(list[T][int], dict[str, T][int])
print(Tuple[Unpack[Ts]], tuple[*Ts])


def hinted(a: int, b: "str", c: Optional[List[int]] = None) -> Dict[str, Any]:
    return {}

print(get_type_hints(hinted))


class WithVars:
    x: ClassVar[int] = 1
    y: Final = 2
    z: "WithVars"

print(get_type_hints(WithVars))
print(get_type_hints(WithVars, include_extras=True)["x"])


@overload
def over(x: int) -> int: ...
@overload
def over(x: str) -> str: ...
def over(x):
    return x

print(over(3), len(typing.get_overloads(over)))

print(cast(int, "not an int"), cast("List[int]", [1]))
UserId = NewType("UserId", int)
print(UserId(5), UserId.__name__, UserId.__supertype__)
Vector: TypeAlias = list[float]
print(Vector)


def deco(f: Callable[P, T]) -> Callable[P, T]:
    return f

@deco
def g(x: int) -> str:
    return str(x)

print(g(1))

print(typing.TYPE_CHECKING, Any, repr(Any))
print(isinstance(1, typing.Hashable), issubclass(list, typing.Sequence), typing.Sized)
print(typing.Text is str, typing.AnyStr)


class Proto(Protocol[T]):
    def meth(self) -> T: ...

print(Proto.__parameters__, Proto[int])
try:
    Proto()
except TypeError as e:
    print("TypeError", e)
