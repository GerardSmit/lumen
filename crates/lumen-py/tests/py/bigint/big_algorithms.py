def isqrt(n):
    if n < 2:
        return n
    x = 1 << ((n.bit_length() + 1) // 2)
    while True:
        y = (x + n // x) // 2
        if y >= x:
            return x
        x = y
def gcd(a, b):
    while b:
        a, b = b, a % b
    return a
def modpow(b, e, m):
    r = 1
    b %= m
    while e:
        if e & 1:
            r = r * b % m
        b = b * b % m
        e >>= 1
    return r
def is_prime(n):
    if n < 2:
        return False
    for p in (2, 3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37):
        if n % p == 0:
            return n == p
    d, s = n - 1, 0
    while d % 2 == 0:
        d //= 2
        s += 1
    for a in (2, 3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37):
        x = modpow(a, d, n)
        if x in (1, n - 1):
            continue
        for _ in range(s - 1):
            x = x * x % n
            if x == n - 1:
                break
        else:
            return False
    return True
print(isqrt(10 ** 60), isqrt(2 * 10 ** 60), isqrt(2 ** 200 - 1) == 2 ** 100 - 1)
print(gcd(2 ** 100 - 1, 2 ** 60 - 1), gcd(10 ** 30, 6 ** 40), gcd(3 ** 50, 5 ** 50))
print(modpow(2, 10 ** 18, 10 ** 9 + 7), modpow(7, 2 ** 100, 2 ** 61 - 1), pow(7, 2 ** 100, 2 ** 61 - 1))
print(is_prime(2 ** 61 - 1), is_prime(2 ** 89 - 1), is_prime(2 ** 67 - 1), is_prime(10 ** 18 + 9), is_prime(10 ** 18 + 7))
def egcd(a, b):
    if b == 0:
        return a, 1, 0
    g, x, y = egcd(b, a % b)
    return g, y, x - (a // b) * y
g, x, y = egcd(2 ** 89 - 1, 10 ** 20 + 39)
print(g, x, y, (2 ** 89 - 1) * x + (10 ** 20 + 39) * y)
print(pow(12345, -1, 10 ** 20 + 39), pow(12345, -1, 10 ** 20 + 39) * 12345 % (10 ** 20 + 39))
def collatz(n):
    c = 0
    while n != 1:
        n = n // 2 if n % 2 == 0 else 3 * n + 1
        c += 1
    return c
print(collatz(27), collatz(2 ** 100 + 1), collatz(10 ** 20))
print(sum(i * i * i for i in range(1, 1001)), sum(range(1, 1001)) ** 2)
c = 1
for k in range(1, 101):
    c = c * (200 - k + 1) // k
print(c)
a, b = 0, 1
for _ in range(500):
    a, b = b, a + b
print(a % 10 ** 20, len(str(a)))
print(sorted([10 ** 20, -10 ** 20, 2 ** 64, 2 ** 63, -2 ** 63, 0, 1, 2 ** 64 + 1]))
