//! C-locale `strftime` with the conventions of the BSD C library (macOS): `-`, `_` and `0` padding
//! flags, ignored `E`/`O` modifiers, an unknown conversion printed without its `%`, and years
//! outside 0..=9999 written the way `_yconv` writes them.

use crate::civil::{is_leap, Tm};

const DAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const MONTHS: [&str; 12] = [
    "January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November",
    "December",
];

/// The zone-dependent conversions, resolved by the caller from its time-zone rules.
#[derive(Clone, Debug, Default)]
pub struct ZoneFields {
    /// `%Z`.
    pub name: String,
    /// `%z` in seconds east of UTC; `None` writes nothing.
    pub offset: Option<i64>,
    /// `%s`: the instant as seconds since the epoch.
    pub epoch: i64,
}

#[derive(Clone, Copy, PartialEq)]
enum Pad {
    Default,
    Less,
    Space,
    Zero,
}

/// Formats `tm` (whose fields the caller has range-checked) by `fmt`.
pub fn strftime(fmt: &str, tm: &Tm, zone: &ZoneFields) -> String {
    let mut out = String::new();
    format(fmt, tm, zone, &mut out);
    out
}

fn format(fmt: &str, tm: &Tm, zone: &ZoneFields, out: &mut String) {
    let chars: Vec<char> = fmt.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '%' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let mut pad = Pad::Default;
        let mut modifier = false;
        loop {
            i += 1;
            let Some(&c) = chars.get(i) else {
                out.push(chars[i - 1]);
                break;
            };
            match c {
                'E' | 'O' if !modifier => modifier = true,
                '-' if pad == Pad::Default && !modifier => pad = Pad::Less,
                '_' if pad == Pad::Default => pad = Pad::Space,
                '0' if pad == Pad::Default => pad = Pad::Zero,
                _ => {
                    conversion(c, pad, tm, zone, out);
                    i += 1;
                    break;
                }
            }
        }
    }
}

/// `n` in C's `%02d`/`%2d`/`%d` style for the padding flag, `width` wide by default.
fn num(n: i64, width: usize, default_zero: bool, pad: Pad, out: &mut String) {
    let zero = match pad {
        Pad::Default => default_zero,
        Pad::Zero => true,
        Pad::Space => false,
        Pad::Less => {
            out.push_str(&n.to_string());
            return;
        }
    };
    let s = if zero { format!("{n:0width$}") } else { format!("{n:width$}") };
    out.push_str(&s);
}

/// BSD `_yconv`: the century (`top`) and/or the two-digit year (`yy`) of `year`.
fn yconv(year: i64, top: bool, yy: bool, out: &mut String) {
    let a = year - 1900;
    let mut trail = a % 100;
    let mut lead = a / 100 + 19;
    if trail < 0 && lead > 0 {
        trail += 100;
        lead -= 1;
    } else if lead < 0 && trail > 0 {
        trail -= 100;
        lead += 1;
    }
    if top {
        if lead == 0 && trail < 0 {
            out.push_str("-0");
        } else {
            out.push_str(&format!("{lead:02}"));
        }
    }
    if yy {
        out.push_str(&format!("{:02}", trail.abs()));
    }
}

/// `(ISO week, ISO year)` from the year, day of year and weekday fields as BSD computes them.
fn iso_week(tm: &Tm) -> (i64, i64) {
    let mut year = tm.year;
    let mut yday = tm.yday as i64 - 1;
    let wday = (tm.wday as i64 + 1) % 7;
    loop {
        let len = if is_leap(year) { 366 } else { 365 };
        let bot = (yday + 11 - wday).rem_euclid(7) - 3;
        let mut top = bot - len % 7;
        if top < -3 {
            top += 7;
        }
        top += len;
        if yday >= top {
            return (1, year + 1);
        }
        if yday >= bot {
            return (1 + (yday - bot) / 7, year);
        }
        year -= 1;
        yday += if is_leap(year) { 366 } else { 365 };
    }
}

fn conversion(c: char, pad: Pad, tm: &Tm, zone: &ZoneFields, out: &mut String) {
    let sunday_wday = ((tm.wday + 1).rem_euclid(7)) as usize;
    let mon = (tm.mon - 1).clamp(0, 11) as usize;
    let yday0 = tm.yday as i64 - 1;
    match c {
        'A' => out.push_str(DAYS[sunday_wday]),
        'a' => out.push_str(&DAYS[sunday_wday][..3]),
        'B' => out.push_str(MONTHS[mon]),
        'b' | 'h' => out.push_str(&MONTHS[mon][..3]),
        'C' => yconv(tm.year, true, false, out),
        'c' => format("%a %b %e %H:%M:%S %Y", tm, zone, out),
        'D' | 'x' => format("%m/%d/%y", tm, zone, out),
        'd' => num(tm.mday as i64, 2, true, pad, out),
        'e' => num(tm.mday as i64, 2, false, pad, out),
        'F' => format("%Y-%m-%d", tm, zone, out),
        'H' => num(tm.hour as i64, 2, true, pad, out),
        'I' => num(((tm.hour + 11) % 12 + 1) as i64, 2, true, pad, out),
        'j' => num(tm.yday as i64, 3, true, pad, out),
        'k' => num(tm.hour as i64, 2, false, pad, out),
        'l' => num(((tm.hour + 11) % 12 + 1) as i64, 2, false, pad, out),
        'M' => num(tm.min as i64, 2, true, pad, out),
        'm' => num(tm.mon as i64, 2, true, pad, out),
        'n' => out.push('\n'),
        'p' => out.push_str(if tm.hour >= 12 { "PM" } else { "AM" }),
        'R' => format("%H:%M", tm, zone, out),
        'r' => format("%I:%M:%S %p", tm, zone, out),
        'S' => num(tm.sec as i64, 2, true, pad, out),
        's' => out.push_str(&zone.epoch.to_string()),
        'T' | 'X' => format("%H:%M:%S", tm, zone, out),
        't' => out.push('\t'),
        'U' => num((yday0 + 7 - sunday_wday as i64) / 7, 2, true, pad, out),
        'u' => out.push_str(&(tm.wday + 1).to_string()),
        'V' => num(iso_week(tm).0, 2, true, pad, out),
        'G' => yconv(iso_week(tm).1, true, true, out),
        'g' => yconv(iso_week(tm).1, false, true, out),
        'v' => format("%e-%b-%Y", tm, zone, out),
        'W' => num((yday0 + 7 - (sunday_wday as i64 + 6) % 7) / 7, 2, true, pad, out),
        'w' => out.push_str(&sunday_wday.to_string()),
        'Y' => yconv(tm.year, true, true, out),
        'y' => yconv(tm.year, false, true, out),
        'Z' => out.push_str(&zone.name),
        'z' => {
            if let Some(off) = zone.offset {
                out.push(if off < 0 { '-' } else { '+' });
                let m = off.abs() / 60;
                num(m / 60 * 100 + m % 60, 4, true, pad, out);
            }
        }
        '+' => format("%a %b %e %H:%M:%S %Z %Y", tm, zone, out),
        other => out.push(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tm(year: i64, mon: i32, mday: i32, hour: i32, wday: i32, yday: i32) -> Tm {
        Tm { year, mon, mday, hour, min: 4, sec: 5, wday, yday, ..Tm::default() }
    }

    #[test]
    fn conversions() {
        let z = ZoneFields { name: "CET".into(), offset: Some(3600), epoch: 0 };
        let t = tm(2005, 1, 2, 3, 6, 2);
        assert_eq!(strftime("%c|%D|%j|%U|%W|%V|%G|%g|%u|%w|%z|%Z", &t, &z), "Sun Jan  2 03:04:05 2005|01/02/05|002|01|00|53|2004|04|7|0|+0100|CET");
        assert_eq!(strftime("%-d|%_d|%0e|%-z|%_z|%Ey|%EEd|%E_d|%0-d|%Q|%|%E", &t, &z), "2| 2|02|+100|+ 100|05|Ed| 2|-d|Q||E");
        assert_eq!(strftime("%Y|%C|%y", &tm(-5, 1, 1, 0, 0, 1), &z), "-005|-0|05");
        assert_eq!(strftime("%Y|%C|%y", &tm(-100, 1, 1, 0, 0, 1), &z), "-100|-1|00");
        assert_eq!(strftime("%Y|%C|%y", &tm(12345, 1, 1, 0, 0, 1), &z), "12345|123|45");
        assert_eq!(strftime("%I %l %p", &tm(2024, 1, 1, 0, 0, 1), &z), "12 12 AM");
        assert_eq!(strftime("%%", &t, &z), "%");
    }
}
