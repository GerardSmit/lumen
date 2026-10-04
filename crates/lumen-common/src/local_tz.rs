//! The process-wide local time zone (V8 keeps one per process; so does CPython's `time.tzset`):
//! the embedder sets it, e.g. from `TZ`. UTC until set, which keeps every local-time conversion a
//! no-op. Offsets come from the generated [`crate::tzdata`] tables, extended past their end by
//! the zone's DST rules.

use crate::civil::{civil_from_days, days_from_civil, is_leap};
use crate::tzdata::{Zone, LINKS, ZONES};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

/// 0 for UTC, else 1 + the zone's index in `ZONES`.
static LOCAL: AtomicUsize = AtomicUsize::new(0);
/// The identifier as set: 0 for "UTC", 1 + an index in `ZONES`, or 1 + `ZONES.len()` + an index
/// in `LINKS` (an alias such as "Europe/London" keeps its name).
static LOCAL_ID: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// The last offset interval looked up: (zone key, start sec, end sec (exclusive), offset sec).
    static LAST: Cell<(usize, i64, i64, i32)> = const { Cell::new((0, 0, 0, 0)) };
}

const DAY_SEC: i64 = 86_400;
/// 2037-01-01T00:00Z: the generated tables stop in 2037, so a zone whose last transition is later
/// than this still observes DST and is extended by its rules (see `equivalent_year_offset`).
const TABLE_END: i64 = 2_114_380_800;

/// Set the local time zone from an IANA name (`None` resets to UTC). Accepts POSIX `TZ` spellings
/// of a zone name: a leading ':' and a path ending in `zoneinfo/<name>`. Returns false (and uses
/// UTC) for a name the time-zone database does not know.
pub fn set(name: Option<&str>) -> bool {
    let resolved = name.map(resolve);
    let (zone, id) = resolved.flatten().unwrap_or((0, 0));
    LOCAL.store(zone, Ordering::Relaxed);
    LOCAL_ID.store(id, Ordering::Relaxed);
    !matches!(resolved, Some(None))
}

/// (LOCAL, LOCAL_ID) for a zone name.
fn resolve(name: &str) -> Option<(usize, usize)> {
    let mut name = name.strip_prefix(':').unwrap_or(name);
    if let Some(pos) = name.rfind("zoneinfo/") {
        name = &name[pos + "zoneinfo/".len()..];
    }
    let canon = crate::tz::canonicalize(name)?;
    if canon == "UTC" {
        return Some((0, 0));
    }
    let zone = zone_index(canon)? + 1;
    let reg = crate::tz::registry_name(name)?;
    let id = match zone_index(reg) {
        Some(i) => i + 1,
        None => 1 + ZONES.len() + LINKS.iter().position(|(a, _)| *a == reg)?,
    };
    Some((zone, id))
}

/// The local zone's identifier: "UTC" or the IANA name it was set to (case-normalized).
pub fn id() -> &'static str {
    match LOCAL_ID.load(Ordering::Relaxed) {
        0 => "UTC",
        k if k <= ZONES.len() => ZONES[k - 1].name,
        k => LINKS[k - 1 - ZONES.len()].0,
    }
}

/// The local zone's index in `ZONES` (`None` for UTC).
pub fn local_zone_index() -> Option<usize> {
    LOCAL.load(Ordering::Relaxed).checked_sub(1)
}

/// The index in `ZONES` of a canonical zone name.
pub fn zone_index(canon: &str) -> Option<usize> {
    ZONES.iter().position(|z| z.name == canon)
}

/// The UTC offset (seconds) of zone `ZONES[index]` at `sec`, cached per thread so runs of nearby
/// instants (the common case) skip the binary search.
pub fn zone_offset_sec(index: usize, sec: i64) -> i32 {
    let key = index + 1;
    LAST.with(|last| {
        let (k, lo, hi, off) = last.get();
        if k == key && lo <= sec && sec < hi {
            return off;
        }
        let (off, lo, hi) = lookup(&ZONES[index], sec);
        last.set((key, lo, hi, off));
        off
    })
}

/// The uncached UTC offset (seconds) of zone `ZONES[index]` at `sec`.
pub fn zone_offset_uncached(index: usize, sec: i64) -> i32 {
    lookup(&ZONES[index], sec).0
}

/// (offset, interval start, interval end) for `sec`.
fn lookup(z: &Zone, sec: i64) -> (i32, i64, i64) {
    let ts = z.transitions;
    let idx = ts.partition_point(|&(t, _)| t <= sec);
    if idx == ts.len() && idx > 0 && ts[idx - 1].0 > TABLE_END {
        return (equivalent_year_offset(z, sec), sec, sec + 1);
    }
    let (off, lo) = if idx == 0 {
        (z.initial, i64::MIN)
    } else {
        (ts[idx - 1].1, ts[idx - 1].0)
    };
    (off, lo, ts.get(idx).map_or(i64::MAX, |t| t.0))
}

/// The offset of a DST-observing zone past the end of its table: the offset at the same moment of
/// an equivalent tabled year (same leap-ness and weekday of January 1st), which reproduces
/// weekday-based rules like "second Sunday in March".
fn equivalent_year_offset(z: &Zone, sec: i64) -> i32 {
    let days = sec.div_euclid(DAY_SEC);
    let year = civil_from_days(days).0;
    let jan1 = days_from_civil(year, 1, 1);
    let leap = is_leap(year);
    let wd = (jan1 + 4).rem_euclid(7);
    for ey in (2009..=2037).rev() {
        let ejan1 = days_from_civil(ey, 1, 1);
        if is_leap(ey) == leap && (ejan1 + 4).rem_euclid(7) == wd {
            let shifted = sec + (ejan1 - jan1) * DAY_SEC;
            let ts = z.transitions;
            let idx = ts.partition_point(|&(t, _)| t <= shifted);
            return if idx == 0 { z.initial } else { ts[idx - 1].1 };
        }
    }
    z.transitions.last().map_or(z.initial, |t| t.1)
}

/// The local zone's offset (ms) at time value `t` (ms since the epoch, finite).
#[inline]
pub fn offset_ms(t: f64) -> f64 {
    match LOCAL.load(Ordering::Relaxed) {
        0 => 0.0,
        k => zone_offset_sec(k - 1, (t / 1000.0).floor() as i64) as f64 * 1000.0,
    }
}

/// The time value (ms) of local time `t`. A repeated local time maps to the earlier instant; a
/// skipped one uses the offset from before the transition (ECMA-262 UTC(t)).
pub fn local_to_utc(t: f64) -> f64 {
    local_to_utc_in(LOCAL.load(Ordering::Relaxed), t)
}

fn local_to_utc_in(k: usize, t: f64) -> f64 {
    if !t.is_finite() {
        return f64::NAN;
    }
    if k == 0 {
        return t;
    }
    let off = |u: f64| zone_offset_sec(k - 1, (u / 1000.0).floor() as i64) as f64 * 1000.0;
    let day = DAY_SEC as f64 * 1000.0;
    let before = off(t - day);
    let after = off(t + day);
    let earlier = t - before;
    if before == after || off(earlier) == before {
        return earlier;
    }
    let later = t - after;
    if off(later) == after {
        later
    } else {
        earlier
    }
}

/// ICU's localized GMT format: "GMT", "GMT+03:00" / "GMT-04:56:02" (long), "GMT+3" / "GMT+5:30"
/// (short).
pub fn gmt_format(off_sec: i32, long: bool) -> String {
    if off_sec == 0 {
        return "GMT".to_string();
    }
    let sign = if off_sec < 0 { '-' } else { '+' };
    let a = off_sec.unsigned_abs();
    let (h, m, s) = (a / 3600, a / 60 % 60, a % 60);
    match (long, s) {
        (true, 0) => format!("GMT{sign}{h:02}:{m:02}"),
        (true, _) => format!("GMT{sign}{h:02}:{m:02}:{s:02}"),
        (false, _) if m == 0 && s == 0 => format!("GMT{sign}{h}"),
        (false, 0) => format!("GMT{sign}{h}:{m:02}"),
        (false, _) => format!("GMT{sign}{h}:{m:02}:{s:02}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str) -> usize {
        resolve(name).unwrap().0
    }

    #[test]
    fn local_tz_resolves_posix_spellings_and_keeps_aliases() {
        assert_eq!(resolve("Etc/UTC"), Some((0, 0)));
        assert_eq!(
            resolve(":America/New_York").unwrap().0,
            key("America/New_York")
        );
        assert_eq!(
            resolve("/usr/share/zoneinfo/Europe/Paris").unwrap().0,
            key("Europe/Paris")
        );
        assert!(resolve("Not/A_Zone").is_none());
        let (zone, id) = resolve("europe/london").unwrap();
        assert_ne!(zone, 0);
        assert_eq!(LINKS[id - 1 - ZONES.len()].0, "Europe/London");
    }

    #[test]
    fn local_tz_disambiguates_gaps_and_overlaps() {
        let ny = key("America/New_York");
        let h = 3_600_000.0;
        // 2024-03-10 02:30 local is skipped: the pre-transition offset (-5h) puts it at 07:30Z.
        let gap = 1_710_037_800_000.0;
        assert_eq!(local_to_utc_in(ny, gap), gap + 5.0 * h);
        // 2024-11-03 01:30 local happens twice: the earlier instant (EDT, -4h) wins.
        let overlap = 1_730_597_400_000.0;
        assert_eq!(local_to_utc_in(ny, overlap), overlap + 4.0 * h);
        assert_eq!(
            local_to_utc_in(ny, 1_704_067_200_000.0),
            1_704_067_200_000.0 + 5.0 * h
        );
        assert!(local_to_utc_in(ny, f64::NAN).is_nan());
    }

    #[test]
    fn local_tz_extends_dst_rules_past_the_table() {
        let ny = key("America/New_York") - 1;
        // 2050-07-01T12:00Z is EDT, 2050-01-01T12:00Z EST.
        assert_eq!(zone_offset_sec(ny, 2_539_944_000), -14_400);
        assert_eq!(zone_offset_sec(ny, 2_524_651_200), -18_000);
        let tokyo = key("Asia/Tokyo") - 1;
        assert_eq!(zone_offset_sec(tokyo, 2_539_944_000), 32_400);
    }

    #[test]
    fn local_tz_gmt_format() {
        assert_eq!(gmt_format(-17_762, true), "GMT-04:56:02");
        assert_eq!(gmt_format(19_800, false), "GMT+5:30");
    }
}
