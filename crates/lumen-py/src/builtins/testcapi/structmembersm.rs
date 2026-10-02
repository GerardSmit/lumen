//! `_testcapi._test_structmembersType_NewAPI` and `_OldAPI` (`structmember.c`): two types whose
//! attributes are the C member kinds, with the conversions, truncation warnings and errors of
//! `PyMember_GetOne` / `PyMember_SetOne`. Both types share one native class; the old API type
//! is the same class under the name and module `tp_name` gives it.

use super::getargsm::{as_integer, long_like, parse_tuple_and_keywords, Slot};
use super::{int_value, name_obj};
use crate::bind::{This, KwArgs};
use crate::builtins::native::{new_type, with_opaque};
use crate::object::*;
use crate::vm::{dict_set_str, Interp};

const NAMES: [&str; 15] = [
    "T_BOOL",
    "T_BYTE",
    "T_UBYTE",
    "T_SHORT",
    "T_USHORT",
    "T_INT",
    "T_UINT",
    "T_LONG",
    "T_ULONG",
    "T_PYSSIZET",
    "T_FLOAT",
    "T_DOUBLE",
    "T_STRING_INPLACE",
    "T_LONGLONG",
    "T_ULONGLONG",
];

const INPLACE: usize = 12;
const TRUNCATED: &str = "Writing negative value into unsigned field";

#[derive(Clone, Copy)]
enum Cell {
    Int(i128),
    Float(f64),
}

#[lumen_bind::class(module = "_testcapi", name = "_test_structmembersType_NewAPI")]
pub struct StructMembers {
    cells: [Cell; 15],
    inplace: Vec<u8>,
}

/// Wraps `n` to a `bits`-wide integer, signed or not.
fn wrap(n: i128, bits: u32, signed: bool) -> i128 {
    let m = n & ((1i128 << bits) - 1);
    if signed && m >= 1i128 << (bits - 1) {
        m - (1i128 << bits)
    } else {
        m
    }
}

fn width(index: usize) -> (u32, bool) {
    match index {
        1 => (8, true),
        2 => (8, false),
        3 => (16, true),
        4 => (16, false),
        5 => (32, true),
        6 => (32, false),
        8 | 14 => (64, false),
        _ => (64, true),
    }
}

/// The value a write stores and the warning it raises.
fn convert(it: &mut Interp, index: usize, v: &Value) -> R<(Cell, Option<&'static str>)> {
    match index {
        0 => match v {
            Value::Bool(b) => Ok((Cell::Int(i128::from(*b)), None)),
            _ => Err(it.type_error("attribute value type must be bool")),
        },
        1..=5 | 7 | 13 => {
            let n = long_like(it, v, i128::from(i64::MIN), i128::from(i64::MAX), "long")?;
            let (bits, signed) = width(index);
            let stored = wrap(n, bits, signed);
            let warning = match index {
                1 if stored != n => Some("Truncation of value to char"),
                2 if stored != n => Some("Truncation of value to unsigned char"),
                3 if stored != n => Some("Truncation of value to short"),
                4 if stored != n => Some("Truncation of value to unsigned short"),
                5 if stored != n => Some("Truncation of value to int"),
                _ => None,
            };
            Ok((Cell::Int(stored), warning))
        }
        6 | 8 | 14 => {
            let b = as_integer(it, v)?;
            let (bits, signed) = width(index);
            let n = match b.to_i128() {
                Some(n) if n >= i128::from(i64::MIN) && n <= i128::from(u64::MAX) => n,
                _ => {
                    let what = if b.is_negative() { "long" } else { "unsigned long" };
                    return Err(it.overflow_err(&format!("Python int too large to convert to C {what}")));
                }
            };
            let stored = wrap(n, bits, signed);
            let warning = if n < 0 {
                Some(TRUNCATED)
            } else if index == 6 && n > i128::from(u32::MAX) {
                Some("Truncation of value to unsigned int")
            } else {
                None
            };
            Ok((Cell::Int(stored), warning))
        }
        9 => {
            if !v.is_int_like() {
                return Err(it.type_error("an integer is required"));
            }
            let n = long_like(it, v, i128::from(i64::MIN), i128::from(i64::MAX), "ssize_t")?;
            Ok((Cell::Int(n), None))
        }
        10 => Ok((Cell::Float(f64::from(it.float_arg(v)? as f32)), None)),
        11 => Ok((Cell::Float(it.float_arg(v)?), None)),
        _ => Err(it.type_error("readonly attribute")),
    }
}

impl StructMembers {
    fn read(&self, index: usize) -> Value {
        if index == INPLACE {
            let end = self.inplace.iter().position(|&b| b == 0).unwrap_or(self.inplace.len());
            return Value::string(String::from_utf8_lossy(&self.inplace[..end]).into_owned());
        }
        match self.cells[index] {
            Cell::Int(n) if index == 0 => Value::Bool(n != 0),
            Cell::Int(n) => int_value(n),
            Cell::Float(f) => Value::Float(f),
        }
    }
}

fn slot_int(slots: &[Slot], i: usize, bits: u32, signed: bool) -> i128 {
    let n = match slots.get(i).and_then(|s| s.out.as_ref()) {
        Some(Value::Int(n)) => i128::from(*n),
        Some(Value::Bool(b)) => i128::from(*b),
        Some(Value::Obj(o)) => match &o.kind {
            Kind::Int(b) => b.to_i128_wrapping(),
            _ => 0,
        },
        _ => 0,
    };
    wrap(n, bits, signed)
}

fn slot_float(slots: &[Slot], i: usize, single: bool) -> f64 {
    match slots.get(i).and_then(|s| s.out.as_ref()) {
        Some(Value::Float(f)) if single => f64::from(*f as f32),
        Some(Value::Float(f)) => *f,
        _ => 0.0,
    }
}

#[lumen_bind::methods]
impl StructMembers {
    #[constructor]
    fn new(it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<StructMembers> {
        let kwlist: Vec<String> = NAMES.iter().map(|n| (*n).to_string()).collect();
        let kw = if kwargs.is_empty() { None } else { Some(it.kwargs_to_dict(&kwargs.to_vec())?) };
        let mut slots: Vec<Slot> = Vec::new();
        parse_tuple_and_keywords(it, args, kw.as_ref(), "|bbBhHiIlknfds#LK", &kwlist, &mut slots, true)?;
        let mut cells = [Cell::Int(0); 15];
        for (i, cell) in cells.iter_mut().enumerate() {
            *cell = match i {
                10 => Cell::Float(slot_float(&slots, i, true)),
                11 => Cell::Float(slot_float(&slots, i, false)),
                INPLACE => Cell::Int(0),
                _ => {
                    let (bits, signed) = if i == 0 { (8, true) } else { width(i) };
                    Cell::Int(slot_int(&slots, i, bits, signed))
                }
            };
        }
        let inplace = match slots.get(INPLACE).and_then(|s| s.out.as_ref()) {
            Some(Value::Obj(o)) => match &o.kind {
                Kind::Bytes(b) => b.clone(),
                _ => Vec::new(),
            },
            _ => Vec::new(),
        };
        if inplace.len() > 5 {
            return Err(it.value_error("string too long"));
        }
        Ok(StructMembers { cells, inplace })
    }

    #[getter(name = "T_BOOL")]
    fn t_bool(&self) -> Value {
        self.read(0)
    }

    #[getter(name = "T_BYTE")]
    fn t_byte(&self) -> Value {
        self.read(1)
    }

    #[getter(name = "T_UBYTE")]
    fn t_ubyte(&self) -> Value {
        self.read(2)
    }

    #[getter(name = "T_SHORT")]
    fn t_short(&self) -> Value {
        self.read(3)
    }

    #[getter(name = "T_USHORT")]
    fn t_ushort(&self) -> Value {
        self.read(4)
    }

    #[getter(name = "T_INT")]
    fn t_int(&self) -> Value {
        self.read(5)
    }

    #[getter(name = "T_UINT")]
    fn t_uint(&self) -> Value {
        self.read(6)
    }

    #[getter(name = "T_LONG")]
    fn t_long(&self) -> Value {
        self.read(7)
    }

    #[getter(name = "T_ULONG")]
    fn t_ulong(&self) -> Value {
        self.read(8)
    }

    #[getter(name = "T_PYSSIZET")]
    fn t_pyssizet(&self) -> Value {
        self.read(9)
    }

    #[getter(name = "T_FLOAT")]
    fn t_float(&self) -> Value {
        self.read(10)
    }

    #[getter(name = "T_DOUBLE")]
    fn t_double(&self) -> Value {
        self.read(11)
    }

    #[getter(name = "T_STRING_INPLACE")]
    fn t_string_inplace(&self) -> Value {
        self.read(12)
    }

    #[getter(name = "T_LONGLONG")]
    fn t_longlong(&self) -> Value {
        self.read(13)
    }

    #[getter(name = "T_ULONGLONG")]
    fn t_ulonglong(&self) -> Value {
        self.read(14)
    }

    #[proto(setattr)]
    fn setattr(slf: This<&Value>, it: &mut Interp, name: &Value, value: &Value) -> R<()> {
        let n = name_obj(it, name)?;
        if let Some(index) = member_index(&n) {
            let (cell, warning) = convert(it, index, value)?;
            with_opaque::<StructMembers, _>(&slf, |s| s.cells[index] = cell);
            if let Some(msg) = warning {
                crate::builtins::warningsm::warn_category(it, "RuntimeWarning", msg, 1)?;
            }
            return Ok(());
        }
        let cls = it.type_of(&slf);
        it.generic_setattr(&slf, &cls, &n, value.clone())
    }

    #[proto(delattr)]
    fn delattr(slf: This<&Value>, it: &mut Interp, name: &Value) -> R<()> {
        let n = name_obj(it, name)?;
        if member_index(&n).is_some() {
            return Err(it.type_error("can't delete numeric/char attribute"));
        }
        let cls = it.type_of(&slf);
        it.generic_delattr(&slf, &cls, &n)
    }
}

fn member_index(name: &Obj) -> Option<usize> {
    let s = Value::Obj(name.clone());
    let s = s.as_str()?;
    NAMES.iter().position(|n| *n == s)
}

#[lumen_bind::module(name = "_testcapi")]
pub mod structmembersm {
    use super::*;

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let new_api = crate::bind::type_object::<StructMembers>(it);
        let old_api = new_type(it, "builtins", "test_structmembersType_OldAPI", None, Layout::Other);
        crate::bind::install_all::<StructMembers>(&old_api);
        if let Some(d) = old_api.dict.borrow().as_ref() {
            dict_set_str(d, "__doc__", Value::str("Type containing all structmember types"));
        }
        let d = it.module_dict(m);
        dict_set_str(&d, "_test_structmembersType_NewAPI", Value::Obj(new_api));
        dict_set_str(&d, "_test_structmembersType_OldAPI", Value::Obj(old_api));
    }
}
