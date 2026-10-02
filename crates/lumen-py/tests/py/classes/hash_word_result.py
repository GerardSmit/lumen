class Proxy:
    def __init__(self, target):
        self.target = target

    def __hash__(self):
        return hash(self.target)

    def __eq__(self, other):
        return other == self.target


for target in ["halibut", "x" * 40, b"bytes", (1, "two"), 2**62, -(2**63), 1.5]:
    p = Proxy(target)
    print(hash(p) == hash(target), len({target, p}))


class Big:
    def __hash__(self):
        return 2**61 - 1


class Huge:
    def __hash__(self):
        return 2**64 + 5


class MinusOne:
    def __hash__(self):
        return -1


print(hash(Big()), hash(Huge()) == hash(2**64 + 5), hash(MinusOne()))
