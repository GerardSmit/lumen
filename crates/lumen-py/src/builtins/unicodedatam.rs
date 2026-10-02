//! `unicodedata` over the shared character database in `lumen_common::ucd`.

/// This module provides access to the Unicode Character Database which
/// defines character properties for all Unicode characters. The data in
/// this database is based on the UnicodeData.txt file version
/// 15.0.0 which is publicly available from ftp://ftp.unicode.org/.
///
/// The module uses the same names and symbols as defined by the
/// UnicodeData File Format 15.0.0.
#[lumen_bind::module(name = "unicodedata")]
pub mod unicodedata {
    use crate::bind::{Py, This};
    use crate::object::*;
    use crate::vm::{dict_set_str, Interp};
    use lumen_bind::Passed;
    use lumen_common::smuggle;
    use lumen_common::ucd::{self, Version};

    const NAME_MAXLEN: usize = 256;

    /// The code point of a one-character str argument (`int(accept={str})`).
    fn chr(it: &mut Interp, f: &str, arg: &str, v: &Value) -> R<u32> {
        if let Some(s) = v.as_str() {
            let mut cps = smuggle::code_points(s);
            if let (Some(c), None) = (cps.next(), cps.next()) {
                return Ok(c);
            }
        }
        let t = it.type_name_of(v);
        Err(it.type_error(&format!("{f}() {arg} must be a unicode character, not {t}")))
    }

    fn text(it: &mut Interp, f: &str, arg: &str, v: &Value) -> R<Vec<u32>> {
        match v.as_str() {
            Some(s) => Ok(smuggle::code_points(s).collect()),
            None => {
                let t = it.type_name_of(v);
                Err(it.type_error(&format!("{f}() {arg} must be str, not {t}")))
            }
        }
    }

    fn string(cps: &[u32]) -> Value {
        let mut out = String::with_capacity(cps.len());
        for &c in cps {
            smuggle::push_code_point(&mut out, c);
        }
        Value::string(out)
    }

    fn or_default(it: &mut Interp, v: Option<Value>, default: Passed<&Value>, msg: &str) -> R<Value> {
        match (v, default.0) {
            (Some(v), _) => Ok(v),
            (None, Some(d)) => Ok(d.clone()),
            (None, None) => Err(it.value_error(msg)),
        }
    }

    fn decimal_impl(it: &mut Interp, ver: Version, c: &Value, default: Passed<&Value>) -> R<Value> {
        let c = chr(it, "decimal", "argument 1", c)?;
        let v = ucd::props(c, ver).decimal.map(|d| Value::Int(d as i64));
        or_default(it, v, default, "not a decimal")
    }

    fn digit_impl(it: &mut Interp, ver: Version, c: &Value, default: Passed<&Value>) -> R<Value> {
        let c = chr(it, "digit", "argument 1", c)?;
        let v = ucd::props(c, ver).digit.map(|d| Value::Int(d as i64));
        or_default(it, v, default, "not a digit")
    }

    fn numeric_impl(it: &mut Interp, ver: Version, c: &Value, default: Passed<&Value>) -> R<Value> {
        let c = chr(it, "numeric", "argument 1", c)?;
        let v = ucd::props(c, ver).numeric.map(Value::Float);
        or_default(it, v, default, "not a numeric character")
    }

    fn name_impl(it: &mut Interp, ver: Version, c: &Value, default: Passed<&Value>) -> R<Value> {
        let c = chr(it, "name", "argument 1", c)?;
        let v = ucd::name(c, ver).map(Value::string);
        or_default(it, v, default, "no such name")
    }

    fn lookup_impl(it: &mut Interp, ver: Version, name: &Value) -> R<Value> {
        let name = match name.as_str() {
            Some(s) => s.to_string(),
            None if it.is_buffer(name) => {
                let b = it.bytes_of(name)?;
                String::from_utf8_lossy(&b).into_owned()
            }
            None => {
                let t = it.type_name_of(name);
                return Err(it.type_error(&format!("a bytes-like object is required, not '{t}'")));
            }
        };
        if name.len() > NAME_MAXLEN {
            let e = it.exc_type("KeyError");
            return Err(it.new_exc(&e, vec![Value::str("name too long")]));
        }
        match ucd::lookup(&name, ver, true) {
            Some(cps) => Ok(string(&cps)),
            None => {
                let e = it.exc_type("KeyError");
                Err(it.new_exc(&e, vec![Value::string(format!("undefined character name '{name}'"))]))
            }
        }
    }

    fn form_of(it: &mut Interp, f: &str, form: &Value) -> R<&'static str> {
        let Some(s) = form.as_str() else {
            let t = it.type_name_of(form);
            return Err(it.type_error(&format!("{f}() argument 1 must be str, not {t}")));
        };
        match s {
            "NFC" => Ok("NFC"),
            "NFD" => Ok("NFD"),
            "NFKC" => Ok("NFKC"),
            "NFKD" => Ok("NFKD"),
            _ => Err(it.value_error("invalid normalization form")),
        }
    }

    fn normalize_impl(it: &mut Interp, ver: Version, form: &Value, unistr: &Value) -> R<Value> {
        if form.as_str().is_some() && unistr.as_str() == Some("") {
            return Ok(unistr.clone());
        }
        let form = form_of(it, "normalize", form)?;
        let cps = text(it, "normalize", "argument 2", unistr)?;
        if cps.iter().all(|&c| c < 0x80) {
            return Ok(unistr.clone());
        }
        let out = ucd::normalize(&cps, form, ver);
        if out == cps {
            return Ok(unistr.clone());
        }
        Ok(string(&out))
    }

    fn is_normalized_impl(it: &mut Interp, ver: Version, form: &Value, unistr: &Value) -> R<bool> {
        if form.as_str().is_some() && unistr.as_str() == Some("") {
            return Ok(true);
        }
        let form = form_of(it, "is_normalized", form)?;
        let cps = text(it, "is_normalized", "argument 2", unistr)?;
        Ok(ucd::normalize(&cps, form, ver) == cps)
    }

    fn category_impl(it: &mut Interp, ver: Version, c: &Value) -> R<&'static str> {
        let c = chr(it, "category", "argument", c)?;
        Ok(ucd::props(c, ver).category)
    }

    fn bidirectional_impl(it: &mut Interp, ver: Version, c: &Value) -> R<&'static str> {
        let c = chr(it, "bidirectional", "argument", c)?;
        Ok(ucd::props(c, ver).bidirectional)
    }

    fn combining_impl(it: &mut Interp, ver: Version, c: &Value) -> R<i64> {
        let c = chr(it, "combining", "argument", c)?;
        Ok(ucd::props(c, ver).combining as i64)
    }

    fn mirrored_impl(it: &mut Interp, ver: Version, c: &Value) -> R<i64> {
        let c = chr(it, "mirrored", "argument", c)?;
        Ok(ucd::props(c, ver).mirrored as i64)
    }

    fn east_asian_width_impl(it: &mut Interp, ver: Version, c: &Value) -> R<&'static str> {
        let c = chr(it, "east_asian_width", "argument", c)?;
        Ok(ucd::props(c, ver).east_asian_width)
    }

    fn decomposition_impl(it: &mut Interp, ver: Version, c: &Value) -> R<String> {
        let c = chr(it, "decomposition", "argument", c)?;
        Ok(ucd::decomposition(c, ver))
    }

    /// Converts a Unicode character into its equivalent decimal value.
    ///
    /// Returns the decimal value assigned to the character chr as integer.
    /// If no such value is defined, default is returned, or, if not given,
    /// ValueError is raised.
    #[op]
    fn decimal(it: &mut Interp, chr: &Value, default: Passed<&Value>) -> R<Value> {
        decimal_impl(it, Version::Current, chr, default)
    }

    /// Converts a Unicode character into its equivalent digit value.
    ///
    /// Returns the digit value assigned to the character chr as integer.
    /// If no such value is defined, default is returned, or, if not given,
    /// ValueError is raised.
    #[op]
    fn digit(it: &mut Interp, chr: &Value, default: Passed<&Value>) -> R<Value> {
        digit_impl(it, Version::Current, chr, default)
    }

    /// Converts a Unicode character into its equivalent numeric value.
    ///
    /// Returns the numeric value assigned to the character chr as float.
    /// If no such value is defined, default is returned, or, if not given,
    /// ValueError is raised.
    #[op]
    fn numeric(it: &mut Interp, chr: &Value, default: Passed<&Value>) -> R<Value> {
        numeric_impl(it, Version::Current, chr, default)
    }

    /// Returns the general category assigned to the character chr as string.
    #[op]
    fn category(it: &mut Interp, chr: &Value) -> R<&'static str> {
        category_impl(it, Version::Current, chr)
    }

    /// Returns the bidirectional class assigned to the character chr as string.
    ///
    /// If no such value is defined, an empty string is returned.
    #[op]
    fn bidirectional(it: &mut Interp, chr: &Value) -> R<&'static str> {
        bidirectional_impl(it, Version::Current, chr)
    }

    /// Returns the canonical combining class assigned to the character chr as integer.
    ///
    /// Returns 0 if no combining class is defined.
    #[op]
    fn combining(it: &mut Interp, chr: &Value) -> R<i64> {
        combining_impl(it, Version::Current, chr)
    }

    /// Returns the mirrored property assigned to the character chr as integer.
    ///
    /// Returns 1 if the character has been identified as a "mirrored"
    /// character in bidirectional text, 0 otherwise.
    #[op]
    fn mirrored(it: &mut Interp, chr: &Value) -> R<i64> {
        mirrored_impl(it, Version::Current, chr)
    }

    /// Returns the east asian width assigned to the character chr as string.
    #[op]
    fn east_asian_width(it: &mut Interp, chr: &Value) -> R<&'static str> {
        east_asian_width_impl(it, Version::Current, chr)
    }

    /// Returns the character decomposition mapping assigned to the character chr as string.
    ///
    /// An empty string is returned in case no such mapping is defined.
    #[op]
    fn decomposition(it: &mut Interp, chr: &Value) -> R<String> {
        decomposition_impl(it, Version::Current, chr)
    }

    /// Return whether the Unicode string unistr is in the normal form 'form'.
    ///
    /// Valid values for form are 'NFC', 'NFKC', 'NFD', and 'NFKD'.
    #[op]
    fn is_normalized(it: &mut Interp, form: &Value, unistr: &Value) -> R<bool> {
        is_normalized_impl(it, Version::Current, form, unistr)
    }

    /// Return the normal form 'form' for the Unicode string unistr.
    ///
    /// Valid values for form are 'NFC', 'NFKC', 'NFD', and 'NFKD'.
    #[op]
    fn normalize(it: &mut Interp, form: &Value, unistr: &Value) -> R<Value> {
        normalize_impl(it, Version::Current, form, unistr)
    }

    /// Returns the name assigned to the character chr as a string.
    ///
    /// If no name is defined, default is returned, or, if not given,
    /// ValueError is raised.
    #[op]
    fn name(it: &mut Interp, chr: &Value, default: Passed<&Value>) -> R<Value> {
        name_impl(it, Version::Current, chr, default)
    }

    /// Look up character by name.
    ///
    /// If a character with the given name is found, return the
    /// corresponding character.  If not found, KeyError is raised.
    #[op]
    fn lookup(it: &mut Interp, name: &Value) -> R<Value> {
        lookup_impl(it, Version::Current, name)
    }

    #[class(name = "UCD", module = "unicodedata", hint(py(final)))]
    pub struct Ucd {
        version: Version,
    }

    fn ver(slf: &This<Py<Ucd>>, it: &mut Interp) -> R<Version> {
        Ok(slf.0.borrow(it)?.version)
    }

    #[methods]
    impl Ucd {
        #[getter]
        fn unidata_version(slf: This<Py<Self>>, it: &mut Interp) -> R<&'static str> {
            Ok(match ver(&slf, it)? {
                Version::Current => ucd::UNIDATA_VERSION,
                Version::V3_2_0 => "3.2.0",
            })
        }

        /// Converts a Unicode character into its equivalent decimal value.
        ///
        /// Returns the decimal value assigned to the character chr as integer.
        /// If no such value is defined, default is returned, or, if not given,
        /// ValueError is raised.
        fn decimal(slf: This<Py<Self>>, it: &mut Interp, chr: &Value, default: Passed<&Value>) -> R<Value> {
            let v = ver(&slf, it)?;
            decimal_impl(it, v, chr, default)
        }

        /// Converts a Unicode character into its equivalent digit value.
        ///
        /// Returns the digit value assigned to the character chr as integer.
        /// If no such value is defined, default is returned, or, if not given,
        /// ValueError is raised.
        fn digit(slf: This<Py<Self>>, it: &mut Interp, chr: &Value, default: Passed<&Value>) -> R<Value> {
            let v = ver(&slf, it)?;
            digit_impl(it, v, chr, default)
        }

        /// Converts a Unicode character into its equivalent numeric value.
        ///
        /// Returns the numeric value assigned to the character chr as float.
        /// If no such value is defined, default is returned, or, if not given,
        /// ValueError is raised.
        fn numeric(slf: This<Py<Self>>, it: &mut Interp, chr: &Value, default: Passed<&Value>) -> R<Value> {
            let v = ver(&slf, it)?;
            numeric_impl(it, v, chr, default)
        }

        /// Returns the general category assigned to the character chr as string.
        fn category(slf: This<Py<Self>>, it: &mut Interp, chr: &Value) -> R<&'static str> {
            let v = ver(&slf, it)?;
            category_impl(it, v, chr)
        }

        /// Returns the bidirectional class assigned to the character chr as string.
        ///
        /// If no such value is defined, an empty string is returned.
        fn bidirectional(slf: This<Py<Self>>, it: &mut Interp, chr: &Value) -> R<&'static str> {
            let v = ver(&slf, it)?;
            bidirectional_impl(it, v, chr)
        }

        /// Returns the canonical combining class assigned to the character chr as integer.
        ///
        /// Returns 0 if no combining class is defined.
        fn combining(slf: This<Py<Self>>, it: &mut Interp, chr: &Value) -> R<i64> {
            let v = ver(&slf, it)?;
            combining_impl(it, v, chr)
        }

        /// Returns the mirrored property assigned to the character chr as integer.
        ///
        /// Returns 1 if the character has been identified as a "mirrored"
        /// character in bidirectional text, 0 otherwise.
        fn mirrored(slf: This<Py<Self>>, it: &mut Interp, chr: &Value) -> R<i64> {
            let v = ver(&slf, it)?;
            mirrored_impl(it, v, chr)
        }

        /// Returns the east asian width assigned to the character chr as string.
        fn east_asian_width(slf: This<Py<Self>>, it: &mut Interp, chr: &Value) -> R<&'static str> {
            let v = ver(&slf, it)?;
            east_asian_width_impl(it, v, chr)
        }

        /// Returns the character decomposition mapping assigned to the character chr as string.
        ///
        /// An empty string is returned in case no such mapping is defined.
        fn decomposition(slf: This<Py<Self>>, it: &mut Interp, chr: &Value) -> R<String> {
            let v = ver(&slf, it)?;
            decomposition_impl(it, v, chr)
        }

        /// Return whether the Unicode string unistr is in the normal form 'form'.
        ///
        /// Valid values for form are 'NFC', 'NFKC', 'NFD', and 'NFKD'.
        fn is_normalized(slf: This<Py<Self>>, it: &mut Interp, form: &Value, unistr: &Value) -> R<bool> {
            let v = ver(&slf, it)?;
            is_normalized_impl(it, v, form, unistr)
        }

        /// Return the normal form 'form' for the Unicode string unistr.
        ///
        /// Valid values for form are 'NFC', 'NFKC', 'NFD', and 'NFKD'.
        fn normalize(slf: This<Py<Self>>, it: &mut Interp, form: &Value, unistr: &Value) -> R<Value> {
            let v = ver(&slf, it)?;
            normalize_impl(it, v, form, unistr)
        }

        /// Returns the name assigned to the character chr as a string.
        ///
        /// If no name is defined, default is returned, or, if not given,
        /// ValueError is raised.
        fn name(slf: This<Py<Self>>, it: &mut Interp, chr: &Value, default: Passed<&Value>) -> R<Value> {
            let v = ver(&slf, it)?;
            name_impl(it, v, chr, default)
        }

        /// Look up character by name.
        ///
        /// If a character with the given name is found, return the
        /// corresponding character.  If not found, KeyError is raised.
        fn lookup(slf: This<Py<Self>>, it: &mut Interp, name: &Value) -> R<Value> {
            let v = ver(&slf, it)?;
            lookup_impl(it, v, name)
        }
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        dict_set_str(&d, "unidata_version", Value::str(ucd::UNIDATA_VERSION));
        let old = Py::new(it, Ucd { version: Version::V3_2_0 });
        dict_set_str(&d, "ucd_3_2_0", old.value().clone());
    }
}
