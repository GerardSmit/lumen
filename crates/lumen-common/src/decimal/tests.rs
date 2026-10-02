use super::*;

fn d(s: &str) -> Decimal {
    Decimal::parse(s).expect("valid numeric string")
}

fn ctx(prec: i64) -> Context {
    Context { prec, ..Context::default() }
}

fn sci(x: &Decimal) -> String {
    x.to_sci_string(true)
}

#[test]
fn parse_and_print() {
    assert_eq!(sci(&d("1.2300")), "1.2300");
    assert_eq!(sci(&d("1e3")), "1E+3");
    assert_eq!(sci(&d("0.00000123")), "0.00000123");
    assert_eq!(sci(&d("0.0000001")), "1E-7");
    assert_eq!(sci(&d("-Inf")), "-Infinity");
    assert_eq!(sci(&d("sNaN0042")), "sNaN42");
    assert_eq!(sci(&d("-nan")), "-NaN");
    assert_eq!(d("12345E3").to_eng_string(true), "12.345E+6");
    assert_eq!(d("0E+4").to_eng_string(true), "0.00E+6");
    assert!(Decimal::parse("1e").is_err());
    assert!(Decimal::parse(".").is_err());
    assert!(Decimal::parse("1 2").is_err());
}

#[test]
fn addition_rounds_and_flags() {
    let c = ctx(5);
    let mut st = 0;
    let r = d("1.23456789").add(&d("0"), &c, &mut st);
    assert_eq!(sci(&r), "1.2346");
    assert_eq!(st, flag::INEXACT | flag::ROUNDED);
    let mut st = 0;
    assert_eq!(sci(&d("1E+20").add(&d("1E-20"), &c, &mut st)), "1.0000E+20");
    assert_eq!(st, flag::INEXACT | flag::ROUNDED);
}

#[test]
fn division_and_integer_division() {
    let c = ctx(5);
    let mut st = 0;
    assert_eq!(sci(&d("1").div(&d("3"), &c, &mut st)), "0.33333");
    let mut st = 0;
    assert_eq!(sci(&d("10").div(&d("4"), &c, &mut st)), "2.5");
    assert_eq!(st, 0);
    let mut st = 0;
    let (q, r) = d("7").divmod(&d("-2"), &c, &mut st);
    assert_eq!((sci(&q), sci(&r)), ("-3".to_string(), "1".to_string()));
    let mut st = 0;
    let z = d("1").div(&d("0"), &c, &mut st);
    assert!(z.is_infinite());
    assert_eq!(st, flag::DIVISION_BY_ZERO);
    let mut st = 0;
    let n = d("0").div(&d("0"), &c, &mut st);
    assert!(n.is_nan());
    assert_eq!(st, flag::DIVISION_UNDEFINED);
}

#[test]
fn overflow_and_underflow() {
    let c = Context { prec: 3, emax: 99, emin: -99, ..Context::default() };
    let mut st = 0;
    let r = d("9.99E+99").mul(&d("10"), &c, &mut st);
    assert!(r.is_infinite());
    assert_eq!(st, flag::OVERFLOW | flag::INEXACT | flag::ROUNDED);
    let mut st = 0;
    let r = d("1E-99").mul(&d("1E-5"), &c, &mut st);
    assert!(r.is_zero());
    assert!(st & flag::UNDERFLOW != 0 && st & flag::CLAMPED != 0);
}

#[test]
fn transcendentals_are_correctly_rounded() {
    let c = ctx(10);
    let mut st = 0;
    assert_eq!(sci(&d("2").sqrt(&c, &mut st)), "1.414213562");
    assert_eq!(sci(&d("1").exp(&c, &mut st)), "2.718281828");
    assert_eq!(sci(&d("1.000000001").exp(&c, &mut st)), "2.718281831");
    assert_eq!(sci(&d("10").ln(&c, &mut st)), "2.302585093");
    assert_eq!(sci(&d("1000").log10(&c, &mut st)), "3");
    assert_eq!(sci(&d("2").pow(&d("0.5"), &ctx(6), &mut st)), "1.41421");
    assert_eq!(sci(&d("2").pow(&d("10"), &ctx(6), &mut st)), "1024");
    assert_eq!(sci(&d("4").pow(&d("-1"), &ctx(6), &mut st)), "0.25");
}

#[test]
fn quantize_and_integral() {
    let c = ctx(28);
    let mut st = 0;
    let q = d("1.2345").quantize(&d("0.01"), Rounding::HalfEven, &c, &mut st);
    assert_eq!(sci(&q), "1.23");
    assert_eq!(st, flag::INEXACT | flag::ROUNDED);
    let mut st = 0;
    assert_eq!(sci(&d("2.5").to_integral_value(Rounding::HalfEven, &c, &mut st)), "2");
    assert_eq!(sci(&d("2.5").to_integral_value(Rounding::HalfUp, &c, &mut st)), "3");
    assert_eq!(st, 0);
}

#[test]
fn comparison_and_total_order() {
    assert_eq!(d("1.0").compare_numeric(&d("1")), Some(Ordering::Equal));
    assert_eq!(d("1.0").compare_total_order(&d("1")), Ordering::Less);
    assert_eq!(d("-0").compare_total_order(&d("0")), Ordering::Less);
    assert_eq!(d("NaN").compare_numeric(&d("1")), None);
    assert_eq!(d("-Inf").compare_numeric(&d("-1E+999")), Some(Ordering::Less));
}

#[test]
fn float_roundtrip_and_ratio() {
    assert_eq!(sci(&Decimal::from_f64(0.5).unwrap()), "0.5");
    assert_eq!(sci(&Decimal::from_f64(1e22).unwrap()), "10000000000000000000000");
    assert_eq!(d("0.1").to_f64(), Some(0.1));
    let (n, den) = d("3.14").as_integer_ratio().unwrap();
    assert_eq!((n.to_string_radix(10), den.to_string_radix(10)), ("157".to_string(), "50".to_string()));
}

#[test]
fn numeric_hash_matches_integers() {
    assert_eq!(d("1").numeric_hash(), Some(1));
    assert_eq!(d("-1").numeric_hash(), Some(-2));
    assert_eq!(d("10E-1").numeric_hash(), Some(1));
    assert_eq!(d("Infinity").numeric_hash(), Some(HASH_INF));
    assert_eq!(d("NaN").numeric_hash(), None);
}

#[test]
fn formatting() {
    let c = Context::default();
    let fmt = |v: &str, spec: &str| d(v).format(&parse_format_spec(spec).unwrap(), None, &c).unwrap();
    assert_eq!(fmt("1234567.891", ",.2f"), "1,234,567.89");
    assert_eq!(fmt("1.5", "e"), "1.5e+0");
    assert_eq!(fmt("-1.5", "010.3f"), "-00001.500");
    assert_eq!(fmt("1", "*^7"), "***1***");
    assert_eq!(fmt("0.000001234", "g"), "0.000001234");
    assert_eq!(fmt("1.5", "%"), "150%");
    assert!(parse_format_spec("<<<").is_err());
}

#[test]
fn logical_and_shift() {
    let c = ctx(9);
    let mut st = 0;
    assert_eq!(sci(&d("1100").logical_and(&d("1010"), &c, &mut st)), "1000");
    assert_eq!(sci(&d("123456789").rotate(&d("2"), &c, &mut st)), "345678912");
    assert_eq!(sci(&d("123456789").shift(&d("-2"), &c, &mut st)), "1234567");
    assert_eq!(st, 0);
}
