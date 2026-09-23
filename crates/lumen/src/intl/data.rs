//! Embedded CLDR-derived formatting data (a deliberate subset). Grown as the intl402 score climbs.

/// The (decimal, group) separators for a locale's `latn` number symbols, and the grouping sizes
/// (primary, secondary) — e.g. Indian `en-IN` groups as 3;2.
pub fn number_symbols(lang: &str, region: &str) -> (&'static str, &'static str, (usize, usize)) {
    // (decimal, group, (primary_group, secondary_group))
    match (lang, region) {
        ("en", "IN") | ("hi", _) | ("bn", "IN") | ("ta", "IN") => (".", ",", (3, 2)),
        ("de", _) | ("es", "ES") | ("it", _) | ("nl", _) | ("da", _) | ("id", _) | ("tr", _) => {
            (",", ".", (3, 3))
        }
        // European Portuguese groups with a no-break space; Brazilian uses ".".
        ("pt", "PT") => (",", "\u{00a0}", (3, 3)),
        // Polish groups with a plain no-break space (U+00A0) and applies minimumGroupingDigits=2.
        ("pl", _) => (",", "\u{00a0}", (3, 3)),
        ("fr", _) | ("ru", _) | ("cs", _) | ("hu", _) | ("fi", _) | ("sv", _) => {
            (",", "\u{202f}", (3, 3))
        }
        ("es", _) => (",", ".", (3, 3)),
        ("pt", _) => (",", ".", (3, 3)),
        // English, Japanese, Chinese, Korean, and the default: "." decimal, "," group, 3;3.
        _ => (".", ",", (3, 3)),
    }
}

/// The localized "NaN" symbol (most locales use "NaN"; Traditional Chinese differs).
pub fn nan_symbol(lang: &str, region: &str) -> &'static str {
    match (lang, region) {
        ("zh", "TW") | ("zh", "HK") | ("zh", "MO") | ("yue", _) => "非數值",
        _ => "NaN",
    }
}

/// The four list patterns (two, start, middle, end) for a (language, type, style). Falls back to
/// English when the language is unknown. `{0}`/`{1}` are the placeholders.
pub fn list_patterns(lang: &str, kind: &str, style: &str) -> [&'static str; 4] {
    const COMMA: [&str; 4] = ["{0}, {1}", "{0}, {1}", "{0}, {1}", "{0}, {1}"];
    const SPACE: [&str; 4] = ["{0} {1}", "{0} {1}", "{0} {1}", "{0} {1}"];
    // The `unit` type: narrow is space-joined everywhere; short/long is comma-joined, except a few
    // languages (Spanish) whose `unit-long` uses the conjunction form.
    if kind == "unit" {
        return match (lang, style) {
            (_, "narrow") => SPACE,
            // Spanish unit lists: the 2-item form uses "y", but 3+ item lists are comma-joined in
            // short (only `long` keeps "y" before the final item).
            ("es", "long") => ["{0} y {1}", "{0}, {1}", "{0}, {1}", "{0} y {1}"],
            ("es", "short") => ["{0} y {1}", "{0}, {1}", "{0}, {1}", "{0}, {1}"],
            _ => COMMA,
        };
    }
    if lang == "es" {
        // Spanish always joins the final item with "y"/"o", in every width.
        return match kind {
            "conjunction" => ["{0} y {1}", "{0}, {1}", "{0}, {1}", "{0} y {1}"],
            "disjunction" => ["{0} o {1}", "{0}, {1}", "{0}, {1}", "{0} o {1}"],
            _ => COMMA,
        };
    }
    // English (and the default for any language we do not yet ship list data for).
    match (kind, style) {
        ("conjunction", "long") => ["{0} and {1}", "{0}, {1}", "{0}, {1}", "{0}, and {1}"],
        ("conjunction", "short") => ["{0} & {1}", "{0}, {1}", "{0}, {1}", "{0}, & {1}"],
        ("conjunction", "narrow") => SPACE,
        ("disjunction", "long") => ["{0} or {1}", "{0}, {1}", "{0}, {1}", "{0}, or {1}"],
        ("disjunction", "short") => ["{0} or {1}", "{0}, {1}", "{0}, {1}", "{0}, or {1}"],
        ("disjunction", "narrow") => ["{0} or {1}", "{0}, {1}", "{0}, {1}", "{0}, or {1}"],
        (_, _) => COMMA,
    }
}

/// Cardinal plural category of `n` (an already-formatted non-negative number, given as its integer
/// value `i` plus whether it had a fractional part) for a language. Returns one of
/// "zero"/"one"/"two"/"few"/"many"/"other". A small subset of the CLDR plural rules.
/// The CLDR cardinal plural category. `e` is the compact/scientific exponent operand (0 for standard
/// notation); `has_fraction` proxies the CLDR `v != 0` operand test.
pub fn plural_cardinal(lang: &str, i: u64, has_fraction: bool, e: i32) -> &'static str {
    match lang {
        // English-like: one iff i==1 and no fraction.
        "en" | "de" | "nl" | "it" | "es" | "pt" | "sv" | "da" | "et" | "fi" => {
            if i == 1 && !has_fraction {
                "one"
            } else {
                "other"
            }
        }
        // French: one iff i = 0,1; many when e=0 and i is a non-zero whole 10^6 multiple, or e∉0..5.
        "fr" => {
            if i == 0 || i == 1 {
                "one"
            } else if (e == 0 && i != 0 && i.is_multiple_of(1_000_000) && !has_fraction)
                || !(0..=5).contains(&e)
            {
                "many"
            } else {
                "other"
            }
        }
        // Manx (gv): one/two/few on i mod 10/100 with v=0; many when v!=0.
        "gv" => {
            if has_fraction {
                "many"
            } else if i % 10 == 1 {
                "one"
            } else if i % 10 == 2 {
                "two"
            } else if matches!(i % 100, 0 | 20 | 40 | 60 | 80) {
                "few"
            } else {
                "other"
            }
        }
        // Polish (pl) — also ru/uk/be: one iff i=1; few for i%10=2..4 (excluding i%100=12..14);
        // everything else (integers) many; fractions other.
        "pl" => {
            if has_fraction {
                "other"
            } else if i == 1 {
                "one"
            } else if matches!(i % 10, 2..=4) && !matches!(i % 100, 12..=14) {
                "few"
            } else {
                "many"
            }
        }
        // Slovenian (sl): one/two/few on i mod 100 with v=0; few also when v!=0.
        "sl" => {
            if !has_fraction && i % 100 == 1 {
                "one"
            } else if !has_fraction && i % 100 == 2 {
                "two"
            } else if has_fraction || matches!(i % 100, 3 | 4) {
                "few"
            } else {
                "other"
            }
        }
        // Japanese/Chinese/Korean: always other.
        "ja" | "zh" | "ko" | "th" | "id" | "vi" => "other",
        _ => {
            if i == 1 && !has_fraction {
                "one"
            } else {
                "other"
            }
        }
    }
}

/// The plural categories a language distinguishes (for PluralRules.resolvedOptions().pluralCategories).
pub fn plural_categories(lang: &str) -> &'static [&'static str] {
    match lang {
        "ja" | "zh" | "ko" | "th" | "id" | "vi" => &["other"],
        "ar" => &["zero", "one", "two", "few", "many", "other"],
        "ru" | "uk" | "pl" => &["one", "few", "many", "other"],
        "cs" | "sk" => &["one", "few", "many", "other"],
        "fr" => &["one", "many", "other"],
        "gv" => &["one", "two", "few", "many", "other"],
        "sl" => &["one", "two", "few", "other"],
        _ => &["one", "other"],
    }
}

/// CLDR calendarPreferenceData for a region, most preferred first (Intl.Locale getCalendars).
/// Regions without an entry use the "001" (gregory-only) preference. Only calendar ids that
/// DateTimeFormat supports are listed (CLDR's bare "islamic"/"islamic-rgsa" are omitted).
pub fn calendar_preferences(region: &str) -> &'static [&'static str] {
    match region {
        "TH" => &["buddhist", "gregory"],
        "AF" | "IR" => &["persian", "gregory", "islamic-civil", "islamic-tbla"],
        "SA" => &["islamic-umalqura", "gregory"],
        "AE" | "BH" | "KW" | "QA" => &[
            "gregory",
            "islamic-umalqura",
            "islamic-civil",
            "islamic-tbla",
        ],
        "EG" => &["gregory", "coptic", "islamic-civil", "islamic-tbla"],
        "IL" => &["gregory", "hebrew", "islamic-civil", "islamic-tbla"],
        "BD" | "DJ" | "DZ" | "EH" | "ER" | "IQ" | "JO" | "KM" | "LB" | "LY" | "MA" | "MR"
        | "OM" | "PK" | "PS" | "SD" | "SY" | "TD" | "TN" | "YE" => {
            &["gregory", "islamic-civil", "islamic-tbla"]
        }
        "CN" | "CX" | "HK" | "MO" | "SG" => &["gregory", "chinese"],
        "TW" => &["gregory", "roc", "chinese"],
        "KR" => &["gregory", "dangi"],
        "JP" => &["gregory", "japanese"],
        "IN" => &["gregory", "indian"],
        "ET" => &["gregory", "ethiopic"],
        _ => &["gregory"],
    }
}

/// The preferred hour cycle from CLDR timeData (Intl.Locale getHourCycles). A language-region
/// entry (e.g. fr_CA) takes precedence over the region's; "001" is the H (h23) default.
pub fn hour_cycle(lang: &str, region: &str) -> &'static str {
    match (lang, region) {
        ("fr", "CA") | ("ku", "SY") => return "h23",
        ("ar", "001") => return "h12",
        _ => {}
    }
    match region {
        "AE" | "AG" | "AS" | "AU" | "BB" | "BD" | "BH" | "BM" | "BN" | "BS" | "BT" | "CA"
        | "CO" | "DJ" | "DM" | "DZ" | "EG" | "EH" | "ER" | "ET" | "FJ" | "FM" | "GD" | "GH"
        | "GM" | "GU" | "GY" | "IN" | "IQ" | "JM" | "JO" | "KI" | "KN" | "KR" | "KW" | "KY"
        | "LB" | "LC" | "LR" | "LS" | "LY" | "MH" | "MP" | "MR" | "MW" | "MY" | "NZ" | "OM"
        | "PG" | "PH" | "PK" | "PR" | "PS" | "QA" | "SA" | "SB" | "SD" | "SL" | "SO" | "SS"
        | "SY" | "SZ" | "TC" | "TD" | "TN" | "TO" | "TT" | "TW" | "UM" | "US" | "VC" | "VG"
        | "VI" | "VU" | "WS" | "YE" | "ZM" => "h12",
        _ => "h23",
    }
}

/// CLDR weekData for a region: (firstDay, weekend days), with days numbered 1 (Mon)..7 (Sun).
pub fn week_data(region: &str) -> (u8, &'static [u8]) {
    let first = match region {
        "MV" => 5,
        "AE" | "AF" | "BH" | "DJ" | "DZ" | "EG" | "IQ" | "IR" | "JO" | "KW" | "LY" | "OM"
        | "QA" | "SD" | "SY" => 6,
        "AG" | "AS" | "BD" | "BR" | "BS" | "BT" | "BW" | "BZ" | "CA" | "CN" | "CO" | "DM"
        | "DO" | "ET" | "GT" | "GU" | "HK" | "HN" | "ID" | "IL" | "IN" | "JM" | "JP" | "KE"
        | "KH" | "KR" | "LA" | "MH" | "MM" | "MO" | "MT" | "MX" | "MZ" | "NI" | "NP" | "PA"
        | "PE" | "PH" | "PK" | "PR" | "PT" | "PY" | "SA" | "SG" | "SV" | "TH" | "TT" | "TW"
        | "UM" | "US" | "VE" | "VI" | "WS" | "YE" | "ZA" | "ZW" => 7,
        _ => 1,
    };
    let weekend: &'static [u8] = match region {
        "IN" | "UG" => &[7],
        "IR" => &[5],
        "AF" => &[4, 5],
        "AE" | "BH" | "DZ" | "EG" | "IL" | "IQ" | "JO" | "KW" | "LY" | "OM" | "QA" | "SA"
        | "SD" | "SY" | "YE" => &[5, 6],
        _ => &[6, 7],
    };
    (first, weekend)
}
