class Item:
    def __init__(self, sku, name, price_cents, qty=0, tags=()):
        self.sku = sku
        self.name = name
        self.price = price_cents
        self.qty = qty
        self.tags = tuple(tags)

    def __repr__(self):
        return f"Item({self.sku!r}, {self.name!r}, {self.price}, {self.qty})"

    def __eq__(self, other):
        return isinstance(other, Item) and self.sku == other.sku

    def __hash__(self):
        return len(self.sku) * 31 + sum(map(ord, self.sku))

    @property
    def value(self):
        return self.price * self.qty


class OutOfStock(Exception):
    pass


class Inventory:
    def __init__(self):
        self._items = {}
        self._events = []

    def __len__(self):
        return len(self._items)

    def __contains__(self, sku):
        return sku in self._items

    def __getitem__(self, sku):
        try:
            return self._items[sku]
        except KeyError:
            raise KeyError(f"unknown sku {sku}") from None

    def __iter__(self):
        return iter(sorted(self._items.values(), key=lambda i: i.sku))

    def add(self, item):
        if item.sku in self._items:
            self._items[item.sku].qty += item.qty
            self._events.append(("restock", item.sku, item.qty))
        else:
            self._items[item.sku] = item
            self._events.append(("new", item.sku, item.qty))

    def remove(self, sku, qty):
        item = self[sku]
        if item.qty < qty:
            raise OutOfStock(f"{sku}: wanted {qty}, have {item.qty}")
        item.qty -= qty
        self._events.append(("sold", sku, qty))
        if item.qty == 0:
            self._events.append(("depleted", sku, 0))
        return item.price * qty

    def by_tag(self):
        groups = {}
        for item in self:
            for tag in item.tags:
                groups.setdefault(tag, []).append(item.sku)
        return groups

    def total_value(self):
        return sum(i.value for i in self._items.values())

    def low_stock(self, threshold=5):
        return [i.sku for i in self if i.qty <= threshold]

    def report(self):
        lines = []
        lines.append(f"{'SKU':<8}{'Name':<16}{'Qty':>5}{'Price':>10}{'Value':>12}")
        lines.append("-" * 51)
        for i in self:
            lines.append(f"{i.sku:<8}{i.name:<16}{i.qty:>5}{i.price / 100:>10.2f}{i.value / 100:>12.2f}")
        lines.append("-" * 51)
        lines.append(f"{'TOTAL':<29}{'':>10}{self.total_value() / 100:>12.2f}")
        return "\n".join(lines)


inv = Inventory()
data = [
    ("A100", "Hammer", 1299, 25, ("tools", "hardware")),
    ("A200", "Screwdriver", 799, 40, ("tools",)),
    ("B100", "Garden Hose", 2450, 8, ("garden",)),
    ("B200", "Rake", 1850, 3, ("garden", "tools")),
    ("C100", "Paint (1L)", 1575, 12, ("paint", "hardware")),
    ("C200", "Brush Set", 999, 0, ("paint",)),
]
for sku, name, price, qty, tags in data:
    inv.add(Item(sku, name, price, qty, tags))

print(len(inv), "A100" in inv, "Z999" in inv)
print(inv.report())
inv.add(Item("C200", "Brush Set", 999, 15))
inv.add(Item("D100", "Ladder", 8900, 2, ("tools",)))
print(inv.report())

revenue = 0
orders = [("A100", 5), ("B200", 3), ("B200", 1), ("D100", 1), ("Z999", 1), ("C100", 12), ("A200", 41)]
for sku, qty in orders:
    try:
        got = inv.remove(sku, qty)
        revenue += got
        print(f"sold {qty} x {sku} = {got / 100:.2f}")
    except OutOfStock as ex:
        print("out of stock:", ex)
    except KeyError as ex:
        print("key error:", ex, "|", ex.args)

print("revenue", revenue)
print("low stock:", inv.low_stock(), inv.low_stock(0))
for tag, skus in sorted(inv.by_tag().items()):
    print(f"{tag:10}{skus}")
print(inv._events)
counts = {}
for ev, *_ in inv._events:
    counts[ev] = counts.get(ev, 0) + 1
print(sorted(counts.items(), key=lambda kv: (-kv[1], kv[0])))
print(inv["A100"], inv["A100"] == Item("A100", "x", 0), inv["A100"] == "A100")
print({i.sku: i.value for i in inv if i.value > 10000})
print(max(inv, key=lambda i: i.value).name, min(inv, key=lambda i: i.price).name)
print(len({Item("A1", "a", 1), Item("A1", "b", 2), Item("A2", "c", 3)}))
print(inv.report())
