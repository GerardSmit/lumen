import itertools
from itertools import (accumulate, chain, combinations, combinations_with_replacement, count, cycle,
                       groupby, islice, permutations, product, repeat, starmap, takewhile, dropwhile,
                       zip_longest, compress, filterfalse, tee, pairwise, batched)
import operator

print(list(islice(count(5), 4)), list(islice(count(0, 3), 4)), list(islice(count(10, -2), 3)))
print(list(islice(cycle("abc"), 7)), list(repeat("x", 3)), list(islice(repeat(1), 2)))
print(list(accumulate([1, 2, 3, 4, 5])), list(accumulate([1, 2, 3, 4], operator.mul)))
print(list(accumulate([5, 3, 8, 1], max)), list(accumulate([1, 2, 3], initial=100)))
print(list(chain([1, 2], (3,), "ab")), list(chain.from_iterable([[1], [2, 3], []])))
print(list(combinations("abcd", 2)))
print(list(combinations_with_replacement("ab", 3)))
print(list(permutations([1, 2, 3])))
print(list(permutations("abc", 2)))
print(list(product([0, 1], repeat=3)))
print(list(product("ab", [1, 2], "z")))
print(list(product()), list(product([])))

data = [("a", 1), ("a", 2), ("b", 3), ("a", 4), ("c", 5), ("c", 6)]
for key, grp in groupby(data, key=lambda t: t[0]):
    print(key, [v for _, v in grp])
print([(k, len(list(g))) for k, g in groupby("aaabccdd")])
print([(k, list(g)) for k, g in groupby(sorted([3, 1, 2, 3, 1, 3]))])

print(list(islice(range(100), 5, 20, 4)), list(islice("abcdef", 3)), list(islice("abcdef", None)))
print(list(takewhile(lambda x: x < 4, [1, 2, 3, 5, 1])), list(dropwhile(lambda x: x < 4, [1, 2, 3, 5, 1])))
print(list(starmap(pow, [(2, 3), (3, 2), (10, 0)])), list(starmap(operator.add, zip([1, 2], [10, 20]))))
print(list(zip_longest("abc", [1], fillvalue="-")), list(zip_longest([1, 2], [3, 4])))
print(list(compress("abcdef", [1, 0, 1, 0, 1, 1])), list(filterfalse(lambda x: x % 2, range(8))))
a, b = tee([1, 2, 3])
print(list(a), list(b), list(pairwise([1, 4, 9, 16])), list(pairwise("a")))
print(list(batched(range(7), 3)), list(batched("abcd", 2)))

primes = []
for n in count(2):
    if all(n % p for p in takewhile(lambda p: p * p <= n, primes)):
        primes.append(n)
    if len(primes) == 12:
        break
print(primes)
fibs = []
x, y = 0, 1
for _ in range(10):
    fibs.append(x)
    x, y = y, x + y
print(fibs, list(accumulate(fibs)))
print(sum(1 for _ in combinations(range(20), 3)), len(list(product(range(4), repeat=4))))
it = iter([1, 2, 3])
print(list(zip(it, it)))
print(dict(zip("abc", count(1))), sorted(set(chain("hello", "world"))))
grid = [[i * 3 + j for j in range(3)] for i in range(3)]
print(list(chain.from_iterable(grid)), list(zip(*grid)))
