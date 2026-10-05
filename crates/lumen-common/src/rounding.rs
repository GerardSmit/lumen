//! Rounding of a discarded decimal tail, shared by the decimal arithmetic (`decimal`), the
//! fixed/exponential float formatters (`float::format`) and ECMA-402 number formatting. Each of
//! them classifies what it discards as a [`Tail`] and asks [`round_up`]; digit strings are rounded
//! in place by [`round_ascii`].

use std::cmp::Ordering;

/// The rounding directions of Python's `decimal` plus ECMA-402's `halfCeil` / `halfFloor`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mode {
    /// Away from zero.
    Up,
    /// Towards zero.
    Down,
    Ceiling,
    Floor,
    /// Ties away from zero.
    HalfUp,
    /// Ties towards zero.
    HalfDown,
    HalfEven,
    /// Ties towards positive infinity.
    HalfCeil,
    /// Ties towards negative infinity.
    HalfFloor,
    /// Away from zero when the last kept digit would be 0 or 5, else towards zero.
    Up05,
}

/// What was discarded: whether any of it is non-zero, and how it compares with half a unit of the
/// last kept digit (`Less` when nothing was discarded).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tail {
    pub nonzero: bool,
    pub half: Ordering,
}

impl Tail {
    pub const ZERO: Tail = Tail { nonzero: false, half: Ordering::Less };

    /// The ASCII digit string `digits` read as the fraction `0.digits` of one kept unit.
    pub fn of_digits(digits: &[u8]) -> Tail {
        let Some(&first) = digits.first() else { return Tail::ZERO };
        let rest_nonzero = digits[1..].iter().any(|&d| d != b'0');
        let half = match first.cmp(&b'5') {
            Ordering::Equal if rest_nonzero => Ordering::Greater,
            o => o,
        };
        Tail { nonzero: first != b'0' || rest_nonzero, half }
    }
}

/// Whether the kept magnitude is incremented by one unit. `odd` is the parity of the kept value
/// and `mult5` whether it is a multiple of 5 (both only consulted by the modes that need them).
pub fn round_up(mode: Mode, negative: bool, tail: Tail, odd: bool, mult5: bool) -> bool {
    if !tail.nonzero {
        return false;
    }
    let half = tail.half;
    match mode {
        Mode::Up => true,
        Mode::Down => false,
        Mode::Ceiling => !negative,
        Mode::Floor => negative,
        Mode::HalfUp => half != Ordering::Less,
        Mode::HalfDown => half == Ordering::Greater,
        Mode::HalfEven => half == Ordering::Greater || (half == Ordering::Equal && odd),
        Mode::HalfCeil => half == Ordering::Greater || (half == Ordering::Equal && !negative),
        Mode::HalfFloor => half == Ordering::Greater || (half == Ordering::Equal && negative),
        Mode::Up05 => mult5,
    }
}

/// Adds one unit to the ASCII digit string. Returns whether a carry grew it by a leading `1`.
pub fn increment_ascii(digits: &mut Vec<u8>) -> bool {
    for d in digits.iter_mut().rev() {
        if *d == b'9' {
            *d = b'0';
        } else {
            *d += 1;
            return false;
        }
    }
    digits.insert(0, b'1');
    true
}

/// Rounds the ASCII digit string `digits` (exact, so everything past `keep` is the true tail) to
/// `keep` digits, padding with zeros when shorter. Returns whether a carry added a leading digit
/// (the string is then `keep + 1` long).
pub fn round_ascii(digits: &mut Vec<u8>, keep: usize, mode: Mode, negative: bool) -> bool {
    if digits.len() <= keep {
        digits.resize(keep, b'0');
        return false;
    }
    let tail = Tail::of_digits(&digits[keep..]);
    let last = keep.checked_sub(1).map(|k| digits[k] - b'0');
    let odd = last.is_some_and(|d| d % 2 == 1);
    let mult5 = last.is_none_or(|d| d % 5 == 0);
    digits.truncate(keep);
    round_up(mode, negative, tail, odd, mult5) && increment_ascii(digits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(s: &str, keep: usize, mode: Mode, neg: bool) -> String {
        let mut d = s.as_bytes().to_vec();
        round_ascii(&mut d, keep, mode, neg);
        String::from_utf8(d).unwrap()
    }

    #[test]
    fn ties() {
        assert_eq!(r("25", 1, Mode::HalfEven, false), "2");
        assert_eq!(r("35", 1, Mode::HalfEven, false), "4");
        assert_eq!(r("25", 1, Mode::HalfUp, false), "3");
        assert_eq!(r("25", 1, Mode::HalfDown, false), "2");
        assert_eq!(r("25", 1, Mode::HalfCeil, true), "2");
        assert_eq!(r("25", 1, Mode::HalfFloor, true), "3");
        assert_eq!(r("251", 1, Mode::HalfDown, false), "3");
    }

    #[test]
    fn directed() {
        assert_eq!(r("21", 1, Mode::Up, false), "3");
        assert_eq!(r("21", 1, Mode::Ceiling, true), "2");
        assert_eq!(r("21", 1, Mode::Floor, true), "3");
        assert_eq!(r("20", 1, Mode::Up, false), "2");
        assert_eq!(r("51", 1, Mode::Up05, false), "6");
        assert_eq!(r("31", 1, Mode::Up05, false), "3");
    }

    #[test]
    fn carry() {
        assert_eq!(r("99", 1, Mode::HalfUp, false), "10");
        assert_eq!(r("9", 0, Mode::Up, false), "1");
        assert_eq!(r("5", 3, Mode::Up, false), "500");
    }
}
