//! `pwd` and `grp` on `lumen_os::ident`: the user and group databases.

use crate::object::*;
use crate::vm::Interp;

/// A user or group id argument as `_Py_Uid_Converter` / `_Py_Gid_Converter` take it: an int in
/// range, or -1 (the "no id" value, `(uid_t)-1`). `Ok(None)` when it is out of range.
fn id_arg(it: &mut Interp, v: &Value, kind: &str) -> R<Option<u32>> {
    if matches!(v, Value::Float(_)) {
        return Err(it.type_error("integer argument expected, got float"));
    }
    if !v.is_int_like() {
        let t = it.type_name_of(v);
        return Err(it.type_error(&format!("{kind} should be integer, not {t}")));
    }
    let n = v.as_bigint().and_then(|b| b.to_i64());
    Ok(match n {
        Some(-1) => Some(u32::MAX),
        Some(n) => u32::try_from(n).ok(),
        None => None,
    })
}

fn text(s: &str) -> Value {
    Value::str(s)
}

/// This module provides access to the Unix password database.
/// It is available on all Unix versions.
///
/// Password database entries are reported as 7-tuples containing the following
/// items from the password database (see `<pwd.h>'), in order:
/// pw_name, pw_passwd, pw_uid, pw_gid, pw_gecos, pw_dir, pw_shell.
/// The uid and gid items are integers, all others are strings. An
/// exception is raised if the entry asked for cannot be found.
#[lumen_bind::module(name = "pwd")]
pub mod pwd {
    use super::*;
    use crate::builtins::sysextra::{structseq_full, structseq_type};
    use crate::vm::{dict_set_str, Interp};
    use lumen_os::ident::PasswdRecord;

    struct StructPasswd;

    const FIELDS: [&str; 7] = ["pw_name", "pw_passwd", "pw_uid", "pw_gid", "pw_gecos", "pw_dir", "pw_shell"];

    fn passwd_type(it: &mut Interp) -> Obj {
        structseq_type::<StructPasswd>(it, "pwd", "struct_passwd", &FIELDS, 7)
    }

    fn entry(it: &mut Interp, p: &PasswdRecord) -> Value {
        let ty = passwd_type(it);
        structseq_full(
            &ty,
            vec![
                text(&p.name),
                text(&p.passwd),
                Value::Int(p.uid as i64),
                Value::Int(p.gid as i64),
                text(&p.gecos),
                text(&p.dir),
                text(&p.shell),
            ],
        )
    }

    /// Return the password database entry for the given numeric user ID.
    ///
    /// See `help(pwd)` for more on password database entries.
    #[op]
    fn getpwuid(it: &mut Interp, uidobj: &Value) -> R<Value> {
        let uid = match id_arg(it, uidobj, "uid")? {
            Some(uid) => uid,
            None => return Err(it.new_exc_str("KeyError", "getpwuid(): uid not found")),
        };
        match lumen_os::ident::getpwuid(uid) {
            Some(p) => Ok(entry(it, &p)),
            None => Err(it.new_exc_str("KeyError", &format!("getpwuid(): uid not found: {uid}"))),
        }
    }

    /// Return the password database entry for the given user name.
    ///
    /// See `help(pwd)` for more on password database entries.
    #[op]
    fn getpwnam(it: &mut Interp, name: &str) -> R<Value> {
        if name.contains('\0') {
            return Err(it.value_error("embedded null byte"));
        }
        match lumen_os::ident::getpwnam(name) {
            Some(p) => Ok(entry(it, &p)),
            None => {
                let shown = it.repr_of(&Value::str(name))?;
                Err(it.new_exc_str("KeyError", &format!("getpwnam(): name not found: {shown}")))
            }
        }
    }

    /// Return a list of all available password database entries, in arbitrary order.
    ///
    /// See help(pwd) for more on password database entries.
    #[op]
    fn getpwall(it: &mut Interp) -> Value {
        let all = lumen_os::ident::getpwall();
        Value::list(all.iter().map(|p| entry(it, p)).collect())
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        let ty = passwd_type(it);
        dict_set_str(&d, "struct_passwd", Value::Obj(ty));
    }
}

/// Access to the Unix group database.
///
/// Group entries are reported as 4-tuples containing the following fields
/// from the group database, in order:
///
///   gr_name   - name of the group
///   gr_passwd - group password (encrypted); often empty
///   gr_gid    - numeric ID of the group
///   gr_mem    - list of members
///
/// The gid is an integer, name and password are strings.  (Note that most
/// users are not explicitly listed as members of the groups they are in
/// according to the password database.  Check both databases to get
/// complete membership information.)
#[lumen_bind::module(name = "grp")]
pub mod grp {
    use super::*;
    use crate::builtins::sysextra::{structseq_full, structseq_type};
    use crate::vm::{dict_set_str, Interp};
    use lumen_os::ident::GroupRecord;

    struct StructGroup;

    const FIELDS: [&str; 4] = ["gr_name", "gr_passwd", "gr_gid", "gr_mem"];

    fn group_type(it: &mut Interp) -> Obj {
        structseq_type::<StructGroup>(it, "grp", "struct_group", &FIELDS, 4)
    }

    fn entry(it: &mut Interp, g: &GroupRecord) -> Value {
        let ty = group_type(it);
        structseq_full(
            &ty,
            vec![
                text(&g.name),
                text(&g.passwd),
                Value::Int(g.gid as i64),
                Value::list(g.members.iter().map(|m| text(m)).collect()),
            ],
        )
    }

    /// Return the group database entry for the given numeric group ID.
    ///
    /// If id is not valid, raise KeyError.
    #[op]
    fn getgrgid(it: &mut Interp, id: &Value) -> R<Value> {
        let gid = match id_arg(it, id, "gid")? {
            Some(gid) => gid,
            None => return Err(it.new_exc_str("OverflowError", "Python int too large to convert to C unsigned int")),
        };
        match lumen_os::ident::getgrgid(gid) {
            Some(g) => Ok(entry(it, &g)),
            None => Err(it.new_exc_str("KeyError", &format!("getgrgid(): gid not found: {gid}"))),
        }
    }

    /// Return the group database entry for the given group name.
    ///
    /// If name is not valid, raise KeyError.
    #[op]
    fn getgrnam(it: &mut Interp, name: &str) -> R<Value> {
        if name.contains('\0') {
            return Err(it.value_error("embedded null byte"));
        }
        match lumen_os::ident::getgrnam(name) {
            Some(g) => Ok(entry(it, &g)),
            None => {
                let shown = it.repr_of(&Value::str(name))?;
                Err(it.new_exc_str("KeyError", &format!("getgrnam(): name not found: {shown}")))
            }
        }
    }

    /// Return a list of all available group entries, in arbitrary order.
    ///
    /// An entry whose name starts with '+' or '-' represents an instruction
    /// to use YP/NIS and may not be accessible via getgrnam or getgrgid.
    #[op]
    fn getgrall(it: &mut Interp) -> Value {
        let all = lumen_os::ident::getgrall();
        Value::list(all.iter().map(|g| entry(it, g)).collect())
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        let ty = group_type(it);
        dict_set_str(&d, "struct_group", Value::Obj(ty));
    }
}
