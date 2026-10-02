//! `time` on the [`Platform`] layer (local time and clocks), with `strftime` from
//! `lumen_common::strftime`.
//!
//! [`Platform`]: crate::platform::Platform

/// This module provides various functions to manipulate time values.
///
/// There are two standard representations of time.  One is the number
/// of seconds since the Epoch, in UTC (a.k.a. GMT).  It may be an integer
/// or a floating point number (to represent fractions of seconds).
/// The epoch is the point where the time starts, the return value of time.gmtime(0).
/// It is January 1, 1970, 00:00:00 (UTC) on all platforms.
///
/// The other representation is a tuple of 9 integers giving local time.
/// The tuple items are:
///   year (including century, e.g. 1998)
///   month (1-12)
///   day (1-31)
///   hours (0-23)
///   minutes (0-59)
///   seconds (0-59)
///   weekday (0-6, Monday is 0)
///   Julian day (day in the year, 1-366)
///   DST (Daylight Savings Time) flag (-1, 0 or 1)
/// If the DST flag is 0, the time is given in the regular time zone;
/// if it is 1, the time is given in the DST time zone;
/// if it is -1, mktime() should guess based on the date and time.
#[lumen_bind::module(name = "time")]
pub mod time {
    use crate::builtins::sysextra::{structseq_full, structseq_hidden, structseq_type};
    use crate::object::*;
    use crate::pyint::BigInt;
    use crate::vm::{dict_set_str, Interp};
    use lumen_common::civil::Tm;
    use lumen_common::strftime::{strftime as format_tm, ZoneFields};
    use std::rc::Rc;

    struct StructTime;

    const FIELDS: [&str; 11] = [
        "tm_year", "tm_mon", "tm_mday", "tm_hour", "tm_min", "tm_sec", "tm_wday", "tm_yday", "tm_isdst", "tm_zone",
        "tm_gmtoff",
    ];
    const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

    fn struct_time_type(it: &mut Interp) -> Obj {
        structseq_type::<StructTime>(it, "time", "struct_time", &FIELDS, 9)
    }

    fn struct_time(it: &mut Interp, tm: Tm) -> Value {
        let ty = struct_time_type(it);
        let ints = [tm.year, tm.mon as i64, tm.mday as i64, tm.hour as i64, tm.min as i64, tm.sec as i64];
        let mut vals: Vec<Value> = ints.iter().map(|&n| Value::Int(n)).collect();
        vals.extend([Value::Int(tm.wday as i64), Value::Int(tm.yday as i64), Value::Int(tm.isdst as i64)]);
        vals.push(tm.zone.map_or(Value::None, Value::string));
        vals.push(Value::Int(tm.gmtoff));
        structseq_full(&ty, vals)
    }

    /// CPython's `_PyTime_AsSecondsDouble`.
    fn secs(ns: i128) -> f64 {
        if ns % 1_000_000_000 == 0 {
            (ns / 1_000_000_000) as f64
        } else {
            ns as f64 / 1e9
        }
    }

    /// A `timespec` as CPython's `clock_gettime` converts it.
    fn timespec_secs(ns: i128) -> f64 {
        ns.div_euclid(1_000_000_000) as f64 + ns.rem_euclid(1_000_000_000) as f64 * 1e-9
    }

    fn ns_value(ns: i128) -> Value {
        match i64::try_from(ns) {
            Ok(n) => Value::Int(n),
            Err(_) => Value::big(BigInt::from(ns)),
        }
    }

    fn float_value(v: &Value) -> Option<f64> {
        match v {
            Value::Obj(o) => match &o.kind {
                Kind::Float(f) => Some(*f),
                _ => None,
            },
            _ => None,
        }
    }

    fn now_ns(it: &Interp) -> i128 {
        it.platform.borrow().wall_time_ns() as i128
    }

    fn elapsed_ns(it: &Interp) -> i128 {
        it.platform.borrow().monotonic_ns().saturating_sub(it.start_ns) as i128 + 1_000_000_000_000
    }

    fn os_err(it: &mut Interp, e: crate::platform::IoError) -> Obj {
        it.os_error_io(&e, None)
    }

    /// A timestamp as whole seconds, rounded toward -inf (CPython's `_PyTime_ObjectToTime_t`).
    fn time_t_arg(it: &mut Interp, v: Option<&Value>) -> R<i64> {
        let v = match v {
            None | Some(Value::None) => return Ok(now_ns(it).div_euclid(1_000_000_000) as i64),
            Some(v) => v,
        };
        let range = |it: &mut Interp| it.overflow_err("timestamp out of range for platform time_t");
        if let Value::Float(f) = v {
            if f.is_nan() {
                return Err(it.value_error("Invalid value NaN (not a number)"));
            }
            let f = f.floor();
            if !(-9.223372036854776e18..9.223372036854776e18).contains(&f) {
                return Err(range(it));
            }
            return Ok(f as i64);
        }
        if let Some(f) = float_value(v) {
            return time_t_arg(it, Some(&Value::Float(f)));
        }
        match it.index_of(v) {
            Err(e) if it.is_exc_instance(&e, "OverflowError") => Err(range(it)),
            r => r,
        }
    }

    /// Seconds as a float, rejecting NaN (CPython's `_PyTime_FromSecondsObject`).
    fn seconds_arg(it: &mut Interp, v: &Value) -> R<f64> {
        if let Value::Float(f) = v {
            if f.is_nan() {
                return Err(it.value_error("Invalid value NaN (not a number)"));
            }
            return Ok(*f);
        }
        if let Some(f) = float_value(v) {
            return seconds_arg(it, &Value::Float(f));
        }
        match it.index_of(v) {
            Ok(n) => Ok(n as f64),
            Err(e) if it.is_exc_instance(&e, "OverflowError") => Err(it.overflow_err("timestamp too large to convert to C _PyTime_t")),
            Err(e) => Err(e),
        }
    }

    fn gmtime_tm(it: &mut Interp, t: i64) -> R<Tm> {
        let tm = Tm::from_epoch(t, 0);
        if i32::try_from(tm.year - 1900).is_err() {
            let errno = lumen_os::errno::errno_of_code("EOVERFLOW").unwrap_or(84);
            return Err(it.os_error_errno(errno, None, None));
        }
        Ok(Tm { zone: Some("UTC".to_string()), ..tm })
    }

    fn localtime_tm(it: &mut Interp, t: i64) -> R<Tm> {
        let r = it.platform.borrow().localtime(t);
        r.map_err(|e| os_err(it, e))
    }

    /// A C `int` item of a time tuple.
    fn c_int(it: &mut Interp, v: &Value) -> R<i32> {
        let n = it.index_of(v)?;
        i32::try_from(n).map_err(|_| {
            let msg = if n > 0 { "signed integer is greater than maximum" } else { "signed integer is less than minimum" };
            it.overflow_err(msg)
        })
    }

    /// The fields of a time tuple or `struct_time` as CPython's `gettmarg` reads them: `wday`
    /// stays in C's convention (0 = Sunday) and `yday` is 0-based, ready for [`check_tm`].
    fn tm_arg(it: &mut Interp, v: &Value, func: &str) -> R<Tm> {
        let Some(items) = v.tuple_items().map(|t| t.to_vec()) else {
            return Err(it.type_error("Tuple or struct_time argument required"));
        };
        if items.len() != 9 {
            return Err(it.type_error(&format!("{func}(): illegal time tuple argument")));
        }
        let mut f = [0i32; 9];
        for (slot, item) in f.iter_mut().zip(&items) {
            *slot = c_int(it, item)?;
        }
        if f[0] < i32::MIN + 1900 {
            return Err(it.overflow_err("year out of range"));
        }
        let mut tm = Tm {
            year: f[0] as i64,
            mon: f[1],
            mday: f[2],
            hour: f[3],
            min: f[4],
            sec: f[5],
            wday: (f[6].wrapping_add(1)) % 7,
            yday: f[7].wrapping_sub(1),
            isdst: f[8],
            gmtoff: 0,
            zone: None,
        };
        if Rc::ptr_eq(&it.type_of(v), &struct_time_type(it)) {
            let hidden = structseq_hidden(v);
            if let Some(z) = hidden.first().filter(|z| !z.is_none()) {
                tm.zone = Some(it.str_arg(z, "tm_zone")?);
            }
            if let Some(g) = hidden.get(1).filter(|g| !g.is_none()) {
                tm.gmtoff = it.index_of(g)?;
            }
        }
        Ok(tm)
    }

    /// CPython's `checktm`: range checks (0 is accepted as January, the 1st and day 1 of the year)
    /// and the switch to Python's `wday`/`yday` conventions.
    fn check_tm(it: &mut Interp, mut tm: Tm) -> R<Tm> {
        if tm.mon == 0 {
            tm.mon = 1;
        } else if !(1..=12).contains(&tm.mon) {
            return Err(it.value_error("month out of range"));
        }
        if tm.mday == 0 {
            tm.mday = 1;
        } else if !(0..=31).contains(&tm.mday) {
            return Err(it.value_error("day of month out of range"));
        }
        if !(0..=23).contains(&tm.hour) {
            return Err(it.value_error("hour out of range"));
        }
        if !(0..=59).contains(&tm.min) {
            return Err(it.value_error("minute out of range"));
        }
        if !(0..=61).contains(&tm.sec) {
            return Err(it.value_error("seconds out of range"));
        }
        if tm.wday < 0 {
            return Err(it.value_error("day of week out of range"));
        }
        if tm.yday == -1 {
            tm.yday = 0;
        } else if !(0..=365).contains(&tm.yday) {
            return Err(it.value_error("day of year out of range"));
        }
        tm.wday = (tm.wday + 6) % 7;
        tm.yday += 1;
        Ok(tm)
    }

    fn asctime_str(tm: &Tm) -> String {
        format!(
            "{} {}{:3} {:02}:{:02}:{:02} {}",
            DAYS[tm.wday as usize],
            MONTHS[(tm.mon - 1) as usize],
            tm.mday,
            tm.hour,
            tm.min,
            tm.sec,
            tm.year
        )
    }

    /// The local zone as CPython's `init_timezone` reports it: `(timezone, altzone, daylight,
    /// (std name, dst name))`, probed at the start and the middle of the current year.
    fn zone_info(it: &mut Interp) -> (i64, i64, bool, [String; 2]) {
        const YEAR: i64 = (365 * 24 + 6) * 3600;
        let t = now_ns(it).div_euclid(1_000_000_000) as i64 / YEAR * YEAR;
        let probe = |it: &mut Interp, t: i64| {
            let tm = it.platform.borrow().localtime(t).unwrap_or_default();
            (-tm.gmtoff, tm.zone.unwrap_or_default())
        };
        let (jan, jan_name) = probe(it, t);
        let (jul, jul_name) = probe(it, t + YEAR / 2);
        let (std, dst, names) = if jan < jul { (jul, jan, [jul_name, jan_name]) } else { (jan, jul, [jan_name, jul_name]) };
        (std, dst, jan != jul, names)
    }

    fn set_zone_attrs(it: &mut Interp, d: &Obj) {
        let (tz, alt, daylight, [std, dst]) = zone_info(it);
        dict_set_str(d, "timezone", Value::Int(tz));
        dict_set_str(d, "altzone", Value::Int(alt));
        dict_set_str(d, "daylight", Value::Int(daylight as i64));
        dict_set_str(d, "tzname", Value::tuple(vec![Value::string(std), Value::string(dst)]));
    }

    /// time() -> floating-point number
    ///
    /// Return the current time in seconds since the Epoch.
    /// Fractions of a second may be present if the system clock provides them.
    #[op(hint(py(text_signature = "")))]
    fn time(it: &mut Interp) -> f64 {
        secs(now_ns(it))
    }

    /// time_ns() -> int
    ///
    /// Return the current time in nanoseconds since the Epoch.
    #[op(hint(py(text_signature = "")))]
    fn time_ns(it: &mut Interp) -> Value {
        ns_value(now_ns(it))
    }

    /// monotonic() -> float
    ///
    /// Monotonic clock, cannot go backward.
    #[op(hint(py(text_signature = "")))]
    fn monotonic(it: &mut Interp) -> f64 {
        secs(elapsed_ns(it))
    }

    /// monotonic_ns() -> int
    ///
    /// Monotonic clock, cannot go backward, as nanoseconds.
    #[op(hint(py(text_signature = "")))]
    fn monotonic_ns(it: &mut Interp) -> Value {
        ns_value(elapsed_ns(it))
    }

    /// perf_counter() -> float
    ///
    /// Performance counter for benchmarking.
    #[op(hint(py(text_signature = "")))]
    fn perf_counter(it: &mut Interp) -> f64 {
        secs(elapsed_ns(it))
    }

    /// perf_counter_ns() -> int
    ///
    /// Performance counter for benchmarking as nanoseconds.
    #[op(hint(py(text_signature = "")))]
    fn perf_counter_ns(it: &mut Interp) -> Value {
        ns_value(elapsed_ns(it))
    }

    fn cpu_ns(it: &mut Interp, thread: bool) -> R<i128> {
        let r = it.platform.borrow().cpu_time_ns(thread);
        r.map_err(|e| os_err(it, e))
    }

    /// process_time() -> float
    ///
    /// Process time for profiling: sum of the kernel and user-space CPU time.
    #[op(hint(py(text_signature = "")))]
    fn process_time(it: &mut Interp) -> R<f64> {
        Ok(secs(cpu_ns(it, false)?))
    }

    /// process_time() -> int
    ///
    /// Process time for profiling as nanoseconds:
    /// sum of the kernel and user-space CPU time.
    #[op(hint(py(text_signature = "")))]
    fn process_time_ns(it: &mut Interp) -> R<Value> {
        Ok(ns_value(cpu_ns(it, false)?))
    }

    /// thread_time() -> float
    ///
    /// Thread time for profiling: sum of the kernel and user-space CPU time.
    #[op(hint(py(text_signature = "")))]
    fn thread_time(it: &mut Interp) -> R<f64> {
        Ok(secs(cpu_ns(it, true)?))
    }

    /// thread_time() -> int
    ///
    /// Thread time for profiling as nanoseconds:
    /// sum of the kernel and user-space CPU time.
    #[op(hint(py(text_signature = "")))]
    fn thread_time_ns(it: &mut Interp) -> R<Value> {
        Ok(ns_value(cpu_ns(it, true)?))
    }

    fn clock(it: &mut Interp, clk_id: &Value, res: bool) -> R<i128> {
        let id = c_int(it, clk_id)? as i64;
        let r = it.platform.borrow().clock_ns(id, res);
        r.map_err(|e| os_err(it, e))
    }

    /// clock_gettime(clk_id) -> float
    ///
    /// Return the time of the specified clock clk_id.
    #[op(hint(py(text_signature = "")))]
    fn clock_gettime(it: &mut Interp, clk_id: &Value) -> R<f64> {
        Ok(timespec_secs(clock(it, clk_id, false)?))
    }

    /// clock_gettime_ns(clk_id) -> int
    ///
    /// Return the time of the specified clock clk_id as nanoseconds.
    #[op(hint(py(text_signature = "")))]
    fn clock_gettime_ns(it: &mut Interp, clk_id: &Value) -> R<Value> {
        Ok(ns_value(clock(it, clk_id, false)?))
    }

    /// clock_getres(clk_id) -> floating-point number
    ///
    /// Return the resolution (precision) of the specified clock clk_id.
    #[op(hint(py(text_signature = "")))]
    fn clock_getres(it: &mut Interp, clk_id: &Value) -> R<f64> {
        Ok(timespec_secs(clock(it, clk_id, true)?))
    }

    fn set_clock(it: &mut Interp, clk_id: &Value, ns: i128) -> R<()> {
        let id = c_int(it, clk_id)? as i64;
        let r = it.platform.borrow_mut().clock_set_ns(id, ns);
        r.map_err(|e| os_err(it, e))
    }

    /// clock_settime(clk_id, time)
    ///
    /// Set the time of the specified clock clk_id.
    #[op(hint(py(text_signature = "")))]
    fn clock_settime(it: &mut Interp, clk_id: &Value, time: &Value) -> R<()> {
        let s = seconds_arg(it, time)?;
        set_clock(it, clk_id, (s * 1e9) as i128)
    }

    /// clock_settime_ns(clk_id, time)
    ///
    /// Set the time of the specified clock clk_id with nanoseconds.
    #[op(hint(py(text_signature = "")))]
    fn clock_settime_ns(it: &mut Interp, clk_id: &Value, time: &Value) -> R<()> {
        let ns = it.index_of(time)?;
        set_clock(it, clk_id, ns as i128)
    }

    /// get_clock_info(name: str) -> dict
    ///
    /// Get information of the specified clock.
    #[op(hint(py(text_signature = "")))]
    fn get_clock_info(it: &mut Interp, name: &str) -> R<Value> {
        let (implementation, monotonic, adjustable, resolution) = match name {
            "time" => ("clock_gettime(CLOCK_REALTIME)", false, true, 1.0000000000000002e-06),
            "monotonic" | "perf_counter" => ("mach_absolute_time()", true, false, 4.166666666666667e-08),
            "process_time" => ("clock_gettime(CLOCK_PROCESS_CPUTIME_ID)", true, false, 1.0000000000000002e-06),
            "thread_time" => ("clock_gettime(CLOCK_THREAD_CPUTIME_ID)", true, false, 4.2000000000000006e-08),
            _ => return Err(it.value_error("unknown clock")),
        };
        Ok(it.new_namespace(vec![
            ("implementation", Value::str(implementation)),
            ("monotonic", Value::Bool(monotonic)),
            ("adjustable", Value::Bool(adjustable)),
            ("resolution", Value::Float(resolution)),
        ]))
    }

    /// sleep(seconds)
    ///
    /// Delay execution for a given number of seconds.  The argument may be
    /// a floating point number for subsecond precision.
    #[op(hint(py(text_signature = "")))]
    fn sleep(it: &mut Interp, seconds: &Value) -> R<()> {
        let s = seconds_arg(it, seconds)?;
        if s < 0.0 {
            return Err(it.value_error("sleep length must be non-negative"));
        }
        it.flush_out();
        let deadline = it.platform.borrow().monotonic_ns().saturating_add((s.min(1e9) * 1e9) as u64);
        loop {
            it.poll()?;
            let left = deadline.saturating_sub(it.platform.borrow().monotonic_ns());
            if left == 0 {
                return Ok(());
            }
            // Sleep in slices so an interrupt is noticed promptly.
            it.platform.borrow_mut().sleep(left.min(20_000_000) as f64 / 1e9);
        }
    }

    /// gmtime([seconds]) -> (tm_year, tm_mon, tm_mday, tm_hour, tm_min,
    ///                        tm_sec, tm_wday, tm_yday, tm_isdst)
    ///
    /// Convert seconds since the Epoch to a time tuple expressing UTC (a.k.a.
    /// GMT).  When 'seconds' is not passed in, convert the current time instead.
    ///
    /// If the platform supports the tm_gmtoff and tm_zone, they are available as
    /// attributes only.
    #[op(hint(py(text_signature = "")))]
    fn gmtime(it: &mut Interp, seconds: Option<&Value>) -> R<Value> {
        let t = time_t_arg(it, seconds)?;
        let tm = gmtime_tm(it, t)?;
        Ok(struct_time(it, tm))
    }

    /// localtime([seconds]) -> (tm_year,tm_mon,tm_mday,tm_hour,tm_min,
    ///                           tm_sec,tm_wday,tm_yday,tm_isdst)
    ///
    /// Convert seconds since the Epoch to a time tuple expressing local time.
    /// When 'seconds' is not passed in, convert the current time instead.
    #[op(hint(py(text_signature = "")))]
    fn localtime(it: &mut Interp, seconds: Option<&Value>) -> R<Value> {
        let t = time_t_arg(it, seconds)?;
        let tm = localtime_tm(it, t)?;
        Ok(struct_time(it, tm))
    }

    /// mktime(tuple) -> floating-point number
    ///
    /// Convert a time tuple in local time to seconds since the Epoch.
    /// Note that mktime(gmtime(0)) will not generally return zero for most
    /// time zones; instead the returned value will either be equal to that
    /// of the timezone or altzone attributes on the time module.
    #[op(hint(py(text_signature = "")))]
    fn mktime(it: &mut Interp, tuple: &Value) -> R<f64> {
        let tm = tm_arg(it, tuple, "mktime")?;
        let r = it.platform.borrow().mktime(&tm);
        match r {
            Some(t) => Ok(t as f64),
            None => Err(it.overflow_err("mktime argument out of range")),
        }
    }

    /// asctime([tuple]) -> string
    ///
    /// Convert a time tuple to a string, e.g. 'Sat Jun 06 16:26:11 1998'.
    /// When the time tuple is not present, current time as returned by localtime()
    /// is used.
    #[op(hint(py(text_signature = "")))]
    fn asctime(it: &mut Interp, tuple: Option<&Value>) -> R<String> {
        let tm = match tuple {
            None => {
                let t = time_t_arg(it, None)?;
                localtime_tm(it, t)?
            }
            Some(v) => {
                let tm = tm_arg(it, v, "asctime")?;
                check_tm(it, tm)?
            }
        };
        Ok(asctime_str(&tm))
    }

    /// ctime(seconds) -> string
    ///
    /// Convert a time in seconds since the Epoch to a string in local time.
    /// This is equivalent to asctime(localtime(seconds)). When the time tuple is
    /// not present, current time as returned by localtime() is used.
    #[op(hint(py(text_signature = "")))]
    fn ctime(it: &mut Interp, seconds: Option<&Value>) -> R<String> {
        let t = time_t_arg(it, seconds)?;
        let tm = localtime_tm(it, t)?;
        Ok(asctime_str(&tm))
    }

    /// strftime(format[, tuple]) -> string
    ///
    /// Convert a time tuple to a string according to a format specification.
    /// See the library reference manual for formatting codes. When the time tuple
    /// is not present, current time as returned by localtime() is used.
    ///
    /// Commonly used format codes:
    ///
    /// %Y  Year with century as a decimal number.
    /// %m  Month as a decimal number [01,12].
    /// %d  Day of the month as a decimal number [01,31].
    /// %H  Hour (24-hour clock) as a decimal number [00,23].
    /// %M  Minute as a decimal number [00,59].
    /// %S  Second as a decimal number [00,61].
    /// %z  Time zone offset from UTC.
    /// %a  Locale's abbreviated weekday name.
    /// %A  Locale's full weekday name.
    /// %b  Locale's abbreviated month name.
    /// %B  Locale's full month name.
    /// %c  Locale's appropriate date and time representation.
    /// %I  Hour (12-hour clock) as a decimal number [01,12].
    /// %p  Locale's equivalent of either AM or PM.
    ///
    /// Other codes may be available on your platform.  See documentation for
    /// the C library strftime function.
    #[op(hint(py(text_signature = "")))]
    fn strftime(it: &mut Interp, format: &str, tuple: Option<&Value>) -> R<String> {
        let tm = match tuple {
            None => {
                let t = time_t_arg(it, None)?;
                localtime_tm(it, t)?
            }
            Some(v) => {
                let mut tm = tm_arg(it, v, "strftime")?;
                tm.isdst = tm.isdst.clamp(-1, 1);
                check_tm(it, tm)?
            }
        };
        let (timezone, _, _, names) = zone_info(it);
        let name = match &tm.zone {
            Some(z) => z.clone(),
            None if tm.isdst < 0 => String::new(),
            None => names[(tm.isdst > 0) as usize].clone(),
        };
        let offset = if cfg!(target_os = "macos") {
            // The BSD C library derives %z from the zone and the DST flag, not from tm_gmtoff.
            (tm.isdst >= 0).then(|| -timezone + if tm.isdst > 0 { 3600 } else { 0 })
        } else {
            Some(tm.gmtoff)
        };
        let epoch = if format.contains("%s") { it.platform.borrow().mktime(&tm).unwrap_or(-1) } else { 0 };
        Ok(format_tm(format, &tm, &ZoneFields { name, offset, epoch }))
    }

    /// strptime(string, format) -> struct_time
    ///
    /// Parse a string to a time tuple according to a format specification.
    /// See the library reference manual for formatting codes (same as
    /// strftime()).
    #[op(hint(py(text_signature = "")))]
    fn strptime(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        let m = it.import_module("_strptime")?;
        let f = it.get_attr_str(&Value::Obj(m), "_strptime_time")?;
        it.call(&f, args.to_vec(), Vec::new())
    }

    /// tzset()
    ///
    /// Initialize, or reinitialize, the local timezone to the value of the
    /// environment variable TZ.  The TZ environment variable should be specified in
    /// standard Unix timezone format as documented in the tzset man page
    /// (eg. 'US/Eastern', 'Europe/Amsterdam'). Unknown timezones will silently
    /// fall back to UTC. If the TZ environment variable is not set, the local
    /// timezone is set to the systems best guess of wallclock time.
    /// Changing the TZ environment variable without calling tzset *may* change
    /// the local timezone used by methods such as localtime, but this behaviour
    /// should not be relied on.
    #[op(hint(py(text_signature = "")))]
    fn tzset(it: &mut Interp) -> R<()> {
        it.platform.borrow_mut().tzset();
        let m = it.import_module("time")?;
        let d = it.module_dict(&m);
        set_zone_attrs(it, &d);
        Ok(())
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        let ty = struct_time_type(it);
        dict_set_str(&d, "struct_time", Value::Obj(ty));
        dict_set_str(&d, "_STRUCT_TM_ITEMS", Value::Int(11));
        for (name, id) in lumen_os::time::clock_ids() {
            dict_set_str(&d, name, Value::Int(*id));
        }
        set_zone_attrs(it, &d);
    }
}
