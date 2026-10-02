import decimal
from decimal import (
    Decimal, Context, localcontext, getcontext, setcontext,
    ROUND_HALF_EVEN, ROUND_DOWN, ROUND_UP, ROUND_CEILING, ROUND_FLOOR, ROUND_05UP,
    InvalidOperation, DivisionByZero, Inexact, Rounded, Overflow, Underflow,
    DecimalException, FloatOperation, BasicContext, ExtendedContext,
)
from fractions import Fraction


def t(f):
    try:
        print(repr(f()))
    except BaseException as e:
        print(type(e).__name__, e)


print(Decimal("1.10") + Decimal("2.205"))
print(Decimal("1") / Decimal("7"))
print(Decimal("-0.00001234"), Decimal("1E+3"), Decimal("123.456E-10"))
print(Decimal(0.1))
print(Decimal(2**70), Decimal(-5))
print(Decimal((1, (1, 2, 3), -2)))
print(Decimal("1_000.5"), Decimal("  7  "))
print(Decimal("NaN123"), Decimal("-sNaN"), Decimal("-Infinity"))
print(Decimal("2.50").normalize(), Decimal("100").normalize())
print(Decimal("2.5").quantize(Decimal("1")), Decimal("3.5").quantize(Decimal("1")))
print(Decimal("2.675").quantize(Decimal("0.01"), rounding=ROUND_UP))
print(Decimal("-7") // Decimal("2"), Decimal("-7") % Decimal("2"), divmod(Decimal("7"), Decimal("-2")))
print(Decimal("2").sqrt(), Decimal("2").exp(), Decimal("2").ln(), Decimal("2").log10())
print(Decimal(2) ** Decimal("0.5"), Decimal(2) ** 10, pow(Decimal(3), 100, 7))
print(Decimal("1.5").as_tuple(), Decimal("-Inf").as_tuple(), Decimal("NaN7").as_tuple())
print(Decimal("0.75").as_integer_ratio(), Decimal("-120").as_integer_ratio())
print(Decimal("1.23456").adjusted(), Decimal("0.000123").adjusted())
print(hash(Decimal("1.5")) == hash(1.5), hash(Decimal(7)) == hash(7))
print(hash(Decimal("0.1")) == hash(Fraction(1, 10)))
print(Decimal(1) == 1, Decimal("0.5") == 0.5, Decimal("0.5") == Fraction(1, 2))
print(Decimal(1) < 2, Decimal("0.1") < 0.1, Decimal(1) > Fraction(1, 3))
t(lambda: Decimal(1) < "a")
t(lambda: Decimal("abc"))
t(lambda: Decimal(1) + 1.5)
t(lambda: Decimal(1) / 0)
t(lambda: Decimal(0) / 0)
t(lambda: Decimal("1e999999999999999999999"))
t(lambda: Decimal((0, (1, 10), 0)))
t(lambda: Context(prec=0))
t(lambda: Context(rounding="x"))
t(lambda: Decimal(None))
t(lambda: Decimal(1).quantize())

print(format(Decimal("1234567.891"), ",.2f"), format(Decimal("0.00123"), "e"), format(Decimal("1e-7"), ".3g"))
print(format(Decimal("-12.5"), "+010.3f"), format(Decimal("0.5"), "%"), format(Decimal("NaN"), ">8"))
print(f"{Decimal('1234.5'):_.1f}", f"{Decimal('42'):^9}|", f"{Decimal('3.14159'):.0f}")

print(getcontext().prec, getcontext().rounding, getcontext().Emax, getcontext().Emin)
with localcontext() as ctx:
    ctx.prec = 5
    print(Decimal(1) / Decimal(7), getcontext() is ctx)
    ctx.rounding = ROUND_DOWN
    print(Decimal(2) / Decimal(3))
print(Decimal(1) / Decimal(7))
with localcontext(prec=3, rounding=ROUND_CEILING) as c2:
    print(Decimal(2) / Decimal(3), c2.prec)

c = Context(prec=4, traps=[Overflow])
c.clear_flags()
print(c.divide(Decimal(1), Decimal(3)), c.flags[Inexact], c.flags[Rounded])
t(lambda: c.multiply(Decimal("9e999999"), Decimal("9e999999")))
print(c.create_decimal("1.23456789"), c.Etiny(), c.Etop())
print(repr(Context(prec=5, Emax=100, Emin=-100)))
t(lambda: Decimal(1) / Decimal(0) if getcontext().traps[DivisionByZero] else None)
t(lambda: Context(traps=[]).divide(1, 0))
t(lambda: ExtendedContext.divide(Decimal(0), Decimal(0)))
t(lambda: BasicContext.power(Decimal(0), 0))

print([issubclass(c, DecimalException) for c in (InvalidOperation, DivisionByZero, Inexact, Rounded, Overflow, Underflow)])
print(Overflow.__mro__[1:3], FloatOperation.__mro__[1:4])
print(issubclass(DivisionByZero, ZeroDivisionError), issubclass(FloatOperation, TypeError))
print(decimal.MAX_PREC, decimal.MIN_EMIN, decimal.MAX_EMAX, decimal.MIN_ETINY)
print(decimal.HAVE_THREADS, decimal.HAVE_CONTEXTVAR, decimal.__version__)

print(Decimal("1.5").to_integral_value(), Decimal("-1.5").to_integral_exact(rounding=ROUND_FLOOR))
print(round(Decimal("2.5")), round(Decimal("2.567"), 2), int(Decimal("-3.9")))
print(float(Decimal("1.25")), complex(Decimal("2")), bool(Decimal("0E5")))
print(Decimal.from_float(0.5), Decimal.from_float(1e100))
print(Decimal("1.5").fma(2, 3), Decimal("5").compare(Decimal("7")), Decimal("1").compare_total(Decimal("1.0")))
print(Decimal("-0").copy_sign(Decimal(1)), Decimal("3").max(Decimal("4")), Decimal("3").min_mag(Decimal("-4")))
print(Decimal("1.00").next_plus(), Decimal("1").next_toward(Decimal("0")))
print(Decimal(10).logb(), Decimal("1.5").scaleb(3), Decimal("1010").logical_and(Decimal("110")))
print(Decimal("12345").rotate(2), Decimal("12345").shift(-2), Decimal("5").number_class())
print(Decimal("1.1").is_finite(), Decimal("NaN").is_nan(), Decimal("-0").is_signed(), Decimal("1E-999").is_subnormal(Context()))
print(sum([Decimal("0.1")] * 10), max(Decimal("1.1"), Decimal("1.2")))
import pickle
print(pickle.loads(pickle.dumps(Decimal("3.14"))), pickle.loads(pickle.dumps(getcontext())) == getcontext())
import copy
print(copy.copy(Decimal("1.5")), copy.deepcopy(Decimal("2.5")))
print(isinstance(Decimal(1), __import__("numbers").Number), isinstance(Decimal(1), __import__("numbers").Real))
