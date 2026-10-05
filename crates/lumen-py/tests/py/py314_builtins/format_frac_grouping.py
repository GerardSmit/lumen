from decimal import Decimal as D


def t(f):
    try:
        print(repr(f()))
    except BaseException as e:
        print(type(e).__name__, e)


t(lambda: format(1234.56789, '.3_f'))
t(lambda: format(1234.56789, '.6,f'))
t(lambda: format(1234.56789, '.6_e'))
t(lambda: f"{1234.56789:.3_f}")
t(lambda: '{:.{}_f}'.format(1234.56789, 3))
t(lambda: format(1234.56789, ',.3_f'))
t(lambda: format(1234567.891, '020,.3_f'))
t(lambda: format(1234567.891, '030,.3_f'))
t(lambda: format(1234567.891, '030_.3_f'))
t(lambda: format(1234567.891, '=30_.3_f'))
t(lambda: format(-0.0, 'z.3_f'))
t(lambda: format(float('-inf'), 'z.3_f'))
t(lambda: format(float('nan'), '020,.3_f'))
t(lambda: format(float('-inf'), '020,'))
t(lambda: format(1234.56789, '.,f'))
t(lambda: format(1234.56789, '._f'))
t(lambda: format(1234.56789, '.,6f'))
t(lambda: format(1234.56789, '.3_,f'))
t(lambda: format(1234.56789, '.3,_f'))
t(lambda: format(1234.56789, '.3__f'))
t(lambda: format(1234.56789, '.3_n'))
t(lambda: format(1234.56789, ',n'))
t(lambda: format(1234.56789, ',x'))
t(lambda: format(1234567, '.3_f'))
t(lambda: format(1234567, ',.3_f'))
t(lambda: format(1234567, '.3_d'))
t(lambda: format(1234567, '._d'))
t(lambda: format(1234567, ',c'))
t(lambda: format(1234567, ',x'))
t(lambda: format(1234567, '_x'))
t(lambda: format('abc', '.3_s'))
t(lambda: format('abc', ',s'))
for v in [1234.56789123, 1234567.12345678, -0.000123456, 1e22, 1.5, float('nan'), 0.0, 1e-7]:
    for s in ['.6_f', '.6,f', '.6_e', '.6,E', '.6_g', '.6,%', '.3_%', '.6_', '_.2f', ',._f', '.,', '.12_f', '.0_f']:
        t(lambda: format(v, s))

for v in [complex(1234.56789, 1234567.12345678), complex(0, 1234.5678), complex(-0.0, 1e22), complex(1, 2)]:
    for s in ['.6_f', '.6,e', '.3_', '_.2f', ',.3_f', '25.3_f', '^25,.9_f', '.3_s', '.3_n', '.3_%', '.3_r', '.3_x']:
        t(lambda: format(v, s))

for v in [D('1234.56789123'), D('1234567.12345678'), D('-0.000123456'), D('12E+5'), D('1.5'), D('NaN'), D('-Infinity'), D('0.00')]:
    for s in ['_.2f', ',.2f', '.6_f', '.6,f', ',.6_f', '_.6,f', '.,f', '._f', '.,', '.6_e', '.6,E', '.6_g', '.6,%', '.3_%',
              '.3_', '.3_n', ',.3_n', '020,.6_f', '020_.6_f', '>20,.6_f', '^30_.9_f', '+,.6_f', '.0_f', '.12_f', '.2,,f',
              '.2__f', '.2_,f', '_,.2f', '.f', '_']:
        t(lambda: format(v, s))
