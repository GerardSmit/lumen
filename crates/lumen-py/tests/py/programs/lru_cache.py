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
            last = self.tail.prev
            self._unlink(last)
            del self.map[last.key]
            self.evicted.append(last.key)
        node = Node(key, value)
        self.map[key] = node
        self._push_front(node)

    def __len__(self):
        return len(self.map)

    def __contains__(self, key):
        return key in self.map

    def keys(self):
        out = []
        node = self.head.next
        while node is not self.tail:
            out.append(node.key)
            node = node.next
        return out

    def stats(self):
        total = self.hits + self.misses
        rate = (self.hits * 100 // total) if total else 0
        return "hits=%d misses=%d rate=%d%%" % (self.hits, self.misses, rate)

    def __repr__(self):
        return "LRUCache(%r)" % (self.keys(),)


def memoize(capacity):
    def decorator(fn):
        cache = LRUCache(capacity)
        sentinel = object()

        def wrapper(*args):
            result = cache.get(args, sentinel)
            if result is sentinel:
                result = fn(*args)
                cache.put(args, result)
            return result

        wrapper.cache = cache
        wrapper.__name__ = fn.__name__
        return wrapper
    return decorator


def basic_demo():
    c = LRUCache(3)
    for k in "abc":
        c.put(k, ord(k))
    print(c)
    print(c.get("a"), c)
    c.put("d", 100)
    print(c, "evicted:", c.evicted)
    print(c.get("b"), c.get("b", "dflt"))
    c.put("c", 999)
    print(c, c.get("c"))
    c.put("e", 5)
    c.put("f", 6)
    print(c, "evicted:", c.evicted)
    print(len(c), "a" in c, "f" in c)
    print(c.stats())


def eviction_order_demo():
    c = LRUCache(4)
    access = [1, 2, 3, 4, 1, 5, 2, 6, 1, 7, 3, 4, 4, 5]
    for k in access:
        if c.get(k) is None:
            c.put(k, k * k)
    print("final:", c.keys())
    print("evicted:", c.evicted)
    print(c.stats())


@memoize(8)
def fib(n):
    if n < 2:
        return n
    return fib(n - 1) + fib(n - 2)


calls = {"slow": 0}


@memoize(2)
def slow_square(x):
    calls["slow"] += 1
    return x * x


def memo_demo():
    print(fib(30), fib.cache.stats(), len(fib.cache))
    print(fib.__name__)
    for x in [1, 2, 1, 3, 1, 2, 3, 3]:
        slow_square(x)
    print("slow calls:", calls["slow"], slow_square.cache.keys())
    print(slow_square.cache.evicted)


def error_demo():
    try:
        LRUCache(0)
    except ValueError as e:
        print("ValueError:", e)
    c = LRUCache(1)
    c.put("x", 1)
    c.put("y", 2)
    print(c, c.evicted)


def stress():
    c = LRUCache(50)
    seed = 12345
    for _ in range(5000):
        seed = (seed * 1103515245 + 12345) % 2147483648
        k = (seed >> 8) % 120
        if c.get(k) is None:
            c.put(k, k)
    print(c.stats(), len(c), len(c.evicted))
    assert len(c.keys()) == len(c.map)
    print(sum(c.keys()), c.keys()[:5])


basic_demo()
eviction_order_demo()
memo_demo()
error_demo()
stress()
