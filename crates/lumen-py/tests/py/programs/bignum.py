def factorial(n):
    r = 1
    for k in range(2, n + 1):
        r *= k
    return r


def fib_pair(n):
    if n == 0:
        return (0, 1)
    a, b = fib_pair(n >> 1)
    c = a * (2 * b - a)
    d = a * a + b * b
    return (d, c + d) if n & 1 else (c, d)


def pi_digits(count):
    q, r, t, k, m, x = 1, 0, 1, 1, 3, 3
    out = []
    while len(out) < count:
        if 4 * q + r - t < m * t:
            out.append(m)
            q, r, m = 10 * q, 10 * (r - m * t), (10 * (3 * q + r)) // t - 10 * m
        else:
            q, r, t, k, m, x = (
                q * k,
                (2 * q + r) * x,
                t * x,
                k + 1,
                (q * (7 * k + 2) + r * x) // (t * x),
                x + 2,
            )
    return out


def isqrt_newton(n):
    if n < 2:
        return n
    x = 1 << ((n.bit_length() + 1) // 2)
    while True:
        y = (x + n // x) // 2
        if y >= x:
            return x
        x = y


def digit_sum(n):
    return sum(int(c) for c in str(n))


f50 = factorial(50)
print("50! =", f50)
print("digits in 100!:", len(str(factorial(100))), "digit sum:", digit_sum(factorial(100)))
print("trailing zeros of 1000!:", len(str(factorial(1000))) - len(str(factorial(1000)).rstrip("0")))
print("bit length 200!:", factorial(200).bit_length())
print("fib(10..15):", [fib_pair(n)[0] for n in range(10, 16)])
print("fib(100) =", fib_pair(100)[0])
print("fib(1000) has", len(str(fib_pair(1000)[0])), "digits; starts", str(fib_pair(1000)[0])[:12])

a, b = 0, 1
for _ in range(300):
    a, b = b, a + b
print("iter fib(300) match:", a == fib_pair(300)[0])

digits = pi_digits(120)
s = "".join(map(str, digits))
print("pi:", s[0] + "." + s[1:])
for i in range(0, 120, 30):
    print(f"{i:3d}: {s[i:i + 30]}")

print("2**200 =", 2 ** 200)
print("3**-2 =", 3 ** -2)
print("pow mod:", pow(7, 10 ** 18, 1000000007), pow(3, -1, 11))
x = 10 ** 60 + 12345
print(divmod(x, 10 ** 30), divmod(-x, 10 ** 30 + 7))
print(x // -7, x % -7, -x // 7, -x % 7)
print("isqrt:", isqrt_newton(10 ** 60), isqrt_newton(2 * 10 ** 40), isqrt_newton(999))
print(hex(2 ** 70 + 255), oct(8 ** 10), bin(10 ** 5))
print(int("123456789012345678901234567890") * 3, int("-ff", 16), int("zz", 36))
print((1 << 100) | 1, (1 << 100) >> 98, -(1 << 70) >> 68, (2 ** 80 - 1) & 0xFFFF, 5 ^ (1 << 65))
print(float(2 ** 70), int(1e22), 10 ** 25 / 10 ** 5, 10 ** 400 // 10 ** 398)
print(abs(-(10 ** 30)), max(10 ** 20, 10 ** 19), sum([10 ** 18] * 5), (10 ** 20).bit_length())
print(f"{2 ** 64:,} {2 ** 64:x} {2 ** 64:_d} {-(2 ** 64):>30}")
print(str(10 ** 40).count("0"), (10 ** 40 - 1) % 9, 12345678901234567890 % 97)

def collatz_peak(n):
    peak = n
    steps = 0
    while n != 1:
        n = n * 3 + 1 if n & 1 else n >> 1
        peak = max(peak, n)
        steps += 1
    return steps, peak


print(collatz_peak(27), collatz_peak(2 ** 100 + 1)[0])
cat = [1]
for n in range(30):
    cat.append(cat[-1] * 2 * (2 * n + 1) // (n + 2))
print(cat[-3:])
from_digits = 0
for d in digits[:40]:
    from_digits = from_digits * 10 + d
print(from_digits, from_digits % 1000003)
