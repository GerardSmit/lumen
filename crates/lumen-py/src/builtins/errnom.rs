//! `errno`: the symbolic error codes of the host, generated from lumen-os's errno tables.

/// This module makes available standard errno system symbols.
///
/// The value of each symbol is the corresponding integer value,
/// e.g., on most systems, errno.ENOENT equals the integer 2.
///
/// The dictionary errno.errorcode maps numeric codes to symbol names,
/// e.g., errno.errorcode[2] could be the string 'ENOENT'.
///
/// Symbols that are not relevant to the underlying system are not defined.
///
/// To map error codes to error messages, use the function os.strerror(),
/// e.g. os.strerror(2) could return 'No such file or directory'.
#[lumen_bind::module(name = "errno")]
pub mod errno {
    use crate::object::*;
    use crate::vm::{dict_set_str, Interp};

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        let errorcode = it.new_dict();
        dict_set_str(&d, "errorcode", Value::Obj(errorcode.clone()));
        for (name, num) in lumen_os::errno::names() {
            dict_set_str(&d, name, Value::Int(num as i64));
            let _ = it.dict_set(&errorcode, Value::Int(num as i64), Value::str(name));
        }
    }
}
