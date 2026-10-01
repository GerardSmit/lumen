def islice(it, n):
    it = iter(it)
    for _ in range(n):
        try:
            yield next(it)
        except StopIteration:
            return

def naturals():
    n = 0
    while True:
        yield n
        n += 1

def fib():
    a, b = 0, 1
    while True:
        yield a
        a, b = b, a + b

def primes():
    found = []
    n = 2
    while True:
        if all(n % p for p in found):
            found.append(n)
            yield n
        n += 1

def cycle(xs):
    while True:
        for x in xs:
            yield x

print(list(islice(naturals(), 5)))
print(list(islice(fib(), 15)))
print(list(islice(primes(), 12)))
print(list(islice(cycle("abc"), 7)))
print(list(islice([1, 2], 10)))

ge = (x * x for x in range(6) if x % 2 == 0)
print(type(ge).__name__)
print(next(ge), next(ge))
print(list(ge))
print(list(ge))
print(sum(x for x in range(101)))
print(max(len(w) for w in ["a", "abc", "ab"]))
evens = (n for n in naturals() if n % 2 == 0)
squares = (n * n for n in evens)
print(list(islice(squares, 6)))
pairs = ((i, j) for i in range(3) for j in range(i))
print(list(pairs))
print(", ".join(str(x) for x in range(5)))
