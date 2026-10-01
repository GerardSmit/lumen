class Node:
    __slots__ = ("key", "value", "prev", "next")

    def __init__(self, key=None, value=None):
        self.key = key
        self.value = value
        self.prev = None
        self.next = None


class LRUCache:
    def __init__(self, capacity):
        if capacity <= 0:
            raise ValueError("capacity must be positive")
        self.capacity = capacity
        self.map = {}
        self.head = Node()
        self.tail = Node()
        self.head.next = self.tail
        self.tail.prev = self.head
        self.hits = 0
        self.misses = 0
        self.evicted = []

    def _unlink(self, node):
        node.prev.next = node.next
        node.next.prev = node.prev

    def _push_front(self, node):
        node.next = self.head.next
        node.prev = self.head
        self.head.next.prev = node
        self.head.next = node

    def get(self, key, default=None):
        node = self.map.get(key)
        if node is None:
            self.misses += 1
            return default
        self.hits += 1
        self._unlink(node)
        self._push_front(node)
        return node.value

    def put(self, key, value):
        node = self.map.get(key)
        if node is not None:
            node.value = value
            self._unlink(node)
            self._push_front(node)
            return
        if len(self.map) >= self.capacity:
            lru = self.tail.prev
            self._unlink(lru)
            del self.map[lru.key]
            self.evicted.append(lru.key)
        node = Node(key, value)
        self.map[key] = node
        self._push_front(node)

    def delete(self, key):
        node = self.map.pop(key, None)
        if node is None:
            return False
        self._unlink(node)
        return True

    def keys(self):
        out = []
        n = self.head.next
        while n is not self.tail:
            out.append(n.key)
            n = n.next
        return out

    def __len__(self):
        return len(self.map)

    def __contains__(self, key):
        return key in self.map

    def __getitem__(self, key):
        sentinel = object()
        v = self.get(key, sentinel)
        if v is sentinel:
            raise KeyError(key)
        return v

    def __setitem__(self, key, value):
        self.put(key, value)

    def __iter__(self):
        return iter(self.keys())

    def __repr__(self):
        return "LRUCache(" + ", ".join(f"{k!r}: {self.map[k].value!r}" for k in self.keys()) + ")"

    def check(self):
        fwd = self.keys()
        back = []
        n = self.tail.prev
        while n is not self.head:
            back.append(n.key)
            n = n.prev
        return fwd == back[::-1] and len(fwd) == len(self.map) <= self.capacity


c = LRUCache(3)
c.put("a", 1)
c.put("b", 2)
c.put("c", 3)
print(c, c.check())
print(c.get("a"), c.keys())
c.put("d", 4)
print(c, c.evicted)
print(c.get("b"), c.get("b", "dflt"), c.misses)
c["c"] = 30
c["e"] = 5
print(list(c), c.evicted)
print("a" in c, "d" in c, len(c))
print(c.delete("d"), c.delete("zzz"), c.keys(), c.check())
try:
    c["nope"]
except KeyError as ex:
    print("KeyError", ex)
try:
    LRUCache(0)
except ValueError as ex:
    print("ValueError", ex)

fibcache = LRUCache(50)
calls = [0]


def fib(n):
    v = fibcache.get(n)
    if v is not None:
        return v
    calls[0] += 1
    v = n if n < 2 else fib(n - 1) + fib(n - 2)
    fibcache.put(n, v)
    return v


print(fib(60), calls[0], fibcache.hits, fibcache.misses)

small = LRUCache(4)
x = 7
trace = []
for i in range(200):
    x = (x * 31 + 17) % 101
    key = x % 9
    if small.get(key) is None:
        small.put(key, i)
        trace.append(key)
print(len(trace), small.hits, small.misses, small.keys(), small.check())
print(small.evicted[:10], len(small.evicted))
print(f"hit rate {small.hits / (small.hits + small.misses):.4f}")
