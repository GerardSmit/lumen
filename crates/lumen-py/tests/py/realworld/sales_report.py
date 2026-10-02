"""A monthly sales report: money in Decimal, exact shares as Fractions, descriptive statistics,
top-N with heapq, banding with bisect, a binary export/import with struct, and a formatted text
report built in a StringIO."""

import bisect
import heapq
import io
import operator
import statistics
import struct
from collections import namedtuple
from decimal import ROUND_HALF_EVEN, ROUND_HALF_UP, Decimal, InvalidOperation, getcontext, localcontext
from fractions import Fraction

RAW = """\
order_id,region,rep,product,units,unit_price,discount
1001,North,Alice,Widget,12,19.99,0.05
1002,South,Bob,Gadget,3,149.50,0
1003,North,Carol,Widget,40,19.99,0.10
1004,East,Alice,Gizmo,7,74.25,0
1005,West,Dave,Gadget,1,149.50,0.15
1006,South,Erin,Widget,25,19.99,0
1007,East,Bob,Gizmo,9,74.25,0.05
1008,North,Alice,Gadget,2,149.50,0
1009,West,Carol,Widget,60,18.75,0.12
1010,South,Dave,Gizmo,4,74.25,0
1011,East,Erin,Widget,15,19.99,0.02
1012,West,Alice,Gizmo,11,72.00,0.08
"""

Order = namedtuple("Order", "order_id region rep product units unit_price discount")
CENT = Decimal("0.01")


def load(text):
    buf = io.StringIO(text)
    header = buf.readline().strip().split(",")
    assert header == list(Order._fields), header
    orders = []
    for line in buf:
        f = line.rstrip("\n").split(",")
        orders.append(Order(int(f[0]), f[1], f[2], f[3], int(f[4]), Decimal(f[5]), Decimal(f[6])))
    return orders


def net(o):
    gross = o.unit_price * o.units
    return (gross * (1 - o.discount)).quantize(CENT, rounding=ROUND_HALF_UP)


def main():
    orders = load(RAW)
    print(f"{len(orders)} orders, first: {orders[0]}")
    totals = {o.order_id: net(o) for o in orders}
    revenue = sum(totals.values(), Decimal(0))
    print("revenue:", revenue, repr(revenue), f"{revenue:,.2f}", f"{revenue:>14,}")

    with localcontext() as ctx:
        ctx.prec = 6
        ctx.rounding = ROUND_HALF_EVEN
        print("avg order (6 digits):", revenue / len(orders), "sqrt:", revenue.sqrt())
    print("prec:", getcontext().prec, Decimal("1") / Decimal("7"))
    print("half-even vs half-up:", Decimal("2.675").quantize(CENT), Decimal("2.665").quantize(CENT, ROUND_HALF_EVEN),
          Decimal("2.665").quantize(CENT, ROUND_HALF_UP))
    try:
        Decimal("12.3.4")
    except InvalidOperation as e:
        print("InvalidOperation:", type(e).__name__)
    print(Decimal("1.10") + Decimal("2.20"), 1.10 + 2.20, Decimal(0.1), Decimal("-0.00").is_signed(), Decimal("1E+3").normalize())

    by_region = {}
    for o in orders:
        by_region[o.region] = by_region.get(o.region, Decimal(0)) + totals[o.order_id]
    print("region shares (exact):")
    for region, amount in sorted(by_region.items(), key=operator.itemgetter(1), reverse=True):
        share = Fraction(amount) / Fraction(revenue)
        print(f"  {region:<6}{amount:>10}  {float(share):6.2%}  ~{share.limit_denominator(20)}")
    print("fractions:", Fraction(3, 4) + Fraction(1, 6), Fraction("0.125"), Fraction(1.5), Fraction(22, 7) ** 2,
          round(Fraction(7, 3), 2), Fraction(5, 3).as_integer_ratio(), Fraction(-7, 2) // 1)

    values = [float(v) for v in totals.values()]
    units = [o.units for o in orders]
    print("mean %.3f median %.3f stdev %.3f pstdev %.3f" % (statistics.mean(values), statistics.median(values),
                                                            statistics.stdev(values), statistics.pstdev(values)))
    print("fmean", round(statistics.fmean(values), 4), "geo", round(statistics.geometric_mean(values), 4),
          "harm", round(statistics.harmonic_mean(values), 4))
    print("median_low/high:", statistics.median_low(units), statistics.median_high(units), "mode:",
          statistics.mode([o.product for o in orders]), statistics.multimode([o.rep for o in orders]))
    print("quartiles:", [round(q, 2) for q in statistics.quantiles(values, n=4)])
    print("deciles:", statistics.quantiles(units, n=10, method="inclusive"))
    prices = [float(o.unit_price) for o in orders]
    print("corr(units, price): %.4f" % statistics.correlation(units, prices))
    slope, intercept = statistics.linear_regression(units, values)
    print(f"revenue ~ {slope:.3f} * units + {intercept:.3f}")
    nd = statistics.NormalDist.from_samples(values)
    print(f"normal: mu={nd.mean:.2f} sigma={nd.stdev:.2f} p(<500)={nd.cdf(500):.4f} q90={nd.inv_cdf(0.9):.2f}")
    print("mean of decimals:", statistics.mean(totals.values()), "of fractions:", statistics.mean([Fraction(1, 3), Fraction(1, 6)]))
    print("variance of ints:", statistics.variance(units), statistics.pvariance([1, 2, 3, 4]))
    try:
        statistics.mean([])
    except statistics.StatisticsError as e:
        print("StatisticsError:", e)

    top = heapq.nlargest(3, orders, key=net)
    print("top 3:", [(o.order_id, str(net(o))) for o in top])
    print("smallest 2 by units:", [o.order_id for o in heapq.nsmallest(2, orders, key=operator.attrgetter("units"))])
    queue = []
    for o in orders:
        heapq.heappush(queue, (-o.units, o.order_id, o.rep))
    picked = [heapq.heappop(queue) for _ in range(4)]
    print("pick list:", picked, "left:", len(queue), "next:", queue[0])
    north = sorted(o.order_id for o in orders if o.region == "North")
    south = sorted(o.order_id for o in orders if o.region == "South")
    print("merged ids:", list(heapq.merge(north, south)), list(heapq.merge([5, 3, 1], [4, 2], reverse=True)))
    print("heapify:", (lambda h: (heapq.heapify(h), h)[1])([9, 4, 7, 1, 8, 2]),
          heapq.heappushpop([1, 5, 9], 3), heapq.heapreplace([1, 5, 9], 7))

    bands = [Decimal(100), Decimal(250), Decimal(500), Decimal(1000)]
    labels = ["tiny", "small", "medium", "large", "huge"]
    banded = {}
    for oid, amount in totals.items():
        banded.setdefault(labels[bisect.bisect_right(bands, amount)], []).append(oid)
    print("bands:", {k: banded[k] for k in labels if k in banded})
    ladder = []
    for v in (30, 10, 20, 10, 40):
        bisect.insort(ladder, v)
    print("ladder:", ladder, bisect.bisect_left(ladder, 10), bisect.bisect(ladder, 10),
          bisect.bisect_left(orders, 1007, key=operator.attrgetter("order_id")))

    rec = struct.Struct("<I6sHq")
    blob = io.BytesIO()
    for o in orders:
        blob.write(rec.pack(o.order_id, o.region.encode().ljust(6, b"\0"), o.units, int(net(o) * 100)))
    data = blob.getvalue()
    print("binary:", len(data), "bytes,", rec.size, "per record,", data[:rec.size].hex(" ", 4))
    blob.seek(rec.size * 3)
    print("record 4:", rec.unpack(blob.read(rec.size)), blob.tell())
    restored = [(oid, region.rstrip(b"\0").decode(), u, Decimal(c).scaleb(-2)) for oid, region, u, c in rec.iter_unpack(data)]
    print("roundtrip ok:", all(totals[oid] == amount for oid, _, _, amount in restored))
    print("header:", struct.pack(">4sBH", b"SALE", 2, len(orders)), struct.calcsize("@ihq"), struct.unpack("<d", struct.pack("<d", 0.1)))
    print(struct.pack("!f", 1.5).hex(), struct.unpack(">h", b"\xff\xfe"), struct.pack("<?x3s", True, b"ab"))

    reps = {}
    for o in orders:
        reps.setdefault(o.rep, []).append(o)
    ranking = sorted(reps.items(), key=lambda kv: (-sum(net(o) for o in kv[1]), kv[0]))
    out = io.StringIO()
    out.write(f"{'rep':<8}|{'orders':^8}|{'units':>7}|{'revenue':>12}|{'avg disc':>9}\n")
    out.write("-" * 48 + "\n")
    for rep, rs in ranking:
        rev = sum(net(o) for o in rs)
        disc = sum(o.discount for o in rs) / len(rs)
        out.write(f"{rep:<8}|{len(rs):^8}|{sum(o.units for o in rs):>7}|{rev:>12,.2f}|{float(disc):>9.1%}\n")
    out.write("=" * 48 + "\n")
    print(out.getvalue(), end="")
    print("{:08.3f}|{:+d}|{:e}|{:_x}|{:#o}|{:,}|{!r:^12}|{:*<7}|{:.3g}".format(3.14159, 7, 12345.678, 0xDEADBEEF, 8, 10**7, "x", "ab", 0.000123456))
    print("%-6s|%5.1f%%|%04d|%x|%r" % ("ok", 99.5, 42, 255, Decimal("1.0")))
    print(sorted(["b10", "a2", "B1", "a10"], key=lambda s: (s[0].lower(), int(s[1:]))),
          sorted(orders, key=operator.attrgetter("region", "rep"))[0].order_id,
          [o.order_id for o in sorted(orders, key=lambda o: o.units, reverse=True)][:3])
    print(format(Decimal("1234.5"), ">12,.3f"), format(Fraction(1, 3), ".5f") if hasattr(Fraction, "__format__") else "", f"{Decimal('0.000012'):.2e}")


main()
