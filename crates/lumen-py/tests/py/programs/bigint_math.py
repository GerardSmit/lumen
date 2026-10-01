import sys


def factorial(n):
    r = 1
    for i in range(2, n + 1):
        r *= i
    return r


def fib_iter(n):
    a, b = 0, 1
    for _ in range(n):
        a, b = b, a + b
    return a


def gcd(a, b):
    while b:
        a, b = b, a % b
    return abs(a)


def lcm(a, b):
    return abs(a * b) // gcd(a, b)


def modexp(base, exp, mod):
    result = 1
    base %= mod
    while exp > 0:
        if exp & 1:
            result = result * base % mod
        exp >>= 1
        base = base * base % mod
    return result


def digit_sum(n):
    return sum(int(c) for c in str(abs(n)))


def collatz_steps(n):
    steps = 0
    peak = n
    while n != 1:
        n = n // 2 if n % 2 == 0 else 3 * n + 1
        steps += 1
        if n > peak:
            peak = n
    return steps, peak


def isqrt(n):
    if n < 0:
        raise ValueError("negative")
    if n < 2:
        return n
    x = 1 << ((n.bit_length() + 1) // 2)
    while True:
        y = (x + n // x) // 2
        if y >= x:
            return x
        x = y


def main():
    f500 = factorial(500)
    s = str(f500)
    print(len(s), s[:20], s[-5:], digit_sum(f500))
    trailing = len(s) - len(s.rstrip("0"))
    print("trailing zeros:", trailing)

    f1000 = fib_iter(1000)
    print(f1000)
    print(len(str(f1000)), f1000 % 1000000007)
    print(fib_iter(100), fib_iter(200))

    for e in (10, 64, 100, 127, 128, 200, 1000):
        v = 2 ** e
        print(e, v if e <= 128 else (len(str(v)), str(v)[:12], str(v)[-6:]))
    print(2 ** 64 - 1, -(2 ** 63), 2 ** 63 - 1)

    a, b = factorial(30), 2 ** 40 * 3 ** 20 * 7
    print(gcd(a, b), lcm(a, b))
    print(gcd(0, 5), gcd(-12, 18), lcm(4, 6), lcm(2 ** 70, 6 ** 20))

    print(modexp(2, 1000, 10 ** 9 + 7), pow(2, 1000, 10 ** 9 + 7))
    big_mod = 2 ** 127 - 1
    print(modexp(3, big_mod - 1, big_mod))
    print(modexp(12345678901234567890, 98765432109876543210, 10 ** 30 + 57))

    print(digit_sum(2 ** 1000), digit_sum(factorial(100)), digit_sum(-12345))

    for n in (27, 97, 871, 6171, 2 ** 40 + 1, 10 ** 12 + 39):
        print(n, collatz_steps(n))

    for n in (0, 1, 2, 15, 16, 17, 10 ** 20, 2 ** 200, 10 ** 100 + 12345):
        r = isqrt(n)
        assert r * r <= n < (r + 1) * (r + 1)
        print(n if n < 10 ** 6 else len(str(n)), r)
    print(isqrt(2 * 10 ** 100))

    q = 2 ** 200 // 3 ** 50
    print(q, 2 ** 200 % 3 ** 50)
    print(divmod(2 ** 200, 3 ** 50) == (q, 2 ** 200 - q * 3 ** 50))

    big = 10 ** 30 + 7
    for x, y in [(big, 13), (-big, 13), (big, -13), (-big, -13), (13, big), (-13, big), (13, -big)]:
        d, m = divmod(x, y)
        assert d * y + m == x
        print(x, y, "->", d, m, x // y, x % y)
    print(divmod(-7, 2), divmod(7, -2), divmod(-7, -2), (-7) // 2, -7 % 2)

    n300 = int("7" * 300)
    t = str(n300)
    print(len(t), t[:10], t[-10:], int(t) == n300, n300 % 97)
    n300b = 10 ** 299 + 123456789
    print(int(str(n300b)) == n300b, str(n300b)[-12:], len(str(n300b * n300b)))
    print(int("-" + "9" * 50), int("  12345678901234567890  "), int("+5"))
    print(int("1_000_000_000_000_000_000_000"))
    print(sum(range(10 ** 6)) * 10 ** 12)
    print(sys.maxsize, sys.maxsize + 1, -sys.maxsize - 1, (sys.maxsize + 1) * 2)
    print(abs(-(10 ** 40)), -(10 ** 40) // 7, (10 ** 40) ** 2 == 10 ** 80)
    print(max(10 ** 30, 10 ** 29), min(-10 ** 30, 5), sorted([10 ** 20, -5, 3, 2 ** 70, 0]))
    print((1 << 100) | 1, (1 << 100) & ((1 << 101) - 1), (1 << 100) ^ (1 << 99), ~(1 << 70))


main()
