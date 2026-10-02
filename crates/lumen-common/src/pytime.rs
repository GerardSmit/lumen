//! Timestamp arithmetic of CPython's `_PyTime_t` (signed 64-bit nanoseconds): rounding modes,
//! conversions between seconds, nanoseconds, `timeval` and `timespec`.

pub const SEC_TO_NS: i64 = 1_000_000_000;
pub const MS_TO_NS: i64 = 1_000_000;
pub const US_TO_NS: i64 = 1_000;
const SEC_TO_US: i64 = 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Round {
    Floor,
    Ceiling,
    HalfEven,
    Up,
}

impl Round {
    pub fn from_i64(v: i64) -> Option<Round> {
        match v {
            0 => Some(Round::Floor),
            1 => Some(Round::Ceiling),
            2 => Some(Round::HalfEven),
            3 => Some(Round::Up),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeError {
    Overflow,
    Nan,
}

pub type TimeResult<T> = Result<T, TimeError>;

/// `_PyTime_Round`: rounds a double to an integral double.
pub fn round_f64(x: f64, round: Round) -> f64 {
    match round {
        Round::HalfEven => {
            let rounded = x.round();
            if (x - rounded).abs() == 0.5 {
                2.0 * (x / 2.0).round()
            } else {
                rounded
            }
        }
        Round::Ceiling => x.ceil(),
        Round::Floor => x.floor(),
        Round::Up => {
            if x >= 0.0 {
                x.ceil()
            } else {
                x.floor()
            }
        }
    }
}

/// `t / k` rounded according to `round`.
pub fn divide(t: i64, k: i64, round: Round) -> i64 {
    let mut q = t / k;
    let r = t % k;
    if r == 0 {
        return q;
    }
    let step = if t >= 0 { 1 } else { -1 };
    match round {
        Round::HalfEven => {
            let twice = (r as i128).abs() * 2;
            let k = k as i128;
            if twice > k || (twice == k && q % 2 != 0) {
                q += step;
            }
        }
        Round::Ceiling => {
            if t > 0 {
                q += 1;
            }
        }
        Round::Floor => {
            if t < 0 {
                q -= 1;
            }
        }
        Round::Up => q += step,
    }
    q
}

pub fn from_seconds(secs: i64) -> TimeResult<i64> {
    secs.checked_mul(SEC_TO_NS).ok_or(TimeError::Overflow)
}

/// `_PyTime_FromSecondsObject` for a float.
pub fn from_seconds_f64(d: f64, round: Round) -> TimeResult<i64> {
    if d.is_nan() {
        return Err(TimeError::Nan);
    }
    let d = round_f64(d * SEC_TO_NS as f64, round);
    if !(-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&d) {
        return Err(TimeError::Overflow);
    }
    Ok(d as i64)
}

/// `_PyTime_AsSecondsDouble`.
pub fn as_seconds_f64(t: i64) -> f64 {
    if t % SEC_TO_NS == 0 {
        (t / SEC_TO_NS) as f64
    } else {
        t as f64 / 1e9
    }
}

/// `_PyTime_AsTimeval`: seconds and microseconds, microseconds in `0..1_000_000`.
pub fn as_timeval(t: i64, round: Round) -> (i64, i64) {
    let us = divide(t, US_TO_NS, round);
    let mut secs = us / SEC_TO_US;
    let mut rem = us % SEC_TO_US;
    if rem < 0 {
        rem += SEC_TO_US;
        secs -= 1;
    }
    (secs, rem)
}

/// `_PyTime_AsTimespec`: seconds and nanoseconds, nanoseconds in `0..1_000_000_000`.
pub fn as_timespec(t: i64) -> (i64, i64) {
    let mut secs = t / SEC_TO_NS;
    let mut nsec = t % SEC_TO_NS;
    if nsec < 0 {
        nsec += SEC_TO_NS;
        secs -= 1;
    }
    (secs, nsec)
}

/// `_PyTime_ObjectToDenominator` for a float: whole seconds and the fraction in `0..denominator`.
pub fn split_f64(d: f64, denominator: i64, round: Round) -> TimeResult<(i64, i64)> {
    if d.is_nan() {
        return Err(TimeError::Nan);
    }
    let mut intpart = d.trunc();
    let mut floatpart = (d - intpart) * denominator as f64;
    floatpart = round_f64(floatpart, round);
    if floatpart >= denominator as f64 {
        floatpart -= denominator as f64;
        intpart += 1.0;
    } else if floatpart < 0.0 {
        floatpart += denominator as f64;
        intpart -= 1.0;
    }
    if !(-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&intpart) {
        return Err(TimeError::Overflow);
    }
    Ok((intpart as i64, floatpart as i64))
}

/// `_PyTime_ObjectToTime_t` for a float.
pub fn time_t_f64(d: f64, round: Round) -> TimeResult<i64> {
    if d.is_nan() {
        return Err(TimeError::Nan);
    }
    let v = round_f64(d, round);
    if !(-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&v) {
        return Err(TimeError::Overflow);
    }
    Ok(v as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn divide_rounds() {
        assert_eq!(divide(1500, 1000, Round::HalfEven), 2);
        assert_eq!(divide(2500, 1000, Round::HalfEven), 2);
        assert_eq!(divide(-1500, 1000, Round::HalfEven), -2);
        assert_eq!(divide(-1, 1000, Round::Floor), -1);
        assert_eq!(divide(1, 1000, Round::Ceiling), 1);
        assert_eq!(divide(-1, 1000, Round::Up), -1);
        assert_eq!(divide(1, 1000, Round::Up), 1);
    }

    #[test]
    fn timeval_normalises() {
        assert_eq!(as_timeval(-1, Round::Floor), (-1, 999_999));
        assert_eq!(as_timespec(-1), (-1, 999_999_999));
    }
}
