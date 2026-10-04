//! `_zoneinfo`: `zoneinfo.ZoneInfo` on `lumen_common::tzrules`. As in CPython, the TZif file is
//! read by `zoneinfo._common.load_data` and found by `zoneinfo._tzpath.find_tzfile`.

/// C implementation of the zoneinfo module
#[lumen_bind::module(name = "_zoneinfo")]
pub mod _zoneinfo {
    #![allow(clippy::new_ret_no_self)]
    use crate::bind::{opaque_instance, This};
    use crate::builtins::native::with_opaque;
    use crate::object::*;
    use crate::vm::{dict_set_str, Interp};
    use lumen_common::tzrules::{After, Applies, ZoneError, ZoneRules};
    use std::collections::HashMap;
    use std::rc::Rc;

    const STRONG_CACHE_SIZE: usize = 8;

    #[derive(Clone, Copy, PartialEq)]
    enum Source {
        NoCache,
        Cache,
        File,
    }

    /// The module state: the base class's caches and the shared timedeltas.
    #[derive(Default)]
    struct State {
        timedeltas: HashMap<i64, Value>,
        weak_cache: Option<Value>,
        /// Most recently used first.
        strong_cache: Vec<(Value, Value)>,
    }

    /// One local time type as Python objects.
    #[derive(Clone)]
    struct TtInfo {
        utcoff: Value,
        dstoff: Value,
        tzname: Value,
        utcoff_secs: i64,
        dstoff_secs: i64,
    }

    /// ZoneInfo(key)
    ///
    /// Create a ZoneInfo object for the time zone `key`.
    #[class(
        name = "ZoneInfo",
        module = "zoneinfo",
        hint(py(base = "datetime.tzinfo"))
    )]
    pub struct ZoneInfo {
        key: Value,
        file_repr: Option<String>,
        source: Source,
        rules: ZoneRules,
        types: Vec<TtInfo>,
        std: Option<TtInfo>,
        dst: Option<TtInfo>,
        fixed_offset: bool,
    }

    fn timedelta(it: &mut Interp, secs: i64) -> R<Value> {
        if let Some(v) = it.native_state::<State>().timedeltas.get(&secs) {
            return Ok(v.clone());
        }
        let m = it.import_module("datetime")?;
        let td = it.get_attr_str(&Value::Obj(m), "timedelta")?;
        let kw = vec![(it.str_obj("seconds"), Value::Int(secs))];
        let v = it.call(&td, Vec::new(), kw)?;
        it.native_state::<State>()
            .timedeltas
            .insert(secs, v.clone());
        Ok(v)
    }

    fn ttinfo(it: &mut Interp, utcoff: i64, dstoff: i64, tzname: Value) -> R<TtInfo> {
        Ok(TtInfo {
            utcoff: timedelta(it, utcoff)?,
            dstoff: timedelta(it, dstoff)?,
            tzname,
            utcoff_secs: utcoff,
            dstoff_secs: dstoff,
        })
    }

    fn module_attr(it: &mut Interp, module: &str, name: &str) -> R<Value> {
        let m = it.import_module(module)?;
        it.get_attr_str(&Value::Obj(m), name)
    }

    fn items(it: &mut Interp, v: &Value) -> R<Vec<Value>> {
        it.iterate_to_vec(v)
    }

    /// Builds a zone from a TZif file object (CPython's `load_data`).
    fn load(
        it: &mut Interp,
        file_obj: &Value,
        key: Value,
        source: Source,
        file_repr: Option<String>,
    ) -> R<ZoneInfo> {
        let load_data = module_attr(it, "zoneinfo._common", "load_data")?;
        let data = it.call(&load_data, vec![file_obj.clone()], Vec::new())?;
        let fields = match &data {
            Value::Obj(o) if o.cls.is_none() && matches!(o.kind, Kind::Tuple(_)) => {
                data.tuple_items().unwrap_or(&[]).to_vec()
            }
            _ => {
                let r = it.repr_of(&data)?;
                return Err(it.type_error(&format!("Invalid data result type: {r}")));
            }
        };
        if fields.len() < 6 {
            return Err(it.new_exc_str("IndexError", "tuple index out of range"));
        }
        let trans_utc = items(it, &fields[1])?;
        let trans_idx_list = items(it, &fields[0])?;
        let utcoff_list = items(it, &fields[2])?;
        let isdst_list = items(it, &fields[3])?;
        let abbr = items(it, &fields[4])?;
        let mut trans = Vec::with_capacity(trans_utc.len());
        let mut idx = Vec::with_capacity(trans_utc.len());
        for (i, t) in trans_utc.iter().enumerate() {
            trans.push(it.index_of(t)?);
            let n = it.index_of(trans_idx_list.get(i).unwrap_or(&Value::Int(-1)))?;
            if n < 0 || n as usize > utcoff_list.len() {
                return Err(it.value_error(&format!(
                    "Invalid transition index found while reading TZif: {n}"
                )));
            }
            idx.push(n as usize);
        }
        let mut utcoff = Vec::with_capacity(utcoff_list.len());
        let mut isdst = Vec::with_capacity(utcoff_list.len());
        for (i, u) in utcoff_list.iter().enumerate() {
            utcoff.push(it.index_of(u)?);
            let d = isdst_list.get(i).cloned().unwrap_or(Value::Bool(false));
            isdst.push(it.truthy(&d)?);
        }
        let tz_str = match &fields[5] {
            Value::None => None,
            v if !it.truthy(v)? => None,
            v => Some(it.bytes_of(v)?.to_vec()),
        };
        let rules = match ZoneRules::new(idx, trans, utcoff, &isdst, tz_str.as_deref()) {
            Ok(r) => r,
            Err(ZoneError::BadIndex(n)) => {
                return Err(it.value_error(&format!(
                    "Invalid transition index found while reading TZif: {n}"
                )));
            }
            Err(ZoneError::NoInfo) => return Err(it.value_error("No time zone information found.")),
            Err(ZoneError::TzStr(e)) => {
                let r = it.repr_of(&fields[5])?;
                return Err(it.value_error(&format!("{} {r}", e.message())));
            }
        };
        let mut types = Vec::with_capacity(rules.utcoff.len());
        for i in 0..rules.utcoff.len() {
            let name = abbr.get(i).cloned().unwrap_or(Value::None);
            types.push(ttinfo(it, rules.utcoff[i], rules.dstoff[i], name)?);
        }
        let (std, dst) = match &rules.after {
            After::Rule(r) => {
                let std = ttinfo(it, r.std_offset, 0, Value::string(r.std_abbr.clone()))?;
                let dst = match &r.dst {
                    Some(d) => Some(ttinfo(
                        it,
                        d.offset,
                        d.offset - r.std_offset,
                        Value::string(d.abbr.clone()),
                    )?),
                    None => None,
                };
                (Some(std), dst)
            }
            After::Type(_) => (None, None),
        };
        let after_std = match &rules.after {
            After::Type(i) => types.get(*i).cloned(),
            After::Rule(_) => std.clone(),
        };
        let fixed_offset = if types.len() > 1 || dst.is_some() {
            false
        } else if types.is_empty() {
            true
        } else {
            match after_std {
                Some(a) => {
                    let t = &types[0];
                    t.utcoff_secs == a.utcoff_secs
                        && t.dstoff_secs == a.dstoff_secs
                        && it.values_eq(&t.tzname, &a.tzname)?
                }
                None => false,
            }
        };
        Ok(ZoneInfo {
            key,
            file_repr,
            source,
            rules,
            types,
            std,
            dst,
            fixed_offset,
        })
    }

    /// Opens and loads the zone `key` (CPython's `zoneinfo_new_instance`).
    fn new_instance(it: &mut Interp, cls: &Obj, key: &Value) -> R<Value> {
        let find = module_attr(it, "zoneinfo._tzpath", "find_tzfile")?;
        let path = it.call(&find, vec![key.clone()], Vec::new())?;
        let file_obj = if path.is_none() {
            let load_tzdata = module_attr(it, "zoneinfo._common", "load_tzdata")?;
            it.call(&load_tzdata, vec![key.clone()], Vec::new())?
        } else {
            let open = module_attr(it, "io", "open")?;
            it.call(&open, vec![path, Value::str("rb")], Vec::new())?
        };
        let loaded = load(it, &file_obj, key.clone(), Source::NoCache, None);
        let closed = it.call_method(&file_obj, "close", Vec::new());
        let zone = loaded?;
        closed?;
        Ok(opaque_instance(cls, zone))
    }

    fn is_base(it: &mut Interp, cls: &Obj) -> bool {
        Rc::ptr_eq(cls, &crate::bind::type_object::<ZoneInfo>(it))
    }

    fn weak_cache(it: &mut Interp, cls: &Obj) -> R<Value> {
        if !is_base(it, cls) {
            return it.get_attr_str(&Value::Obj(cls.clone()), "_weak_cache");
        }
        if let Some(c) = it.native_state::<State>().weak_cache.clone() {
            return Ok(c);
        }
        let c = new_weak_cache(it)?;
        it.native_state::<State>().weak_cache = Some(c.clone());
        Ok(c)
    }

    fn new_weak_cache(it: &mut Interp) -> R<Value> {
        let wvd = module_attr(it, "weakref", "WeakValueDictionary")?;
        it.call(&wvd, Vec::new(), Vec::new())
    }

    fn strong_position(it: &mut Interp, key: &Value) -> R<Option<usize>> {
        let keys: Vec<Value> = it
            .native_state::<State>()
            .strong_cache
            .iter()
            .map(|(k, _)| k.clone())
            .collect();
        for (i, k) in keys.iter().enumerate() {
            if it.values_eq(key, k)? {
                return Ok(Some(i));
            }
        }
        Ok(None)
    }

    fn set_source(v: &Value, source: Source) {
        with_opaque::<ZoneInfo, _>(v, |z| z.source = source);
    }

    fn timestamp_of(it: &mut Interp, dt: &Value) -> R<i64> {
        let ord = it.call_method(dt, "toordinal", Vec::new())?;
        let ord = it.index_of(&ord)?;
        let mut secs = 0;
        for (name, scale) in [("hour", 3600), ("minute", 60), ("second", 1)] {
            let v = it.get_attr_str(dt, name)?;
            secs += it.index_of(&v)? * scale;
        }
        Ok((ord - 719_163) * 86_400 + secs)
    }

    fn int_attr(it: &mut Interp, v: &Value, name: &str) -> R<i64> {
        let a = it.get_attr_str(v, name)?;
        it.index_of(&a)
    }

    impl ZoneInfo {
        fn ttinfo(&self, a: Applies) -> &TtInfo {
            match a {
                Applies::Type(i) => &self.types[i],
                Applies::Std => self.std.as_ref().expect("a TZ footer has a standard time"),
                Applies::Dst => self
                    .dst
                    .as_ref()
                    .or(self.std.as_ref())
                    .expect("a TZ footer has a standard time"),
            }
        }

        fn after_std(&self) -> &TtInfo {
            match &self.rules.after {
                After::Type(i) => &self.types[*i],
                After::Rule(_) => self.ttinfo(Applies::Std),
            }
        }
    }

    /// The zone's local time type at `dt` (`None` for `datetime.time`'s calls).
    fn find(it: &mut Interp, slf: &Value, dt: &Value) -> R<Option<TtInfo>> {
        let Some((fixed, std)) =
            with_opaque::<ZoneInfo, _>(slf, |z| (z.fixed_offset, z.after_std().clone()))
        else {
            return Err(it.type_error("descriptor requires a 'zoneinfo.ZoneInfo' object"));
        };
        if dt.is_none() {
            return Ok(fixed.then_some(std));
        }
        let ts = timestamp_of(it, dt)?;
        let fold = int_attr(it, dt, "fold")? != 0;
        let year = int_attr(it, dt, "year")?;
        Ok(with_opaque::<ZoneInfo, _>(slf, |z| {
            z.ttinfo(z.rules.find_local(ts, fold, year)).clone()
        }))
    }

    #[methods]
    impl ZoneInfo {
        #[constructor(hint(py(text_signature = "")))]
        fn new(cls: This<Value>, it: &mut Interp, #[kw] key: &Value) -> R<Value> {
            let Value::Obj(cls) = cls.0 else {
                return Err(it.type_error("ZoneInfo.__new__(X): X is not a type object"));
            };
            let base = is_base(it, &cls);
            if base {
                if let Some(i) = strong_position(it, key)? {
                    let st = it.native_state::<State>();
                    let entry = st.strong_cache.remove(i);
                    let zone = entry.1.clone();
                    st.strong_cache.insert(0, entry);
                    return Ok(zone);
                }
            }
            let cache = weak_cache(it, &cls)?;
            let mut instance = it.call_method(&cache, "get", vec![key.clone(), Value::None])?;
            if instance.is_none() {
                let tmp = new_instance(it, &cls, key)?;
                instance = it.call_method(&cache, "setdefault", vec![key.clone(), tmp])?;
                set_source(&instance, Source::Cache);
            }
            if base {
                let st = it.native_state::<State>();
                st.strong_cache.insert(0, (key.clone(), instance.clone()));
                st.strong_cache.truncate(STRONG_CACHE_SIZE);
            }
            Ok(instance)
        }

        // CPython's clinic classmethods carry no docstring.
        #[classmethod(hint(py(text_signature = "($type, file_obj, /, key=None)")))]
        fn from_file(
            cls: This<Value>,
            it: &mut Interp,
            file_obj: &Value,
            #[kw] key: Option<&Value>,
        ) -> R<Value> {
            let Value::Obj(cls) = cls.0 else {
                return Err(it.type_error("from_file() needs a class"));
            };
            let file_repr = it.repr_of(file_obj)?;
            let key = key.cloned().unwrap_or(Value::None);
            let zone = load(it, file_obj, key, Source::File, Some(file_repr))?;
            Ok(opaque_instance(&cls, zone))
        }

        #[classmethod(hint(py(text_signature = "($type, /, key)")))]
        fn no_cache(cls: This<Value>, it: &mut Interp, #[kw] key: &Value) -> R<Value> {
            let Value::Obj(cls) = cls.0 else {
                return Err(it.type_error("no_cache() needs a class"));
            };
            new_instance(it, &cls, key)
        }

        #[classmethod(hint(py(text_signature = "($type, /, *, only_keys=None)")))]
        fn clear_cache(
            cls: This<Value>,
            it: &mut Interp,
            #[kwonly] only_keys: Option<&Value>,
        ) -> R<()> {
            let Value::Obj(cls) = cls.0 else {
                return Err(it.type_error("clear_cache() needs a class"));
            };
            let base = is_base(it, &cls);
            let cache = weak_cache(it, &cls)?;
            match only_keys.filter(|k| !k.is_none()) {
                None => {
                    it.call_method(&cache, "clear", Vec::new())?;
                    if base {
                        it.native_state::<State>().strong_cache.clear();
                    }
                }
                Some(keys) => {
                    let iter = it.get_iter(keys)?;
                    while let Some(k) = it.iter_next(&iter)? {
                        if base {
                            if let Some(i) = strong_position(it, &k)? {
                                it.native_state::<State>().strong_cache.remove(i);
                            }
                        }
                        it.call_method(&cache, "pop", vec![k, Value::None])?;
                    }
                }
            }
            Ok(())
        }

        /// Retrieve a timedelta representing the UTC offset in a zone at the given datetime.
        #[method(hint(py(text_signature = "($self, dt, /)")))]
        fn utcoffset(slf: This<Value>, it: &mut Interp, dt: &Value) -> R<Value> {
            Ok(find(it, &slf.0, dt)?.map_or(Value::None, |t| t.utcoff))
        }

        /// Retrieve a timedelta representing the amount of DST applied in a zone at the given datetime.
        #[method(hint(py(text_signature = "($self, dt, /)")))]
        fn dst(slf: This<Value>, it: &mut Interp, dt: &Value) -> R<Value> {
            Ok(find(it, &slf.0, dt)?.map_or(Value::None, |t| t.dstoff))
        }

        /// Retrieve a string containing the abbreviation for the time zone that applies in a zone at a given datetime.
        #[method(hint(py(text_signature = "($self, dt, /)")))]
        fn tzname(slf: This<Value>, it: &mut Interp, dt: &Value) -> R<Value> {
            Ok(find(it, &slf.0, dt)?.map_or(Value::None, |t| t.tzname))
        }

        /// Given a datetime with local time in UTC, retrieve an adjusted datetime in local time.
        #[method(hint(py(text_signature = "")))]
        fn fromutc(slf: This<Value>, it: &mut Interp, dt: &Value) -> R<Value> {
            let datetime = module_attr(it, "datetime", "datetime")?;
            if !it.isinstance_value(dt, &datetime)? {
                return Err(it.type_error("fromutc: argument must be a datetime"));
            }
            let tz = it.get_attr_str(dt, "tzinfo")?;
            if !tz.is(&slf.0) {
                return Err(it.value_error("fromutc: dt.tzinfo is not self"));
            }
            let ts = timestamp_of(it, dt)?;
            let year = int_attr(it, dt, "year")?;
            let Some((tti, fold)) = with_opaque::<ZoneInfo, _>(&slf.0, |z| {
                let (a, fold) = z.rules.find_utc(ts, year);
                (z.ttinfo(a).utcoff.clone(), fold)
            }) else {
                return Err(it.type_error("descriptor requires a 'zoneinfo.ZoneInfo' object"));
            };
            let local = it.binary_op(crate::ast::BinOp::Add, dt, &tti)?;
            if !fold {
                return Ok(local);
            }
            let replace = it.get_attr_str(&local, "replace")?;
            let kw = vec![(it.str_obj("fold"), Value::Int(1))];
            it.call(&replace, Vec::new(), kw)
        }

        /// Function for serialization with the pickle protocol.
        #[method(name = "__reduce__", hint(py(text_signature = "")))]
        fn reduce(slf: This<Value>, it: &mut Interp) -> R<Value> {
            let Some((source, key)) =
                with_opaque::<ZoneInfo, _>(&slf.0, |z| (z.source, z.key.clone()))
            else {
                return Err(it.type_error("descriptor requires a 'zoneinfo.ZoneInfo' object"));
            };
            if source == Source::File {
                let err = module_attr(it, "pickle", "PicklingError")?;
                let e = it.call(
                    &err,
                    vec![Value::str(
                        "Cannot pickle a ZoneInfo file from a file stream.",
                    )],
                    Vec::new(),
                )?;
                return Err(match e {
                    Value::Obj(o) => o,
                    _ => it.type_error("exceptions must derive from BaseException"),
                });
            }
            let ctor = it.get_attr_str(&slf.0, "_unpickle")?;
            let args = Value::tuple(vec![key, Value::Int((source == Source::Cache) as i64)]);
            Ok(Value::tuple(vec![ctor, args]))
        }

        #[classmethod(hint(py(text_signature = "($type, key, from_cache, /)")))]
        fn _unpickle(
            cls: This<Value>,
            it: &mut Interp,
            key: &Value,
            from_cache: &Value,
        ) -> R<Value> {
            let Value::Obj(c) = &cls.0 else {
                return Err(it.type_error("_unpickle() needs a class"));
            };
            let from_cache = it.index_of(from_cache)? & 0xff != 0;
            if from_cache {
                it.call(&cls.0, vec![key.clone()], Vec::new())
            } else {
                new_instance(it, c, key)
            }
        }

        /// Function to initialize subclasses.
        #[classmethod(hint(py(text_signature = "")))]
        fn __init_subclass__(
            cls: This<Value>,
            it: &mut Interp,
            #[varargs] args: &[Value],
            #[varkw] kwargs: crate::bind::KwArgs,
        ) -> R<()> {
            let _ = (args, kwargs);
            let cache = new_weak_cache(it)?;
            it.set_attr_str(&cls.0, "_weak_cache", cache)
        }

        #[getter]
        fn key(&self) -> Value {
            self.key.clone()
        }

        #[proto(repr)]
        fn repr(slf: This<Value>, it: &mut Interp) -> R<String> {
            let Some((key, file_repr)) =
                with_opaque::<ZoneInfo, _>(&slf.0, |z| (z.key.clone(), z.file_repr.clone()))
            else {
                return Err(it.type_error("descriptor requires a 'zoneinfo.ZoneInfo' object"));
            };
            let cls = it.type_of(&slf.0);
            let name = if is_base(it, &cls) {
                "zoneinfo.ZoneInfo".to_string()
            } else {
                it.tp_name(&cls)
            };
            if key.is_none() {
                return Ok(format!(
                    "{name}.from_file({})",
                    file_repr.unwrap_or_default()
                ));
            }
            Ok(format!("{name}(key={})", it.repr_of(&key)?))
        }

        #[proto(str)]
        fn str(slf: This<Value>, it: &mut Interp) -> R<Value> {
            let key = with_opaque::<ZoneInfo, _>(&slf.0, |z| z.key.clone()).unwrap_or(Value::None);
            if key.is_none() {
                return Ok(Value::string(Self::repr(slf, it)?));
            }
            Ok(key)
        }
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        let ty = crate::bind::type_object::<ZoneInfo>(it);
        dict_set_str(&d, "ZoneInfo", Value::Obj(ty));
    }
}
