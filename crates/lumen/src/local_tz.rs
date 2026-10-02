//! The engine's local time zone: the zone of `Date`'s local-time methods, the default of
//! `Intl.DateTimeFormat` and `Temporal.Now.timeZoneId`. The zone itself and its offsets live in
//! [`lumen_common::local_tz`]; this adds the en-US display names `Date` and Intl show.

#[cfg(feature = "intl")]
pub(crate) use lumen_common::local_tz::zone_index;
use lumen_common::local_tz::{gmt_format, local_zone_index, zone_offset_sec};
pub(crate) use lumen_common::local_tz::{id, local_to_utc, offset_ms, set};

/// The name `Date.prototype.toString` shows for the local zone at `t`
/// ("Central European Summer Time", or "GMT+03:00" where ICU has no name).
pub(crate) fn long_name(t: f64) -> String {
    match local_zone_index() {
        None => "Coordinated Universal Time".to_string(),
        Some(index) => zone_display_name(index, (t / 1000.0).floor() as i64, "long"),
    }
}

/// The en-US display name of zone `ZONES[index]` at `sec` under an Intl `timeZoneName` style
/// (long, short, longGeneric, shortGeneric, longOffset, shortOffset).
pub(crate) fn zone_display_name(index: usize, sec: i64, style: &str) -> String {
    let off = zone_offset_sec(index, sec);
    let long_gmt = matches!(style, "long" | "longGeneric" | "longOffset");
    #[cfg(feature = "intl")]
    if !matches!(style, "longOffset" | "shortOffset") {
        use lumen_common::local_tz::zone_offset_uncached;
        let names = crate::tznames::zone_names(index);
        // The names are those of the zone's current offsets: the lower is standard time, the
        // higher daylight time.
        let jan = zone_offset_uncached(index, 1_736_942_400);
        let jul = zone_offset_uncached(index, 1_752_580_800);
        // A historical offset outside the current pair reads as standard time, like ICU's
        // metazone names for the zone's earlier periods.
        let daylight = jan != jul && off == jan.max(jul);
        let name = match style {
            "long" => names[daylight as usize],
            "short" => names[2 + daylight as usize],
            "longGeneric" => names[4],
            _ => names[5],
        };
        if !name.is_empty() {
            return name.to_string();
        }
    }
    gmt_format(off, long_gmt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_common::local_tz::zone_index;

    #[test]
    fn local_tz_display_names() {
        let ny = zone_index("America/New_York").unwrap();
        assert_eq!(
            zone_display_name(ny, 1_704_067_200, "longOffset"),
            "GMT-05:00"
        );
        #[cfg(feature = "intl")]
        {
            use lumen_common::tzdata::ZONES;
            assert_eq!(
                zone_display_name(ny, 1_720_000_000, "long"),
                "Eastern Daylight Time"
            );
            assert_eq!(zone_display_name(ny, 1_704_067_200, "short"), "EST");
            let dublin = zone_index("Europe/Dublin").unwrap();
            assert_eq!(
                zone_display_name(dublin, 1_720_000_000, "long"),
                "Irish Standard Time"
            );
            assert_eq!(
                zone_display_name(dublin, 1_704_067_200, "long"),
                "Greenwich Mean Time"
            );
            assert_eq!(crate::tznames::zone_names(ZONES.len()), [""; 6]);
            assert_ne!(crate::tznames::zone_names(ZONES.len() - 1)[0], "");
        }
    }
}
