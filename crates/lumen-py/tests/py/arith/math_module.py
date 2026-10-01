import math
print(math.pi, math.e, math.tau, math.inf, math.nan != math.nan)
print(math.sqrt(16), math.sqrt(2), math.isqrt(99), math.isqrt(10 ** 20), math.cbrt(27) if hasattr(math, "cbrt") else 3.0)
print(math.floor(2.7), math.floor(-2.1), math.ceil(2.1), math.ceil(-2.9), math.trunc(-2.9), math.trunc(2.9))
print(type(math.floor(2.5)).__name__, math.floor(5), math.ceil(10 ** 30 + 0.0) == 10 ** 30)
print(math.fabs(-3), math.copysign(3, -0.0), math.fmod(7, 3), math.fmod(-7, 3), math.remainder(7, 3))
print(math.gcd(12, 18), math.gcd(0, 5), math.gcd(-4, 6), math.gcd(), math.lcm(4, 6), math.lcm(3, 0))
print(math.factorial(0), math.factorial(5), math.factorial(25))
print(math.comb(5, 2), math.comb(52, 5), math.perm(5, 2), math.perm(5))
print(math.exp(0), math.exp(1), math.log(1), math.log(math.e), math.log(8, 2), math.log2(1024), math.log10(1000), math.log1p(0))
print(math.sin(0), math.cos(0), math.tan(0), math.atan2(1, 1), math.atan(1) * 4 == math.pi)
print(round(math.sin(math.pi / 6), 10), round(math.cos(math.pi / 3), 10), math.degrees(math.pi), math.radians(180))
print(math.hypot(3, 4), math.hypot(1, 1), math.pow(2, 10), math.pow(2, 0.5), math.expm1(0))
print(math.isnan(math.nan), math.isinf(-math.inf), math.isfinite(1e308), math.isfinite(math.inf))
print(math.isclose(0.1 + 0.2, 0.3), math.isclose(1, 1.1), math.isclose(1, 1.05, rel_tol=0.1), math.isclose(0, 1e-12, abs_tol=1e-9))
print(math.fsum([0.1] * 10), sum([0.1] * 10), math.prod([1, 2, 3, 4]), math.prod([]), math.prod([2.5, 2]))
print(math.frexp(8.0), math.ldexp(0.5, 4), math.modf(3.75), math.modf(-3.75))
print(math.sinh(0), math.cosh(0), math.tanh(0), math.asin(1), math.acos(1))
print(math.dist((0, 0), (3, 4)), math.copysign(1, -5), math.nextafter(1.0, 2.0) > 1.0)
for expr in ("math.sqrt(-1)", "math.log(0)", "math.log(-1)", "math.factorial(-1)", "math.acos(2)", "math.exp(1000)", "math.isqrt(-1)", "math.fmod(1, 0)"):
    try:
        print(eval(expr))
    except (ValueError, OverflowError) as e:
        print(expr, type(e).__name__)
