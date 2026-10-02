//! `syslog` on `lumen_os::syslog` (`Modules/syslogmodule.c`).

/// Sends logging messages to the system logger.
#[lumen_bind::module(name = "syslog")]
pub mod syslog {
    use crate::object::*;
    use crate::vm::{dict_get_str, dict_set_str, Interp};

    #[derive(Default)]
    pub struct State {
        open: bool,
    }

    const LOG_USER: i32 = 8;
    const LOG_INFO: i32 = 6;

    fn default_ident(it: &mut Interp) -> Option<String> {
        let sys = it.sys_module.clone()?;
        let d = it.module_dict(&sys);
        let argv = dict_get_str(&d, "argv")?;
        let first = list_of(&argv)?.borrow().first().cloned()?;
        let arg0 = first.as_str()?.to_string();
        let base = arg0.rsplit('/').next().unwrap_or("").to_string();
        (!base.is_empty()).then_some(base)
    }

    fn open(it: &mut Interp, ident: Option<String>, option: i32, facility: i32) {
        let ident = ident.or_else(|| default_ident(it));
        lumen_os::syslog::openlog(ident.as_deref(), option, facility);
        it.native_state::<State>().open = true;
    }

    /// Set logging options of subsequent syslog() calls.
    #[op]
    fn openlog(it: &mut Interp, #[kw] ident: Option<&str>, #[kw] logoption: Option<i32>, #[kw] facility: Option<i32>) {
        open(it, ident.map(str::to_string), logoption.unwrap_or(0), facility.unwrap_or(LOG_USER));
    }

    /// syslog([priority=LOG_INFO,] message)
    /// Send the string message to the system logger.
    #[op]
    fn syslog(it: &mut Interp, #[varargs] args: &[Value]) -> R<()> {
        let bad = |it: &mut Interp| it.type_error("[priority,] message string");
        let (priority, message) = match args {
            [m] => (LOG_INFO, m),
            [p, m] if p.is_int_like() => (it.index_of(p)? as i32, m),
            _ => return Err(bad(it)),
        };
        let Some(message) = message.as_str().map(str::to_string) else {
            return Err(bad(it));
        };
        if message.contains('\0') {
            return Err(it.value_error("embedded null character"));
        }
        if !it.native_state::<State>().open {
            open(it, None, 0, LOG_USER);
        }
        lumen_os::syslog::syslog(priority, &message);
        Ok(())
    }

    /// Reset the syslog module values and call the system library closelog().
    #[op]
    fn closelog(it: &mut Interp) {
        if it.native_state::<State>().open {
            it.native_state::<State>().open = false;
            lumen_os::syslog::closelog();
        }
    }

    /// Set the priority mask to maskpri and return the previous mask value.
    #[op]
    fn setlogmask(maskpri: i32) -> i32 {
        lumen_os::syslog::setlogmask(maskpri)
    }

    /// Calculates the mask for the individual priority pri.
    #[op(name = "LOG_MASK")]
    fn log_mask(pri: i32) -> i32 {
        1i32.wrapping_shl(pri as u32)
    }

    /// Calculates the mask for all priorities up to and including pri.
    #[op(name = "LOG_UPTO")]
    fn log_upto(pri: i32) -> i32 {
        1i32.wrapping_shl(pri.wrapping_add(1) as u32).wrapping_sub(1)
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        for (name, v) in lumen_os::syslog::constants() {
            dict_set_str(&d, name, Value::Int(v));
        }
    }
}
