//! `_testcapi` wrappers of the time APIs: the `_PyTime_t` conversions of `pytime.c` (arithmetic in
//! [`lumen_common::pytime`]) and the datetime C API of `datetime.c`, which the `datetime` module
//! provides here.

#![allow(non_snake_case)]

use super::int_value;
use crate::bind::index;
use crate::object::*;
use crate::pyint::BigInt;
use crate::vm::Interp;
use lumen_common::pytime::{self, Round, TimeError};

fn time_error(it: &mut Interp, e: TimeError) -> Obj {
    match e {
        TimeError::Overflow => it.overflow_err("timestamp out of range for platform time_t"),
        TimeError::Nan => it.value_error("Invalid value NaN (not a number)"),
    }
}

fn rounding(it: &mut Interp, round: i64) -> R<Round> {
    Round::from_i64(round).ok_or_else(|| it.value_error("invalid rounding"))
}

fn as_i64(b: &BigInt) -> Option<i64> {
    b.to_i128().and_then(|n| i64::try_from(n).ok())
}

/// `_PyTime_FromNanosecondsObject`: an `int` that fits a signed 64-bit integer.
fn nanoseconds(it: &mut Interp, v: &Value) -> R<i64> {
    match v.as_bigint() {
        Some(b) if is_float(v).is_none() => as_i64(&b).ok_or_else(|| it.overflow_err("int too big to convert")),
        _ => {
            let t = it.tp_name_of(v);
            Err(it.type_error(&format!("expect int, got {t}")))
        }
    }
}

fn is_float(v: &Value) -> Option<f64> {
    match v {
        Value::Float(f) => Some(*f),
        Value::Obj(o) => match o.kind {
            Kind::Float(f) => Some(f),
            _ => None,
        },
        _ => None,
    }
}

/// `PyLong_AsLongLong` of a timestamp: `OverflowError` when it does not fit `time_t`.
fn time_t_of(it: &mut Interp, v: &Value) -> R<i64> {
    let i = index(it, v)?;
    match i.as_bigint().as_ref().and_then(as_i64) {
        Some(n) => Ok(n),
        None => Err(it.overflow_err("timestamp out of range for platform time_t")),
    }
}

fn pair(a: i64, b: i64) -> Value {
    Value::tuple(vec![int_value(i128::from(a)), int_value(i128::from(b))])
}

fn datetime_attr(it: &mut Interp, name: &str) -> R<Value> {
    let m = it.import_module("datetime")?;
    it.get_attr_str(&Value::Obj(m), name)
}

fn datetime_call(it: &mut Interp, class: &str, args: Vec<Value>) -> R<Value> {
    let c = datetime_attr(it, class)?;
    it.call(&c, args, Vec::new())
}

fn datetime_check(it: &mut Interp, class: &str, obj: &Value, exact: Option<&Value>) -> R<bool> {
    let cls = datetime_attr(it, class)?;
    let exact = match exact {
        Some(e) => it.truthy(e)?,
        None => false,
    };
    if exact {
        let t = Value::Obj(it.type_of(obj));
        return Ok(t.is(&cls));
    }
    it.isinstance_value(obj, &cls)
}

fn ints(values: &[&str], obj: &Value, it: &mut Interp) -> R<Vec<Value>> {
    values.iter().map(|n| it.get_attr_str(obj, n)).collect()
}

fn timestamp_args(ts: &Value, tz: Option<&Value>) -> Vec<Value> {
    match tz {
        Some(t) => vec![ts.clone(), t.clone()],
        None => vec![ts.clone()],
    }
}

#[lumen_bind::module(name = "_testcapi")]
pub mod timecapi {
    use super::*;

    // ---- _PyTime_t ----------------------------------------------------------------------------

    /// PyTime_FromSeconds(seconds): `_PyTime_FromSeconds`, in nanoseconds.
    #[op]
    fn PyTime_FromSeconds(it: &mut Interp, seconds: &Value) -> R<Value> {
        let i = index(it, seconds)?;
        let n = i.as_bigint().and_then(|b| b.to_i128());
        match n {
            Some(n) if n > i128::from(i32::MAX) => Err(it.overflow_err("signed integer is greater than maximum")),
            Some(n) if n < i128::from(i32::MIN) => Err(it.overflow_err("signed integer is less than minimum")),
            Some(n) => match pytime::from_seconds(n as i64) {
                Ok(ns) => Ok(int_value(i128::from(ns))),
                Err(e) => Err(time_error(it, e)),
            },
            None => Err(it.overflow_err("signed integer is greater than maximum")),
        }
    }

    /// PyTime_FromSecondsObject(obj, round): `_PyTime_FromSecondsObject`, in nanoseconds.
    #[op]
    fn PyTime_FromSecondsObject(it: &mut Interp, obj: &Value, round: i64) -> R<Value> {
        let round = rounding(it, round)?;
        let ns = match is_float(obj) {
            Some(d) => pytime::from_seconds_f64(d, round).map_err(|e| time_error(it, e))?,
            None => {
                let secs = time_t_of(it, obj)?;
                pytime::from_seconds(secs).map_err(|_| it.overflow_err("timestamp too large to convert to C _PyTime_t"))?
            }
        };
        Ok(int_value(i128::from(ns)))
    }

    /// PyTime_AsSecondsDouble(ns): `_PyTime_AsSecondsDouble`.
    #[op]
    fn PyTime_AsSecondsDouble(it: &mut Interp, ns: &Value) -> R<f64> {
        Ok(pytime::as_seconds_f64(nanoseconds(it, ns)?))
    }

    /// PyTime_AsTimeval(ns, round) -> (seconds, microseconds)
    #[op]
    fn PyTime_AsTimeval(it: &mut Interp, ns: &Value, round: i64) -> R<Value> {
        let round = rounding(it, round)?;
        let t = nanoseconds(it, ns)?;
        let (s, us) = pytime::as_timeval(t, round);
        Ok(pair(s, us))
    }

    /// PyTime_AsTimeval_clamp(ns, round) -> (seconds, microseconds)
    #[op]
    fn PyTime_AsTimeval_clamp(it: &mut Interp, ns: &Value, round: i64) -> R<Value> {
        let round = rounding(it, round)?;
        let t = nanoseconds(it, ns)?;
        let (s, us) = pytime::as_timeval(t, round);
        Ok(pair(s, us))
    }

    /// PyTime_AsTimespec(ns) -> (seconds, nanoseconds)
    #[op]
    fn PyTime_AsTimespec(it: &mut Interp, ns: &Value) -> R<Value> {
        let (s, n) = pytime::as_timespec(nanoseconds(it, ns)?);
        Ok(pair(s, n))
    }

    /// PyTime_AsTimespec_clamp(ns) -> (seconds, nanoseconds)
    #[op]
    fn PyTime_AsTimespec_clamp(it: &mut Interp, ns: &Value) -> R<Value> {
        let (s, n) = pytime::as_timespec(nanoseconds(it, ns)?);
        Ok(pair(s, n))
    }

    /// PyTime_AsMilliseconds(ns, round): milliseconds, as nanoseconds.
    #[op]
    fn PyTime_AsMilliseconds(it: &mut Interp, ns: &Value, round: i64) -> R<Value> {
        let t = nanoseconds(it, ns)?;
        let round = rounding(it, round)?;
        Ok(int_value(i128::from(pytime::divide(t, pytime::MS_TO_NS, round))))
    }

    /// PyTime_AsMicroseconds(ns, round): microseconds, as nanoseconds.
    #[op]
    fn PyTime_AsMicroseconds(it: &mut Interp, ns: &Value, round: i64) -> R<Value> {
        let t = nanoseconds(it, ns)?;
        let round = rounding(it, round)?;
        Ok(int_value(i128::from(pytime::divide(t, pytime::US_TO_NS, round))))
    }

    /// pytime_object_to_time_t(obj, round): `_PyTime_ObjectToTime_t`.
    #[op(hint(py(arg_style = "parse", arg_name = "pytime_object_to_time_t")))]
    fn pytime_object_to_time_t(it: &mut Interp, obj: &Value, round: i64) -> R<Value> {
        let round = rounding(it, round)?;
        let secs = match is_float(obj) {
            Some(d) => pytime::time_t_f64(d, round).map_err(|e| time_error(it, e))?,
            None => time_t_of(it, obj)?,
        };
        Ok(int_value(i128::from(secs)))
    }

    /// pytime_object_to_timeval(obj, round) -> (seconds, microseconds)
    #[op(hint(py(arg_style = "parse", arg_name = "pytime_object_to_timeval")))]
    fn pytime_object_to_timeval(it: &mut Interp, obj: &Value, round: i64) -> R<Value> {
        object_to_denominator(it, obj, round, 1_000_000)
    }

    /// pytime_object_to_timespec(obj, round) -> (seconds, nanoseconds)
    #[op(hint(py(arg_style = "parse", arg_name = "pytime_object_to_timespec")))]
    fn pytime_object_to_timespec(it: &mut Interp, obj: &Value, round: i64) -> R<Value> {
        object_to_denominator(it, obj, round, 1_000_000_000)
    }

    // ---- datetime C API -----------------------------------------------------------------------

    #[op]
    fn test_datetime_capi() {}

    #[op]
    fn datetime_check_date(it: &mut Interp, obj: &Value, exact: Option<&Value>) -> R<bool> {
        datetime_check(it, "date", obj, exact)
    }

    #[op]
    fn datetime_check_time(it: &mut Interp, obj: &Value, exact: Option<&Value>) -> R<bool> {
        datetime_check(it, "time", obj, exact)
    }

    #[op]
    fn datetime_check_datetime(it: &mut Interp, obj: &Value, exact: Option<&Value>) -> R<bool> {
        datetime_check(it, "datetime", obj, exact)
    }

    #[op]
    fn datetime_check_delta(it: &mut Interp, obj: &Value, exact: Option<&Value>) -> R<bool> {
        datetime_check(it, "timedelta", obj, exact)
    }

    #[op]
    fn datetime_check_tzinfo(it: &mut Interp, obj: &Value, exact: Option<&Value>) -> R<bool> {
        datetime_check(it, "tzinfo", obj, exact)
    }

    /// make_timezones_capi(): three `timezone` objects for UTC-5.
    #[op]
    fn make_timezones_capi(it: &mut Interp) -> R<Value> {
        let offset = datetime_call(it, "timedelta", vec![Value::Int(0), Value::Int(-18000), Value::Int(0)])?;
        let name = Value::str("EST");
        let a = datetime_call(it, "timezone", vec![offset.clone(), name.clone()])?;
        let b = datetime_call(it, "timezone", vec![offset.clone(), name])?;
        let c = datetime_call(it, "timezone", vec![offset])?;
        Ok(Value::tuple(vec![a, b, c]))
    }

    /// get_timezones_offset_zero(): the UTC singleton twice, then a `+00:00` zone that is not it.
    #[op]
    fn get_timezones_offset_zero(it: &mut Interp) -> R<Value> {
        let offset = datetime_call(it, "timedelta", vec![Value::Int(0), Value::Int(0), Value::Int(0)])?;
        let a = datetime_call(it, "timezone", vec![offset.clone()])?;
        let b = datetime_call(it, "timezone", vec![offset.clone()])?;
        let c = datetime_call(it, "timezone", vec![offset, Value::str("")])?;
        Ok(Value::tuple(vec![a, b, c]))
    }

    /// get_timezone_utc_capi(macro=False): `datetime.timezone.utc`.
    #[op]
    fn get_timezone_utc_capi(it: &mut Interp, macro_: Option<&Value>) -> R<Value> {
        let _ = macro_;
        let tz = datetime_attr(it, "timezone")?;
        it.get_attr_str(&tz, "utc")
    }

    #[op]
    fn get_date_fromdate(it: &mut Interp, macro_: &Value, year: i64, month: i64, day: i64) -> R<Value> {
        let _ = macro_;
        datetime_call(it, "date", vec![Value::Int(year), Value::Int(month), Value::Int(day)])
    }

    #[op]
    #[allow(clippy::too_many_arguments)]
    fn get_datetime_fromdateandtime(it: &mut Interp, macro_: &Value, year: i64, month: i64, day: i64, hour: i64, minute: i64, second: i64, microsecond: i64) -> R<Value> {
        let _ = macro_;
        let args = [year, month, day, hour, minute, second, microsecond].map(Value::Int).to_vec();
        datetime_call(it, "datetime", args)
    }

    #[op]
    #[allow(clippy::too_many_arguments)]
    fn get_datetime_fromdateandtimeandfold(it: &mut Interp, macro_: &Value, year: i64, month: i64, day: i64, hour: i64, minute: i64, second: i64, microsecond: i64, fold: i64) -> R<Value> {
        let _ = macro_;
        let args = [year, month, day, hour, minute, second, microsecond].map(Value::Int).to_vec();
        let c = datetime_attr(it, "datetime")?;
        let key = it.str_obj("fold");
        it.call(&c, args, vec![(key, Value::Int(fold))])
    }

    #[op]
    fn get_time_fromtime(it: &mut Interp, macro_: &Value, hour: i64, minute: i64, second: i64, microsecond: i64) -> R<Value> {
        let _ = macro_;
        let args = [hour, minute, second, microsecond].map(Value::Int).to_vec();
        datetime_call(it, "time", args)
    }

    #[op]
    fn get_time_fromtimeandfold(it: &mut Interp, macro_: &Value, hour: i64, minute: i64, second: i64, microsecond: i64, fold: i64) -> R<Value> {
        let _ = macro_;
        let args = [hour, minute, second, microsecond].map(Value::Int).to_vec();
        let c = datetime_attr(it, "time")?;
        let key = it.str_obj("fold");
        it.call(&c, args, vec![(key, Value::Int(fold))])
    }

    #[op]
    fn get_delta_fromdsu(it: &mut Interp, macro_: &Value, days: i64, seconds: i64, microseconds: i64) -> R<Value> {
        let _ = macro_;
        datetime_call(it, "timedelta", vec![Value::Int(days), Value::Int(seconds), Value::Int(microseconds)])
    }

    #[op]
    fn get_date_fromtimestamp(it: &mut Interp, ts: &Value, macro_: Option<&Value>) -> R<Value> {
        let _ = macro_;
        let date = datetime_attr(it, "date")?;
        let f = it.get_attr_str(&date, "fromtimestamp")?;
        it.call(&f, vec![ts.clone()], Vec::new())
    }

    #[op]
    fn get_datetime_fromtimestamp(it: &mut Interp, ts: &Value, tzinfo: &Value, usetz: Option<&Value>, macro_: Option<&Value>) -> R<Value> {
        let _ = macro_;
        let usetz = match usetz {
            Some(u) => it.truthy(u)?,
            None => false,
        };
        let class = datetime_attr(it, "datetime")?;
        let f = it.get_attr_str(&class, "fromtimestamp")?;
        let args = timestamp_args(ts, usetz.then_some(tzinfo));
        it.call(&f, args, Vec::new())
    }

    #[op]
    fn PyDateTime_GET(it: &mut Interp, obj: &Value) -> R<Value> {
        Ok(Value::tuple(ints(&["year", "month", "day"], obj, it)?))
    }

    #[op]
    fn PyDateTime_DATE_GET(it: &mut Interp, obj: &Value) -> R<Value> {
        Ok(Value::tuple(ints(&["hour", "minute", "second", "microsecond", "tzinfo"], obj, it)?))
    }

    #[op]
    fn PyDateTime_TIME_GET(it: &mut Interp, obj: &Value) -> R<Value> {
        Ok(Value::tuple(ints(&["hour", "minute", "second", "microsecond", "tzinfo"], obj, it)?))
    }

    #[op]
    fn PyDateTime_DELTA_GET(it: &mut Interp, obj: &Value) -> R<Value> {
        Ok(Value::tuple(ints(&["days", "seconds", "microseconds"], obj, it)?))
    }
}

fn object_to_denominator(it: &mut Interp, obj: &Value, round: i64, denominator: i64) -> R<Value> {
    let round = rounding(it, round)?;
    let (secs, frac) = match is_float(obj) {
        Some(d) => pytime::split_f64(d, denominator, round).map_err(|e| time_error(it, e))?,
        None => (time_t_of(it, obj)?, 0),
    };
    Ok(pair(secs, frac))
}
