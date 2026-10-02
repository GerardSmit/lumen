# float format()/round()/% digit rounding over random doubles; expected output from CPython.
import random, struct
r = random.Random(1234)
vals = [0.0, -0.0, 0.5, 1.5, 2.5, 0.125, 0.375, 1e16, 1e-5, 9.995, 0.15, 1.005, 123456789.0, 5e-324,
        1.7976931348623157e308, 0.0001, 0.00001, 999999.5, 9.5, 99.95, 1e22, 1e21, 123.456, 2**53+0.0]
for _ in range(30):
    vals.append(struct.unpack('d', struct.pack('Q', r.getrandbits(63)))[0])
    vals.append(r.uniform(-1000, 1000))
    vals.append(round(r.uniform(0, 100), r.randint(0, 4)) + 0.5 * 10**-r.randint(0, 5))
specs = ['', '.0f', '.1f', '.2f', '.3f', '.10f', 'f', 'e', '.0e', '.1e', '.3e', '#.0e', '.17e', 'g', '.0g', '.1g',
         '.2g', '.3g', '.6g', '.12g', '.17g', '#g', '#.3g', '.0%', '.2%', '%', 'G', 'E', '.5', '.1', '#.0f', 'n', '+.3e', '015.4g', ',.2f']
out = []
for v in vals:
    for s in specs:
        try:
            out.append(format(v, s))
        except Exception as e:
            out.append(type(e).__name__)
    for n in (0, 1, 2, 3, 5, 10):
        out.append(repr(round(v, n)))
    out.append('%.3f %e %g %.0e %#.2g %r' % (v, v, v, v, v, v))
for line in out:
    print(line)
