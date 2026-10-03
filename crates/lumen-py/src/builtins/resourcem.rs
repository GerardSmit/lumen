//! `resource` on `lumen_os::rlimit`: resource limits and usage (`Modules/resource.c`).

#[lumen_bind::module(name = "resource")]
pub mod resource {
    use crate::builtins::sysextra::{structseq_full, structseq_type};
    use crate::object::*;
    use crate::vm::{dict_del_str, dict_set_str, Interp};
    use lumen_os::rlimit::{self, Limits, Rusage};

    struct StructRusage;

    const RUSAGE_FIELDS: [&str; 16] = [
        "ru_utime",
        "ru_stime",
        "ru_maxrss",
        "ru_ixrss",
        "ru_idrss",
        "ru_isrss",
        "ru_minflt",
        "ru_majflt",
        "ru_nswap",
        "ru_inblock",
        "ru_oublock",
        "ru_msgsnd",
        "ru_msgrcv",
        "ru_nsignals",
        "ru_nvcsw",
        "ru_nivcsw",
    ];

    fn rusage_type(it: &mut Interp) -> Obj {
        structseq_type::<StructRusage>(it, "resource", "struct_rusage", &RUSAGE_FIELDS, 16)
    }

    fn check_resource(it: &mut Interp, resource: i32) -> R<()> {
        if resource < 0 || resource >= rlimit::RLIM_NLIMITS {
            return Err(it.value_error("invalid resource specified"));
        }
        Ok(())
    }

    fn limits_of(it: &mut Interp, v: &Value) -> R<Limits> {
        let items = it.iterate_to_vec(v)?;
        if items.len() != 2 {
            return Err(it.value_error("expected a tuple of 2 integers"));
        }
        let cur = it.index_of(&items[0])? as u64 & rlimit::RLIM_INFINITY;
        let max = it.index_of(&items[1])? as u64 & rlimit::RLIM_INFINITY;
        Ok(Limits { cur, max })
    }

    fn limits_value(l: Limits) -> Value {
        Value::tuple(vec![Value::Int(l.cur as i64), Value::Int(l.max as i64)])
    }

    pub fn rusage_value(it: &mut Interp, u: &Rusage) -> Value {
        let ty = rusage_type(it);
        let mut vals = vec![Value::Float(u.utime), Value::Float(u.stime)];
        vals.extend(u.counters.iter().map(|&n| Value::Int(n)));
        structseq_full(&ty, vals)
    }

    #[op]
    fn getrusage(it: &mut Interp, who: i32) -> R<Value> {
        match rlimit::getrusage(who) {
            Ok(u) => Ok(rusage_value(it, &u)),
            Err(e) if e.errno() == 22 => Err(it.value_error("invalid who parameter")),
            Err(e) => Err(it.os_error_errno(e.errno(), None, None)),
        }
    }

    #[op]
    fn getrlimit(it: &mut Interp, resource: i32) -> R<Value> {
        check_resource(it, resource)?;
        match rlimit::getrlimit(resource) {
            Ok(l) => Ok(limits_value(l)),
            Err(e) => Err(it.os_error_errno(e.errno(), None, None)),
        }
    }

    #[op]
    fn setrlimit(it: &mut Interp, resource: i32, limits: &Value) -> R<()> {
        check_resource(it, resource)?;
        let l = limits_of(it, limits)?;
        match rlimit::setrlimit(resource, l) {
            Ok(()) => Ok(()),
            Err(e) if e.errno() == 22 => Err(it.value_error("current limit exceeds maximum limit")),
            Err(e) if e.errno() == 1 => Err(it.value_error("not allowed to raise maximum limit")),
            Err(e) => Err(it.os_error_errno(e.errno(), None, None)),
        }
    }

    #[op]
    fn prlimit(it: &mut Interp, pid: i32, resource: i32, limits: Option<&Value>) -> R<Value> {
        check_resource(it, resource)?;
        let new = match limits {
            None | Some(Value::None) => None,
            Some(v) => Some(limits_of(it, v)?),
        };
        match rlimit::prlimit(pid, resource, new) {
            Ok(l) => Ok(limits_value(l)),
            Err(e) if e.errno() == 22 => Err(it.value_error("current limit exceeds maximum limit")),
            Err(e) => Err(it.os_error_errno(e.errno(), None, None)),
        }
    }

    #[op]
    fn getpagesize() -> i64 {
        rlimit::pagesize()
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        dict_set_str(&d, "error", Value::Obj(it.exc_type("OSError")));
        let ty = rusage_type(it);
        dict_set_str(&d, "struct_rusage", Value::Obj(ty));
        for (name, v) in rlimit::constants() {
            dict_set_str(&d, name, Value::Int(v));
        }
        dict_set_str(&d, "RLIM_INFINITY", Value::Int(rlimit::RLIM_INFINITY as i64));
        if !rlimit::HAVE_PRLIMIT {
            dict_del_str(&d, "prlimit");
        }
    }
}
