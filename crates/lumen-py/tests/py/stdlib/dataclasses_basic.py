from dataclasses import dataclass, field, fields, replace, FrozenInstanceError


@dataclass
class Point:
    x: int
    y: int = 0

    def norm2(self):
        return self.x * self.x + self.y * self.y


@dataclass
class Inventory:
    owner: str
    items: list = field(default_factory=list)
    tags: dict = field(default_factory=dict)
    secret: str = field(default="hidden", repr=False)
    cached: int = field(default=0, compare=False)


@dataclass(frozen=True)
class Money:
    amount: int
    currency: str = "USD"

    def __add__(self, other):
        return Money(self.amount + other.amount, self.currency)


@dataclass(order=True)
class Version:
    major: int
    minor: int = 0
    patch: int = 0


@dataclass(order=True)
class Job:
    priority: int
    name: str = field(compare=False)


@dataclass
class Base:
    id: int
    kind: str = "base"


@dataclass
class Derived(Base):
    extra: float = 1.5


@dataclass
class WithPost:
    a: int
    b: int = field(init=False)

    def __post_init__(self):
        self.b = self.a * 2


p = Point(3, 4)
print(p, p.norm2(), Point(1), Point(y=2, x=1))
print(p == Point(3, 4), p == Point(3, 5), p != Point(0, 0))
p.x = 10
print(p)

inv1, inv2 = Inventory("ann"), Inventory("ann")
inv1.items.append("sword")
print(inv1, inv2, inv1 == inv2, inv1.secret)
inv2.items.append("sword")
inv2.cached = 99
print(inv1 == inv2)

m = Money(5)
print(m, m + Money(7), {m: 1}[Money(5)], hash(m) == hash(Money(5)))
try:
    m.amount = 1
except FrozenInstanceError:
    print("FrozenInstanceError")

vs = [Version(1, 2, 3), Version(1, 0), Version(0, 9, 9), Version(2)]
print(sorted(vs))
print(Version(1) < Version(1, 1), Version(2) >= Version(1, 99), max(vs))
jobs = [Job(3, "c"), Job(1, "z"), Job(2, "m"), Job(1, "a")]
print(sorted(jobs), Job(1, "q") == Job(1, "r"))

print([f.name for f in fields(Point)], [f.name for f in fields(Derived)])
print([(f.name, f.type.__name__) for f in fields(Inventory)])
print(replace(p, y=-1), replace(m, currency="EUR"), p)
d = Derived(7)
print(d, d.id, d.kind, d.extra, d == Derived(7, "base", 1.5))
w = WithPost(21)
print(w, w.b)

try:
    Point()
except TypeError:
    print("TypeError missing x")
try:
    Point(1, 2, 3)
except TypeError:
    print("TypeError too many")
try:
    Point(1) < Point(2)
except TypeError:
    print("TypeError unordered")
print(Point.__eq__ is not object.__eq__, "x" in Point.__dataclass_fields__, hash(Point.__hash__ is None) is not None)
try:
    hash(p)
except TypeError:
    print("unhashable mutable dataclass")
