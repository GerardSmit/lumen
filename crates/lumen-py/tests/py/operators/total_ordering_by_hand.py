def total_ordering(cls):
    if "__lt__" in cls.__dict__:
        cls.__gt__ = lambda s, o: o < s if isinstance(o, cls) else NotImplemented
        cls.__le__ = lambda s, o: not (o < s) if isinstance(o, cls) else NotImplemented
        cls.__ge__ = lambda s, o: not (s < o) if isinstance(o, cls) else NotImplemented
    return cls

@total_ordering
class Card:
    order = "23456789TJQKA"
    def __init__(self, r):
        self.r = r
    def __eq__(self, o):
        return isinstance(o, Card) and self.r == o.r
    def __lt__(self, o):
        return self.order.index(self.r) < self.order.index(o.r)
    def __hash__(self):
        return hash(self.r)
    def __repr__(self):
        return "Card(%s)" % self.r
a, b = Card("5"), Card("K")
print(a < b, a <= b, a > b, a >= b, a == b, a != b)
print(a <= Card("5"), a >= Card("5"))
hand = [Card(r) for r in "A2KT9"]
print(sorted(hand), max(hand), min(hand))
print(sorted(hand, reverse=True))
print(sorted(hand, key=lambda c: c.order.index(c.r), reverse=True)[0])
print(a == 3, a != 3)
try:
    a >= 3
except TypeError:
    print("TypeError")

class Money:
    def __init__(self, c): self.c = c
    def __index__(self): return self.c
    def __int__(self): return self.c
    def __bool__(self): return self.c != 0
    def __hash__(self): return self.c
    def __eq__(self, o): return self.c == int(o)
    def __lt__(self, o): return self.c < int(o)
print([1, 2, 3, 4][Money(2)], bool(Money(0)), Money(3) == 3, Money(2) < 5, {Money(1): 1}[1])
print(Money(2) in [1, 2], 2 == Money(2))
print(sorted([Money(3), Money(1), Money(2)], key=int) and "sorted")
print(sorted(["b", "A", "a", "B"]), sorted([3, 1.5, 2]), sorted("hello"))
print(sorted([(2, "b"), (1, "z"), (2, "a")]))
print(sorted([3, 1, 2], key=lambda x: -x), sorted([[2], [1, 5], [1]]))
print(max([1, 3, 2], key=lambda x: -x), min("bca"), max((1, 2), (1, 3)))
print(max([], default="e"), min(5, 3, 9), max("a", "B"))
