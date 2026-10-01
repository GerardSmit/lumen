from dataclasses import dataclass, field, asdict, astuple, replace, fields

@dataclass
class Point:
    x: int
    y: int = 0

@dataclass(order=True, frozen=True)
class Version:
    major: int
    minor: int = 0

@dataclass
class Bag:
    name: str
    items: list = field(default_factory=list)
    tags: dict = field(default_factory=dict, repr=False)
    count: int = field(default=0, compare=False)

p = Point(1, 2)
print(p, p.x, p == Point(1, 2), p == Point(1, 3), Point(5))
print(asdict(p), astuple(p), replace(p, y=9), [f.name for f in fields(p)])
print(Version(1, 2) < Version(1, 3), sorted([Version(2), Version(1, 5), Version(1)]))
try:
    Version(1).major = 5
except Exception as e:
    print(type(e).__name__)
b1, b2 = Bag("a"), Bag("b")
b1.items.append(1)
print(b1, b2, b1 == Bag("a", [1], count=7))

@dataclass
class Post:
    a: int
    b: int = 0
    total: int = field(init=False)
    def __post_init__(self):
        self.total = self.a + self.b
print(Post(1, 2), Post(3).total)
print(Point.__dataclass_fields__.keys() == {"x", "y"}, repr(Point(-1)))
