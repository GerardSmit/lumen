def egcd(a, b):
    x0, x1, y0, y1 = 1, 0, 0, 1
    while b:
        q, a, b = a // b, b, a % b
        x0, x1 = x1, x0 - q * x1
        y0, y1 = y1, y0 - q * y1
    return a, x0, y0


def modinv(a, m):
    g, x, _ = egcd(a % m, m)
    if g != 1:
        raise ValueError("no inverse")
    return x % m


def modexp(b, e, m):
    r = 1
    b %= m
    while e:
        if e & 1:
            r = r * b % m
        b = b * b % m
        e >>= 1
    return r


def is_probable_prime(n):
    if n < 2:
        return False
    small = (2, 3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37)
    for p in small:
        if n % p == 0:
            return n == p
    d, s = n - 1, 0
    while d % 2 == 0:
        d //= 2
        s += 1
    for a in small:
        x = modexp(a, d, n)
        if x == 1 or x == n - 1:
            continue
        for _ in range(s - 1):
            x = x * x % n
            if x == n - 1:
                break
        else:
            return False
    return True


def next_prime(n):
    n += 1
    while not is_probable_prime(n):
        n += 1
    return n


def rsa_demo():
    p = next_prime(10 ** 20 + 7)
    q = next_prime(10 ** 21 + 13)
    n = p * q
    phi = (p - 1) * (q - 1)
    e = 65537
    d = modinv(e, phi)
    print("p =", p)
    print("q =", q)
    print("n =", n)
    print("d =", d)
    assert e * d % phi == 1
    msg = int.from_bytes(b"lumen big ints!", "big")
    cipher = modexp(msg, e, n)
    plain = modexp(cipher, d, n)
    print("msg    =", msg)
    print("cipher =", cipher)
    print("plain  =", plain, plain.to_bytes(15, "big"))
    sig = modexp(msg, d, n)
    print("verify:", modexp(sig, e, n) == msg, modexp(sig + 1, e, n) == msg)

    tp, tq = 61, 53
    tn = tp * tq
    tphi = (tp - 1) * (tq - 1)
    te = 17
    td = modinv(te, tphi)
    print("toy:", tn, tphi, td, [modexp(modexp(m, te, tn), td, tn) for m in (65, 123, 3000)])


def primes_demo():
    known = [2, 3, 97, 7919, 104729, 2 ** 31 - 1, 2 ** 61 - 1, 2 ** 89 - 1, 2 ** 107 - 1, 2 ** 127 - 1]
    print([is_probable_prime(k) for k in known])
    composites = [1, 0, 4, 561, 1105, 1729, 2821, 6601, 3215031751, 2 ** 67 - 1, 2 ** 101 - 1, (2 ** 31 - 1) * (2 ** 61 - 1)]
    print([is_probable_prime(k) for k in composites])
    print([p for p in range(90, 140) if is_probable_prime(p)])
    print(sum(1 for k in range(10000) if is_probable_prime(k)))
    print(next_prime(10 ** 30), next_prime(2 ** 100), next_prime(2 ** 64))
    carm = [k for k in range(3, 10000, 2) if modexp(2, k - 1, k) == 1 and not is_probable_prime(k)]
    print("fermat base-2 pseudoprimes < 10000:", carm)


def fib_pair(n):
    if n == 0:
        return 0, 1
    a, b = fib_pair(n >> 1)
    c = a * (2 * b - a)
    d = a * a + b * b
    if n & 1:
        return d, c + d
    return c, d


def fib_demo():
    f2000 = fib_pair(2000)[0]
    s = str(f2000)
    print("digits:", len(s), "first:", s[:15], "last:", s[-15:])
    a, b = 0, 1
    for _ in range(2000):
        a, b = b, a + b
    print("matches iterative:", a == f2000)
    print([fib_pair(i)[0] for i in range(15)])
    print(fib_pair(300)[0], fib_pair(500)[0] % 10 ** 9)
    ok = True
    for i in range(1, 60):
        ok = ok and fib_pair(i + 2)[0] == fib_pair(i)[0] + fib_pair(i + 1)[0]
        ok = ok and fib_pair(i)[0] ** 2 - fib_pair(i + 1)[0] * fib_pair(i - 1)[0] == (-1) ** (i - 1)
    print("identities hold:", ok)
    print("pisano-ish last digits:", [fib_pair(60 * k)[0] % 10 for k in range(1, 8)])


rsa_demo()
primes_demo()
fib_demo()
