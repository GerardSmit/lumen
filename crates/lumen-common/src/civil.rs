//! Proleptic-Gregorian calendar arithmetic over day counts since 1970-01-01.

#[inline]
pub fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// Days in month `m` (1..=12) of year `y`; 0 for an invalid month.
#[inline]
pub fn days_in_month(y: i64, m: u8) -> u8 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => 28 + is_leap(y) as u8,
        _ => 0,
    }
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm); `m` may be any month 1..=12.
#[inline]
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// `(year, month 1..=12, day 1..=31)` for a day count since 1970-01-01.
#[inline]
pub fn civil_from_days(z: i64) -> (i64, u8, u8) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u8, d as u8)
}

/// ISO weekday of a day count: 1 = Monday .. 7 = Sunday.
#[inline]
pub fn iso_weekday(days: i64) -> i64 {
    // 1970-01-01 was a Thursday (4).
    (days + 3).rem_euclid(7) + 1
}

/// 1-based day of the year.
#[inline]
pub fn day_of_year(y: i64, m: i64, d: i64) -> i64 {
    days_from_civil(y, m, d) - days_from_civil(y, 1, 1) + 1
}

/// `(ISO week number, ISO week-numbering year)`.
pub fn iso_week(y: i64, m: i64, d: i64) -> (i64, i64) {
    let z = days_from_civil(y, m, d);
    let thursday = z + (4 - iso_weekday(z));
    let (ty, _, _) = civil_from_days(thursday);
    ((thursday - days_from_civil(ty, 1, 1)) / 7 + 1, ty)
}

/// Broken-down time like C's `struct tm`, with the full year, a 1-based month, `wday` 0 = Monday
/// and a 1-based `yday` (the conventions of Python's `struct_time`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tm {
    pub year: i64,
    pub mon: i32,
    pub mday: i32,
    pub hour: i32,
    pub min: i32,
    pub sec: i32,
    pub wday: i32,
    pub yday: i32,
    /// Positive in daylight-saving time, 0 outside it, negative when unknown.
    pub isdst: i32,
    /// Seconds east of UTC.
    pub gmtoff: i64,
    /// The zone abbreviation, when known.
    pub zone: Option<String>,
}

impl Tm {
    /// The fields of `sec` seconds since the epoch shifted by `gmtoff` (UTC when 0).
    pub fn from_epoch(sec: i64, gmtoff: i64) -> Tm {
        let local = sec + gmtoff;
        let days = local.div_euclid(86_400);
        let rem = local.rem_euclid(86_400);
        let (y, m, d) = civil_from_days(days);
        Tm {
            year: y,
            mon: m as i32,
            mday: d as i32,
            hour: (rem / 3600) as i32,
            min: (rem / 60 % 60) as i32,
            sec: (rem % 60) as i32,
            wday: (iso_weekday(days) - 1) as i32,
            yday: day_of_year(y, m as i64, d as i64) as i32,
            isdst: 0,
            gmtoff,
            zone: None,
        }
    }

    /// Seconds since the epoch of these fields read as UTC (`timegm`); out-of-range fields carry
    /// over like C's `mktime` (month 13 is January of the next year).
    pub fn to_epoch_utc(&self) -> i64 {
        let m0 = self.mon as i64 - 1;
        let y = self.year + m0.div_euclid(12);
        let days = days_from_civil(y, m0.rem_euclid(12) + 1, 1) + self.mday as i64 - 1;
        days * 86_400 + self.hour as i64 * 3600 + self.min as i64 * 60 + self.sec as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        for z in (-800_000..800_000).step_by(37) {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m as i64, d as i64), z);
            assert!(d <= days_in_month(y, m));
        }
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn weeks() {
        assert_eq!(iso_weekday(0), 4);
        assert_eq!(iso_weekday(days_from_civil(2024, 1, 1)), 1);
        assert_eq!(iso_week(2021, 1, 3), (53, 2020));
        assert_eq!(iso_week(2024, 12, 30), (1, 2025));
        assert_eq!(day_of_year(2024, 12, 31), 366);
        assert!(is_leap(2000) && !is_leap(1900));
    }

    #[test]
    fn broken_down() {
        let t = Tm::from_epoch(1_500_000_000, 0);
        assert_eq!(
            (t.year, t.mon, t.mday, t.hour, t.min, t.sec, t.wday, t.yday),
            (2017, 7, 14, 2, 40, 0, 4, 195)
        );
        assert_eq!(t.to_epoch_utc(), 1_500_000_000);
        assert_eq!(Tm::from_epoch(-1, 0).to_epoch_utc(), -1);
        let t = Tm {
            year: 2023,
            mon: 13,
            mday: 1,
            ..Tm::default()
        };
        assert_eq!(t.to_epoch_utc(), 1_704_067_200);
    }
}
