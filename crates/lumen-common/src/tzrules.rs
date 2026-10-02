//! Time-zone rules from a TZif file (RFC 8536): its transitions, local time types and POSIX `TZ`
//! footer, resolved for a local or a UTC wall-clock time. The algorithms of CPython's `_zoneinfo`.
//!
//! Local time types are referred to by index ([`Applies`]), so a binding keeps its own objects
//! for them.

use crate::civil::{days_from_civil, days_in_month, is_leap};

/// A transition date of a POSIX `TZ` rule, with its local time of day in seconds.
#[derive(Clone, Debug, PartialEq)]
pub enum TransitionRule {
    /// `Jn` (`julian`, 1..=365, February 29th never counted) or `n` (0..=365).
    Day { julian: bool, day: i64, secs: i64 },
    /// `Mm.w.d`: day `d` (0 = Sunday) of week `w` (5 = last) of month `m`.
    Calendar { month: i64, week: i64, day: i64, secs: i64 },
}

impl TransitionRule {
    /// The local wall-clock seconds since 1970-01-01 of this transition in `year`.
    pub fn year_to_timestamp(&self, year: i64) -> i64 {
        match *self {
            TransitionRule::Day { julian, day, secs } => {
                let days_before_year = days_from_civil(year, 1, 1) - 1;
                let day = if julian && day >= 59 && is_leap(year) { day + 1 } else { day };
                (days_before_year + day) * 86_400 + secs
            }
            TransitionRule::Calendar { month, week, day, secs } => {
                let first = days_from_civil(year, month, 1);
                let first_day = (first + 3).rem_euclid(7);
                let mut month_day = (day - (first_day + 1)).rem_euclid(7) + 1 + (week - 1) * 7;
                if month_day > days_in_month(year, month as u8) as i64 {
                    month_day -= 7;
                }
                (first + month_day - 1) * 86_400 + secs
            }
        }
    }
}

/// The daylight-saving part of a POSIX `TZ` string.
#[derive(Clone, Debug, PartialEq)]
pub struct DstRule {
    pub abbr: String,
    /// Seconds east of UTC.
    pub offset: i64,
    pub start: TransitionRule,
    pub end: TransitionRule,
}

/// A POSIX `TZ` string: `std offset[dst[offset],start[/time],end[/time]]`.
#[derive(Clone, Debug, PartialEq)]
pub struct PosixTz {
    pub std_abbr: String,
    /// Seconds east of UTC.
    pub std_offset: i64,
    pub dst: Option<DstRule>,
}

/// Why a `TZ` string did not parse (CPython's wording, followed by the string's repr).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TzStrError {
    StdFormat,
    StdOffset,
    DstFormat,
    DstOffset,
    MissingRules,
    MalformedRule,
    Extraneous,
}

impl TzStrError {
    pub fn message(self) -> &'static str {
        match self {
            TzStrError::StdFormat => "Invalid STD format in",
            TzStrError::StdOffset => "Invalid STD offset in",
            TzStrError::DstFormat => "Invalid DST format in",
            TzStrError::DstOffset => "Invalid DST offset in",
            TzStrError::MissingRules => "Missing transition rules in TZ string:",
            TzStrError::MalformedRule => "Malformed transition rule in TZ string:",
            TzStrError::Extraneous => "Extraneous characters at end of TZ string:",
        }
    }
}

struct Cursor<'a> {
    s: &'a [u8],
    i: usize,
}

impl Cursor<'_> {
    fn peek(&self) -> u8 {
        self.s.get(self.i).copied().unwrap_or(0)
    }

    fn digits(&mut self, min: usize, max: usize) -> Option<i64> {
        let mut v = 0;
        for k in 0..max {
            let c = self.peek();
            if !c.is_ascii_digit() {
                return (k >= min).then_some(v);
            }
            v = v * 10 + (c - b'0') as i64;
            self.i += 1;
        }
        Some(v)
    }

    fn abbr(&mut self) -> Option<String> {
        let start;
        let end;
        if self.peek() == b'<' {
            self.i += 1;
            start = self.i;
            while self.peek() != b'>' {
                let c = self.peek();
                if !c.is_ascii_alphanumeric() && c != b'+' && c != b'-' {
                    return None;
                }
                self.i += 1;
            }
            end = self.i;
            self.i += 1;
        } else {
            start = self.i;
            while self.peek().is_ascii_alphabetic() {
                self.i += 1;
            }
            end = self.i;
            if end == start {
                return None;
            }
        }
        Some(String::from_utf8_lossy(&self.s[start..end]).into_owned())
    }

    /// `[+|-]h[hh][:mm[:ss]]` as signed `(h, m, s)` seconds.
    fn time(&mut self) -> Option<(i64, i64, i64)> {
        let sign = match self.peek() {
            b'-' => {
                self.i += 1;
                -1
            }
            b'+' => {
                self.i += 1;
                1
            }
            _ => 1,
        };
        let h = self.digits(1, 3)? * sign;
        let (mut m, mut s) = (0, 0);
        if self.peek() == b':' {
            self.i += 1;
            m = self.digits(2, 2)? * sign;
            if self.peek() == b':' {
                self.i += 1;
                s = self.digits(2, 2)? * sign;
            }
        }
        Some((h, m, s))
    }

    /// A UTC offset, negated as POSIX defines it ("EST5" is UTC-5).
    fn delta(&mut self) -> Option<i64> {
        let (h, m, s) = self.time()?;
        if !(-24..=24).contains(&h) {
            return None;
        }
        Some(-(h * 3600 + m * 60 + s))
    }

    fn rule(&mut self) -> Option<TransitionRule> {
        let mut time = (2, 0, 0);
        if self.peek() == b'M' {
            self.i += 1;
            let month = self.digits(1, 2)?;
            if self.peek() != b'.' {
                return None;
            }
            self.i += 1;
            let week = self.digits(1, 1)?;
            if self.peek() != b'.' {
                return None;
            }
            self.i += 1;
            let day = self.digits(1, 1)?;
            if self.peek() == b'/' {
                self.i += 1;
                time = self.time()?;
            }
            if !(1..=12).contains(&month) || !(1..=5).contains(&week) || !(0..=6).contains(&day) || !(-167..=167).contains(&time.0) {
                return None;
            }
            let secs = time.0 * 3600 + time.1 * 60 + time.2;
            Some(TransitionRule::Calendar { month, week, day, secs })
        } else {
            let julian = self.peek() == b'J';
            if julian {
                self.i += 1;
            }
            let day = self.digits(1, 3)?;
            if self.peek() == b'/' {
                self.i += 1;
                time = self.time()?;
            }
            if day < julian as i64 || day > 365 || !(-167..=167).contains(&time.0) {
                return None;
            }
            let secs = time.0 * 3600 + time.1 * 60 + time.2;
            Some(TransitionRule::Day { julian, day, secs })
        }
    }
}

/// Parses the `TZ` footer of a TZif file.
pub fn parse_tz_str(s: &[u8]) -> Result<PosixTz, TzStrError> {
    let s = s.split(|&b| b == 0).next().unwrap_or(&[]);
    let mut c = Cursor { s, i: 0 };
    let std_abbr = c.abbr().ok_or(TzStrError::StdFormat)?;
    let std_offset = c.delta().ok_or(TzStrError::StdOffset)?;
    if c.peek() == 0 {
        return Ok(PosixTz { std_abbr, std_offset, dst: None });
    }
    let abbr = c.abbr().ok_or(TzStrError::DstFormat)?;
    let offset = if c.peek() == b',' { std_offset + 3600 } else { c.delta().ok_or(TzStrError::DstOffset)? };
    let mut rules = Vec::with_capacity(2);
    for _ in 0..2 {
        if c.peek() != b',' {
            return Err(TzStrError::MissingRules);
        }
        c.i += 1;
        rules.push(c.rule().ok_or(TzStrError::MalformedRule)?);
    }
    if c.peek() != 0 {
        return Err(TzStrError::Extraneous);
    }
    let end = rules.pop().unwrap_or(TransitionRule::Day { julian: false, day: 0, secs: 0 });
    let start = rules.pop().unwrap_or(TransitionRule::Day { julian: false, day: 0, secs: 0 });
    Ok(PosixTz { std_abbr, std_offset, dst: Some(DstRule { abbr, offset, start, end }) })
}

/// Which local time type applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applies {
    /// The file's local time type of this index.
    Type(usize),
    /// The standard time of the `TZ` footer.
    Std,
    /// The daylight-saving time of the `TZ` footer.
    Dst,
}

/// What applies after the last transition.
#[derive(Clone, Debug, PartialEq)]
pub enum After {
    /// A `TZ` footer.
    Rule(PosixTz),
    /// Without a footer: the type of the last transition (or the last type).
    Type(usize),
}

/// A zone's rules: CPython's `_zoneinfo` state, with local time types as indices.
#[derive(Clone, Debug)]
pub struct ZoneRules {
    pub trans_utc: Vec<i64>,
    /// Transitions in local wall-clock seconds, for `fold` 0 and 1.
    pub trans_local: [Vec<i64>; 2],
    /// The local time type after each transition.
    pub trans_idx: Vec<usize>,
    pub utcoff: Vec<i64>,
    /// The daylight-saving amount of each type, inferred from its neighbours.
    pub dstoff: Vec<i64>,
    /// The type before the first transition: the first standard-time type.
    pub before: Option<usize>,
    pub after: After,
}

/// Why TZif data could not become [`ZoneRules`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZoneError {
    /// A transition refers to a type past the end (its index).
    BadIndex(usize),
    /// No types and no footer.
    NoInfo,
    TzStr(TzStrError),
}

impl ZoneRules {
    /// Rules from the fields of a TZif file (as `zoneinfo._common.load_data` reads them).
    pub fn new(
        trans_idx: Vec<usize>,
        trans_utc: Vec<i64>,
        utcoff: Vec<i64>,
        isdst: &[bool],
        tz_str: Option<&[u8]>,
    ) -> Result<ZoneRules, ZoneError> {
        let n_types = utcoff.len();
        if let Some(&bad) = trans_idx.iter().find(|&&i| i > n_types) {
            return Err(ZoneError::BadIndex(bad));
        }
        if trans_idx.contains(&n_types) {
            return Err(ZoneError::BadIndex(n_types));
        }
        let dstoff = utcoff_to_dstoff(&trans_idx, &utcoff, isdst);
        let trans_local = ts_to_local(&trans_idx, &trans_utc, &utcoff);
        let before = isdst.iter().position(|d| !d).or(if n_types > 0 { Some(0) } else { None });
        let after = match tz_str.filter(|s| !s.is_empty()) {
            Some(s) => After::Rule(parse_tz_str(s).map_err(ZoneError::TzStr)?),
            None if n_types == 0 => return Err(ZoneError::NoInfo),
            None => After::Type(trans_idx.last().copied().unwrap_or(n_types - 1)),
        };
        Ok(ZoneRules { trans_utc, trans_local, trans_idx, utcoff, dstoff, before, after })
    }

    fn utcoff_of(&self, a: Applies) -> i64 {
        match (a, &self.after) {
            (Applies::Type(i), _) => self.utcoff[i],
            (Applies::Std, After::Rule(r)) => r.std_offset,
            (Applies::Dst, After::Rule(r)) => r.dst.as_ref().map_or(r.std_offset, |d| d.offset),
            _ => 0,
        }
    }

    /// The type of the `TZ` footer (or the fixed last type) at local time `ts` in `year`.
    fn rule_at(&self, ts: i64, fold: bool, year: i64) -> Applies {
        let r = match &self.after {
            After::Type(i) => return Applies::Type(*i),
            After::Rule(r) => r,
        };
        let Some(d) = &r.dst else { return Applies::Std };
        let mut start = d.start.year_to_timestamp(year);
        let mut end = d.end.year_to_timestamp(year);
        let dst_diff = d.offset - r.std_offset;
        if fold == (dst_diff >= 0) {
            end -= dst_diff;
        } else {
            start += dst_diff;
        }
        let isdst = if start < end { ts >= start && ts < end } else { ts < end || ts >= start };
        if isdst {
            Applies::Dst
        } else {
            Applies::Std
        }
    }

    /// The type and fold of the footer at UTC wall-clock seconds `ts` in `year`.
    fn rule_from_utc(&self, ts: i64, year: i64) -> (Applies, bool) {
        let r = match &self.after {
            After::Type(i) => return (Applies::Type(*i), false),
            After::Rule(r) => r,
        };
        let Some(d) = &r.dst else { return (Applies::Std, false) };
        let start = d.start.year_to_timestamp(year) - r.std_offset;
        let end = d.end.year_to_timestamp(year) - d.offset;
        let dst_diff = d.offset - r.std_offset;
        let isdst = if start < end { ts >= start && ts < end } else { ts < end || ts >= start };
        let (lo, hi) = if dst_diff > 0 { (end, end + dst_diff) } else { (start, start - dst_diff) };
        let fold = ts >= lo && ts < hi;
        (if isdst { Applies::Dst } else { Applies::Std }, fold)
    }

    /// The type in effect at local wall-clock seconds `ts` (of `year`), disambiguated by `fold`.
    pub fn find_local(&self, ts: i64, fold: bool, year: i64) -> Applies {
        let local = &self.trans_local[fold as usize];
        let n = local.len();
        if n > 0 && ts < local[0] {
            return self.before.map_or(Applies::Std, Applies::Type);
        }
        if n == 0 || ts > local[n - 1] {
            return self.rule_at(ts, fold, year);
        }
        let idx = local.partition_point(|&t| t <= ts) - 1;
        Applies::Type(self.trans_idx[idx])
    }

    /// The type in effect at UTC wall-clock seconds `ts` (of `year`) and whether the local time
    /// is the second of a repeated pair.
    pub fn find_utc(&self, ts: i64, year: i64) -> (Applies, bool) {
        let n = self.trans_utc.len();
        let before = || self.before.map_or(Applies::Std, Applies::Type);
        if n >= 1 && ts < self.trans_utc[0] {
            return (before(), false);
        }
        if n == 0 || ts > self.trans_utc[n - 1] {
            let (tti, mut fold) = self.rule_from_utc(ts, year);
            // Just after the last explicit transition the fold is against the type before it.
            if n > 0 {
                let prev = if n == 1 { before() } else { Applies::Type(self.trans_idx[n - 2]) };
                let diff = self.utcoff_of(prev) - self.utcoff_of(tti);
                if diff > 0 && ts < self.trans_utc[n - 1] + diff {
                    fold = true;
                }
            }
            return (tti, fold);
        }
        let idx = self.trans_utc.partition_point(|&t| t <= ts);
        let (prev, tti) = if idx >= 2 {
            (Applies::Type(self.trans_idx[idx - 2]), Applies::Type(self.trans_idx[idx - 1]))
        } else {
            (before(), Applies::Type(self.trans_idx[0]))
        };
        let shift = self.utcoff_of(prev) - self.utcoff_of(tti);
        (tti, shift > ts - self.trans_utc[idx - 1])
    }
}

/// The DST amount of each type (CPython's heuristic: the offset against a neighbouring
/// standard-time transition, else one hour).
fn utcoff_to_dstoff(trans_idx: &[usize], utcoffs: &[i64], isdsts: &[bool]) -> Vec<i64> {
    let n_types = utcoffs.len();
    let mut dstoffs = vec![0i64; n_types];
    // CPython counts every type here, not only the DST ones, so the early exit never triggers
    // and the one-hour fallback always runs.
    let dst_count = n_types;
    let mut dst_found = 0;
    for i in 1..trans_idx.len() {
        if dst_count == dst_found {
            break;
        }
        let idx = trans_idx[i];
        let mut comp_idx = trans_idx[i - 1];
        if !isdsts[idx] || dstoffs[idx] != 0 {
            continue;
        }
        let mut dstoff = 0;
        let utcoff = utcoffs[idx];
        if !isdsts[comp_idx] {
            dstoff = utcoff - utcoffs[comp_idx];
        }
        if dstoff == 0 && idx + 1 < n_types {
            let Some(&next) = trans_idx.get(i + 1) else { continue };
            comp_idx = next;
            if isdsts[comp_idx] {
                continue;
            }
            dstoff = utcoff - utcoffs[comp_idx];
        }
        if dstoff != 0 {
            dst_found += 1;
            dstoffs[idx] = dstoff;
        }
    }
    if dst_found < dst_count {
        for idx in 0..n_types {
            if isdsts[idx] && dstoffs[idx] == 0 {
                dstoffs[idx] = 3600;
            }
        }
    }
    dstoffs
}

/// Transitions in local wall-clock seconds for fold 0 (the larger of the offsets around each)
/// and fold 1 (the smaller).
fn ts_to_local(trans_idx: &[usize], trans_utc: &[i64], utcoff: &[i64]) -> [Vec<i64>; 2] {
    if trans_utc.is_empty() {
        return [Vec::new(), Vec::new()];
    }
    let mut out = [trans_utc.to_vec(), trans_utc.to_vec()];
    let (mut o0, mut o1) = if utcoff.len() > 1 { (utcoff[0], utcoff[trans_idx[0]]) } else { (utcoff[0], utcoff[0]) };
    if o1 > o0 {
        std::mem::swap(&mut o0, &mut o1);
    }
    out[0][0] += o0;
    out[1][0] += o1;
    for i in 1..trans_idx.len() {
        let (mut o0, mut o1) = (utcoff[trans_idx[i - 1]], utcoff[trans_idx[i]]);
        if o1 > o0 {
            std::mem::swap(&mut o0, &mut o1);
        }
        out[0][i] += o0;
        out[1][i] += o1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tz_strings() {
        let t = parse_tz_str(b"CET-1CEST,M3.5.0,M10.5.0/3").unwrap();
        assert_eq!((t.std_abbr.as_str(), t.std_offset), ("CET", 3600));
        let d = t.dst.unwrap();
        assert_eq!((d.abbr.as_str(), d.offset), ("CEST", 7200));
        // 2024-03-31T02:00 local and 2024-10-27T03:00 local.
        assert_eq!(d.start.year_to_timestamp(2024), 1_711_850_400);
        assert_eq!(d.end.year_to_timestamp(2024), 1_729_998_000);
        assert_eq!(parse_tz_str(b"<+0330>-3:30").unwrap().std_offset, 12_600);
        assert_eq!(parse_tz_str(b"EST5EDT"), Err(TzStrError::DstOffset));
        assert_eq!(parse_tz_str(b"EST5EDT4"), Err(TzStrError::MissingRules));
        assert_eq!(parse_tz_str(b"EST5EDT,M13.1.0,M11.1.0"), Err(TzStrError::MalformedRule));
        assert_eq!(parse_tz_str(b"5"), Err(TzStrError::StdFormat));
        assert_eq!(parse_tz_str(b"AAA"), Err(TzStrError::StdOffset));
        let j = TransitionRule::Day { julian: true, day: 60, secs: 0 };
        assert_eq!(j.year_to_timestamp(2024), days_from_civil(2024, 3, 1) * 86_400);
    }

    #[test]
    fn footer_lookup() {
        let z = ZoneRules::new(vec![], vec![], vec![3600], &[false], Some(b"CET-1CEST,M3.5.0,M10.5.0/3")).unwrap();
        assert_eq!(z.find_local(1_711_850_400 - 1, false, 2024), Applies::Std);
        assert_eq!(z.find_local(1_711_850_400 + 3600, false, 2024), Applies::Dst);
        // 02:30 on the autumn change exists twice: fold 0 is summer time, fold 1 winter time.
        let amb = 1_729_998_000 - 1800;
        assert_eq!(z.find_local(amb, false, 2024), Applies::Dst);
        assert_eq!(z.find_local(amb, true, 2024), Applies::Std);
        let utc = 1_729_990_800 + 1800;
        assert_eq!(z.find_utc(utc, 2024), (Applies::Std, true));
    }
}
