import math


def sieve(n):
    flags = bytearray([1]) * (n + 1)
    flags[0:2] = b"\x00\x00"
    for i in range(2, int(n ** 0.5) + 1):
        if flags[i]:
            flags[i * i::i] = bytearray(len(range(i * i, n + 1, i)))
    return [i for i, f in enumerate(flags) if f]


def gcd(a, b):
    while b:
        a, b = b, a % b
    return abs(a)


def egcd(a, b):
    if b == 0:
        return (a, 1, 0)
    g, x, y = egcd(b, a % b)
    return (g, y, x - (a // b) * y)


def lcm(a, b):
    return a // gcd(a, b) * b


def modpow(base, exp, mod):
    result = 1
    base %= mod
    while exp > 0:
        if exp & 1:
            result = result * base % mod
        base = base * base % mod
        exp >>= 1
    return result


def modinv(a, m):
    g, x, _ = egcd(a % m, m)
    if g != 1:
        raise ValueError(f"{a} has no inverse mod {m}")
    return x % m


def is_prime_mr(n):
    if n < 2:
        return False
    small = (2, 3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37)
    for p in small:
        if n % p == 0:
            return n == p
    d, r = n - 1, 0
    while d % 2 == 0:
        d //= 2
        r += 1
    for a in small:
        x = modpow(a, d, n)
        if x in (1, n - 1):
            continue
        for _ in range(r - 1):
            x = x * x % n
            if x == n - 1:
                break
        else:
            return False
    return True


def factorize(n):
    factors = {}
    d = 2
    while d * d <= n:
        while n % d == 0:
            factors[d] = factors.get(d, 0) + 1
            n //= d
        d += 1 if d == 2 else 2
    if n > 1:
        factors[n] = factors.get(n, 0) + 1
    return factors


def totient(n):
    result = n
    for p in factorize(n):
        result = result // p * (p - 1)
    return result


def collatz(n):
    steps = 0
    while n != 1:
        n = n // 2 if n % 2 == 0 else 3 * n + 1
        steps += 1
    return steps


def fmt_factors(f):
    return " * ".join(f"{p}^{e}" if e > 1 else str(p) for p, e in sorted(f.items())) or "1"


primes = sieve(200)
print(len(primes), primes[:15], primes[-5:])
print("pi(10^5) =", len(sieve(100000)))
twins = [(p, q) for p, q in zip(primes, primes[1:]) if q - p == 2]
print(len(twins), twins[:6])
gaps = {}
for p, q in zip(primes, primes[1:]):
    gaps[q - p] = gaps.get(q - p, 0) + 1
print(sorted(gaps.items()))

print(gcd(48, 18), gcd(17, 5), gcd(0, 9), gcd(-12, 18), math.gcd(2 ** 40, 6 ** 20) == gcd(2 ** 40, 6 ** 20))
print(lcm(4, 6), lcm(21, 6), egcd(240, 46), egcd(17, 31))
print(modpow(2, 100, 1000000007), modpow(3, 200, 13), pow(2, 100, 1000000007) == modpow(2, 100, 1000000007))
print(modinv(3, 11), modinv(10, 17), (modinv(7, 1000003) * 7) % 1000003)
try:
    modinv(6, 9)
except ValueError as ex:
    print("ValueError:", ex)

print([n for n in range(1, 60) if is_prime_mr(n)] == [p for p in primes if p < 60])
big_primes = [2 ** 31 - 1, 2 ** 61 - 1, 10 ** 18 + 9, 10 ** 18 + 3]
print([is_prime_mr(p) for p in big_primes])
print(is_prime_mr(561), is_prime_mr(3215031751), is_prime_mr(2 ** 67 - 1))

for n in (360, 97, 1001, 2 ** 20, 600851475143, 123456789):
    f = factorize(n)
    print(n, fmt_factors(f), totient(n))

print(all(sum(totient(d) for d in range(1, n + 1) if n % d == 0) == n for n in range(1, 40)))
best = max(range(1, 3000), key=lambda n: (collatz(n), -n))
print("longest collatz < 3000:", best, collatz(best), collatz(27), collatz(97))

perfect = [n for n in range(2, 10000) if sum(d for d in range(1, n) if n % d == 0) == n] if False else None
divsum = lambda n: sum(d + (n // d if d * d != n else 0) for d in range(1, int(n ** 0.5) + 1) if n % d == 0) - n
print([n for n in range(2, 10000) if divsum(n) == n])
amicable = sorted({n for n in range(2, 3000) if divsum(n) != n and divsum(divsum(n)) == n})
print(amicable)
print(sum(1 for n in range(1, 2000) if math.gcd(n, 2000) == 1), totient(2000))
crt = next(x for x in range(3 * 5 * 7) if x % 3 == 2 and x % 5 == 3 and x % 7 == 2)
print("crt", crt, math.isqrt(10 ** 20), math.comb(10, 3), math.factorial(15), math.lcm(4, 6, 10))
digits_of_e = sum(1 for _ in str(modpow(7, 77, 10 ** 12)))
print(modpow(7, 77, 10 ** 12), digits_of_e)
