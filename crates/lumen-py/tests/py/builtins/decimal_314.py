import _decimal as d
def t(f):
    try: print(f())
    except BaseException as e: print(type(e).__name__, e)
t(lambda: d.IEEE_CONTEXT_MAX_BITS)
t(lambda: d.IEEEContext(64))
t(lambda: d.IEEEContext(32).Emax)
t(lambda: (d.Decimal.from_number(3), d.Decimal.from_number(1.5), d.Decimal.from_number(d.Decimal('2.5'))))
for x in ('x', 1j, '1', None, True):
    t(lambda: d.Decimal.from_number(x))
t(lambda: [c.__mro__ for c in (d.ConversionSyntax, d.DivisionImpossible, d.DivisionUndefined, d.InvalidContext)])
t(lambda: [(c.__module__, c.__name__) for c in (d.ConversionSyntax, d.DivisionImpossible, d.DivisionUndefined, d.InvalidContext)])
for b in (0, 1, 31, 32, 64, 96, 128, 256, 512, 544, 100, -1):
    t(lambda: (lambda c: (b, c.prec, c.Emax, c.Emin, c.rounding, c.clamp, c.capitals, c.flags, sorted(map(str, c.traps))))(d.IEEEContext(b)))
t(lambda: d.IEEEContext("a"))
t(lambda: d.IEEEContext())
class D(d.Decimal): pass
t(lambda: type(D.from_number(2)).__name__)
t(lambda: d.Decimal.from_float(1.5))
t(lambda: d.Context().create_decimal_from_float(1.5))
t(lambda: d.Decimal("x"))
t(lambda: d.getcontext().traps[d.ConversionSyntax])
t(lambda: sorted(n for n in dir(d) if not n.startswith('__')))
